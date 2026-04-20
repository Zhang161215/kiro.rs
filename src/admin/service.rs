//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;

use super::error::AdminServiceError;
use super::types::{
    AddCredentialRequest, AddCredentialResponse, BalanceResponse, CredentialStatusItem,
    CredentialsStatusResponse, KvCacheConfigResponse, LoadBalancingModeResponse,
    RequestDetailItem, RequestDetailsResponse, SetKvCacheConfigRequest,
    SetLoadBalancingModeRequest,
};

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;
/// 请求明细默认返回条数
const REQUEST_DETAILS_DEFAULT_LIMIT: usize = 100;
/// 请求明细最大返回条数
const REQUEST_DETAILS_MAX_LIMIT: usize = 1000;
/// 模拟 KV 缓存记录文件名
const KV_CACHE_RECORDS_FILE: &str = "kiro_kv_cache_records.jsonl";

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

        Self {
            token_manager,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
            request_details_path,
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
        let new_cred = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: req.refresh_token,
            profile_arn: None,
            expires_at: None,
            auth_method: Some(req.auth_method),
            client_id: req.client_id,
            client_secret: req.client_secret,
            priority: req.priority,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            proxy_url: req.proxy_url,
            proxy_username: req.proxy_username,
            proxy_password: req.proxy_password,
            disabled: false, // 新添加的凭据默认启用
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
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
}
