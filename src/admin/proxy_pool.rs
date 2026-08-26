//! 代理 IP 池管理
//!
//! 独立于凭据管理，以「地址 + 用户名」去重，持久化为 `proxy_pool.json`。
//! 导入同时支持：
//! - `host:port`
//! - `host:port:user:pass`（Kiro Account Manager 导出）
//! - `http(s)://` / `socks5://`，可带 `user:pass@`
//!
//! 账密与 URL 分开存储；探测和分配时一并带上，避免把 `ip:port:user:pass`
//! 当成残 URL 交给 reqwest（会直接 builder error）。

use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// 健康检查探测端点：返回 204 的轻量公网地址，不依赖上游 Kiro。
const PROXY_HEALTH_CHECK_URL: &str = "https://www.gstatic.com/generate_204";
/// 单次探测超时（秒）
const PROXY_PROBE_TIMEOUT_SECS: u64 = 8;
/// 连续探测失败阈值：达到后自动禁用
const MAX_PROXY_PROBE_FAILURES: u32 = 3;

/// 健康检查探测状态
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyHealth {
    /// 尚未探测
    #[default]
    Unknown,
    /// 最近一次探测成功
    Healthy,
    /// 最近一次探测失败
    Unhealthy,
}

/// 一条导入行解析后的结果：URL 不含账密。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedProxy {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// 持久化的代理条目
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyEntry {
    pub id: u64,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub health: ProxyHealth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    #[serde(default)]
    pub consecutive_failures: u32,
    /// 是否由健康检查自动禁用（区别于用户手动禁用）
    #[serde(default)]
    pub auto_disabled: bool,
}

fn default_true() -> bool {
    true
}

/// 把一行代理文本解析成可用的 URL + 可选账密。
pub fn parse_proxy_line(raw: &str) -> anyhow::Result<ParsedProxy> {
    let cfg = ProxyConfig::parse_line(raw)?;
    Ok(ParsedProxy {
        url: cfg.url,
        username: cfg.username,
        password: cfg.password,
    })
}

fn hostport_of(url: &str) -> Option<String> {
    if let Some((_, rest)) = url.split_once("://") {
        return Some(rest.to_string());
    }
    parse_proxy_line(url)
        .ok()
        .and_then(|p| p.url.split_once("://").map(|(_, rest)| rest.to_string()))
}

impl ProxyEntry {
    fn from_parsed(id: u64, parsed: ParsedProxy, label: Option<String>) -> Self {
        Self {
            id,
            url: parsed.url,
            username: parsed.username,
            password: parsed.password,
            label: label.filter(|s| !s.trim().is_empty()),
            enabled: true,
            health: ProxyHealth::Unknown,
            latency_ms: None,
            last_checked_at: None,
            consecutive_failures: 0,
            auto_disabled: false,
        }
    }

    fn matches_parsed(&self, parsed: &ParsedProxy) -> bool {
        self.url == parsed.url && self.username == parsed.username
    }

    pub fn to_proxy_config(&self) -> ProxyConfig {
        ProxyConfig::from_url_and_auth(
            &self.url,
            self.username.clone(),
            self.password.clone(),
        )
        .unwrap_or_else(|_| {
            let mut cfg = ProxyConfig::new(&self.url);
            if let Some(user) = &self.username {
                cfg = cfg.with_auth(user, self.password.as_deref().unwrap_or(""));
            }
            cfg
        })
    }
}

/// 一次全量健康检查的摘要
#[derive(Debug, Clone, Default)]
pub struct CheckSummary {
    pub healthy: usize,
    pub unhealthy: usize,
    pub auto_disabled: usize,
}

enum ProbeResult {
    Ok { latency_ms: u32 },
    Err { error: String },
}

pub struct ProxyPoolManager {
    entries: Mutex<Vec<ProxyEntry>>,
    next_id: AtomicU64,
    path: Option<PathBuf>,
    tls_backend: TlsBackend,
}

fn repair_loaded(entries: &mut [ProxyEntry]) -> bool {
    let mut repaired = false;
    for entry in entries.iter_mut() {
        match parse_proxy_line(&entry.url) {
            Ok(parsed) => {
                let url_changed = parsed.url != entry.url;
                let auth_filled = entry.username.is_none() && parsed.username.is_some();
                if url_changed {
                    entry.url = parsed.url;
                }
                if auth_filled {
                    entry.username = parsed.username;
                    entry.password = parsed.password;
                }
                if url_changed || auth_filled {
                    repaired = true;
                }
            }
            Err(_) => {}
        }
    }
    repaired
}

impl ProxyPoolManager {
    pub fn new(path: Option<PathBuf>, tls_backend: TlsBackend) -> Self {
        let mut entries = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Vec<ProxyEntry>>(&s).ok())
            .unwrap_or_default();
        let repaired = repair_loaded(&mut entries);
        let next_id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        let this = Self {
            entries: Mutex::new(entries),
            next_id: AtomicU64::new(next_id),
            path,
            tls_backend,
        };
        if repaired {
            if let Err(e) = this.persist() {
                tracing::warn!("修复历史代理 URL 后持久化失败: {}", e);
            }
        }
        this
    }

    pub fn list(&self) -> Vec<ProxyEntry> {
        self.entries.lock().clone()
    }

    pub fn find_by_url(&self, url: &str) -> Option<ProxyEntry> {
        let parsed = parse_proxy_line(url).ok();
        let needle = parsed
            .as_ref()
            .map(|p| p.url.as_str())
            .unwrap_or(url);
        let needle_hp = hostport_of(needle);
        self.entries
            .lock()
            .iter()
            .find(|e| {
                e.url == needle
                    || e.url == url
                    || hostport_of(&e.url)
                        .zip(needle_hp.as_deref())
                        .is_some_and(|(a, b)| a == b)
            })
            .cloned()
    }

    pub fn add(&self, url: String, label: Option<String>) -> anyhow::Result<ProxyEntry> {
        let parsed = parse_proxy_line(&url)?;
        let mut entries = self.entries.lock();
        if entries.iter().any(|e| e.matches_parsed(&parsed)) {
            anyhow::bail!("代理已存在: {}", parsed.url);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let entry = ProxyEntry::from_parsed(id, parsed, label);
        entries.push(entry.clone());
        drop(entries);
        self.persist()?;
        Ok(entry)
    }

    /// 批量添加：单次加锁完成所有插入，最后统一持久化。返回 (新增, 错误列表)。
    pub fn batch_add(&self, urls: Vec<String>) -> (Vec<ProxyEntry>, Vec<String>) {
        let mut added = vec![];
        let mut errors = vec![];
        let mut entries = self.entries.lock();
        for raw in urls {
            let raw = raw.trim();
            if raw.is_empty() || raw.starts_with('#') {
                continue;
            }
            let parsed = match parse_proxy_line(raw) {
                Ok(p) => p,
                Err(e) => {
                    errors.push(e.to_string());
                    continue;
                }
            };
            if entries.iter().any(|e| e.matches_parsed(&parsed))
                || added.iter().any(|e: &ProxyEntry| e.matches_parsed(&parsed))
            {
                errors.push(format!("代理已存在: {}", parsed.url));
                continue;
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let entry = ProxyEntry::from_parsed(id, parsed, None);
            entries.push(entry.clone());
            added.push(entry);
        }
        drop(entries);
        if !added.is_empty() {
            if let Err(e) = self.persist() {
                tracing::warn!("批量添加代理后持久化失败: {}", e);
            }
        }
        (added, errors)
    }

    pub fn delete(&self, id: u64) -> anyhow::Result<()> {
        let mut entries = self.entries.lock();
        let before = entries.len();
        entries.retain(|e| e.id != id);
        if entries.len() == before {
            anyhow::bail!("代理不存在: {}", id);
        }
        drop(entries);
        self.persist()?;
        Ok(())
    }

    pub fn set_enabled(&self, id: u64, enabled: bool) -> anyhow::Result<()> {
        let mut entries = self.entries.lock();
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| anyhow::anyhow!("代理不存在: {}", id))?;
        entry.enabled = enabled;
        if enabled {
            entry.auto_disabled = false;
            entry.consecutive_failures = 0;
        }
        drop(entries);
        self.persist()?;
        Ok(())
    }

    /// 所有「可用于分配」的代理：已启用且非 Unhealthy
    pub fn assignable_entries(&self) -> Vec<ProxyEntry> {
        self.entries
            .lock()
            .iter()
            .filter(|e| e.enabled && e.health != ProxyHealth::Unhealthy)
            .cloned()
            .collect()
    }

    fn persist(&self) -> anyhow::Result<()> {
        let path = match &self.path {
            Some(p) => p,
            None => return Ok(()),
        };
        let json = serde_json::to_string_pretty(&*self.entries.lock())?;
        std::fs::write(path, json)?;
        Ok(())
    }

    async fn probe_one(&self, entry: &ProxyEntry) -> ProbeResult {
        let proxy = entry.to_proxy_config();
        let client = match build_client(Some(&proxy), PROXY_PROBE_TIMEOUT_SECS, self.tls_backend) {
            Ok(c) => c,
            Err(e) => {
                return ProbeResult::Err {
                    error: format!("代理地址无法解析: {}", e),
                };
            }
        };
        let started = Instant::now();
        match client.get(PROXY_HEALTH_CHECK_URL).send().await {
            Ok(resp) if resp.status().is_success() || resp.status().is_redirection() => {
                ProbeResult::Ok {
                    latency_ms: started.elapsed().as_millis().min(u32::MAX as u128) as u32,
                }
            }
            Ok(resp) => ProbeResult::Err {
                error: format!("探测端点返回非预期状态: {}", resp.status()),
            },
            Err(e) => ProbeResult::Err {
                error: describe_reqwest_error(&e),
            },
        }
    }

    fn apply_probe_result(entry: &mut ProxyEntry, result: &ProbeResult) -> (bool, bool) {
        entry.last_checked_at = Some(chrono::Utc::now().to_rfc3339());
        match result {
            ProbeResult::Ok { latency_ms } => {
                entry.health = ProxyHealth::Healthy;
                entry.latency_ms = Some(*latency_ms);
                entry.consecutive_failures = 0;
                (false, false)
            }
            ProbeResult::Err { error } => {
                entry.health = ProxyHealth::Unhealthy;
                entry.latency_ms = None;
                entry.consecutive_failures += 1;
                tracing::warn!(
                    "代理 #{} 探测失败（{}/{}）: {}",
                    entry.id,
                    entry.consecutive_failures,
                    MAX_PROXY_PROBE_FAILURES,
                    error
                );
                let mut newly_disabled = false;
                if entry.consecutive_failures >= MAX_PROXY_PROBE_FAILURES && entry.enabled {
                    entry.enabled = false;
                    entry.auto_disabled = true;
                    newly_disabled = true;
                }
                (true, newly_disabled)
            }
        }
    }

    /// 全量健康检查：并发探测所有已启用代理，回写并持久化一次。
    pub async fn check_all(&self) -> CheckSummary {
        let targets: Vec<ProxyEntry> = self
            .entries
            .lock()
            .iter()
            .filter(|e| e.enabled)
            .cloned()
            .collect();
        if targets.is_empty() {
            return CheckSummary::default();
        }

        let probes = targets.iter().map(|entry| {
            let entry = entry.clone();
            async move { (entry.id, self.probe_one(&entry).await) }
        });
        let results = futures::future::join_all(probes).await;

        let mut summary = CheckSummary::default();
        {
            let mut entries = self.entries.lock();
            for (id, result) in &results {
                if let Some(entry) = entries.iter_mut().find(|e| e.id == *id) {
                    let (unhealthy, newly_disabled) = Self::apply_probe_result(entry, result);
                    if unhealthy {
                        summary.unhealthy += 1;
                    } else {
                        summary.healthy += 1;
                    }
                    if newly_disabled {
                        summary.auto_disabled += 1;
                    }
                }
            }
        }
        if let Err(e) = self.persist() {
            tracing::warn!("健康检查后持久化失败: {}", e);
        }
        summary
    }

    /// 单个代理即时探测，回写并持久化。
    pub async fn check_one(&self, id: u64) -> anyhow::Result<ProxyEntry> {
        let target = self
            .entries
            .lock()
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("代理不存在: {}", id))?;
        let result = self.probe_one(&target).await;
        let entry = {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("代理不存在: {}", id))?;
            Self::apply_probe_result(entry, &result);
            entry.clone()
        };
        self.persist()?;
        Ok(entry)
    }
}

/// 把 reqwest 错误展开成人能看懂的原因（顶层只有 "error sending request"）。
pub fn describe_reqwest_error(err: &reqwest::Error) -> String {
    let mut cause: Option<&dyn std::error::Error> = std::error::Error::source(err);
    let mut deepest: Option<String> = None;
    while let Some(e) = cause {
        deepest = Some(e.to_string());
        cause = e.source();
    }
    let kind = if err.is_timeout() {
        "超时"
    } else if err.is_connect() {
        "无法建立连接"
    } else {
        "请求失败"
    };
    match deepest {
        Some(detail) => format!("{}: {}", kind, detail),
        None => format!("{}: {}", kind, err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ParsedProxy {
        parse_proxy_line(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn kam_host_port_user_pass() {
        let x = p("204.1.74.19:7069:SlxbQ9k8uOb9:d1aOTTLi0v6T");
        assert_eq!(x.url, "socks5h://204.1.74.19:7069");
        assert_eq!(x.username.as_deref(), Some("SlxbQ9k8uOb9"));
        assert_eq!(x.password.as_deref(), Some("d1aOTTLi0v6T"));
    }

    #[test]
    fn malformed_scheme_glued_kam() {
        let x = p("http://204.1.74.19:7069:SlxbQ9k8uOb9:d1aOTTLi0v6T");
        assert_eq!(x.url, "socks5h://204.1.74.19:7069");
        assert_eq!(x.username.as_deref(), Some("SlxbQ9k8uOb9"));
        assert_eq!(x.password.as_deref(), Some("d1aOTTLi0v6T"));
    }

    #[test]
    fn socks_with_auth() {
        let x = p("socks5://user:pass@1.2.3.4:1080");
        assert_eq!(x.url, "socks5://1.2.3.4:1080");
        assert_eq!(x.username.as_deref(), Some("user"));
        assert_eq!(x.password.as_deref(), Some("pass"));
    }

    #[test]
    fn bare_host_port() {
        let x = p("127.0.0.1:7897");
        assert_eq!(x.url, "http://127.0.0.1:7897");
        assert!(x.username.is_none());
        assert!(x.password.is_none());
    }

    #[test]
    fn password_with_colon() {
        let x = p("1.2.3.4:8080:user:p:a:ss");
        assert_eq!(x.password.as_deref(), Some("p:a:ss"));
    }

    #[test]
    fn http_url_no_auth() {
        let x = p("http://10.0.0.1:3128");
        assert_eq!(x.url, "http://10.0.0.1:3128");
        assert!(x.username.is_none());
    }

    #[test]
    fn rejects_missing_port() {
        assert!(parse_proxy_line("1.2.3.4").is_err());
    }

    #[test]
    fn repair_legacy_glued_url() {
        let mut entries = vec![ProxyEntry {
            id: 4,
            url: "http://204.1.74.19:7069:SlxbQ9k8uOb9:d1aOTTLi0v6T".into(),
            username: None,
            password: None,
            label: None,
            enabled: true,
            health: ProxyHealth::Unknown,
            latency_ms: None,
            last_checked_at: None,
            consecutive_failures: 12,
            auto_disabled: true,
        }];
        assert!(repair_loaded(&mut entries));
        assert_eq!(entries[0].url, "socks5h://204.1.74.19:7069");
        assert_eq!(entries[0].username.as_deref(), Some("SlxbQ9k8uOb9"));
        assert_eq!(entries[0].password.as_deref(), Some("d1aOTTLi0v6T"));
    }

    #[test]
    fn find_by_url_matches_naked_host_port_to_socks() {
        let pool = ProxyPoolManager::new(None, TlsBackend::Rustls);
        pool.add("socks5://user:pass@204.1.74.19:7069".into(), None)
            .unwrap();
        let hit = pool.find_by_url("204.1.74.19:7069").expect("host:port");
        assert_eq!(hit.username.as_deref(), Some("user"));
        let cfg = hit.to_proxy_config();
        assert!(cfg.url.to_ascii_lowercase().starts_with("socks"));
        assert_eq!(cfg.username.as_deref(), Some("user"));
        assert_eq!(cfg.password.as_deref(), Some("pass"));
    }
}
