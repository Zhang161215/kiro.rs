//! OpenAI 兼容端点处理器
//!
//! `POST /v1/chat/completions`：将 OpenAI 请求转成内部 Anthropic 请求，
//! 复用 `anthropic::handlers::post_messages` 的完整管线（转换/上游/流处理/KV 记录/错误映射），
//! 再把 Anthropic 输出翻译回 OpenAI 格式。

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use futures::StreamExt;
use serde_json::{Value, json};

use crate::anthropic::handlers::post_messages;
use crate::anthropic::middleware::AppState;

use super::convert::{StreamTranslator, anthropic_response_to_openai, openai_to_anthropic_body};
use super::types::ChatCompletionRequest;

/// OpenAI 错误响应
fn openai_error(status: StatusCode, message: impl Into<String>, err_type: &str) -> Response {
    let body = json!({
        "error": {
            "message": message.into(),
            "type": err_type,
            "code": status.as_u16(),
        }
    });
    (status, Json(body)).into_response()
}

fn new_completion_id() -> String {
    format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// POST /v1/chat/completions
pub async fn post_chat_completions(State(state): State<AppState>, body: Bytes) -> Response {
    // 1. 解析 OpenAI 请求
    let req: ChatCompletionRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                format!("无效的请求体: {}", e),
                "invalid_request_error",
            );
        }
    };

    let is_stream = req.stream;
    let model_echo = req.model.clone();
    let id = new_completion_id();
    let created = now_ts();

    // 2. 转换为内部 Anthropic 请求
    let anthropic_value = openai_to_anthropic_body(req);
    let anthropic_body = match serde_json::to_vec(&anthropic_value) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("请求转换失败: {}", e),
                "internal_error",
            );
        }
    };

    // 3. 复用 Anthropic 管线
    let response = post_messages(State(state), anthropic_body).await;
    let status = response.status();

    // 4. 上游错误：包装成 OpenAI 错误形状透传
    if !status.is_success() {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or(text);
        return openai_error(status, message, "upstream_error");
    }

    // 5. 成功：按流式/非流式翻译
    if is_stream {
        stream_openai_response(response, id, model_echo, created)
    } else {
        non_stream_openai_response(response, id, model_echo, created).await
    }
}

/// 非流式：聚合 Anthropic JSON → OpenAI JSON
async fn non_stream_openai_response(
    response: Response,
    id: String,
    model: String,
    created: i64,
) -> Response {
    let bytes = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("读取上游响应失败: {}", e),
                "internal_error",
            );
        }
    };
    let anthropic: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("解析上游响应失败: {}", e),
                "internal_error",
            );
        }
    };
    let openai = anthropic_response_to_openai(&anthropic, model, id, created);
    Json(openai).into_response()
}

/// 流式：Anthropic SSE → OpenAI SSE
fn stream_openai_response(
    response: Response,
    id: String,
    model: String,
    created: i64,
) -> Response {
    let translator = StreamTranslator::new(id, model, created);
    let data_stream = response.into_body().into_data_stream();

    // 状态：(上游数据流, 缓冲区, 翻译器, 上游是否结束, [DONE] 是否已发)
    let init = (data_stream, Vec::<u8>::new(), translator, false, false);

    let out = futures::stream::unfold(init, |state| async move {
        let (mut data_stream, mut buffer, mut translator, upstream_ended, done_sent) = state;

        if done_sent {
            return None;
        }

        // 上游已结束：补发 finish（若需）+ [DONE]
        if upstream_ended {
            let mut out = Vec::new();
            if !translator.is_finished() {
                let chunk = translator.finish();
                if let Ok(s) = serde_json::to_string(&chunk) {
                    out.extend_from_slice(format!("data: {}\n\n", s).as_bytes());
                }
            }
            out.extend_from_slice(b"data: [DONE]\n\n");
            return Some((
                Ok::<Bytes, std::convert::Infallible>(Bytes::from(out)),
                (data_stream, buffer, translator, true, true),
            ));
        }

        // 拉取下一段上游数据
        match data_stream.next().await {
            Some(Ok(chunk)) => {
                buffer.extend_from_slice(&chunk);
                let out = drain_sse_blocks(&mut buffer, &mut translator);
                Some((
                    Ok(Bytes::from(out)),
                    (data_stream, buffer, translator, false, false),
                ))
            }
            Some(Err(_)) | None => {
                // 处理缓冲区残留后标记上游结束
                let out = drain_sse_blocks(&mut buffer, &mut translator);
                Some((
                    Ok(Bytes::from(out)),
                    (data_stream, buffer, translator, true, false),
                ))
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(out))
        .unwrap()
}

/// 从缓冲区提取所有完整的 SSE 块（以 \n\n 分隔），翻译为 OpenAI SSE 字节
fn drain_sse_blocks(buffer: &mut Vec<u8>, translator: &mut StreamTranslator) -> Vec<u8> {
    let mut out = Vec::new();

    loop {
        // 查找 \n\n 边界
        let Some(pos) = find_double_newline(buffer) else {
            break;
        };
        let block: Vec<u8> = buffer.drain(..pos + 2).collect();
        let block_str = String::from_utf8_lossy(&block);

        let mut event_name = String::new();
        let mut data_str = String::new();
        for line in block_str.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                event_name = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data_str.is_empty() {
                    data_str.push('\n');
                }
                data_str.push_str(rest.trim());
            }
        }

        if event_name == "ping" || data_str.is_empty() {
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(&data_str) else {
            continue;
        };

        for chunk in translator.handle(&event_name, &data) {
            if let Ok(s) = serde_json::to_string(&chunk) {
                out.extend_from_slice(format!("data: {}\n\n", s).as_bytes());
            }
        }
    }

    out
}

/// 查找 `\n\n` 的起始位置
fn find_double_newline(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|w| w == b"\n\n")
}
