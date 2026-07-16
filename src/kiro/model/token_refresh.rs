use serde::{Deserialize, Serialize};

/// 刷新 Token 的请求体 (Social 认证)
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// 刷新 Token 的响应体 (Social 认证)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub profile_arn: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
}

/// IdC Token 刷新请求体 (AWS SSO OIDC)
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdcRefreshRequest {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    pub grant_type: String,
}

/// IdC Token 刷新响应体 (AWS SSO OIDC)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdcRefreshResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    // #[serde(default)]
    // pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub profile_arn: Option<String>,
}

/// 外部 IdP Token 刷新请求体 (external_idp，如 Microsoft Entra ID / Azure AD)
///
/// 使用 OAuth2 `refresh_token` grant，属于公共客户端（无 client_secret）。
/// 以 `application/x-www-form-urlencoded` 编码提交。
#[derive(Debug, Serialize)]
pub struct ExternalIdpRefreshRequest {
    pub grant_type: String,
    pub refresh_token: String,
    pub client_id: String,
    /// 空格分隔的 scope 列表（需含 offline_access）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// 外部 IdP Token 刷新响应体 (external_idp)
///
/// 标准 OIDC token 响应；不含 profileArn（由 ListAvailableProfiles 懒解析）。
#[derive(Debug, Deserialize)]
pub struct ExternalIdpRefreshResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
}
