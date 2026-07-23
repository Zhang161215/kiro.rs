//! OpenAI Responses API 兼容端点 (`/v1/responses`)
//!
//! Codex CLI 等新客户端使用 Responses API 而非 Chat Completions。本模块把
//! Responses 请求转换为内部 Anthropic 请求，复用 `anthropic::handlers::post_messages`
//! 的完整管线（转换/上游/流处理/KV 记录/错误映射），再把 Anthropic 输出翻译回
//! Responses 格式（非流式 `response` 对象 / 流式 `response.*` 事件序列）。
//!
//! 通过内存态 store 支持 `previous_response_id` 多轮上下文（重启后丢失，代理场景可接受）。

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::anthropic::handlers::post_messages;
use crate::anthropic::middleware::AppState;

use super::convert::{convert_tool_choice, image_url_to_anthropic};

// ============ 请求类型 ============

/// Responses API 请求体（仅覆盖代理所需字段）
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ResponsesRequest {
    #[serde(default)]
    pub model: String,
    /// string | array(items) | object
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub stream: bool,
    /// Responses API 的 tools 为扁平格式：{ type:"function", name, description, parameters }
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub store: Option<bool>,
    #[serde(default)]
    pub max_output_tokens: Option<i32>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub metadata: Option<HashMap<String, String>>,
}

// ============ 内存态 store（previous_response_id 上下文） ============

#[derive(Clone)]
struct StoredResponse {
    previous_response_id: Option<String>,
    instructions: Option<String>,
    input: Value,
    output: Vec<Value>,
    stored_at: i64,
}

const MAX_HISTORY_DEPTH: usize = 64;
const STORE_CAP: usize = 5000;

fn store() -> &'static Mutex<HashMap<String, StoredResponse>> {
    static STORE: OnceLock<Mutex<HashMap<String, StoredResponse>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn save_response(
    id: &str,
    previous_response_id: Option<String>,
    instructions: Option<String>,
    input: Value,
    output: Vec<Value>,
) {
    let mut guard = store().lock();
    // 简易容量控制：超过上限时清理最旧的一批
    if guard.len() >= STORE_CAP {
        let mut entries: Vec<(String, i64)> =
            guard.iter().map(|(k, v)| (k.clone(), v.stored_at)).collect();
        entries.sort_by_key(|(_, ts)| *ts);
        for (k, _) in entries.into_iter().take(STORE_CAP / 5) {
            guard.remove(&k);
        }
    }
    guard.insert(
        id.to_string(),
        StoredResponse {
            previous_response_id,
            instructions,
            input,
            output,
            stored_at: now_ts(),
        },
    );
}

/// 展开 previous_response_id 链（oldest→newest），把祖先的 input/instructions/output
/// 还原成 Anthropic 消息，追加到 messages / system。返回 false 表示链首节点不存在。
fn expand_history(prev_id: &str, messages: &mut Vec<Value>, system: &mut Vec<Value>) -> bool {
    let guard = store().lock();
    if !guard.contains_key(prev_id) {
        return false;
    }

    // 反向收集链
    let mut chain: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut cursor = Some(prev_id.to_string());
    let mut depth = 0;
    while let Some(cid) = cursor {
        if depth >= MAX_HISTORY_DEPTH || visited.contains(&cid) {
            break;
        }
        let Some(node) = guard.get(&cid) else { break };
        visited.insert(cid.clone());
        chain.push(cid.clone());
        cursor = node.previous_response_id.clone();
        depth += 1;
    }
    chain.reverse();

    for cid in chain {
        if let Some(node) = guard.get(&cid) {
            if let Some(instr) = &node.instructions {
                if !instr.trim().is_empty() {
                    system.push(json!({ "text": instr }));
                }
            }
            parse_input_into_messages(&node.input, messages, system);
            output_items_to_anthropic(&node.output, messages);
        }
    }
    true
}

// ============ 工具函数 ============

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

fn gen_id(prefix: &str) -> String {
    format!("{}_{}", prefix, uuid::Uuid::new_v4().simple())
}

fn as_str_owned(v: Option<&Value>) -> Option<String> {
    v.and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// 任意值 → 字符串：null→""，string→原样，其他→紧凑 JSON
fn stringify(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// OpenAI 错误响应（OpenAI 形状）
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

// ============ 请求：Responses input → Anthropic messages ============

/// 把 Responses `input`（string | array | object）解析进 messages / system
fn parse_input_into_messages(input: &Value, messages: &mut Vec<Value>, system: &mut Vec<Value>) {
    match input {
        Value::String(s) => {
            if !s.trim().is_empty() {
                messages.push(json!({ "role": "user", "content": s }));
            }
        }
        Value::Array(items) => {
            let mut pending: Vec<Value> = Vec::new();
            for item in items {
                handle_input_item(item, messages, &mut pending, system);
            }
            flush_pending(messages, &mut pending);
        }
        Value::Object(_) => {
            let mut pending: Vec<Value> = Vec::new();
            handle_input_item(input, messages, &mut pending, system);
            flush_pending(messages, &mut pending);
        }
        _ => {}
    }
}

fn flush_pending(messages: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    let content = std::mem::take(pending);
    messages.push(json!({ "role": "user", "content": Value::Array(content) }));
}

/// 追加一个 tool_use 到 assistant 消息；若上一条已是“纯 tool_use”assistant 则合并，
/// 以保持并行工具调用在同一 turn（符合 Kiro 的 tool_use/tool_result 配对要求）。
fn push_assistant_tool_use(messages: &mut Vec<Value>, tool_use: Value) {
    if let Some(last) = messages.last_mut() {
        let is_assistant = last.get("role").and_then(|v| v.as_str()) == Some("assistant");
        let all_tool_use = last
            .get("content")
            .and_then(|c| c.as_array())
            .map(|arr| {
                !arr.is_empty()
                    && arr
                        .iter()
                        .all(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            })
            .unwrap_or(false);
        if is_assistant && all_tool_use {
            if let Some(arr) = last.get_mut("content").and_then(|c| c.as_array_mut()) {
                arr.push(tool_use);
                return;
            }
        }
    }
    messages.push(json!({ "role": "assistant", "content": [tool_use] }));
}

fn handle_input_item(
    item: &Value,
    messages: &mut Vec<Value>,
    pending: &mut Vec<Value>,
    system: &mut Vec<Value>,
) {
    let typ = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");

    match typ {
        "function_call_output" | "tool_result" => {
            flush_pending(messages, pending);
            let call_id = as_str_owned(item.get("call_id"))
                .or_else(|| as_str_owned(item.get("tool_call_id")))
                .unwrap_or_default();
            let mut out = stringify(item.get("output"));
            if out.is_empty() {
                out = stringify(item.get("content"));
            }
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": out,
                }]
            }));
        }
        "function_call" => {
            flush_pending(messages, pending);
            let call_id = as_str_owned(item.get("call_id"))
                .or_else(|| as_str_owned(item.get("id")))
                .unwrap_or_default();
            let name = as_str_owned(item.get("name")).unwrap_or_default();
            let args = stringify(item.get("arguments"));
            let input_val: Value = serde_json::from_str(&args).unwrap_or_else(|_| json!({}));
            push_assistant_tool_use(
                messages,
                json!({
                    "type": "tool_use",
                    "id": call_id,
                    "name": name,
                    "input": input_val,
                }),
            );
        }
        "custom_tool_call" => {
            // Codex custom 工具调用（历史回放）：input 是原始文本，包进 {input: ...}
            flush_pending(messages, pending);
            let call_id = as_str_owned(item.get("call_id"))
                .or_else(|| as_str_owned(item.get("id")))
                .unwrap_or_default();
            let name = as_str_owned(item.get("name")).unwrap_or_default();
            let raw_input = stringify(item.get("input"));
            push_assistant_tool_use(
                messages,
                json!({
                    "type": "tool_use",
                    "id": call_id,
                    "name": name,
                    "input": { "input": raw_input },
                }),
            );
        }
        "custom_tool_call_output" => {
            flush_pending(messages, pending);
            let call_id = as_str_owned(item.get("call_id"))
                .or_else(|| as_str_owned(item.get("tool_call_id")))
                .unwrap_or_default();
            let mut out = stringify(item.get("output"));
            if out.is_empty() {
                out = stringify(item.get("content"));
            }
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": out,
                }]
            }));
        }
        // 工具定义注入条目：在 collect_and_convert_tools 中单独提取，这里跳过
        "additional_tools" => {}
        // 推理条目（含 encrypted_content）：无法回放，跳过
        "reasoning" => {}
        "input_text" | "text" => {
            if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    pending.push(json!({ "type": "text", "text": t }));
                }
            }
        }
        "input_image" | "image" | "image_url" => {
            if let Some(img) = responses_image_to_anthropic(item) {
                pending.push(img);
            }
        }
        "output_text" => {
            flush_pending(messages, pending);
            if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    messages.push(json!({ "role": "assistant", "content": t }));
                }
            }
        }
        "message" => {
            flush_pending(messages, pending);
            build_message_from_item(item, role, messages, system);
        }
        _ => {
            if !role.is_empty() {
                flush_pending(messages, pending);
                build_message_from_item(item, role, messages, system);
            }
        }
    }
}

/// Responses input image item → Anthropic image block
fn responses_image_to_anthropic(item: &Value) -> Option<Value> {
    let url = match item.get("image_url") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(obj @ Value::Object(_)) => as_str_owned(obj.get("url")),
        _ => as_str_owned(item.get("url")),
    }?;
    Some(image_url_to_anthropic(&url))
}

/// message 类型 input item → Anthropic 消息（system/developer 进 system 列表）
fn build_message_from_item(
    item: &Value,
    role: &str,
    messages: &mut Vec<Value>,
    system: &mut Vec<Value>,
) {
    let role = if role.is_empty() { "user" } else { role };

    if role == "system" || role == "developer" {
        let text = content_to_plain(item.get("content"));
        if !text.is_empty() {
            system.push(json!({ "text": text }));
        }
        return;
    }

    let arole = if role == "assistant" {
        "assistant"
    } else {
        "user"
    };

    match item.get("content") {
        Some(Value::String(s)) => {
            messages.push(json!({ "role": arole, "content": s }));
        }
        Some(Value::Array(parts)) => {
            let mut blocks: Vec<Value> = Vec::new();
            let mut any_non_text = false;
            for part in parts {
                match part.get("type").and_then(|v| v.as_str()) {
                    Some("input_text") | Some("text") | Some("output_text") => {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            blocks.push(json!({ "type": "text", "text": t }));
                        }
                    }
                    Some("input_image") | Some("image") | Some("image_url") => {
                        if let Some(img) = responses_image_to_anthropic(part) {
                            any_non_text = true;
                            blocks.push(img);
                        }
                    }
                    _ => {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                blocks.push(json!({ "type": "text", "text": t }));
                            }
                        }
                    }
                }
            }
            if blocks.is_empty() {
                return;
            }
            if !any_non_text {
                // 纯文本合并为字符串
                let joined: String = blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                    .collect();
                messages.push(json!({ "role": arole, "content": joined }));
            } else {
                messages.push(json!({ "role": arole, "content": Value::Array(blocks) }));
            }
        }
        _ => {
            if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    messages.push(json!({ "role": arole, "content": t }));
                }
            }
        }
    }
}

/// 收集并转换 tools → (Anthropic tools, custom 工具名集合)
///
/// tools 来源有两处：
/// 1. 顶层 `req.tools`（标准 Responses/Chat 形状）
/// 2. `input` 数组里 `type:"additional_tools"` 的条目（Codex 通过它注入 exec/wait 等工具）
///
/// 支持的工具类型：
/// - `function`：扁平 `{name,description,parameters}` 或嵌套 `{function:{...}}` → Anthropic 标准工具
/// - `custom`（Codex `exec` 等 freeform/grammar 工具）：合成一个 `{input:string}` 的 schema，
///   并记入 custom 集合，供响应侧改用 `custom_tool_call` 输出
/// 其他无 name 的内置类型（local_shell/web_search 等）跳过。
fn collect_and_convert_tools(req: &ResponsesRequest) -> (Option<Value>, HashSet<String>) {
    let mut raw: Vec<Value> = Vec::new();
    if let Some(ts) = &req.tools {
        raw.extend(ts.iter().cloned());
    }
    if let Value::Array(items) = &req.input {
        for it in items {
            if it.get("type").and_then(|v| v.as_str()) == Some("additional_tools") {
                if let Some(ts) = it.get("tools").and_then(|v| v.as_array()) {
                    raw.extend(ts.iter().cloned());
                }
            }
        }
    }

    let mut out: Vec<Value> = Vec::new();
    let mut custom: HashSet<String> = HashSet::new();
    for t in &raw {
        let ttype = t.get("type").and_then(|v| v.as_str()).unwrap_or("function");
        let (name, desc, params) = if let Some(f) = t.get("function") {
            (f.get("name"), f.get("description"), f.get("parameters"))
        } else {
            (t.get("name"), t.get("description"), t.get("parameters"))
        };
        let name = name.and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let desc = desc.and_then(|v| v.as_str()).unwrap_or("");
        if ttype == "custom" {
            custom.insert(name.to_string());
            out.push(json!({
                "name": name,
                "description": desc,
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "input": { "type": "string", "description": "Raw text input for this tool" }
                    },
                    "required": ["input"]
                }
            }));
        } else {
            let params = params
                .cloned()
                .filter(|p| p.is_object())
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            out.push(json!({
                "name": name,
                "description": desc,
                "input_schema": params,
            }));
        }
    }

    (
        if out.is_empty() {
            None
        } else {
            Some(Value::Array(out))
        },
        custom,
    )
}

/// 从工具调用参数（JSON 字符串）中提取 custom 工具的原始文本输入。
/// 若参数是 `{"input":"..."}` 取其 input；否则原样返回参数字符串。
fn extract_custom_input(args: &str) -> String {
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(args) {
        if let Some(Value::String(s)) = map.get("input") {
            return s.clone();
        }
    }
    args.to_string()
}

/// content（string | parts 数组）→ 纯文本
fn content_to_plain(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                    out.push_str(t);
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// 已存储的 output items → Anthropic 消息（用于历史展开）
fn output_items_to_anthropic(items: &[Value], messages: &mut Vec<Value>) {
    for item in items {
        match item.get("type").and_then(|v| v.as_str()) {
            Some("message") => {
                let text: String = item
                    .get("content")
                    .and_then(|c| c.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                if !text.is_empty() {
                    messages.push(json!({ "role": "assistant", "content": text }));
                }
            }
            Some("function_call") => {
                let call_id = as_str_owned(item.get("call_id"))
                    .or_else(|| as_str_owned(item.get("id")))
                    .unwrap_or_default();
                let name = as_str_owned(item.get("name")).unwrap_or_default();
                let args = stringify(item.get("arguments"));
                let input_val: Value = serde_json::from_str(&args).unwrap_or_else(|_| json!({}));
                messages.push(json!({
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": input_val,
                    }]
                }));
            }
            _ => {}
        }
    }
}

// ============ 响应：Anthropic → Responses output items ============

/// Anthropic content blocks → Responses output items（1 个 message + N 个工具调用）
/// custom_tools 中的工具名会输出为 `custom_tool_call`（input 为原始文本），其余为 `function_call`。
fn anthropic_content_to_output(blocks: &[Value], custom_tools: &HashSet<String>) -> Vec<Value> {
    let mut output: Vec<Value> = Vec::new();
    let mut text = String::new();
    let mut tool_items: Vec<Value> = Vec::new();

    for block in blocks {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                let id = as_str_owned(block.get("id")).unwrap_or_default();
                let name = as_str_owned(block.get("name")).unwrap_or_default();
                let arguments = block
                    .get("input")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                if custom_tools.contains(&name) {
                    tool_items.push(json!({
                        "id": gen_id("ctc"),
                        "type": "custom_tool_call",
                        "status": "completed",
                        "call_id": id,
                        "name": name,
                        "input": extract_custom_input(&arguments),
                    }));
                } else {
                    tool_items.push(json!({
                        "id": gen_id("fc"),
                        "type": "function_call",
                        "status": "completed",
                        "call_id": id,
                        "name": name,
                        "arguments": arguments,
                    }));
                }
            }
            _ => {}
        }
    }

    if !text.trim().is_empty() {
        output.push(json!({
            "id": gen_id("msg"),
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "text": text }],
        }));
    }
    output.extend(tool_items);

    if output.is_empty() {
        output.push(json!({
            "id": gen_id("msg"),
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "text": "" }],
        }));
    }
    output
}

fn usage_from_anthropic(anthropic: &Value) -> (i32, i32) {
    let input = anthropic
        .pointer("/usage/input_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let output = anthropic
        .pointer("/usage/output_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    (input, output)
}

fn build_responses_object(
    id: &str,
    model: &str,
    created: i64,
    output: &[Value],
    input_tokens: i32,
    output_tokens: i32,
    prev_id: &Option<String>,
    metadata: &Option<HashMap<String, String>>,
) -> Value {
    let mut obj = json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "status": "completed",
        "model": model,
        "output": output,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
        },
    });
    if let Some(p) = prev_id {
        if !p.is_empty() {
            obj["previous_response_id"] = json!(p);
        }
    }
    if let Some(m) = metadata {
        obj["metadata"] = json!(m);
    }
    obj
}

// ============ 请求体构建 ============

/// 构建内部 Anthropic body。返回 (anthropic_body_value, custom_tool_names)。
/// Err(Response) 表示应立即返回的错误。
fn build_anthropic_body(req: &ResponsesRequest) -> Result<(Value, HashSet<String>), Response> {
    let mut system: Vec<Value> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    if let Some(prev) = req.previous_response_id.as_deref() {
        if !prev.is_empty() && !expand_history(prev, &mut messages, &mut system) {
            return Err(openai_error(
                StatusCode::NOT_FOUND,
                format!("previous_response_id not found: {}", prev),
                "invalid_request_error",
            ));
        }
    }

    if let Some(instr) = &req.instructions {
        if !instr.trim().is_empty() {
            system.push(json!({ "text": instr }));
        }
    }

    parse_input_into_messages(&req.input, &mut messages, &mut system);

    let has_user = messages
        .iter()
        .any(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"));
    if !has_user {
        return Err(openai_error(
            StatusCode::BAD_REQUEST,
            "input must contain at least one user message",
            "invalid_request_error",
        ));
    }

    let max_tokens = req.max_output_tokens.filter(|v| *v > 0).unwrap_or(8192);

    let mut body = json!({
        "model": req.model,
        "max_tokens": max_tokens,
        "stream": req.stream,
        "messages": messages,
    });
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    let (tools, custom_tools) = collect_and_convert_tools(req);
    if let Some(tools) = tools {
        body["tools"] = tools;
    }
    if let Some(tc) = convert_tool_choice(req.tool_choice.clone()) {
        body["tool_choice"] = tc;
    }

    Ok((body, custom_tools))
}

// ============ 主 Handler ============

/// POST /v1/responses
pub async fn post_responses(State(state): State<AppState>, body: Bytes) -> Response {
    let req: ResponsesRequest = match serde_json::from_slice(&body) {
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
    let resp_id = gen_id("resp");
    let created = now_ts();
    let store_flag = req.store.unwrap_or(true);
    let stored_input = req.input.clone();
    let stored_instr = req.instructions.clone();
    let prev_id = req.previous_response_id.clone();
    let metadata = req.metadata.clone();

    let tool_types: Vec<String> = req
        .tools
        .as_ref()
        .map(|ts| {
            ts.iter()
                .map(|t| {
                    t.get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();
    let tool_names: Vec<String> = req
        .tools
        .as_ref()
        .map(|ts| {
            ts.iter()
                .map(|t| {
                    t.get("name")
                        .or_else(|| t.get("function").and_then(|f| f.get("name")))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();
    tracing::info!(
        model = %req.model,
        stream = %is_stream,
        has_prev = %prev_id.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
        tools_count = req.tools.as_ref().map(|t| t.len()).unwrap_or(0),
        tool_types = ?tool_types,
        tool_names = ?tool_names,
        tool_choice = ?req.tool_choice,
        "Received POST /v1/responses request"
    );

    // 调试：设置 KIRO_DEBUG_DUMP=1 时把原始请求体落盘，便于抓取客户端真实格式
    if std::env::var("KIRO_DEBUG_DUMP").is_ok() {
        let _ = std::fs::write("/tmp/kiro_last_responses_req.json", &body);
    }

    let (anthropic_value, custom_tools) = match build_anthropic_body(&req) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    if std::env::var("KIRO_DEBUG_DUMP").is_ok() {
        if let Ok(s) = serde_json::to_vec_pretty(&anthropic_value) {
            let _ = std::fs::write("/tmp/kiro_last_anthropic_body.json", s);
        }
    }

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

    // 复用 Anthropic 管线
    let response = post_messages(State(state), anthropic_body).await;
    let status = response.status();

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

    let ctx = StoreCtx {
        resp_id: resp_id.clone(),
        model: model_echo.clone(),
        created,
        prev_id,
        metadata,
        store_flag,
        stored_input,
        stored_instr,
        custom_tools,
    };

    if is_stream {
        stream_responses(response, ctx)
    } else {
        non_stream_responses(response, ctx).await
    }
}

/// 传递给非流式/流式翻译的上下文（含 store 所需信息）
struct StoreCtx {
    resp_id: String,
    model: String,
    created: i64,
    prev_id: Option<String>,
    metadata: Option<HashMap<String, String>>,
    store_flag: bool,
    stored_input: Value,
    stored_instr: Option<String>,
    /// custom 类型工具名集合（响应侧对这些工具改用 custom_tool_call 输出）
    custom_tools: HashSet<String>,
}

/// 非流式：Anthropic JSON → Responses 对象
async fn non_stream_responses(response: Response, ctx: StoreCtx) -> Response {
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

    let empty: Vec<Value> = Vec::new();
    let blocks = anthropic
        .get("content")
        .and_then(|c| c.as_array())
        .unwrap_or(&empty);
    let output = anthropic_content_to_output(blocks, &ctx.custom_tools);
    let (input_tokens, output_tokens) = usage_from_anthropic(&anthropic);

    let obj = build_responses_object(
        &ctx.resp_id,
        &ctx.model,
        ctx.created,
        &output,
        input_tokens,
        output_tokens,
        &ctx.prev_id,
        &ctx.metadata,
    );

    if ctx.store_flag {
        save_response(
            &ctx.resp_id,
            ctx.prev_id.clone(),
            ctx.stored_instr.clone(),
            ctx.stored_input.clone(),
            output.clone(),
        );
    }

    Json(obj).into_response()
}

/// 流式：Anthropic SSE → Responses SSE 事件序列
fn stream_responses(response: Response, ctx: StoreCtx) -> Response {
    use futures::StreamExt;

    // 起始 created + in_progress 事件（output/usage 为空）
    let skeleton = |status: &str| {
        let mut obj = json!({
            "id": ctx.resp_id,
            "object": "response",
            "created_at": ctx.created,
            "status": status,
            "model": ctx.model,
            "output": [],
            "usage": Value::Null,
        });
        if let Some(p) = &ctx.prev_id {
            if !p.is_empty() {
                obj["previous_response_id"] = json!(p);
            }
        }
        obj
    };
    let head_bytes = {
        let created_frame = sse_frame(
            "response.created",
            &json!({ "type": "response.created", "response": skeleton("in_progress") }),
        );
        let in_progress_frame = sse_frame(
            "response.in_progress",
            &json!({ "type": "response.in_progress", "response": skeleton("in_progress") }),
        );
        Bytes::from(format!("{}{}", created_frame, in_progress_frame))
    };

    let translator = ResponsesStreamTranslator::new(ctx);
    let data_stream = response.into_body().into_data_stream();

    let head = futures::stream::once(async move { Ok::<Bytes, std::convert::Infallible>(head_bytes) });

    // 状态：(上游流, 缓冲区, 翻译器, 上游是否结束, DONE 是否已发)
    let init = (data_stream, Vec::<u8>::new(), translator, false, false);
    let tail = futures::stream::unfold(init, |state| async move {
        let (mut data_stream, mut buffer, mut translator, upstream_ended, done_sent) = state;

        if done_sent {
            return None;
        }

        if upstream_ended {
            let mut out = Vec::new();
            if !translator.is_finished() {
                out.extend_from_slice(translator.finalize().as_bytes());
            }
            out.extend_from_slice(b"data: [DONE]\n\n");
            return Some((
                Ok::<Bytes, std::convert::Infallible>(Bytes::from(out)),
                (data_stream, buffer, translator, true, true),
            ));
        }

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
                let out = drain_sse_blocks(&mut buffer, &mut translator);
                Some((
                    Ok(Bytes::from(out)),
                    (data_stream, buffer, translator, true, false),
                ))
            }
        }
    });

    let body_stream = head.chain(tail);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .unwrap()
}

/// 从缓冲区提取完整 SSE 块（\n\n 分隔），翻译为 Responses SSE 字节
fn drain_sse_blocks(buffer: &mut Vec<u8>, translator: &mut ResponsesStreamTranslator) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(pos) = buffer.windows(2).position(|w| w == b"\n\n") else {
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

        for frame in translator.handle(&event_name, &data) {
            out.extend_from_slice(frame.as_bytes());
        }
    }
    out
}

fn sse_frame(event: &str, value: &Value) -> String {
    match serde_json::to_string(value) {
        Ok(s) => format!("event: {}\ndata: {}\n\n", event, s),
        Err(_) => String::new(),
    }
}

// ============ 流式翻译状态机 ============

enum BlockKind {
    Message {
        item_id: String,
        output_index: i32,
        content_index: i32,
        text: String,
    },
    Tool {
        item_id: String,
        output_index: i32,
        call_id: String,
        name: String,
        args: String,
        is_custom: bool,
    },
}

struct ResponsesStreamTranslator {
    ctx: StoreCtx,
    next_output_index: i32,
    blocks: HashMap<i64, BlockKind>,
    completed_items: Vec<Value>,
    input_tokens: i32,
    output_tokens: i32,
    finished: bool,
}

impl ResponsesStreamTranslator {
    fn new(ctx: StoreCtx) -> Self {
        Self {
            ctx,
            next_output_index: 0,
            blocks: HashMap::new(),
            completed_items: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            finished: false,
        }
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn handle(&mut self, event: &str, data: &Value) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        match event {
            "message_start" => {
                if let Some(it) = data
                    .pointer("/message/usage/input_tokens")
                    .and_then(|v| v.as_i64())
                {
                    self.input_tokens = it as i32;
                }
            }
            "content_block_start" => {
                let idx = data.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                let block = data.get("content_block");
                let btype = block
                    .and_then(|b| b.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match btype {
                    "text" => {
                        let item_id = gen_id("msg");
                        let oi = self.next_output_index;
                        self.next_output_index += 1;
                        out.push(sse_frame(
                            "response.output_item.added",
                            &json!({
                                "type": "response.output_item.added",
                                "output_index": oi,
                                "item": {
                                    "id": item_id,
                                    "type": "message",
                                    "role": "assistant",
                                    "status": "in_progress",
                                    "content": [],
                                }
                            }),
                        ));
                        out.push(sse_frame(
                            "response.content_part.added",
                            &json!({
                                "type": "response.content_part.added",
                                "item_id": item_id,
                                "output_index": oi,
                                "content_index": 0,
                                "part": { "type": "output_text", "text": "" }
                            }),
                        ));
                        self.blocks.insert(
                            idx,
                            BlockKind::Message {
                                item_id,
                                output_index: oi,
                                content_index: 0,
                                text: String::new(),
                            },
                        );
                    }
                    "tool_use" => {
                        let oi = self.next_output_index;
                        self.next_output_index += 1;
                        let call_id = block
                            .and_then(|b| b.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .and_then(|b| b.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let is_custom = self.ctx.custom_tools.contains(&name);
                        let item_id = gen_id(if is_custom { "ctc" } else { "fc" });
                        if is_custom {
                            out.push(sse_frame(
                                "response.output_item.added",
                                &json!({
                                    "type": "response.output_item.added",
                                    "output_index": oi,
                                    "item": {
                                        "id": item_id,
                                        "type": "custom_tool_call",
                                        "status": "in_progress",
                                        "call_id": call_id,
                                        "name": name,
                                        "input": "",
                                    }
                                }),
                            ));
                        } else {
                            out.push(sse_frame(
                                "response.output_item.added",
                                &json!({
                                    "type": "response.output_item.added",
                                    "output_index": oi,
                                    "item": {
                                        "id": item_id,
                                        "type": "function_call",
                                        "status": "in_progress",
                                        "call_id": call_id,
                                        "name": name,
                                        "arguments": "",
                                    }
                                }),
                            ));
                        }
                        self.blocks.insert(
                            idx,
                            BlockKind::Tool {
                                item_id,
                                output_index: oi,
                                call_id,
                                name,
                                args: String::new(),
                                is_custom,
                            },
                        );
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let idx = data.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                let delta = data.get("delta");
                let dtype = delta
                    .and_then(|d| d.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match dtype {
                    "text_delta" => {
                        if let Some(t) = delta.and_then(|d| d.get("text")).and_then(|v| v.as_str()) {
                            if let Some(BlockKind::Message {
                                item_id,
                                output_index,
                                content_index,
                                text,
                            }) = self.blocks.get_mut(&idx)
                            {
                                text.push_str(t);
                                out.push(sse_frame(
                                    "response.output_text.delta",
                                    &json!({
                                        "type": "response.output_text.delta",
                                        "item_id": item_id.clone(),
                                        "output_index": *output_index,
                                        "content_index": *content_index,
                                        "delta": t,
                                    }),
                                ));
                            }
                        }
                    }
                    "input_json_delta" => {
                        if let Some(pj) = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(|v| v.as_str())
                        {
                            if let Some(BlockKind::Tool {
                                item_id,
                                output_index,
                                args,
                                is_custom,
                                ..
                            }) = self.blocks.get_mut(&idx)
                            {
                                args.push_str(pj);
                                // custom 工具需从完整 JSON 里抽出 input 文本，无法增量转发，
                                // 仅缓冲，待 content_block_stop 时一次性发出。
                                if !*is_custom {
                                    out.push(sse_frame(
                                        "response.function_call_arguments.delta",
                                        &json!({
                                            "type": "response.function_call_arguments.delta",
                                            "item_id": item_id.clone(),
                                            "output_index": *output_index,
                                            "delta": pj,
                                        }),
                                    ));
                                }
                            }
                        }
                    }
                    _ => {} // thinking_delta 忽略
                }
            }
            "content_block_stop" => {
                let idx = data.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                if let Some(kind) = self.blocks.remove(&idx) {
                    match kind {
                        BlockKind::Message {
                            item_id,
                            output_index,
                            content_index,
                            text,
                        } => {
                            out.push(sse_frame(
                                "response.content_part.done",
                                &json!({
                                    "type": "response.content_part.done",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "content_index": content_index,
                                    "part": { "type": "output_text", "text": text }
                                }),
                            ));
                            let item = json!({
                                "id": item_id,
                                "type": "message",
                                "role": "assistant",
                                "status": "completed",
                                "content": [{ "type": "output_text", "text": text }],
                            });
                            out.push(sse_frame(
                                "response.output_item.done",
                                &json!({
                                    "type": "response.output_item.done",
                                    "output_index": output_index,
                                    "item": item,
                                }),
                            ));
                            self.completed_items.push(item);
                        }
                        BlockKind::Tool {
                            item_id,
                            output_index,
                            call_id,
                            name,
                            args,
                            is_custom,
                        } => {
                            if is_custom {
                                let input = extract_custom_input(&args);
                                out.push(sse_frame(
                                    "response.custom_tool_call_input.delta",
                                    &json!({
                                        "type": "response.custom_tool_call_input.delta",
                                        "item_id": item_id,
                                        "output_index": output_index,
                                        "delta": input,
                                    }),
                                ));
                                out.push(sse_frame(
                                    "response.custom_tool_call_input.done",
                                    &json!({
                                        "type": "response.custom_tool_call_input.done",
                                        "item_id": item_id,
                                        "output_index": output_index,
                                        "input": input,
                                    }),
                                ));
                                let item = json!({
                                    "id": item_id,
                                    "type": "custom_tool_call",
                                    "status": "completed",
                                    "call_id": call_id,
                                    "name": name,
                                    "input": input,
                                });
                                out.push(sse_frame(
                                    "response.output_item.done",
                                    &json!({
                                        "type": "response.output_item.done",
                                        "output_index": output_index,
                                        "item": item,
                                    }),
                                ));
                                self.completed_items.push(item);
                            } else {
                                let item = json!({
                                    "id": item_id,
                                    "type": "function_call",
                                    "status": "completed",
                                    "call_id": call_id,
                                    "name": name,
                                    "arguments": args,
                                });
                                out.push(sse_frame(
                                    "response.function_call_arguments.done",
                                    &json!({
                                        "type": "response.function_call_arguments.done",
                                        "item_id": item_id,
                                        "output_index": output_index,
                                        "arguments": args,
                                    }),
                                ));
                                out.push(sse_frame(
                                    "response.output_item.done",
                                    &json!({
                                        "type": "response.output_item.done",
                                        "output_index": output_index,
                                        "item": item,
                                    }),
                                ));
                                self.completed_items.push(item);
                            }
                        }
                    }
                }
            }
            "message_delta" => {
                if let Some(ot) = data
                    .pointer("/usage/output_tokens")
                    .and_then(|v| v.as_i64())
                {
                    self.output_tokens = ot as i32;
                }
            }
            "message_stop" => {
                out.push(self.finalize());
            }
            _ => {}
        }
        out
    }

    /// 生成 response.completed 事件（幂等），并按需写入 store
    fn finalize(&mut self) -> String {
        self.finished = true;

        let output = if self.completed_items.is_empty() {
            vec![json!({
                "id": gen_id("msg"),
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": "" }],
            })]
        } else {
            self.completed_items.clone()
        };

        let obj = build_responses_object(
            &self.ctx.resp_id,
            &self.ctx.model,
            self.ctx.created,
            &output,
            self.input_tokens,
            self.output_tokens,
            &self.ctx.prev_id,
            &self.ctx.metadata,
        );

        if self.ctx.store_flag {
            save_response(
                &self.ctx.resp_id,
                self.ctx.prev_id.clone(),
                self.ctx.stored_instr.clone(),
                self.ctx.stored_input.clone(),
                output.clone(),
            );
        }

        sse_frame(
            "response.completed",
            &json!({ "type": "response.completed", "response": obj }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 从 SSE 帧文本里解析出 (event, data) 序列
    fn parse_frames(frames: &[String]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for f in frames {
            let mut ev = String::new();
            let mut data = String::new();
            for line in f.lines() {
                if let Some(r) = line.strip_prefix("event:") {
                    ev = r.trim().to_string();
                } else if let Some(r) = line.strip_prefix("data:") {
                    data = r.trim().to_string();
                }
            }
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                out.push((ev, v));
            }
        }
        out
    }

    fn ctx() -> StoreCtx {
        StoreCtx {
            resp_id: "resp_test".into(),
            model: "claude-sonnet-5".into(),
            created: 1700000000,
            prev_id: None,
            metadata: None,
            store_flag: false,
            stored_input: json!("hi"),
            stored_instr: None,
            custom_tools: HashSet::new(),
        }
    }

    #[test]
    fn test_parse_string_input() {
        let mut msgs = Vec::new();
        let mut sys = Vec::new();
        parse_input_into_messages(&json!("你好"), &mut msgs, &mut sys);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "你好");
    }

    #[test]
    fn test_parse_array_input_with_parts() {
        let input = json!([
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}
        ]);
        let mut msgs = Vec::new();
        let mut sys = Vec::new();
        parse_input_into_messages(&input, &mut msgs, &mut sys);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        // 纯文本合并为字符串
        assert_eq!(msgs[0]["content"], "hello");
    }

    #[test]
    fn test_parse_function_call_and_output() {
        let input = json!([
            {"type":"message","role":"user","content":"用工具"},
            {"type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"SH\"}"},
            {"type":"function_call_output","call_id":"call_1","output":"sunny"}
        ]);
        let mut msgs = Vec::new();
        let mut sys = Vec::new();
        parse_input_into_messages(&input, &mut msgs, &mut sys);
        assert_eq!(msgs.len(), 3);
        // user
        assert_eq!(msgs[0]["role"], "user");
        // assistant tool_use
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["id"], "call_1");
        assert_eq!(msgs[1]["content"][0]["name"], "get_weather");
        assert_eq!(msgs[1]["content"][0]["input"]["city"], "SH");
        // tool_result 作为 user turn
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(msgs[2]["content"][0]["content"], "sunny");
    }

    #[test]
    fn test_parallel_function_calls_merge() {
        let input = json!([
            {"type":"message","role":"user","content":"并行"},
            {"type":"function_call","call_id":"c1","name":"a","arguments":"{}"},
            {"type":"function_call","call_id":"c2","name":"b","arguments":"{}"}
        ]);
        let mut msgs = Vec::new();
        let mut sys = Vec::new();
        parse_input_into_messages(&input, &mut msgs, &mut sys);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "assistant");
        // 两个 tool_use 合并进同一 assistant 消息
        assert_eq!(msgs[1]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_build_body_with_instructions_and_tools() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-5",
            "instructions":"你是猫娘",
            "input":"喵",
            "max_output_tokens": 1024,
            "tools":[{"type":"function","function":{"name":"f","description":"d","parameters":{"type":"object","properties":{}}}}],
            "tool_choice":"auto"
        })).unwrap();
        let (body, _custom) = build_anthropic_body(&req).unwrap();
        assert_eq!(body["model"], "claude-sonnet-5");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["system"][0]["text"], "你是猫娘");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["tools"][0]["name"], "f");
        assert_eq!(body["tool_choice"]["type"], "auto");
    }

    #[test]
    fn test_anthropic_to_output_text_and_tool() {
        let blocks = vec![
            json!({"type":"text","text":"答案"}),
            json!({"type":"tool_use","id":"tu1","name":"calc","input":{"x":1}}),
        ];
        let out = anthropic_content_to_output(&blocks, &HashSet::new());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["type"], "message");
        assert_eq!(out[0]["content"][0]["type"], "output_text");
        assert_eq!(out[0]["content"][0]["text"], "答案");
        assert_eq!(out[1]["type"], "function_call");
        assert_eq!(out[1]["call_id"], "tu1");
        assert_eq!(out[1]["name"], "calc");
        assert_eq!(out[1]["arguments"], "{\"x\":1}");
    }

    #[test]
    fn test_anthropic_to_output_empty_fallback() {
        let out = anthropic_content_to_output(&[], &HashSet::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "message");
        assert_eq!(out[0]["content"][0]["text"], "");
    }

    #[test]
    fn test_stream_translator_text() {
        let mut t = ResponsesStreamTranslator::new(ctx());
        let mut frames = Vec::new();
        frames.extend(t.handle("message_start", &json!({"message":{"usage":{"input_tokens":10}}})));
        frames.extend(t.handle("content_block_start", &json!({"index":0,"content_block":{"type":"text"}})));
        frames.extend(t.handle("content_block_delta", &json!({"index":0,"delta":{"type":"text_delta","text":"Hello"}})));
        frames.extend(t.handle("content_block_delta", &json!({"index":0,"delta":{"type":"text_delta","text":" world"}})));
        frames.extend(t.handle("content_block_stop", &json!({"index":0})));
        frames.extend(t.handle("message_delta", &json!({"usage":{"output_tokens":5}})));
        frames.extend(t.handle("message_stop", &json!({})));

        let evts = parse_frames(&frames);
        let names: Vec<&str> = evts.iter().map(|(e, _)| e.as_str()).collect();
        assert!(names.contains(&"response.output_item.added"));
        assert!(names.contains(&"response.content_part.added"));
        assert!(names.contains(&"response.output_text.delta"));
        assert!(names.contains(&"response.content_part.done"));
        assert!(names.contains(&"response.output_item.done"));
        assert!(names.contains(&"response.completed"));

        // 拼接的文本
        let deltas: String = evts
            .iter()
            .filter(|(e, _)| e == "response.output_text.delta")
            .map(|(_, d)| d["delta"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(deltas, "Hello world");

        // completed 里 usage + 完整文本
        let (_, completed) = evts.iter().find(|(e, _)| e == "response.completed").unwrap();
        let resp = &completed["response"];
        assert_eq!(resp["status"], "completed");
        assert_eq!(resp["usage"]["input_tokens"], 10);
        assert_eq!(resp["usage"]["output_tokens"], 5);
        assert_eq!(resp["output"][0]["content"][0]["text"], "Hello world");
        assert!(t.is_finished());
    }

    #[test]
    fn test_stream_translator_tool_call() {
        let mut t = ResponsesStreamTranslator::new(ctx());
        let mut frames = Vec::new();
        frames.extend(t.handle("message_start", &json!({"message":{"usage":{"input_tokens":3}}})));
        frames.extend(t.handle("content_block_start", &json!({"index":0,"content_block":{"type":"tool_use","id":"call_x","name":"search"}})));
        frames.extend(t.handle("content_block_delta", &json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}})));
        frames.extend(t.handle("content_block_delta", &json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"\"cat\"}"}})));
        frames.extend(t.handle("content_block_stop", &json!({"index":0})));
        frames.extend(t.handle("message_stop", &json!({})));

        let evts = parse_frames(&frames);
        // 工具调用参数增量拼接
        let args: String = evts
            .iter()
            .filter(|(e, _)| e == "response.function_call_arguments.delta")
            .map(|(_, d)| d["delta"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(args, "{\"q\":\"cat\"}");

        // output_item.done 是 function_call
        let done = evts.iter().find(|(e, d)| e == "response.output_item.done" && d["item"]["type"] == "function_call");
        assert!(done.is_some());
        let (_, d) = done.unwrap();
        assert_eq!(d["item"]["call_id"], "call_x");
        assert_eq!(d["item"]["name"], "search");
        assert_eq!(d["item"]["arguments"], "{\"q\":\"cat\"}");

        // completed 输出里含该 function_call
        let (_, completed) = evts.iter().find(|(e, _)| e == "response.completed").unwrap();
        assert_eq!(completed["response"]["output"][0]["type"], "function_call");
    }

    #[test]
    fn test_previous_response_id_history() {
        // 存一个历史响应
        save_response(
            "resp_prev",
            None,
            Some("系统提示".into()),
            json!("第一轮问题"),
            vec![json!({
                "id":"msg_1","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"第一轮回答"}]
            })],
        );

        let req: ResponsesRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-5",
            "previous_response_id":"resp_prev",
            "input":"第二轮问题"
        })).unwrap();
        let (body, _custom) = build_anthropic_body(&req).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        // 历史: user(第一轮问题) + assistant(第一轮回答) + 本轮 user(第二轮问题)
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["content"], "第一轮问题");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], "第一轮回答");
        assert_eq!(msgs[2]["content"], "第二轮问题");
        // 祖先 instructions 进入 system
        assert_eq!(body["system"][0]["text"], "系统提示");
    }

    #[test]
    fn test_codex_additional_tools_extraction() {
        // 模拟 Codex 请求：工具藏在 input 的 additional_tools 条目里，含 custom(exec) 与 function(wait)
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "input": [
                {"type":"additional_tools","role":"developer","tools":[
                    {"type":"custom","name":"exec","description":"run js","format":{"type":"grammar"}},
                    {"type":"function","name":"wait","description":"wait","parameters":{"type":"object","properties":{"ms":{"type":"number"}}}}
                ]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"列出目录"}]}
            ],
            "stream": true
        })).unwrap();
        let (body, custom) = build_anthropic_body(&req).unwrap();
        // 两个工具都应转换出来
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"exec"));
        assert!(names.contains(&"wait"));
        // exec 是 custom
        assert!(custom.contains("exec"));
        assert!(!custom.contains("wait"));
        // exec 应有合成的 {input:string} schema
        let exec = tools.iter().find(|t| t["name"] == "exec").unwrap();
        assert_eq!(exec["input_schema"]["properties"]["input"]["type"], "string");
        // additional_tools 不应变成消息；只有 user 消息
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn test_custom_tool_call_output_nonstream() {
        // custom 工具的非流式输出应为 custom_tool_call，input 取自 {"input": ...}
        let mut custom = HashSet::new();
        custom.insert("exec".to_string());
        let blocks = vec![json!({
            "type":"tool_use","id":"call_1","name":"exec",
            "input":{"input":"await tools.shell('ls')"}
        })];
        let out = anthropic_content_to_output(&blocks, &custom);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "custom_tool_call");
        assert_eq!(out[0]["call_id"], "call_1");
        assert_eq!(out[0]["name"], "exec");
        assert_eq!(out[0]["input"], "await tools.shell('ls')");
    }

    #[test]
    fn test_stream_custom_tool_call() {
        let mut c = ctx();
        c.custom_tools.insert("exec".to_string());
        let mut t = ResponsesStreamTranslator::new(c);
        let mut frames = Vec::new();
        frames.extend(t.handle("content_block_start", &json!({"index":0,"content_block":{"type":"tool_use","id":"call_e","name":"exec"}})));
        frames.extend(t.handle("content_block_delta", &json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"ls "}})));
        frames.extend(t.handle("content_block_delta", &json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"-la\"}"}})));
        frames.extend(t.handle("content_block_stop", &json!({"index":0})));
        frames.extend(t.handle("message_stop", &json!({})));

        let evts = parse_frames(&frames);
        let names: Vec<&str> = evts.iter().map(|(e, _)| e.as_str()).collect();
        // custom 工具用 custom_tool_call_input 事件，而非 function_call_arguments
        assert!(names.contains(&"response.custom_tool_call_input.done"));
        assert!(!names.contains(&"response.function_call_arguments.delta"));
        // output_item.added 类型应为 custom_tool_call
        let added = evts.iter().find(|(e, _)| e == "response.output_item.added").unwrap();
        assert_eq!(added.1["item"]["type"], "custom_tool_call");
        // input.done 里是解出来的原始文本
        let done = evts.iter().find(|(e, _)| e == "response.custom_tool_call_input.done").unwrap();
        assert_eq!(done.1["input"], "ls -la");
    }

    #[test]
    fn test_previous_response_id_not_found() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-5",
            "previous_response_id":"resp_does_not_exist_xyz",
            "input":"x"
        })).unwrap();
        let r = build_anthropic_body(&req);
        assert!(r.is_err());
    }

    #[test]
    fn test_image_input() {
        let input = json!([
            {"type":"message","role":"user","content":[
                {"type":"input_text","text":"看图"},
                {"type":"input_image","image_url":"data:image/png;base64,AAAA"}
            ]}
        ]);
        let mut msgs = Vec::new();
        let mut sys = Vec::new();
        parse_input_into_messages(&input, &mut msgs, &mut sys);
        assert_eq!(msgs.len(), 1);
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "AAAA");
    }
}
