//! Admin API HTTP 处理器

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};

use super::{
    middleware::AdminState,
    types::{
        AddCredentialRequest, AddProxyRequest, AssignProxyRequest, BatchAddProxyRequest,
        ProxyCheckRequest, RequestDetailsQuery, SetDisabledRequest, SetKvCacheConfigRequest,
        SetLoadBalancingModeRequest, SetModelsRequest, SetOverageRequest, SetPriorityRequest,
        SetProxyEnabledRequest, SuccessResponse, UpdateCredentialRequest,
    },
};
use axum::Json as AxumJson;
use serde_json::json;

/// GET /api/admin/credentials
/// 获取所有凭据状态
pub async fn get_all_credentials(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_all_credentials();
    Json(response)
}

/// POST /api/admin/credentials/:id/disabled
/// 设置凭据禁用状态
pub async fn set_credential_disabled(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetDisabledRequest>,
) -> impl IntoResponse {
    match state.service.set_disabled(id, payload.disabled) {
        Ok(_) => {
            let action = if payload.disabled { "禁用" } else { "启用" };
            Json(SuccessResponse::new(format!("凭据 #{} 已{}", id, action))).into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/priority
/// 设置凭据优先级
pub async fn set_credential_priority(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetPriorityRequest>,
) -> impl IntoResponse {
    match state.service.set_priority(id, payload.priority) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 优先级已设置为 {}",
            id, payload.priority
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/reset
/// 重置失败计数并重新启用
pub async fn reset_failure_count(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.reset_and_enable(id) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 失败计数已重置并重新启用",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/:id/balance
/// 获取指定凭据的余额
pub async fn get_credential_balance(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.get_balance(id).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials
/// 添加新凭据
pub async fn add_credential(
    State(state): State<AdminState>,
    Json(payload): Json<AddCredentialRequest>,
) -> impl IntoResponse {
    match state.service.add_credential(payload).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/credentials/:id
/// 删除凭据
pub async fn delete_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.delete_credential(id) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 已删除", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/refresh
/// 强制刷新凭据 Token
pub async fn force_refresh_token(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.force_refresh_token(id).await {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} Token 已强制刷新",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/details
/// 获取请求明细（模拟 KV 缓存统计）
pub async fn get_request_details(
    State(state): State<AdminState>,
    Query(query): Query<RequestDetailsQuery>,
) -> impl IntoResponse {
    match state.service.get_request_details(query.limit) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/details
/// 清空请求明细
pub async fn clear_request_details(State(state): State<AdminState>) -> impl IntoResponse {
    match state.service.clear_request_details() {
        Ok(_) => Json(SuccessResponse::new("请求明细已清空".to_string())).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/load-balancing
/// 获取负载均衡模式
pub async fn get_load_balancing_mode(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_load_balancing_mode();
    Json(response)
}

/// PUT /api/admin/config/load-balancing
/// 设置负载均衡模式
pub async fn set_load_balancing_mode(
    State(state): State<AdminState>,
    Json(payload): Json<SetLoadBalancingModeRequest>,
) -> impl IntoResponse {
    match state.service.set_load_balancing_mode(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/kv-cache
/// 获取 KV 缓存配置
pub async fn get_kv_cache_config(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_kv_cache_config();
    Json(response)
}

/// PUT /api/admin/config/kv-cache
/// 设置 KV 缓存配置
pub async fn set_kv_cache_config(
    State(state): State<AdminState>,
    Json(payload): Json<SetKvCacheConfigRequest>,
) -> impl IntoResponse {
    match state.service.set_kv_cache_config(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/models
/// 获取模型配置
pub async fn get_models_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_models())
}

/// PUT /api/admin/config/models
/// 设置模型配置（保存即热更新生效）
pub async fn set_models_config(
    State(state): State<AdminState>,
    Json(payload): Json<SetModelsRequest>,
) -> impl IntoResponse {
    match state.service.set_models(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/restart
/// 重启服务（进程退出，由容器 restart 策略自动拉起）
pub async fn restart_service(State(state): State<AdminState>) -> impl IntoResponse {
    state.service.restart_service();
    Json(SuccessResponse::new(
        "服务将在约 0.5 秒后重启（由容器自动拉起）".to_string(),
    ))
}

/// PATCH /api/admin/credentials/:id
/// 部分更新凭据可编辑字段（email / region / proxy）
pub async fn update_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<UpdateCredentialRequest>,
) -> impl IntoResponse {
    match state.service.update_credential(id, payload) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 已更新", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/proxy-check
/// 检测代理连通性。请求体可省略，省略时检测该凭据已保存的代理。
pub async fn check_credential_proxy(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    payload: Option<Json<ProxyCheckRequest>>,
) -> impl IntoResponse {
    let req = payload.map(|Json(req)| req).unwrap_or_default();
    match state.service.check_proxy(id, req).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

// ============ 代理 IP 池 ============

/// GET /api/admin/proxy-pool
pub async fn list_proxies(State(state): State<AdminState>) -> impl IntoResponse {
    Json(json!({ "proxies": state.service.list_proxies() }))
}

/// POST /api/admin/proxy-pool  添加单个代理
pub async fn add_proxy(
    State(state): State<AdminState>,
    Json(payload): Json<AddProxyRequest>,
) -> impl IntoResponse {
    match state.service.add_proxy(payload.url, payload.label) {
        Ok(entry) => AxumJson(entry).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/proxy-pool/batch  批量添加
pub async fn batch_add_proxies(
    State(state): State<AdminState>,
    Json(payload): Json<BatchAddProxyRequest>,
) -> impl IntoResponse {
    let (added, errors) = state.service.batch_add_proxies(payload.urls);
    Json(json!({ "added": added, "errors": errors }))
}

/// DELETE /api/admin/proxy-pool/:id
pub async fn delete_proxy(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.delete_proxy(id) {
        Ok(_) => Json(SuccessResponse::new(format!("代理 #{} 已删除", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/proxy-pool/:id/enabled
pub async fn set_proxy_enabled(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetProxyEnabledRequest>,
) -> impl IntoResponse {
    match state.service.set_proxy_enabled(id, payload.enabled) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "代理 #{} 已{}",
            id,
            if payload.enabled { "启用" } else { "禁用" }
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/proxy-pool/:id/check  探测单个代理
pub async fn check_pool_proxy(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.check_pool_proxy(id).await {
        Ok(entry) => AxumJson(entry).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/proxy-pool/check-all  探测全部
pub async fn check_all_proxies(State(state): State<AdminState>) -> impl IntoResponse {
    let (healthy, unhealthy, auto_disabled) = state.service.check_all_pool_proxies().await;
    Json(json!({
        "healthy": healthy,
        "unhealthy": unhealthy,
        "autoDisabled": auto_disabled,
    }))
}

/// POST /api/admin/proxy-pool/assign  分配代理给凭据
pub async fn assign_proxy(
    State(state): State<AdminState>,
    Json(payload): Json<AssignProxyRequest>,
) -> impl IntoResponse {
    if payload.round_robin {
        return match state.service.assign_proxies_round_robin(&payload.credential_ids) {
            Ok((assigned, proxies)) => Json(json!({
                "assigned": assigned,
                "proxies": proxies,
            }))
            .into_response(),
            Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
        };
    }

    match payload.proxy_url {
        Some(url) if !url.trim().is_empty() => {
            let assigned = state
                .service
                .assign_proxy_to_credentials(url.trim(), &payload.credential_ids);
            Json(json!({ "assigned": assigned })).into_response()
        }
        _ => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "缺少 proxyUrl 或未指定 roundRobin" })),
        )
            .into_response(),
    }
}

/// POST /api/admin/credentials/:id/overage
/// 切换超额开关（占位实现）
pub async fn set_credential_overage(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetOverageRequest>,
) -> impl IntoResponse {
    match state.service.set_credential_overage(id, payload.enabled) {
        Ok(_) => {
            let action = if payload.enabled { "开启" } else { "关闭" };
            Json(SuccessResponse::new(format!(
                "凭据 #{} 超额已请求{}（实际生效请到 Kiro / AWS 控制台确认）",
                id, action
            )))
            .into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/keys
/// 查看 API Key 与 Admin API Key（脱敏 + 原值，仅授权后台返回）
pub async fn get_admin_keys(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_admin_keys()).into_response()
}

fn stats_window(
    params: &std::collections::HashMap<String, String>,
) -> Result<super::usage_stats::StatsQueryWindow, String> {
    super::usage_stats::parse_stats_window(
        params.get("range").map(String::as_str),
        params.get("startDate").map(String::as_str),
        params.get("endDate").map(String::as_str),
        params.get("granularity").map(String::as_str),
    )
}

fn stats_bad_request(message: String) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(super::types::AdminErrorResponse::invalid_request(message)),
    )
        .into_response()
}

/// GET /api/admin/stats/overview
pub async fn stats_overview(State(state): State<AdminState>) -> impl IntoResponse {
    let (overview, available, total) = state.service.stats_overview();
    Json(json!({
        "todayCalls": overview.today_calls,
        "todayInputTokens": overview.today_input_tokens,
        "todayOutputTokens": overview.today_output_tokens,
        "todayErrors": overview.today_errors,
        "todayCredits": overview.today_credits,
        "weekCalls": overview.week_calls,
        "weekInputTokens": overview.week_input_tokens,
        "weekOutputTokens": overview.week_output_tokens,
        "weekCredits": overview.week_credits,
        "activeClientKeys": 0,
        "activeCredentials": available,
        "totalCredentials": total,
    }))
}

/// GET /api/admin/stats/timeseries?range=24h|7d|30d&granularity=hour|day
pub async fn stats_timeseries(
    State(state): State<AdminState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let window = match stats_window(&params) {
        Ok(window) => window,
        Err(message) => return stats_bad_request(message),
    };
    Json(state.service.stats_timeseries(window)).into_response()
}

/// GET /api/admin/stats/by-model
pub async fn stats_by_model(
    State(state): State<AdminState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let window = match stats_window(&params) {
        Ok(window) => window,
        Err(message) => return stats_bad_request(message),
    };
    Json(state.service.stats_by_model(window)).into_response()
}

/// GET /api/admin/stats/by-credential
pub async fn stats_by_credential(
    State(state): State<AdminState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let window = match stats_window(&params) {
        Ok(window) => window,
        Err(message) => return stats_bad_request(message),
    };
    Json(state.service.stats_by_credential(window)).into_response()
}
