//! HTTP Client 构建模块
//!
//! 提供统一的 HTTP Client 构建功能，支持代理配置

use reqwest::{Client, Proxy};
use std::time::Duration;

use crate::model::config::TlsBackend;

/// 代理配置
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ProxyConfig {
    /// 代理地址，支持 http/https/socks5
    pub url: String,
    /// 代理认证用户名
    pub username: Option<String>,
    /// 代理认证密码
    pub password: Option<String>,
}

impl ProxyConfig {
    /// 从 url 创建代理配置
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            username: None,
            password: None,
        }
    }

    /// 设置认证信息
    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    /// 解析一行代理文本：`host:port`、`host:port:user:pass`、带 scheme 的 URL。
    pub fn parse_line(raw: &str) -> anyhow::Result<Self> {
        parse_proxy_line(raw)
    }

    /// 从 URL + 可选旁路账密组装实际出站配置。
    ///
    /// 与 `KiroCredentials::effective_proxy` 同一条规则：输入没有 `://`
    ///（裸 `host:port`）且最终带用户名时，按 SOCKS5h 出站。显式 `http://`
    /// 即使带账密也保持 HTTP，避免误伤 HTTP 代理。
    pub fn from_url_and_auth(
        url: &str,
        username: Option<String>,
        password: Option<String>,
    ) -> anyhow::Result<Self> {
        let original_had_scheme = url.contains("://");
        let mut proxy = Self::parse_line(url)?;
        let user = nonempty_owned(username).or_else(|| proxy.username.take());
        let pass = nonempty_owned(password).or_else(|| proxy.password.take());
        if let Some(u) = user {
            proxy.username = Some(u);
            proxy.password = Some(pass.unwrap_or_default());
            if !original_had_scheme && proxy.url.to_ascii_lowercase().starts_with("http://") {
                proxy.url = format!("socks5h://{}", &proxy.url["http://".len()..]);
            }
        }
        Ok(proxy)
    }
}

// socks5h 必须排在 socks5 前面，否则 `socks5h://` 会被误切成 `socks5://` + `h://...`
const SCHEMES: [&str; 5] = ["http://", "https://", "socks5h://", "socks5://", "socks4://"];

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn nonempty_owned(s: Option<String>) -> Option<String> {
    s.and_then(|s| nonempty(&s))
}

fn decode_auth(s: &str) -> String {
    urlencoding::decode(s)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

fn validate_port(port: &str) -> anyhow::Result<()> {
    let n: u32 = port
        .parse()
        .map_err(|_| anyhow::anyhow!("端口号无效: {}", port))?;
    if n == 0 || n > 65535 {
        anyhow::bail!("端口号无效: {}", port);
    }
    Ok(())
}

fn split_scheme(raw: &str) -> Option<(&'static str, &str)> {
    let lower = raw.to_ascii_lowercase();
    for scheme in SCHEMES {
        if lower.starts_with(scheme) {
            return Some((scheme, &raw[scheme.len()..]));
        }
    }
    None
}

fn parse_host_port_auth(s: &str) -> anyhow::Result<(String, Option<String>, Option<String>)> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("代理地址为空");
    }

    if s.starts_with('[') {
        let close = s
            .find(']')
            .ok_or_else(|| anyhow::anyhow!("IPv6 地址缺少 ]"))?;
        let host = &s[..=close];
        let rest = s[close + 1..].strip_prefix(':').ok_or_else(|| {
            anyhow::anyhow!("IPv6 代理缺少端口号")
        })?;
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.is_empty() || parts[0].is_empty() {
            anyhow::bail!("IPv6 代理缺少端口号");
        }
        validate_port(parts[0])?;
        let hostport = format!("{}:{}", host, parts[0]);
        return match parts.len() {
            1 => Ok((hostport, None, None)),
            n if n >= 3 => Ok((
                hostport,
                nonempty(parts[1]),
                Some(parts[2..].join(":")),
            )),
            _ => anyhow::bail!(
                "无法识别的代理格式（需要 host:port 或 host:port:user:pass）: {}",
                s
            ),
        };
    }

    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        2 => {
            if parts[0].is_empty() {
                anyhow::bail!("代理主机为空");
            }
            validate_port(parts[1])?;
            Ok((format!("{}:{}", parts[0], parts[1]), None, None))
        }
        n if n >= 4 => {
            if parts[0].is_empty() {
                anyhow::bail!("代理主机为空");
            }
            validate_port(parts[1])?;
            Ok((
                format!("{}:{}", parts[0], parts[1]),
                nonempty(parts[2]),
                Some(parts[3..].join(":")),
            ))
        }
        _ => anyhow::bail!(
            "无法识别的代理格式（需要 host:port 或 host:port:user:pass）: {}",
            s
        ),
    }
}

fn parse_proxy_line(raw: &str) -> anyhow::Result<ProxyConfig> {
    let raw = raw.trim();
    if raw.is_empty() {
        anyhow::bail!("代理 URL 不能为空");
    }

    if let Some((scheme, rest)) = split_scheme(raw) {
        let (auth, hostport) = if let Some(idx) = rest.rfind('@') {
            (Some(&rest[..idx]), &rest[idx + 1..])
        } else {
            (None, rest)
        };
        let (hostport, user_from_hp, pass_from_hp) = parse_host_port_auth(hostport)?;
        let (username, password) = if let Some(auth) = auth {
            match auth.split_once(':') {
                Some((u, p)) => (nonempty(&decode_auth(u)), Some(decode_auth(p))),
                None => (nonempty(&decode_auth(auth)), Some(String::new())),
            }
        } else {
            (user_from_hp, pass_from_hp)
        };
        let scheme = if auth.is_none() && username.is_some() && !scheme.starts_with("socks") {
            "socks5h://"
        } else {
            scheme
        };
        return Ok(ProxyConfig {
            url: format!("{}{}", scheme, hostport),
            username,
            password,
        });
    }

    if let Some(idx) = raw.rfind('@') {
        let auth = &raw[..idx];
        let hostport = &raw[idx + 1..];
        if hostport.contains(':') && !auth.contains('/') {
            let (hostport, _, _) = parse_host_port_auth(hostport)?;
            let (u, p) = match auth.split_once(':') {
                Some((u, p)) => (decode_auth(u), decode_auth(p)),
                None => (decode_auth(auth), String::new()),
            };
            return Ok(ProxyConfig {
                url: format!("http://{}", hostport),
                username: nonempty(&u),
                password: nonempty(&p),
            });
        }
    }

    let (hostport, username, password) = parse_host_port_auth(raw)?;
    // KAM 导出的 `ip:port:user:pass` 实际是 SOCKS5；无账密的裸 host:port 仍按 HTTP。
    let scheme = if username.is_some() { "socks5h://" } else { "http://" };
    Ok(ProxyConfig {
        url: format!("{}{}", scheme, hostport),
        username,
        password,
    })
}

fn socks5_to_socks5h(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        if scheme.eq_ignore_ascii_case("socks5") {
            return format!("socks5h://{}", rest);
        }
    }
    url.to_string()
}

fn proxy_url_for_reqwest(cfg: &ProxyConfig) -> String {
    let url = socks5_to_socks5h(&cfg.url);
    let Some(user) = cfg.username.as_deref().filter(|s| !s.is_empty()) else {
        return url;
    };
    let Some((scheme, rest)) = url.split_once("://") else {
        return url;
    };
    if rest.contains('@') {
        return url;
    }
    let pass = cfg.password.as_deref().unwrap_or("");
    format!(
        "{}://{}:{}@{}",
        scheme,
        urlencoding::encode(user),
        urlencoding::encode(pass),
        rest
    )
}

/// 构建 HTTP Client
///
/// # Arguments
/// * `proxy` - 可选的代理配置
/// * `timeout_secs` - 超时时间（秒）
///
/// # Returns
/// 配置好的 reqwest::Client
pub fn build_client(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    let mut builder = Client::builder().timeout(Duration::from_secs(timeout_secs));

    if tls_backend == TlsBackend::Rustls {
        builder = builder.use_rustls_tls();
    }

    if let Some(proxy_config) = proxy {
        let proxy_url = proxy_url_for_reqwest(proxy_config);
        let mut proxy = Proxy::all(&proxy_url)?;

        // HTTP 代理额外带 Proxy-Authorization；SOCKS5 账密已经写进 URL。
        let is_socks = proxy_config
            .url
            .to_ascii_lowercase()
            .starts_with("socks");
        if !is_socks {
            if let Some(username) = &proxy_config.username {
                proxy = proxy.basic_auth(username, proxy_config.password.as_deref().unwrap_or(""));
            }
        }

        builder = builder.proxy(proxy);
        tracing::debug!("HTTP Client 使用代理: {}", proxy_config.url);
    }

    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_config_new() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        assert_eq!(config.url, "http://127.0.0.1:7890");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn test_proxy_config_with_auth() {
        let config = ProxyConfig::new("socks5://127.0.0.1:1080").with_auth("user", "pass");
        assert_eq!(config.url, "socks5://127.0.0.1:1080");
        assert_eq!(config.username, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));
    }

    #[test]
    fn test_build_client_without_proxy() {
        let client = build_client(None, 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn parse_kam_host_port_user_pass() {
        let x = ProxyConfig::parse_line("204.1.74.19:7069:SlxbQ9k8uOb9:d1aOTTLi0v6T").unwrap();
        assert_eq!(x.url, "socks5h://204.1.74.19:7069");
        assert_eq!(x.username.as_deref(), Some("SlxbQ9k8uOb9"));
        assert_eq!(x.password.as_deref(), Some("d1aOTTLi0v6T"));
    }

    #[test]
    fn parse_malformed_scheme_glued_kam() {
        let x = ProxyConfig::parse_line("http://204.1.74.19:7069:SlxbQ9k8uOb9:d1aOTTLi0v6T").unwrap();
        assert_eq!(x.url, "socks5h://204.1.74.19:7069");
        assert_eq!(x.username.as_deref(), Some("SlxbQ9k8uOb9"));
        assert_eq!(x.password.as_deref(), Some("d1aOTTLi0v6T"));
    }

    #[test]
    fn glued_kam_url_cannot_build_client_until_parsed() {
        let bad = ProxyConfig::new("http://204.1.74.19:7069:SlxbQ9k8uOb9:d1aOTTLi0v6T");
        assert!(build_client(Some(&bad), 8, TlsBackend::Rustls).is_err());

        let ok = ProxyConfig::parse_line("204.1.74.19:7069:SlxbQ9k8uOb9:d1aOTTLi0v6T").unwrap();
        assert_eq!(ok.url, "socks5h://204.1.74.19:7069");
        assert!(build_client(Some(&ok), 8, TlsBackend::Rustls).is_ok());
        let wired = proxy_url_for_reqwest(&ok);
        assert!(wired.starts_with("socks5h://"));
        assert!(wired.contains('@'));
        assert!(wired.ends_with("204.1.74.19:7069"));
    }

    #[test]
    fn from_url_and_auth_naked_host_port_with_user_is_socks5h() {
        let cfg = ProxyConfig::from_url_and_auth(
            "204.1.74.19:7069",
            Some("user".into()),
            Some("pass".into()),
        )
        .unwrap();
        assert_eq!(cfg.url, "socks5h://204.1.74.19:7069");
        assert_eq!(cfg.username.as_deref(), Some("user"));
        assert_eq!(cfg.password.as_deref(), Some("pass"));
        let wired = proxy_url_for_reqwest(&cfg);
        assert!(wired.starts_with("socks5h://"));
        assert!(wired.contains('@'));
        assert!(wired.ends_with("204.1.74.19:7069"));
        assert!(!wired.contains("http://"));
    }

    #[test]
    fn from_url_and_auth_explicit_http_keeps_http() {
        let cfg = ProxyConfig::from_url_and_auth(
            "http://proxy:3128",
            Some("user".into()),
            Some("pass".into()),
        )
        .unwrap();
        assert_eq!(cfg.url, "http://proxy:3128");
        assert_eq!(cfg.username.as_deref(), Some("user"));
    }

    #[test]
    fn parse_socks5h_url() {
        let x = ProxyConfig::parse_line("socks5h://204.1.74.19:7069").unwrap();
        assert_eq!(x.url, "socks5h://204.1.74.19:7069");
        assert!(x.username.is_none());
    }
}
