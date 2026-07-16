//! Anthropic API Handler 函数

use std::convert::Infallible;

use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;
use anyhow::Error;
use axum::{
    body::{Body, Bytes as BodyBytes},
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use std::collections::HashMap;

use super::converter::{ConversionError, convert_request_with_models, get_context_window_size_with_config};
use super::failure_prompt_log;
use super::kv_cache::{
    KvCacheRecordInput, build_prompt_hashes, estimate_prompt_block_tokens,
    record_simulated_kv_cache,
};
use super::middleware::AppState;
use super::stream::{BufferedStreamContext, SseEvent, StreamContext, extract_thinking_from_complete_text};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, SystemMessage, Thinking,
};
use super::websearch;

fn scale_cache_tokens(value: i32, from_total: i32, to_total: i32) -> i32 {
    if value <= 0 || to_total <= 0 {
        return 0;
    }
    if from_total <= 0 {
        return value.max(0).min(to_total.max(0));
    }
    let scaled = (value as f64) * (to_total as f64) / (from_total as f64);
    if scaled.is_finite() {
        (scaled.round() as i32).clamp(0, to_total)
    } else {
        0
    }
}

fn normalize_cache_tokens(
    total_input_tokens: i32,
    cache_creation_input_tokens: i32,
    cache_read_input_tokens: i32,
    min_non_cache_input_tokens: i32,
) -> (i32, i32) {
    let total = total_input_tokens.max(0);
    if total <= 0 {
        return (0, 0);
    }

    let mut cache_creation = cache_creation_input_tokens.max(0);
    let mut cache_read = cache_read_input_tokens.max(0);
    let min_non_cache = min_non_cache_input_tokens.max(0).min(total);
    let max_cache_total = total.saturating_sub(min_non_cache);
    let cache_total = cache_creation.saturating_add(cache_read);

    if cache_total > max_cache_total {
        let overflow = cache_total - max_cache_total;
        let reduce_creation = overflow.min(cache_creation);
        cache_creation -= reduce_creation;
        let remaining = overflow - reduce_creation;
        if remaining > 0 {
            cache_read = cache_read.saturating_sub(remaining);
        }
    }

    (cache_creation, cache_read)
}

fn scale_cache_usage_tokens(
    cache_creation_input_tokens: i32,
    cache_read_input_tokens: i32,
    estimated_total_input_tokens: i32,
    actual_total_input_tokens: i32,
    estimated_non_cache_input_tokens: i32,
) -> (i32, i32) {
    let min_non_cache = if estimated_non_cache_input_tokens > 0 {
        1
    } else {
        0
    };
    let scaled_creation = scale_cache_tokens(
        cache_creation_input_tokens,
        estimated_total_input_tokens,
        actual_total_input_tokens,
    );
    let scaled_read = scale_cache_tokens(
        cache_read_input_tokens,
        estimated_total_input_tokens,
        actual_total_input_tokens,
    );
    normalize_cache_tokens(
        actual_total_input_tokens,
        scaled_creation,
        scaled_read,
        min_non_cache,
    )
}

fn non_cache_input_tokens(
    total_input_tokens: i32,
    cache_creation_input_tokens: i32,
    cache_read_input_tokens: i32,
) -> i32 {
    let total = (total_input_tokens.max(0)) as i64;
    let cache_creation = (cache_creation_input_tokens.max(0)) as i64;
    let cache_read = (cache_read_input_tokens.max(0)) as i64;
    let cache_total = cache_creation.saturating_add(cache_read);
    let result = total.saturating_sub(cache_total);
    result.clamp(0, i64::from(i32::MAX)) as i32
}

const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

#[derive(Debug, Clone, Default)]
struct BillingHeaderRectifyResult {
    applied: bool,
    removed_count: usize,
    extracted_values: Vec<String>,
}

fn is_billing_header_line(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.len() >= BILLING_HEADER_PREFIX.len()
        && trimmed[..BILLING_HEADER_PREFIX.len()].eq_ignore_ascii_case(BILLING_HEADER_PREFIX)
}

fn rectify_billing_header(system: &mut Option<Vec<SystemMessage>>) -> BillingHeaderRectifyResult {
    let Some(blocks) = system.as_mut() else {
        return BillingHeaderRectifyResult::default();
    };

    let mut extracted_values = Vec::new();
    blocks.retain(|block| {
        if is_billing_header_line(&block.text) {
            extracted_values.push(block.text.clone());
            false
        } else {
            true
        }
    });

    let removed_count = extracted_values.len();
    BillingHeaderRectifyResult {
        applied: removed_count > 0,
        removed_count,
        extracted_values,
    }
}

fn is_control_character_json_error(err: &serde_json::Error) -> bool {
    err.to_string().contains("control character")
}

fn escape_control_char(ch: char, out: &mut String) {
    match ch {
        '\u{0008}' => out.push_str("\\b"),
        '\u{0009}' => out.push_str("\\t"),
        '\u{000A}' => out.push_str("\\n"),
        '\u{000C}' => out.push_str("\\f"),
        '\u{000D}' => out.push_str("\\r"),
        _ => {
            let code = ch as u32;
            out.push_str("\\u00");
            let high = ((code >> 4) & 0xF) as u8;
            let low = (code & 0xF) as u8;
            let to_hex = |v: u8| -> char {
                match v {
                    0..=9 => (b'0' + v) as char,
                    _ => (b'a' + (v - 10)) as char,
                }
            };
            out.push(to_hex(high));
            out.push(to_hex(low));
        }
    }
}

fn sanitize_json_control_chars_in_strings(input: &str) -> (String, usize) {
    let mut output = String::with_capacity(input.len());
    let mut replaced = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for ch in input.chars() {
        if in_string {
            if escaped {
                output.push(ch);
                escaped = false;
                continue;
            }

            match ch {
                '\\' => {
                    output.push(ch);
                    escaped = true;
                }
                '"' => {
                    output.push(ch);
                    in_string = false;
                }
                '\u{0000}'..='\u{001F}' => {
                    escape_control_char(ch, &mut output);
                    replaced += 1;
                }
                _ => output.push(ch),
            }
        } else {
            if ch == '"' {
                in_string = true;
            }
            output.push(ch);
        }
    }

    (output, replaced)
}

fn parse_json_with_control_char_tolerance<T>(
    body: &BodyBytes,
) -> Result<(T, bool), serde_json::Error>
where
    T: DeserializeOwned,
{
    match serde_json::from_slice::<T>(body) {
        Ok(payload) => Ok((payload, false)),
        Err(primary_err) => {
            if !is_control_character_json_error(&primary_err) {
                return Err(primary_err);
            }

            let body_str = match std::str::from_utf8(body) {
                Ok(s) => s,
                Err(_) => return Err(primary_err),
            };
            let (sanitized, replaced_count) = sanitize_json_control_chars_in_strings(body_str);
            if replaced_count == 0 {
                return Err(primary_err);
            }

            match serde_json::from_str::<T>(&sanitized) {
                Ok(payload) => Ok((payload, true)),
                Err(secondary_err) => Err(secondary_err),
            }
        }
    }
}

fn json_parse_error_response(err: serde_json::Error) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new(
            "invalid_request_error",
            format!("Failed to parse the request body as JSON: {}", err),
        )),
    )
        .into_response()
}

fn parse_request_body<T>(endpoint: &'static str, body: &BodyBytes) -> Result<(T, bool), Response>
where
    T: DeserializeOwned,
{
    match parse_json_with_control_char_tolerance::<T>(body) {
        Ok((payload, sanitized)) => {
            if sanitized {
                tracing::warn!(endpoint, "请求体包含未转义控制字符，已自动转义后继续处理");
            }
            Ok((payload, sanitized))
        }
        Err(err) => {
            tracing::warn!(endpoint, error = %err, "请求体 JSON 解析失败");
            Err(json_parse_error_response(err))
        }
    }
}

/// 将 KiroProvider 错误映射为 HTTP 响应
fn map_provider_error(
    _provider: &crate::kiro::provider::KiroProvider,
    endpoint: &'static str,
    model: &str,
    request_body: &str,
    err: Error,
) -> Response {
    let err_str = err.to_string();

    // 记录 malformed request / tool call failed 对应的 prompt，便于后续排障。
    failure_prompt_log::maybe_record_failure_prompt(
        None,
        endpoint,
        model,
        request_body,
        "provider_error",
        &err_str,
    );

    // 上游保护触发（所有凭据在该模型上均繁忙）→ 返回 429
    if err.downcast_ref::<crate::kiro::token_manager::UpstreamBusyError>().is_some() {
        tracing::warn!(error = %err, "上游保护触发：所有凭据繁忙");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse::new(
                "rate_limit_error",
                &err_str,
            )),
        )
            .into_response();
    }

    // 上下文窗口满了（对话历史累积超出模型上下文窗口限制）
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Context window is full. Reduce conversation history, system prompt, or tools.",
            )),
        )
            .into_response();
    }

    // 单次输入太长（请求体本身超出上游限制）
    if err_str.contains("Input is too long") {
        tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Input is too long. Reduce the size of your messages.",
            )),
        )
            .into_response();
    }
    tracing::error!("Kiro API 调用失败: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            format!("上游 API 调用失败: {}", err),
        )),
    )
        .into_response()
}

/// GET /v1/models
///
/// 返回可用的模型列表。
/// 始终返回内置硬编码模型 + 全局注册表中配置的额外模型（config.json / admin 后台热更新）。
pub async fn get_models(State(_state): State<AppState>) -> impl IntoResponse {
    tracing::info!("Received GET /v1/models request");

    let models = vec![
        Model {
            id: "claude-sonnet-4-5-20250929".to_string(),
            object: "model".to_string(),
            created: 1727568000,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929-thinking".to_string(),
            object: "model".to_string(),
            created: 1727568000,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101".to_string(),
            object: "model".to_string(),
            created: 1730419200,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101-thinking".to_string(),
            object: "model".to_string(),
            created: 1730419200,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6".to_string(),
            object: "model".to_string(),
            created: 1770314400,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1770314400,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6".to_string(),
            object: "model".to_string(),
            created: 1770314400,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1770314400,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-7".to_string(),
            object: "model".to_string(),
            created: 1775000000,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-7-thinking".to_string(),
            object: "model".to_string(),
            created: 1775000000,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001".to_string(),
            object: "model".to_string(),
            created: 1727740800,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001-thinking".to_string(),
            object: "model".to_string(),
            created: 1727740800,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        // OpenAI GPT（Kiro 上游 GPT-5.6，档位 Sol/Terra/Luna，272K 上下文）
        Model {
            id: "gpt-5.6".to_string(),
            object: "model".to_string(),
            created: 1775000000,
            owned_by: "openai".to_string(),
            display_name: "GPT-5.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "gpt-5.6-sol".to_string(),
            object: "model".to_string(),
            created: 1775000000,
            owned_by: "openai".to_string(),
            display_name: "GPT-5.6 Sol (旗舰)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "gpt-5.6-terra".to_string(),
            object: "model".to_string(),
            created: 1775000000,
            owned_by: "openai".to_string(),
            display_name: "GPT-5.6 Terra (均衡)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "gpt-5.6-luna".to_string(),
            object: "model".to_string(),
            created: 1775000000,
            owned_by: "openai".to_string(),
            display_name: "GPT-5.6 Luna (高效)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
    ];

    // 追加全局注册表中配置的额外模型（admin 后台热更新的来源）
    let registry_models = super::model_registry::get_models();
    let mut all_models = models;
    for entry in registry_models.iter() {
        all_models.push(Model {
            id: entry.id.clone(),
            object: "model".to_string(),
            created: entry.created,
            owned_by: "anthropic".to_string(),
            display_name: entry.display_name.clone(),
            model_type: "chat".to_string(),
            max_tokens: entry.max_tokens as i32,
        });
        all_models.push(Model {
            id: format!("{}-thinking", entry.id),
            object: "model".to_string(),
            created: entry.created,
            owned_by: "anthropic".to_string(),
            display_name: format!("{} (Thinking)", entry.display_name),
            model_type: "chat".to_string(),
            max_tokens: entry.max_tokens as i32,
        });
    }

    Json(ModelsResponse {
        object: "list".to_string(),
        data: all_models,
    })
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(State(state): State<AppState>, body: BodyBytes) -> Response {
    let (mut payload, _) = match parse_request_body::<MessagesRequest>("/v1/messages", &body) {
        Ok(parsed) => parsed,
        Err(resp) => return resp,
    };

    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages request"
    );
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    let mut special_settings: Vec<String> = Vec::new();
    {
        let rectify_result = rectify_billing_header(&mut payload.system);
        if rectify_result.applied {
            special_settings.push("billing_header_rectifier".to_string());
            tracing::info!(
                removed_count = rectify_result.removed_count,
                "已预清洗 system 中的 x-anthropic-billing-header"
            );
            tracing::debug!(
                extracted_values = ?rectify_result.extracted_values,
                "billing header 清洗明细"
            );
        }
    }

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
    }

    // 转换请求
    let conversion_result = match convert_request_with_models(&payload, &super::model_registry::get_models()) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // 构建 Kiro 请求
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 构建 prompt block 指纹（用于模拟 KV cache 命中）
    let prompt_hashes = build_prompt_hashes(&payload.system, &payload.messages, &payload.tools);

    // 估算输入 tokens（本地、稳定、可复现）
    let block_tokens =
        estimate_prompt_block_tokens(&payload.system, &payload.messages, &payload.tools);
    let input_tokens = block_tokens.iter().copied().sum::<i32>().max(1);

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    if payload.stream {
        // 流式响应
        handle_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            prompt_hashes,
            block_tokens,
            thinking_enabled,
            "/v1/messages",
            special_settings,
            conversion_result.tool_name_map,
            get_context_window_size_with_config(&payload.model, &super::model_registry::get_models()),
        )
        .await
    } else {
        // 非流式响应
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            prompt_hashes,
            block_tokens,
            "/v1/messages",
            false,
            special_settings,
            conversion_result.tool_name_map,
            state.extract_thinking,
            get_context_window_size_with_config(&payload.model, &super::model_registry::get_models()),
        )
        .await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    prompt_hashes: Vec<String>,
    block_tokens: Vec<i32>,
    thinking_enabled: bool,
    endpoint: &'static str,
    special_settings: Vec<String>,
    tool_name_map: HashMap<String, String>,
    context_window: i32,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let api_response = match provider.call_api_stream(request_body).await {
        Ok(resp) => resp,
        Err(e) => {
            return map_provider_error(provider.as_ref(), endpoint, model, request_body, e);
        }
    };
    let _upstream_guard = api_response.upstream_guard;
    let response = api_response.response;

    // 创建流处理上下文
    let mut ctx = StreamContext::new_with_thinking(model, input_tokens, thinking_enabled, false, tool_name_map);
    ctx.set_context_window_size(context_window);

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流
    let stream = create_sse_stream(
        response,
        ctx,
        initial_events,
        endpoint,
        model.to_string(),
        request_body.to_string(),
        prompt_hashes,
        block_tokens,
        special_settings,
    );

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
    endpoint: &'static str,
    model: String,
    request_body: String,
    prompt_hashes: Vec<String>,
    block_tokens: Vec<i32>,
    special_settings: Vec<String>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();

    let processing_stream = stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            0.0_f64,
            endpoint,
            model,
            request_body,
            false,
            prompt_hashes,
            block_tokens,
            special_settings,
        ),
        |(
            mut body_stream,
            mut ctx,
            mut decoder,
            finished,
            mut ping_interval,
            credits_used,
            endpoint,
            model,
            request_body,
            mut failure_prompt_recorded,
            prompt_hashes,
            block_tokens,
            special_settings,
        )| async move {
            if finished {
                return None;
            }

            // 使用 select! 同时等待数据和 ping 定时器
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            let mut events = Vec::new();
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        if let Ok(event) = Event::from_frame(frame) {
                                            if let Event::Metering(_) = &event {
                                                // Metering is unit type, no data to extract
                                            }
                                            if !failure_prompt_recorded {
                                                let maybe_error = match &event {
                                                    Event::Error {
                                                        error_code,
                                                        error_message,
                                                    } => Some(format!(
                                                        "{} - {}",
                                                        error_code, error_message
                                                    )),
                                                    Event::Exception {
                                                        exception_type,
                                                        message,
                                                    } => Some(format!(
                                                        "{} - {}",
                                                        exception_type, message
                                                    )),
                                                    _ => None,
                                                };
                                                if let Some(error_text) = maybe_error {
                                                    failure_prompt_recorded = failure_prompt_log::maybe_record_failure_prompt(
                                                        None,
                                                        endpoint,
                                                        &model,
                                                        &request_body,
                                                        "stream_event",
                                                        &error_text,
                                                    );
                                                }
                                            }
                                            let sse_events = ctx.process_kiro_event(&event);
                                            events.extend(sse_events);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("解码事件失败: {}", e);
                                    }
                                }
                            }

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            Some((
                                stream::iter(bytes),
                                (
                                    body_stream,
                                    ctx,
                                    decoder,
                                    false,
                                    ping_interval,
                                    credits_used,
                                    endpoint,
                                    model,
                                    request_body,
                                    failure_prompt_recorded,
                                    prompt_hashes,
                                    block_tokens,
                                    special_settings,
                                ),
                            ))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            let (_, final_output_tokens) = ctx.final_usage_tokens();
                            let estimated_input_tokens = ctx.input_tokens;
                            let kv = record_simulated_kv_cache(
                                None,
                                KvCacheRecordInput {
                                    endpoint,
                                    model: model.clone(),
                                    stream: true,
                                    prompt_hashes: prompt_hashes.clone(),
                                    block_tokens: block_tokens.clone(),
                                    input_tokens: estimated_input_tokens,
                                    output_tokens: final_output_tokens,
                                    credits_used,
                                    special_settings: special_settings.clone(),
                                },
                            );
                            ctx.set_extra_usage(
                                kv.cache_creation_input_tokens,
                                kv.cache_read_input_tokens,
                                credits_used,
                            );
                            // 发送最终事件并结束
                            let final_events = ctx.generate_final_events();
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((
                                stream::iter(bytes),
                                (
                                    body_stream,
                                    ctx,
                                    decoder,
                                    true,
                                    ping_interval,
                                    credits_used,
                                    endpoint,
                                    model,
                                    request_body,
                                    failure_prompt_recorded,
                                    prompt_hashes,
                                    block_tokens,
                                    special_settings,
                                ),
                            ))
                        }
                        None => {
                            let (_, final_output_tokens) = ctx.final_usage_tokens();
                            let estimated_input_tokens = ctx.input_tokens;
                            let kv = record_simulated_kv_cache(
                                None,
                                KvCacheRecordInput {
                                    endpoint,
                                    model: model.clone(),
                                    stream: true,
                                    prompt_hashes: prompt_hashes.clone(),
                                    block_tokens: block_tokens.clone(),
                                    input_tokens: estimated_input_tokens,
                                    output_tokens: final_output_tokens,
                                    credits_used,
                                    special_settings: special_settings.clone(),
                                },
                            );
                            ctx.set_extra_usage(
                                kv.cache_creation_input_tokens,
                                kv.cache_read_input_tokens,
                                credits_used,
                            );
                            // 流结束，发送最终事件
                            let final_events = ctx.generate_final_events();
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((
                                stream::iter(bytes),
                                (
                                    body_stream,
                                    ctx,
                                    decoder,
                                    true,
                                    ping_interval,
                                    credits_used,
                                    endpoint,
                                    model,
                                    request_body,
                                    failure_prompt_recorded,
                                    prompt_hashes,
                                    block_tokens,
                                    special_settings,
                                ),
                            ))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((
                        stream::iter(bytes),
                        (
                            body_stream,
                            ctx,
                            decoder,
                            false,
                            ping_interval,
                            credits_used,
                            endpoint,
                            model,
                            request_body,
                            failure_prompt_recorded,
                            prompt_hashes,
                            block_tokens,
                            special_settings,
                        ),
                    ))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
}

/// 上下文窗口大小（200k tokens）


/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    prompt_hashes: Vec<String>,
    block_tokens: Vec<i32>,
    endpoint: &'static str,
    use_context_input_tokens: bool,
    special_settings: Vec<String>,
    tool_name_map: HashMap<String, String>,
    extract_thinking: bool,
    context_window: i32,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let api_response = match provider.call_api(request_body).await {
        Ok(resp) => resp,
        Err(e) => {
            return map_provider_error(provider.as_ref(), endpoint, model, request_body, e);
        }
    };
    let _upstream_guard = api_response.upstream_guard;
    let response = api_response.response;

    // 读取响应体
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("读取响应体失败: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("读取响应失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    let request_credits_used = 0.0_f64;
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;
    let mut failure_prompt_recorded = false;

    // 收集工具调用的增量 JSON
    let mut tool_json_buffers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                if let Ok(event) = Event::from_frame(frame) {
                    match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;

                            // 累积工具的 JSON 输入
                            let buffer = tool_json_buffers
                                .entry(tool_use.tool_use_id.clone())
                                .or_insert_with(String::new);
                            buffer.push_str(&tool_use.input);

                            // 如果是完整的工具调用，添加到列表
                            if tool_use.stop {
                                let input: serde_json::Value = if buffer.is_empty() {
                                    serde_json::json!({})
                                } else {
                                    serde_json::from_str(buffer).unwrap_or_else(|e| {
                                        tracing::warn!(
                                            "工具输入 JSON 解析失败: {}, tool_use_id: {}",
                                            e,
                                            tool_use.tool_use_id
                                        );
                                        serde_json::json!({})
                                    })
                                };

                                tool_uses.push(json!({
                                    "type": "tool_use",
                                    "id": tool_use.tool_use_id,
                                    "name": tool_name_map.get(&tool_use.name).cloned().unwrap_or_else(|| tool_use.name.clone()),
                                    "input": input
                                }));
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            // 从上下文使用百分比计算实际的 input_tokens
                            let actual_input_tokens = (context_usage.context_usage_percentage
                                * (context_window as f64)
                                / 100.0)
                                as i32;
                            context_input_tokens = Some(actual_input_tokens);
                            // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                            if context_usage.context_usage_percentage >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                            tracing::debug!(
                                "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                context_usage.context_usage_percentage,
                                actual_input_tokens
                            );
                        }
                        Event::Metering(_) => {
                            // Metering is unit type in current API, no data to extract
                        }
                        Event::Error {
                            error_code,
                            error_message,
                        } => {
                            if !failure_prompt_recorded {
                                let error_text = format!("{} - {}", error_code, error_message);
                                failure_prompt_recorded =
                                    failure_prompt_log::maybe_record_failure_prompt(
                                        None,
                                        endpoint,
                                        model,
                                        request_body,
                                        "non_stream_event",
                                        &error_text,
                                    );
                            }
                        }
                        Event::Exception {
                            exception_type,
                            message,
                        } => {
                            if exception_type == "ContentLengthExceededException" {
                                stop_reason = "max_tokens".to_string();
                            }
                            if !failure_prompt_recorded {
                                let error_text = format!("{} - {}", exception_type, message);
                                failure_prompt_recorded =
                                    failure_prompt_log::maybe_record_failure_prompt(
                                        None,
                                        endpoint,
                                        model,
                                        request_body,
                                        "non_stream_event",
                                        &error_text,
                                    );
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 构建响应内容
    let mut content: Vec<serde_json::Value> = Vec::new();

    if !text_content.is_empty() {
        // 如果启用了 extract_thinking，从文本中提取 <thinking> 块
        if extract_thinking {
            let (thinking, remaining) = extract_thinking_from_complete_text(&text_content);
            if let Some(thinking_text) = thinking {
                content.push(json!({
                    "type": "thinking",
                    "thinking": thinking_text
                }));
            }
            if !remaining.is_empty() {
                content.push(json!({
                    "type": "text",
                    "text": remaining
                }));
            }
        } else {
            content.push(json!({
                "type": "text",
                "text": text_content
            }));
        }
    }

    content.extend(tool_uses);

    // 估算输出 tokens
    let output_tokens = token::estimate_output_tokens(&content);

    // 决定对外返回的 input_tokens 口径
    let final_total_input_tokens = if use_context_input_tokens {
        context_input_tokens.unwrap_or(input_tokens)
    } else {
        input_tokens
    }
    .max(0);

    // KV cache 模拟始终使用“本地估算 tokens”（input_tokens），保证稳定可复现。
    let kv = record_simulated_kv_cache(
        None,
        KvCacheRecordInput {
            endpoint,
            model: model.to_string(),
            stream: false,
            prompt_hashes,
            block_tokens,
            input_tokens,
            output_tokens,
            credits_used: request_credits_used,
            special_settings,
        },
    );
    // 如果对外 input_tokens 使用了 contextUsageEvent（/cc/v1/messages），则按比例缩放 cache tokens，
    // 让 usage 字段在同一 token 口径下更一致（同时保持“<1k 不做缓存”的估算口径）。
    let estimated_non_cache_input_tokens = non_cache_input_tokens(
        input_tokens,
        kv.cache_creation_input_tokens,
        kv.cache_read_input_tokens,
    );
    let (cache_creation_input_tokens, cache_read_input_tokens) =
        if use_context_input_tokens && context_input_tokens.is_some() {
            scale_cache_usage_tokens(
                kv.cache_creation_input_tokens,
                kv.cache_read_input_tokens,
                input_tokens,
                final_total_input_tokens,
                estimated_non_cache_input_tokens,
            )
        } else {
            normalize_cache_tokens(
                final_total_input_tokens,
                kv.cache_creation_input_tokens,
                kv.cache_read_input_tokens,
                if estimated_non_cache_input_tokens > 0 {
                    1
                } else {
                    0
                },
            )
        };
    let response_input_tokens = non_cache_input_tokens(
        final_total_input_tokens,
        cache_creation_input_tokens,
        cache_read_input_tokens,
    );

    // 构建 Anthropic 响应
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": response_input_tokens,
            "output_tokens": output_tokens,
            "cache_creation_input_tokens": cache_creation_input_tokens,
            "cache_read_input_tokens": cache_read_input_tokens,
            "credits_used": request_credits_used
        }
    });

    (StatusCode::OK, Json(response_body)).into_response()
}

/// 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
///
/// - Opus 4.6/4.7：覆写为 adaptive 类型
/// - 其他模型：覆写为 enabled 类型
/// - budget_tokens 固定为 20000
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") {
        return;
    }

    let is_opus_4_6_or_newer = model_lower.contains("opus")
        && (model_lower.contains("4-6") || model_lower.contains("4.6")
            || model_lower.contains("4-7") || model_lower.contains("4.7"));

    let thinking_type = if is_opus_4_6_or_newer { "adaptive" } else { "enabled" };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });

    if is_opus_4_6_or_newer {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(body: BodyBytes) -> Response {
    let (payload, _) =
        match parse_request_body::<CountTokensRequest>("/v1/messages/count_tokens", &body) {
            Ok(parsed) => parsed,
            Err(resp) => return resp,
        };

    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
    .into_response()
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(State(state): State<AppState>, body: BodyBytes) -> Response {
    let (mut payload, _) = match parse_request_body::<MessagesRequest>("/cc/v1/messages", &body) {
        Ok(parsed) => parsed,
        Err(resp) => return resp,
    };

    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    let mut special_settings: Vec<String> = Vec::new();
    {
        let rectify_result = rectify_billing_header(&mut payload.system);
        if rectify_result.applied {
            special_settings.push("billing_header_rectifier".to_string());
            tracing::info!(
                removed_count = rectify_result.removed_count,
                "已预清洗 system 中的 x-anthropic-billing-header"
            );
            tracing::debug!(
                extracted_values = ?rectify_result.extracted_values,
                "billing header 清洗明细"
            );
        }
    }

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
    }

    // 转换请求
    let conversion_result = match convert_request_with_models(&payload, &super::model_registry::get_models()) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // 构建 Kiro 请求
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 构建 prompt block 指纹（用于模拟 KV cache 命中）
    let prompt_hashes = build_prompt_hashes(&payload.system, &payload.messages, &payload.tools);

    // 估算输入 tokens（本地、稳定、可复现）
    let block_tokens =
        estimate_prompt_block_tokens(&payload.system, &payload.messages, &payload.tools);
    let input_tokens = block_tokens.iter().copied().sum::<i32>().max(1);

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    if payload.stream {
        // 流式响应（缓冲模式）
        handle_stream_request_buffered(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            prompt_hashes,
            block_tokens,
            thinking_enabled,
            "/cc/v1/messages",
            special_settings,
            conversion_result.tool_name_map,
            get_context_window_size_with_config(&payload.model, &super::model_registry::get_models()),
        )
        .await
    } else {
        // 非流式响应（复用现有逻辑，已经使用正确的 input_tokens）
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            prompt_hashes,
            block_tokens,
            "/cc/v1/messages",
            true,
            special_settings,
            conversion_result.tool_name_map,
            state.extract_thinking,
            get_context_window_size_with_config(&payload.model, &super::model_registry::get_models()),
        )
        .await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用从 contextUsageEvent 计算的正确 input_tokens 生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    estimated_input_tokens: i32,
    prompt_hashes: Vec<String>,
    block_tokens: Vec<i32>,
    thinking_enabled: bool,
    endpoint: &'static str,
    special_settings: Vec<String>,
    tool_name_map: HashMap<String, String>,
    context_window: i32,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let api_response = match provider.call_api_stream(request_body).await {
        Ok(resp) => resp,
        Err(e) => {
            return map_provider_error(provider.as_ref(), endpoint, model, request_body, e);
        }
    };
    let _upstream_guard = api_response.upstream_guard;
    let response = api_response.response;

    // 创建缓冲流处理上下文
    let mut ctx = BufferedStreamContext::new(model, estimated_input_tokens, thinking_enabled, tool_name_map);
    ctx.set_context_window_size(context_window);

    // 创建缓冲 SSE 流
    let stream = create_buffered_sse_stream(
        response,
        ctx,
        endpoint,
        model.to_string(),
        request_body.to_string(),
        prompt_hashes,
        block_tokens,
        special_settings,
    );

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 创建缓冲 SSE 事件流
fn create_buffered_sse_stream(
    response: reqwest::Response,
    ctx: BufferedStreamContext,
    endpoint: &'static str,
    model: String,
    request_body: String,
    prompt_hashes: Vec<String>,
    block_tokens: Vec<i32>,
    special_settings: Vec<String>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            0.0_f64,
            endpoint,
            model,
            request_body,
            false,
            prompt_hashes,
            block_tokens,
            special_settings,
        ),
        |(
            mut body_stream,
            mut ctx,
            mut decoder,
            finished,
            mut ping_interval,
            credits_used,
            endpoint,
            model,
            request_body,
            mut failure_prompt_recorded,
            prompt_hashes,
            block_tokens,
            special_settings,
        )| async move {
            if finished {
                return None;
            }

            loop {
                tokio::select! {
                    biased;

                    _ = ping_interval.tick() => {
                        tracing::trace!("发送 ping 保活事件（缓冲模式）");
                        let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                            return Some((
                                stream::iter(bytes),
                                (
                                    body_stream,
                                    ctx,
                                    decoder,
                                    false,
                                    ping_interval,
                                    credits_used,
                                    endpoint,
                                    model,
                                    request_body,
                                    failure_prompt_recorded,
                                    prompt_hashes,
                                    block_tokens,
                                    special_settings,
                                ),
                            ));
                        }

                    chunk_result = body_stream.next() => {
                        match chunk_result {
                            Some(Ok(chunk)) => {
                                if let Err(e) = decoder.feed(&chunk) {
                                    tracing::warn!("缓冲区溢出: {}", e);
                                }

                                for result in decoder.decode_iter() {
                                    match result {
                                        Ok(frame) => {
                                            if let Ok(event) = Event::from_frame(frame) {
                                                if let Event::Metering(_) = &event {
                                                    // Metering is unit type, no data
                                                }
                                                if !failure_prompt_recorded {
                                                    let maybe_error = match &event {
                                                        Event::Error {
                                                            error_code,
                                                            error_message,
                                                        } => Some(format!(
                                                            "{} - {}",
                                                            error_code, error_message
                                                        )),
                                                        Event::Exception {
                                                            exception_type,
                                                            message,
                                                        } => Some(format!(
                                                            "{} - {}",
                                                            exception_type, message
                                                        )),
                                                        _ => None,
                                                    };
                                                    if let Some(error_text) = maybe_error {
                                                        failure_prompt_recorded = failure_prompt_log::maybe_record_failure_prompt(
                                                            None,
                                                            endpoint,
                                                            &model,
                                                            &request_body,
                                                            "buffered_stream_event",
                                                            &error_text,
                                                        );
                                                    }
                                                }
                                                ctx.process_and_buffer(&event);
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("解码事件失败: {}", e);
                                        }
                                    }
                                }
                                // 继续读取下一个 chunk
                            }
                            Some(Err(e)) => {
                                tracing::error!("读取响应流失败: {}", e);
                                let (final_total_input_tokens, final_output_tokens) =
                                    ctx.final_usage_tokens();
                                let estimated_input_tokens = ctx.estimated_input_tokens();
                                let kv = record_simulated_kv_cache(
                                    None,
                                    KvCacheRecordInput {
                                        endpoint,
                                        model: model.clone(),
                                        stream: true,
                                        prompt_hashes: prompt_hashes.clone(),
                                        block_tokens: block_tokens.clone(),
                                        input_tokens: estimated_input_tokens,
                                        output_tokens: final_output_tokens,
                                        credits_used,
                                        special_settings: special_settings.clone(),
                                    },
                                );
                                let estimated_non_cache_input_tokens = non_cache_input_tokens(
                                    estimated_input_tokens,
                                    kv.cache_creation_input_tokens,
                                    kv.cache_read_input_tokens,
                                );
                                let (cache_creation_input_tokens, cache_read_input_tokens) =
                                    scale_cache_usage_tokens(
                                        kv.cache_creation_input_tokens,
                                        kv.cache_read_input_tokens,
                                        estimated_input_tokens,
                                        final_total_input_tokens,
                                        estimated_non_cache_input_tokens,
                                    );
                                ctx.set_extra_usage(
                                    cache_creation_input_tokens,
                                    cache_read_input_tokens,
                                    credits_used,
                                );
                                let all_events = ctx.finish_and_get_all_events();
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((
                                    stream::iter(bytes),
                                    (
                                        body_stream,
                                        ctx,
                                        decoder,
                                        true,
                                        ping_interval,
                                        credits_used,
                                        endpoint,
                                        model,
                                        request_body,
                                        failure_prompt_recorded,
                                        prompt_hashes,
                                        block_tokens,
                                        special_settings,
                                    ),
                                ));
                            }
                            None => {
                                let (final_total_input_tokens, final_output_tokens) =
                                    ctx.final_usage_tokens();
                                let estimated_input_tokens = ctx.estimated_input_tokens();
                                let kv = record_simulated_kv_cache(
                                    None,
                                    KvCacheRecordInput {
                                        endpoint,
                                        model: model.clone(),
                                        stream: true,
                                        prompt_hashes: prompt_hashes.clone(),
                                        block_tokens: block_tokens.clone(),
                                        input_tokens: estimated_input_tokens,
                                        output_tokens: final_output_tokens,
                                        credits_used,
                                        special_settings: special_settings.clone(),
                                    },
                                );
                                let estimated_non_cache_input_tokens = non_cache_input_tokens(
                                    estimated_input_tokens,
                                    kv.cache_creation_input_tokens,
                                    kv.cache_read_input_tokens,
                                );
                                let (cache_creation_input_tokens, cache_read_input_tokens) =
                                    scale_cache_usage_tokens(
                                        kv.cache_creation_input_tokens,
                                        kv.cache_read_input_tokens,
                                        estimated_input_tokens,
                                        final_total_input_tokens,
                                        estimated_non_cache_input_tokens,
                                    );
                                ctx.set_extra_usage(
                                    cache_creation_input_tokens,
                                    cache_read_input_tokens,
                                    credits_used,
                                );
                                let all_events = ctx.finish_and_get_all_events();
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((
                                    stream::iter(bytes),
                                    (
                                        body_stream,
                                        ctx,
                                        decoder,
                                        true,
                                        ping_interval,
                                        credits_used,
                                        endpoint,
                                        model,
                                        request_body,
                                        failure_prompt_recorded,
                                        prompt_hashes,
                                        block_tokens,
                                        special_settings,
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_messages_body(content: &str) -> String {
        format!(
            r#"{{"model":"claude-haiku-4-5-20251001","max_tokens":16,"messages":[{{"role":"user","content":"{content}"}}],"stream":false}}"#
        )
    }

    #[test]
    fn test_parse_json_with_control_char_tolerance_escapes_unescaped_newline() {
        let body = format!(
            r#"{{"model":"claude-haiku-4-5-20251001","max_tokens":16,"messages":[{{"role":"user","content":"hello{}world"}}],"stream":false}}"#,
            '\n'
        );
        assert!(serde_json::from_str::<MessagesRequest>(&body).is_err());

        let bytes = BodyBytes::from(body);
        let (parsed, sanitized) =
            parse_json_with_control_char_tolerance::<MessagesRequest>(&bytes).unwrap();

        assert!(sanitized);
        assert_eq!(parsed.messages[0].content.as_str(), Some("hello\nworld"));
    }

    #[test]
    fn test_parse_json_with_control_char_tolerance_keeps_valid_payload_unchanged() {
        let body = make_messages_body("hello");
        let bytes = BodyBytes::from(body);
        let (parsed, sanitized) =
            parse_json_with_control_char_tolerance::<MessagesRequest>(&bytes).unwrap();

        assert!(!sanitized);
        assert_eq!(parsed.model, "claude-haiku-4-5-20251001");
        assert_eq!(parsed.messages.len(), 1);
    }

    #[test]
    fn test_sanitize_json_control_chars_in_strings_replaces_newline() {
        let input = format!(
            r#"{{"messages":[{{"role":"user","content":"hello{}world"}}]}}"#,
            '\n'
        );
        let (sanitized, replaced) = sanitize_json_control_chars_in_strings(&input);
        assert_eq!(replaced, 1);
        assert!(sanitized.contains("hello\\nworld"));
    }

    #[test]
    fn test_parse_json_with_control_char_tolerance_count_tokens_with_newline() {
        let body = format!(
            r#"{{"model":"claude-haiku-4-5-20251001","messages":[{{"role":"user","content":"hello{}world"}}]}}"#,
            '\n'
        );
        assert!(serde_json::from_str::<CountTokensRequest>(&body).is_err());

        let (sanitized_body, replaced) = sanitize_json_control_chars_in_strings(&body);
        assert_eq!(replaced, 1);
        let pre_parsed: CountTokensRequest = serde_json::from_str(&sanitized_body).unwrap();
        assert_eq!(
            pre_parsed.messages[0].content.as_str(),
            Some("hello\nworld")
        );

        let bytes = BodyBytes::from(body);
        let (parsed, sanitized) =
            parse_json_with_control_char_tolerance::<CountTokensRequest>(&bytes).unwrap();
        assert!(sanitized);
        assert_eq!(parsed.messages[0].content.as_str(), Some("hello\nworld"));
    }

    #[test]
    fn test_is_billing_header_line_requires_prefix_start() {
        assert!(is_billing_header_line(
            "x-anthropic-billing-header: cc_version=2.1.36;"
        ));
        assert!(is_billing_header_line(
            "  X-Anthropic-Billing-Header: cc_entrypoint=cli;"
        ));
        assert!(!is_billing_header_line(
            "prefix x-anthropic-billing-header: cc_version=2.1.36;"
        ));
    }

    #[test]
    fn test_rectify_billing_header_only_removes_matching_blocks() {
        let mut system = Some(vec![
            SystemMessage {
                text: "You are a helpful assistant.".to_string(),
            },
            SystemMessage {
                text: "x-anthropic-billing-header: cc_version=2.1.36;".to_string(),
            },
            SystemMessage {
                text: "Follow instructions carefully.".to_string(),
            },
        ]);

        let result = rectify_billing_header(&mut system);
        assert!(result.applied);
        assert_eq!(result.removed_count, 1);
        assert_eq!(result.extracted_values.len(), 1);
        assert_eq!(system.unwrap().len(), 2);
    }

    #[test]
    fn test_scale_cache_usage_tokens_preserves_non_cache_segment() {
        // estimated_total=1000, estimated_cache=999, estimated_non_cache=1
        // scaled to actual_total=10 后，至少保留 1 个 non-cache token。
        let (cache_creation, cache_read) = scale_cache_usage_tokens(800, 199, 1000, 10, 1);
        let non_cache = non_cache_input_tokens(10, cache_creation, cache_read);
        assert_eq!(non_cache, 1);
    }

    #[test]
    fn test_scale_cache_usage_tokens_zero_total_input_returns_zero_cache() {
        let (cache_creation, cache_read) = scale_cache_usage_tokens(120, 240, 1000, 0, 10);
        assert_eq!(cache_creation, 0);
        assert_eq!(cache_read, 0);
    }

    #[test]
    fn test_non_cache_input_tokens_subtracts_read_and_creation() {
        assert_eq!(non_cache_input_tokens(100, 30, 50), 20);
        assert_eq!(non_cache_input_tokens(100, 80, 40), 0);
    }
}
