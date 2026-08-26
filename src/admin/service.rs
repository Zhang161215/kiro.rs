//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;

use super::error::AdminServiceError;
use super::proxy_pool::{ProxyEntry, ProxyPoolManager};
use super::usage_stats::{
    CredentialDistribution, ModelDistribution, OverviewStats, StatsQueryWindow, TimeSeriesPoint,
    UsageAggregator,
};
use super::types::{
    AddCredentialRequest, AddCredentialResponse, AdminKeysResponse, BalanceResponse,
    CredentialStatusItem, CredentialsStatusResponse, KeyEntry, KvCacheConfigResponse,
    LoadBalancingModeResponse, ModelsConfigResponse, ProxyCheckRequest, ProxyCheckResponse,
    RequestDetailItem, RequestDetailsResponse, SetKvCacheConfigRequest,
    SetLoadBalancingModeRequest, SetModelsRequest, UpdateCredentialRequest,
};

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;
/// 代理连通性探测目标：返回 204 空响应，体积小、全球可达
const PROXY_PROBE_URL: &str = "https://www.gstatic.com/generate_204";
/// 代理探测超时（秒）。探测要快速给结论，不能用业务请求那种长超时
const PROXY_PROBE_TIMEOUT_SECS: u64 = 8;
/// 请求明细默认返回条数
const REQUEST_DETAILS_DEFAULT_LIMIT: usize = 100;
/// 请求明细最大返回条数
const REQUEST_DETAILS_MAX_LIMIT: usize = 1000;
/// 模拟 KV 缓存记录文件名
const KV_CACHE_RECORDS_FILE: &str = "kiro_kv_cache_records.jsonl";

/// 凭据编辑里的代理字段：把 `host:port:user:pass` 拆成 URL + 账密。
/// 表单已显式给出的用户名/密码优先；未给出时用解析结果补上。
fn normalize_proxy_fields(
    proxy_url: Option<String>,
    proxy_username: Option<String>,
    proxy_password: Option<String>,
) -> (Option<String>, Option<String>, Option<String>) {
    let Some(raw) = proxy_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return (proxy_url, proxy_username, proxy_password);
    };
    match super::proxy_pool::parse_proxy_line(raw) {
        Ok(parsed) => {
            let username = match proxy_username {
                Some(s) => Some(s),
                None => parsed.username,
            };
            let password = match proxy_password {
                Some(s) => Some(s),
                None => parsed.password,
            };
            // 裸 host:port 不要改写成 http://，否则旁路账密在探测/出站时
            // 不会再被当成 SOCKS5h（HTTP CONNECT 打到 SOCKS 口会超时）。
            let url = if raw.contains("://") {
                parsed.url
            } else {
                parsed
                    .url
                    .split_once("://")
                    .map(|(_, hp)| hp.to_string())
                    .unwrap_or(parsed.url)
            };
            (Some(url), username, password)
        }
        Err(_) => (proxy_url, proxy_username, proxy_password),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KvCacheRecordRow {
    recorded_at: String,
    request_id: String,
    endpoint: String,
    model: String,
    #[serde(default)]
    credential_id: u64,
    stream: bool,
    cache_hit: bool,
    cache_creation_input_tokens: i32,
    cache_read_input_tokens: i32,
    input_tokens: i32,
    output_tokens: i32,
    #[serde(default)]
    credits_used: f64,
    #[serde(default)]
    special_settings: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct ModelPricing {
    input_per_million: f64,
    output_per_million: f64,
    cache_write_per_million: f64,
    cache_read_per_million: f64,
}

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    cache_path: Option<PathBuf>,
    /// 已注册的端点名称集合（用于 add_credential 校验）
    known_endpoints: HashSet<String>,
    request_details_path: PathBuf,
    /// 代理 IP 池（以 URL 为主键，独立于凭据）
    proxy_pool: Arc<ProxyPoolManager>,
    usage: Mutex<UsageStatsCache>,
}

struct UsageStatsCache {
    file_len: u64,
    agg: UsageAggregator,
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        known_endpoints: impl IntoIterator<Item = String>,
    ) -> Self {
        let cache_dir = token_manager
            .cache_dir()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let cache_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_balance_cache.json"));
        let request_details_path = cache_dir.join(KV_CACHE_RECORDS_FILE);

        let balance_cache = Self::load_balance_cache_from(&cache_path);

        let proxy_pool_path = token_manager.cache_dir().map(|d| d.join("proxy_pool.json"));
        let proxy_pool = Arc::new(ProxyPoolManager::new(
            proxy_pool_path,
            token_manager.config().tls_backend,
        ));
        let usage = Mutex::new(UsageStatsCache {
            file_len: u64::MAX,
            agg: UsageAggregator::new(),
        });

        Self {
            token_manager,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
            request_details_path,
            proxy_pool,
            usage,
        }
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        let snapshot = self.token_manager.snapshot();
        let default_endpoint = self.token_manager.config().default_endpoint.clone();

        let mut credentials: Vec<CredentialStatusItem> = snapshot
            .entries
            .into_iter()
            .map(|entry| CredentialStatusItem {
                id: entry.id,
                priority: entry.priority,
                disabled: entry.disabled,
                failure_count: entry.failure_count,
                is_current: entry.id == snapshot.current_id,
                expires_at: entry.expires_at,
                auth_method: entry.auth_method,
                has_profile_arn: entry.has_profile_arn,
                refresh_token_hash: entry.refresh_token_hash,
                api_key_hash: entry.api_key_hash,
                masked_api_key: entry.masked_api_key,
                email: entry.email,
                success_count: entry.success_count,
                last_used_at: entry.last_used_at.clone(),
                has_proxy: entry.has_proxy,
                proxy_url: entry.proxy_url,
                refresh_failure_count: entry.refresh_failure_count,
                disabled_reason: entry.disabled_reason,
                endpoint: entry.endpoint.unwrap_or_else(|| default_endpoint.clone()),
                auth_region: entry.auth_region,
                api_region: entry.api_region,
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
            credentials,
        }
    }

    /// 设置凭据禁用状态
    pub fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        // 先获取当前凭据 ID，用于判断是否需要切换
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;

        // 只有禁用的是当前凭据时才尝试切换到下一个
        if disabled && id == current_id {
            let _ = self.token_manager.switch_to_next();
        }
        Ok(())
    }

    /// 设置凭据优先级
    pub fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 重置失败计数并重新启用
    pub fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // 先查缓存
        {
            let cache = self.balance_cache.lock();
            if let Some(cached) = cache.get(&id) {
                let now = Utc::now().timestamp() as f64;
                if (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    tracing::debug!("凭据 #{} 余额命中缓存", id);
                    return Ok(cached.data.clone());
                }
            }
        }

        // 缓存未命中或已过期，从上游获取
        let balance = self.fetch_balance(id).await?;

        // 更新缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance.clone(),
                },
            );
        }
        self.save_balance_cache();

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        let remaining = (usage_limit - current_usage).max(0.0);
        let usage_percentage = if usage_limit > 0.0 {
            (current_usage / usage_limit * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
        })
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 校验端点名：未指定则默认合法，指定则必须已注册
        if let Some(ref name) = req.endpoint {
            if !self.known_endpoints.contains(name) {
                let mut known: Vec<&str> =
                    self.known_endpoints.iter().map(|s| s.as_str()).collect();
                known.sort();
                return Err(AdminServiceError::InvalidCredential(format!(
                    "未知端点 \"{}\"，已注册端点: {:?}",
                    name, known
                )));
            }
        }

        // 构建凭据对象
        let email = req.email.clone();
        let (proxy_url, proxy_username, proxy_password) =
            normalize_proxy_fields(req.proxy_url, req.proxy_username, req.proxy_password);
        let new_cred = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: req.refresh_token,
            profile_arn: req.profile_arn,
            expires_at: None,
            auth_method: Some(req.auth_method),
            client_id: req.client_id,
            client_secret: req.client_secret,
            provider: req.provider,
            token_endpoint: req.token_endpoint,
            issuer_url: req.issuer_url,
            scopes: req.scopes,
            priority: req.priority,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            proxy_url,
            proxy_username,
            proxy_password,
            disabled: false, // 新添加的凭据默认启用
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
            // 经 Admin API 主动添加的凭据必须落盘，否则重启即丢
            ephemeral: false,
        };

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取订阅等级，避免首次请求时 Free 账号绕过 Opus 模型过滤
        if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("添加凭据后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_credential(id)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        Ok(())
    }

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {
        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 设置负载均衡模式
    pub fn set_load_balancing_mode(
        &self,
        req: SetLoadBalancingModeRequest,
    ) -> Result<LoadBalancingModeResponse, AdminServiceError> {
        // 验证模式值
        if req.mode != "priority" && req.mode != "balanced" {
            return Err(AdminServiceError::InvalidCredential(
                "mode 必须是 'priority' 或 'balanced'".to_string(),
            ));
        }

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    /// 获取 KV 缓存配置
    pub fn get_kv_cache_config(&self) -> KvCacheConfigResponse {
        use crate::anthropic::kv_cache::{get_cache_read_efficiency, get_kv_cache_ttl_secs};
        KvCacheConfigResponse {
            cache_read_efficiency: get_cache_read_efficiency(),
            kv_cache_ttl_secs: get_kv_cache_ttl_secs(),
        }
    }

    /// 设置 KV 缓存配置
    pub fn set_kv_cache_config(
        &self,
        req: SetKvCacheConfigRequest,
    ) -> Result<KvCacheConfigResponse, AdminServiceError> {
        use crate::anthropic::kv_cache::set_kv_cache_config;
        use crate::model::config::Config;

        let config_path = self
            .token_manager
            .config()
            .config_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| {
                AdminServiceError::InternalError("配置文件路径未知".to_string())
            })?;

        let mut config = Config::load(&config_path)
            .map_err(|e| AdminServiceError::InternalError(format!("加载配置失败: {}", e)))?;

        if let Some(efficiency) = req.cache_read_efficiency {
            let clamped = efficiency.clamp(0.0, 1.0);
            config.cache_read_efficiency = clamped;
        }
        if let Some(ttl) = req.kv_cache_ttl_secs {
            config.kv_cache_ttl_secs = ttl.max(60);
        }

        config
            .save()
            .map_err(|e| AdminServiceError::InternalError(format!("保存配置失败: {}", e)))?;

        // 更新运行时全局配置
        set_kv_cache_config(config.cache_read_efficiency, config.kv_cache_ttl_secs);

        Ok(KvCacheConfigResponse {
            cache_read_efficiency: config.cache_read_efficiency,
            kv_cache_ttl_secs: config.kv_cache_ttl_secs,
        })
    }

    /// 获取模型配置（读全局注册表）
    pub fn get_models(&self) -> ModelsConfigResponse {
        ModelsConfigResponse {
            models: crate::anthropic::model_registry::get_models(),
        }
    }

    /// 设置模型配置：校验 → 持久化到 config.json → 热更新全局注册表（立即生效）
    pub fn set_models(
        &self,
        req: SetModelsRequest,
    ) -> Result<ModelsConfigResponse, AdminServiceError> {
        use crate::model::config::Config;

        // 基础校验：id / kiroModelId 不能为空，id 不能重复
        let mut seen = std::collections::HashSet::new();
        for m in &req.models {
            if m.id.trim().is_empty() {
                return Err(AdminServiceError::InvalidCredential(
                    "模型 id 不能为空".to_string(),
                ));
            }
            if m.kiro_model_id.trim().is_empty() {
                return Err(AdminServiceError::InvalidCredential(format!(
                    "模型 {} 的 kiroModelId 不能为空",
                    m.id
                )));
            }
            if !seen.insert(m.id.to_lowercase()) {
                return Err(AdminServiceError::InvalidCredential(format!(
                    "模型 id 重复: {}",
                    m.id
                )));
            }
        }

        // 持久化到 config.json
        let config_path = self
            .token_manager
            .config()
            .config_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| AdminServiceError::InternalError("配置文件路径未知".to_string()))?;

        let mut config = Config::load(&config_path)
            .map_err(|e| AdminServiceError::InternalError(format!("加载配置失败: {}", e)))?;
        config.models = req.models.clone();
        config
            .save()
            .map_err(|e| AdminServiceError::InternalError(format!("保存配置失败: {}", e)))?;

        // 热更新全局注册表，立即生效无需重启
        crate::anthropic::model_registry::set_models(req.models.clone());

        Ok(ModelsConfigResponse { models: req.models })
    }

    /// 重启服务（兜底）：返回后延迟退出进程，由 Docker restart:always 自动拉起
    pub fn restart_service(&self) {
        tokio::spawn(async {
            // 给 HTTP 响应发出的时间
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            tracing::warn!("收到 admin 重启请求，进程即将退出（由容器 restart 策略拉起）");
            std::process::exit(0);
        });
    }

    /// 强制刷新指定凭据的 Token
    pub async fn force_refresh_token(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .force_refresh_token_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    /// 获取请求明细（来自模拟 KV 缓存 JSONL）
    pub fn get_request_details(
        &self,
        limit: Option<usize>,
    ) -> Result<RequestDetailsResponse, AdminServiceError> {
        let limit = limit
            .unwrap_or(REQUEST_DETAILS_DEFAULT_LIMIT)
            .clamp(1, REQUEST_DETAILS_MAX_LIMIT);

        let file = match File::open(&self.request_details_path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RequestDetailsResponse {
                    total: 0,
                    records: Vec::new(),
                });
            }
            Err(e) => {
                return Err(AdminServiceError::InternalError(format!(
                    "读取请求明细文件失败: {}",
                    e
                )));
            }
        };

        let reader = BufReader::new(file);
        let mut rows = Vec::new();

        for (line_no, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(line) => line,
                Err(e) => {
                    tracing::warn!("读取请求明细第 {} 行失败: {}", line_no + 1, e);
                    continue;
                }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let stream =
                serde_json::Deserializer::from_str(line).into_iter::<KvCacheRecordRow>();
            let mut parsed = false;
            let mut had_error = false;
            for item in stream {
                match item {
                    Ok(row) => {
                        rows.push(row);
                        parsed = true;
                    }
                    Err(e) => {
                        tracing::warn!("解析请求明细第 {} 行失败: {}", line_no + 1, e);
                        had_error = true;
                        break;
                    }
                }
            }
            if !parsed && !had_error {
                tracing::warn!("解析请求明细第 {} 行失败: 空或无效 JSON", line_no + 1);
            }
        }

        let total = rows.len();
        let records = rows
            .into_iter()
            .rev()
            .take(limit)
            .map(Self::map_request_detail)
            .collect();

        Ok(RequestDetailsResponse { total, records })
    }

    /// 清空请求明细（截断 JSONL 文件）
    pub fn clear_request_details(&self) -> Result<(), AdminServiceError> {
        match File::create(&self.request_details_path) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AdminServiceError::InternalError(format!(
                "清空请求明细文件失败: {}",
                e
            ))),
        }
    }

    // ============ 请求明细映射与费用计算 ============
    fn map_request_detail(row: KvCacheRecordRow) -> RequestDetailItem {
        let total_input_tokens = row.input_tokens.max(0);
        let cache_creation_tokens = row.cache_creation_input_tokens.max(0);
        let cached_tokens = row.cache_read_input_tokens.max(0);
        let input_tokens = total_input_tokens
            .saturating_sub(cache_creation_tokens.saturating_add(cached_tokens))
            .max(0);
        let output_tokens = row.output_tokens.max(0);
        let cache_ratio = if total_input_tokens > 0 {
            (cached_tokens as f64 / total_input_tokens as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let cost_usd = Self::calculate_request_cost(
            &row.model,
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cached_tokens,
        );

        RequestDetailItem {
            recorded_at: row.recorded_at,
            request_id: row.request_id,
            endpoint: row.endpoint,
            model: row.model,
            credential_id: row.credential_id,
            stream: row.stream,
            cache_hit: row.cache_hit,
            input_tokens,
            cached_tokens,
            output_tokens,
            cache_ratio,
            cost_usd,
            credits_used: if row.credits_used.is_finite() {
                row.credits_used.max(0.0)
            } else {
                0.0
            },
            special_settings: row.special_settings,
        }
    }

    fn calculate_request_cost(
        model: &str,
        input_tokens: i32,
        output_tokens: i32,
        cache_creation_tokens: i32,
        cache_read_tokens: i32,
    ) -> f64 {
        let pricing = Self::model_pricing(model);
        let input = input_tokens.max(0) as f64;
        let output = output_tokens.max(0) as f64;
        let cache_creation = cache_creation_tokens.max(0) as f64;
        let cache_read = cache_read_tokens.max(0) as f64;
        let usd = (input * pricing.input_per_million
            + cache_creation * pricing.cache_write_per_million
            + cache_read * pricing.cache_read_per_million
            + output * pricing.output_per_million)
            / 1_000_000.0;

        if usd.is_finite() { usd.max(0.0) } else { 0.0 }
    }

    fn model_pricing(model: &str) -> ModelPricing {
        let model = model.to_lowercase();
        if model.contains("opus") {
            ModelPricing {
                input_per_million: 15.0,
                output_per_million: 75.0,
                cache_write_per_million: 18.75,
                cache_read_per_million: 1.5,
            }
        } else if model.contains("haiku") {
            ModelPricing {
                input_per_million: 0.8,
                output_per_million: 4.0,
                cache_write_per_million: 1.0,
                cache_read_per_million: 0.08,
            }
        } else {
            // Sonnet（默认）
            ModelPricing {
                input_per_million: 3.0,
                output_per_million: 15.0,
                cache_write_per_million: 3.75,
                cache_read_per_million: 0.3,
            }
        }
    }

    // ============ 余额缓存持久化 ============

    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<u64, CachedBalance> {
        let path = match cache_path {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // 文件中使用字符串 key 以兼容 JSON 格式
        let map: HashMap<String, CachedBalance> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析余额缓存失败，将忽略: {}", e);
                return HashMap::new();
            }
        };

        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                let id = k.parse::<u64>().ok()?;
                // 丢弃超过 TTL 的条目
                if (now - v.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    Some((id, v))
                } else {
                    None
                }
            })
            .collect()
    }

    fn save_balance_cache(&self) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // 持有锁期间完成序列化和写入，防止并发损坏
        let cache = self.balance_cache.lock();
        let map: HashMap<String, &CachedBalance> =
            cache.iter().map(|(k, v)| (k.to_string(), v)).collect();

        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("保存余额缓存失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化余额缓存失败: {}", e),
        }
    }

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();

        // 1. 凭据不存在
        if msg.contains("不存在") {
            return AdminServiceError::NotFound { id };
        }

        // 2. API Key 凭据不支持刷新：客户端请求错误，映射为 400
        if msg.contains("API Key 凭据不支持刷新") {
            return AdminServiceError::InvalidCredential(msg);
        }

        // 3. 上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error =
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭证已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("Token 刷新失败") ||
            msg.contains("暂时不可用") ||
            // 网络错误（reqwest 错误）
            msg.contains("error trying to connect") ||
            msg.contains("connection") ||
            msg.contains("timeout") ||
            msg.contains("timed out");

        if is_upstream_error {
            AdminServiceError::UpstreamError(msg)
        } else {
            // 4. 默认归类为内部错误（本地验证失败、配置错误等）
            // 包括：缺少 refreshToken、refreshToken 已被截断、无法生成 machineId 等
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类添加凭据错误
    fn classify_add_error(&self, e: anyhow::Error) -> AdminServiceError {
        let msg = e.to_string();

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
            || msg.contains("凭据已存在")
            || msg.contains("refreshToken 重复")
            || msg.contains("kiroApiKey 重复")
            || msg.contains("缺少 kiroApiKey")
            || msg.contains("kiroApiKey 为空")
            || msg.contains("凭证已过期或无效")
            || msg.contains("权限不足")
            || msg.contains("已被限流");

        if is_invalid_credential {
            AdminServiceError::InvalidCredential(msg)
        } else if msg.contains("error trying to connect")
            || msg.contains("connection")
            || msg.contains("timeout")
        {
            AdminServiceError::UpstreamError(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类删除凭据错误
    fn classify_delete_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据") {
            AdminServiceError::InvalidCredential(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 部分更新凭据可编辑字段（PATCH /credentials/:id）
    pub fn update_credential(
        &self,
        id: u64,
        req: UpdateCredentialRequest,
    ) -> Result<(), AdminServiceError> {
        let (proxy_url, proxy_username, proxy_password) =
            normalize_proxy_fields(req.proxy_url, req.proxy_username, req.proxy_password);
        self.token_manager
            .update_credential_fields(
                id,
                req.email,
                req.auth_region,
                req.api_region,
                proxy_url,
                proxy_username,
                proxy_password,
            )
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("不存在") {
                    AdminServiceError::NotFound { id }
                } else {
                    AdminServiceError::InternalError(msg)
                }
            })?;
        // 清除该凭据的余额缓存（可能因 region/proxy 变化导致结果不同）
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        Ok(())
    }

    /// 检测代理连通性。
    ///
    /// `req` 里给了 `proxy_url` 就检测这一组待保存的参数（可在写库前先试通），
    /// 否则回退到该凭据当前已保存的代理配置。
    ///
    /// 编辑页不回显账密：若只传了已保存的 URL、账密为空，必须用库里的用户名/密码，
    /// 并把裸 `host:port` + 账密按 SOCKS5h 出站（与真实请求同一条规则）。
    pub async fn check_proxy(
        &self,
        id: u64,
        req: ProxyCheckRequest,
    ) -> Result<ProxyCheckResponse, AdminServiceError> {
        let overlay_user = req.proxy_username.filter(|s| !s.trim().is_empty());
        let overlay_pass = req.proxy_password.filter(|s| !s.trim().is_empty());

        let proxy = match req.proxy_url.as_deref().map(str::trim) {
            Some(url) if !url.is_empty() => {
                let stored = self.token_manager.proxy_fields_for(id).ok();
                let url_is_saved = stored
                    .as_ref()
                    .and_then(|(u, _, _)| u.as_deref().map(str::trim))
                    == Some(url);

                if overlay_user.is_none() && overlay_pass.is_none() && url_is_saved {
                    self.token_manager
                        .effective_proxy_for(id, None)
                        .map_err(|_| AdminServiceError::NotFound { id })?
                } else {
                    let pool = self.proxy_pool.find_by_url(url);
                    let user = overlay_user.or_else(|| {
                        pool.as_ref().and_then(|e| e.username.clone())
                    });
                    let pass = overlay_pass.or_else(|| {
                        pool.as_ref().and_then(|e| e.password.clone())
                    });
                    match ProxyConfig::from_url_and_auth(url, user, pass) {
                        Ok(cfg) => Some(cfg),
                        Err(e) => {
                            return Ok(ProxyCheckResponse {
                                ok: false,
                                proxy_url: Some(url.to_string()),
                                latency_ms: None,
                                error: Some(e.to_string()),
                            });
                        }
                    }
                }
            }
            _ => self
                .token_manager
                .effective_proxy_for(id, None)
                .map_err(|_| AdminServiceError::NotFound { id })?,
        };

        let Some(proxy) = proxy else {
            return Ok(ProxyCheckResponse {
                ok: false,
                proxy_url: None,
                latency_ms: None,
                error: Some("该凭据未配置代理".to_string()),
            });
        };

        Ok(Self::probe_proxy(&proxy, self.token_manager.config().tls_backend).await)
    }

    /// 通过给定代理请求探测端点，2xx/3xx 视为连通。
    async fn probe_proxy(
        proxy: &ProxyConfig,
        tls_backend: crate::model::config::TlsBackend,
    ) -> ProxyCheckResponse {
        let fail = |error: String| ProxyCheckResponse {
            ok: false,
            proxy_url: Some(proxy.url.clone()),
            latency_ms: None,
            error: Some(error),
        };

        let client = match build_client(Some(proxy), PROXY_PROBE_TIMEOUT_SECS, tls_backend) {
            Ok(client) => client,
            Err(e) => return fail(format!("代理地址无法解析: {}", e)),
        };

        let started = std::time::Instant::now();
        match client.get(PROXY_PROBE_URL).send().await {
            Ok(resp) if resp.status().is_success() || resp.status().is_redirection() => {
                ProxyCheckResponse {
                    ok: true,
                    proxy_url: Some(proxy.url.clone()),
                    latency_ms: Some(started.elapsed().as_millis().min(u32::MAX as u128) as u32),
                    error: None,
                }
            }
            Ok(resp) => fail(format!("探测端点返回非预期状态: {}", resp.status())),
            Err(e) => fail(super::proxy_pool::describe_reqwest_error(&e)),
        }
    }

    // ============ 代理 IP 池 ============

    pub fn list_proxies(&self) -> Vec<ProxyEntry> {
        self.proxy_pool.list()
    }

    pub fn add_proxy(&self, url: String, label: Option<String>) -> Result<ProxyEntry, AdminServiceError> {
        self.proxy_pool
            .add(url, label)
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))
    }

    pub fn batch_add_proxies(&self, urls: Vec<String>) -> (usize, Vec<String>) {
        let (added, errors) = self.proxy_pool.batch_add(urls);
        (added.len(), errors)
    }

    pub fn delete_proxy(&self, id: u64) -> Result<(), AdminServiceError> {
        self.proxy_pool
            .delete(id)
            .map_err(|_| AdminServiceError::NotFound { id })
    }

    pub fn set_proxy_enabled(&self, id: u64, enabled: bool) -> Result<(), AdminServiceError> {
        self.proxy_pool
            .set_enabled(id, enabled)
            .map_err(|_| AdminServiceError::NotFound { id })
    }

    pub async fn check_pool_proxy(&self, id: u64) -> Result<ProxyEntry, AdminServiceError> {
        self.proxy_pool
            .check_one(id)
            .await
            .map_err(|_| AdminServiceError::NotFound { id })
    }

    pub async fn check_all_pool_proxies(&self) -> (usize, usize, usize) {
        let s = self.proxy_pool.check_all().await;
        (s.healthy, s.unhealthy, s.auto_disabled)
    }

    /// 把一个代理 URL 分配给若干凭据（写入 URL + 账密）。返回成功条数。
    pub fn assign_proxy_to_credentials(
        &self,
        proxy_url: &str,
        credential_ids: &[u64],
    ) -> usize {
        let (url, username, password) = match self.proxy_pool.find_by_url(proxy_url) {
            Some(e) => (
                e.url,
                e.username.unwrap_or_default(),
                e.password.unwrap_or_default(),
            ),
            None => match super::proxy_pool::parse_proxy_line(proxy_url) {
                Ok(p) => (
                    p.url,
                    p.username.unwrap_or_default(),
                    p.password.unwrap_or_default(),
                ),
                Err(_) => (proxy_url.to_string(), String::new(), String::new()),
            },
        };
        self.write_proxy_to_credentials(&url, &username, &password, credential_ids)
    }

    fn write_proxy_to_credentials(
        &self,
        url: &str,
        username: &str,
        password: &str,
        credential_ids: &[u64],
    ) -> usize {
        let mut ok = 0;
        for &id in credential_ids {
            if self
                .token_manager
                .update_credential_fields(
                    id,
                    None,
                    None,
                    None,
                    Some(url.to_string()),
                    Some(username.to_string()),
                    Some(password.to_string()),
                )
                .is_ok()
            {
                self.balance_cache.lock().remove(&id);
                ok += 1;
            }
        }
        ok
    }

    /// 把可用代理按轮询分配给若干凭据。返回 (成功条数, 使用的代理数)。
    pub fn assign_proxies_round_robin(&self, credential_ids: &[u64]) -> Result<(usize, usize), AdminServiceError> {
        let entries = self.proxy_pool.assignable_entries();
        if entries.is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "没有可用代理（需已启用且健康检查未失败）".to_string(),
            ));
        }
        let mut ok = 0;
        for (i, &id) in credential_ids.iter().enumerate() {
            let entry = &entries[i % entries.len()];
            if self
                .token_manager
                .update_credential_fields(
                    id,
                    None,
                    None,
                    None,
                    Some(entry.url.clone()),
                    Some(entry.username.clone().unwrap_or_default()),
                    Some(entry.password.clone().unwrap_or_default()),
                )
                .is_ok()
            {
                self.balance_cache.lock().remove(&id);
                ok += 1;
            }
        }
        Ok((ok, entries.len()))
    }

    /// 设置超额开关（POST /credentials/:id/overage）
    ///
    /// 当前为占位实现：仅校验凭据存在并清除余额缓存，方便前端按钮即时反馈。
    /// 真正调用 AWS Q `setOverageConfiguration` 需要新增模型与签名逻辑。
    pub fn set_credential_overage(
        &self,
        id: u64,
        _enabled: bool,
    ) -> Result<(), AdminServiceError> {
        let snapshot = self.token_manager.snapshot();
        if !snapshot.entries.iter().any(|e| e.id == id) {
            return Err(AdminServiceError::NotFound { id });
        }
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        Ok(())
    }

    /// 获取脱敏与原值的密钥信息（GET /keys）
    pub fn get_admin_keys(&self) -> AdminKeysResponse {
        let cfg = self.token_manager.config();
        let api_key_full = cfg.api_key.clone().unwrap_or_default();
        let admin_key_full = cfg.admin_api_key.clone().unwrap_or_default();
        AdminKeysResponse {
            api_key: KeyEntry {
                masked: mask_secret(&api_key_full),
                full: api_key_full,
            },
            admin_api_key: KeyEntry {
                masked: mask_secret(&admin_key_full),
                full: admin_key_full,
            },
        }
    }

    fn refresh_usage(&self) -> parking_lot::MutexGuard<'_, UsageStatsCache> {
        let len = std::fs::metadata(&self.request_details_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let mut cache = self.usage.lock();
        if cache.file_len != len {
            cache.agg.rebuild_from_kv_jsonl(&self.request_details_path);
            cache.file_len = len;
        }
        cache
    }

    pub fn stats_overview(&self) -> (OverviewStats, usize, usize) {
        let snapshot = self.refresh_usage().agg.overview();
        let creds = self.get_all_credentials();
        (snapshot, creds.available, creds.total)
    }

    pub fn stats_timeseries(&self, window: StatsQueryWindow) -> Vec<TimeSeriesPoint> {
        self.refresh_usage().agg.query_timeseries(window)
    }

    pub fn stats_by_model(&self, window: StatsQueryWindow) -> Vec<ModelDistribution> {
        self.refresh_usage().agg.query_by_model(window)
    }

    pub fn stats_by_credential(&self, window: StatsQueryWindow) -> Vec<CredentialDistribution> {
        let mut rows = self.refresh_usage().agg.query_by_credential(window);
        let emails: HashMap<u64, String> = self
            .get_all_credentials()
            .credentials
            .into_iter()
            .filter_map(|c| c.email.map(|e| (c.id, e)))
            .collect();
        for row in &mut rows {
            if let Some(email) = emails.get(&row.credential_id) {
                row.email = Some(email.clone());
            }
        }
        rows
    }
}

fn mask_secret(s: &str) -> String {
    let n = s.chars().count();
    if n == 0 {
        return String::new();
    }
    if n <= 10 {
        return "*".repeat(n);
    }
    let head: String = s.chars().take(6).collect();
    let tail: String = s.chars().rev().take(4).collect::<String>().chars().rev().collect();
    format!("{}…{}", head, tail)
}

#[cfg(test)]
mod tests {
    use super::normalize_proxy_fields;

    #[test]
    fn normalize_keeps_naked_host_port() {
        let (url, user, pass) = normalize_proxy_fields(
            Some("204.1.74.19:7069".into()),
            Some("u".into()),
            Some("p".into()),
        );
        assert_eq!(url.as_deref(), Some("204.1.74.19:7069"));
        assert_eq!(user.as_deref(), Some("u"));
        assert_eq!(pass.as_deref(), Some("p"));
    }

    #[test]
    fn normalize_splits_kam_line_to_host_port() {
        let (url, user, pass) = normalize_proxy_fields(
            Some("204.1.74.19:7069:SlxbQ9k8uOb9:d1aOTTLi0v6T".into()),
            None,
            None,
        );
        assert_eq!(url.as_deref(), Some("204.1.74.19:7069"));
        assert_eq!(user.as_deref(), Some("SlxbQ9k8uOb9"));
        assert_eq!(pass.as_deref(), Some("d1aOTTLi0v6T"));
    }
}
