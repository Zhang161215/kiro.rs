//! OpenAI ↔ 内部 Anthropic 格式转换
//!
//! - 请求：OpenAI ChatCompletionRequest → Anthropic MessagesRequest（复用下游全部管线）
//! - 响应：Anthropic 输出（JSON / SSE 事件）→ OpenAI 格式

use std::collections::HashMap;

use serde_json::{Value, json};

use super::types::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Choice, ChunkChoice, Delta,
    OaFunctionCall, OaToolCall, RespMessage, Usage,
};

// ============ 请求转换 ============

/// OpenAI 请求 → Anthropic MessagesRequest 的 JSON body
///
/// 直接构造 `serde_json::Value`，交给 `post_messages` 反序列化为 `MessagesRequest`，
/// 复用其全部管线（转换/上游/流处理/KV 记录/错误映射）。
pub fn openai_to_anthropic_body(req: ChatCompletionRequest) -> Value {
    let max_tokens = req.effective_max_tokens();
    let stream = req.stream;
    let model = req.model.clone();

    let mut system: Vec<Value> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    let mut pending_tool_results: Vec<Value> = Vec::new();

    for m in req.messages {
        match m.role.as_str() {
            "system" | "developer" => {
                let text = content_to_plain_text(&m.content);
                if !text.is_empty() {
                    system.push(json!({ "text": text }));
                }
            }
            "tool" => {
                // 累积 tool_result，等到下一条非 tool 消息前合并成一个 user turn
                let tool_use_id = m.tool_call_id.clone().unwrap_or_default();
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content_to_plain_text(&m.content),
                }));
            }
            "assistant" => {
                flush_tool_results(&mut messages, &mut pending_tool_results);
                let mut blocks: Vec<Value> = Vec::new();
                let text = content_to_plain_text(&m.content);
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                if let Some(tool_calls) = m.tool_calls {
                    for tc in tool_calls {
                        let input: Value = serde_json::from_str(&tc.function.arguments)
                            .unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.function.name,
                            "input": input,
                        }));
                    }
                }
                let content = if blocks.is_empty() {
                    Value::String(String::new())
                } else {
                    Value::Array(blocks)
                };
                messages.push(json!({ "role": "assistant", "content": content }));
            }
            _ => {
                // user 及未知角色按 user 处理
                flush_tool_results(&mut messages, &mut pending_tool_results);
                messages.push(json!({
                    "role": "user",
                    "content": openai_content_to_anthropic(&m.content),
                }));
            }
        }
    }
    // 收尾：flush 尾部残留的 tool_result
    flush_tool_results(&mut messages, &mut pending_tool_results);

    // 兜底：Anthropic 要求 messages 非空
    if messages.is_empty() {
        messages.push(json!({ "role": "user", "content": "" }));
    }

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": stream,
        "messages": messages,
    });

    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    if let Some(tools) = convert_tools(req.tools) {
        body["tools"] = tools;
    }
    if let Some(tc) = convert_tool_choice(req.tool_choice) {
        body["tool_choice"] = tc;
    }

    body
}

fn flush_tool_results(messages: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if !pending.is_empty() {
        messages.push(json!({
            "role": "user",
            "content": Value::Array(std::mem::take(pending)),
        }));
    }
}

/// 将 OpenAI content（string | parts 数组 | null）提取为纯文本
fn content_to_plain_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    out.push_str(t);
                } else if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                        out.push_str(t);
                    }
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// 将 OpenAI content 转为 Anthropic content（保留图片）
fn openai_content_to_anthropic(content: &Value) -> Value {
    match content {
        Value::String(s) => Value::String(s.clone()),
        Value::Array(parts) => {
            let mut blocks: Vec<Value> = Vec::new();
            for part in parts {
                match part.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            blocks.push(json!({ "type": "text", "text": t }));
                        }
                    }
                    Some("image_url") => {
                        if let Some(url) = part
                            .get("image_url")
                            .and_then(|v| v.get("url"))
                            .and_then(|v| v.as_str())
                        {
                            blocks.push(image_url_to_anthropic(url));
                        }
                    }
                    _ => {}
                }
            }
            if blocks.is_empty() {
                Value::String(String::new())
            } else {
                Value::Array(blocks)
            }
        }
        _ => Value::String(String::new()),
    }
}

/// data:URL（base64）或普通 URL → Anthropic image block
pub(crate) fn image_url_to_anthropic(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        // data:<media_type>;base64,<data>
        if let Some((meta, data)) = rest.split_once(',') {
            let media_type = meta.split(';').next().unwrap_or("image/png").to_string();
            return json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data }
            });
        }
    }
    json!({
        "type": "image",
        "source": { "type": "url", "url": url }
    })
}

pub(crate) fn convert_tools(tools: Option<Vec<super::types::OaTool>>) -> Option<Value> {
    let tools = tools?;
    if tools.is_empty() {
        return None;
    }
    let mut out: Vec<Value> = Vec::new();
    for t in tools {
        let params = if t.function.parameters.is_object() {
            t.function.parameters
        } else {
            json!({ "type": "object", "properties": {} })
        };
        out.push(json!({
            "name": t.function.name,
            "description": t.function.description,
            "input_schema": params,
        }));
    }
    Some(Value::Array(out))
}

/// OpenAI tool_choice → Anthropic tool_choice
pub(crate) fn convert_tool_choice(choice: Option<Value>) -> Option<Value> {
    let choice = choice?;
    match &choice {
        Value::String(s) => match s.as_str() {
            "auto" => Some(json!({ "type": "auto" })),
            "required" | "any" => Some(json!({ "type": "any" })),
            "none" => None,
            _ => None,
        },
        Value::Object(obj) => {
            // { "type":"function", "function": { "name": "..." } }
            if let Some(name) = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
            {
                Some(json!({ "type": "tool", "name": name }))
            } else {
                None
            }
        }
        _ => None,
    }
}

// ============ 响应转换（非流式） ============

/// Anthropic 非流式响应 JSON → OpenAI ChatCompletionResponse
pub fn anthropic_response_to_openai(
    anthropic: &Value,
    model: String,
    id: String,
    created: i64,
) -> ChatCompletionResponse {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<OaToolCall> = Vec::new();

    if let Some(blocks) = anthropic.get("content").and_then(|c| c.as_array()) {
        for block in blocks {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                    }
                }
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
                        reasoning.push_str(t);
                    }
                }
                Some("tool_use") => {
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let tid = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let arguments = block
                        .get("input")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    tool_calls.push(OaToolCall {
                        id: tid,
                        call_type: "function".to_string(),
                        function: OaFunctionCall { name, arguments },
                        index: None,
                    });
                }
                _ => {}
            }
        }
    }

    let stop_reason = anthropic.get("stop_reason").and_then(|v| v.as_str());
    let has_tool = !tool_calls.is_empty();
    let finish_reason = map_finish_reason(stop_reason, has_tool).or(Some("stop".to_string()));

    let input_tokens = anthropic
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let output_tokens = anthropic
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    ChatCompletionResponse {
        id,
        object: "chat.completion",
        created,
        model,
        choices: vec![Choice {
            index: 0,
            message: RespMessage {
                role: "assistant",
                content: if text.is_empty() && has_tool {
                    None
                } else {
                    Some(text)
                },
                reasoning_content: if reasoning.is_empty() {
                    None
                } else {
                    Some(reasoning)
                },
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
            },
            finish_reason,
        }],
        usage: Usage {
            prompt_tokens: input_tokens,
            completion_tokens: output_tokens,
            total_tokens: input_tokens + output_tokens,
        },
    }
}

/// Anthropic stop_reason → OpenAI finish_reason
fn map_finish_reason(stop_reason: Option<&str>, has_tool: bool) -> Option<String> {
    match stop_reason {
        Some("end_turn") | Some("stop_sequence") => Some("stop".to_string()),
        Some("max_tokens") => Some("length".to_string()),
        Some("tool_use") => Some("tool_calls".to_string()),
        Some(other) => Some(other.to_string()),
        None => {
            if has_tool {
                Some("tool_calls".to_string())
            } else {
                None
            }
        }
    }
}

// ============ 响应转换（流式） ============

/// 将 Anthropic SSE 事件流翻译为 OpenAI chunk 的状态机
pub struct StreamTranslator {
    id: String,
    model: String,
    created: i64,
    role_sent: bool,
    finish_reason: Option<String>,
    /// Anthropic block index → OpenAI tool_calls 序号
    tool_index_map: HashMap<i64, i32>,
    next_tool_index: i32,
    finished: bool,
}

impl StreamTranslator {
    pub fn new(id: String, model: String, created: i64) -> Self {
        Self {
            id,
            model,
            created,
            role_sent: false,
            finish_reason: None,
            tool_index_map: HashMap::new(),
            next_tool_index: 0,
            finished: false,
        }
    }

    fn chunk(&self, delta: Delta, finish_reason: Option<String>) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: self.id.clone(),
            object: "chat.completion.chunk",
            created: self.created,
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta,
                finish_reason,
            }],
        }
    }

    /// 处理一个 Anthropic SSE 事件，返回若干 OpenAI chunk
    pub fn handle(&mut self, event: &str, data: &Value) -> Vec<ChatCompletionChunk> {
        let mut chunks = Vec::new();
        match event {
            "message_start" => {
                if !self.role_sent {
                    self.role_sent = true;
                    chunks.push(self.chunk(
                        Delta {
                            role: Some("assistant"),
                            content: Some(String::new()),
                            ..Default::default()
                        },
                        None,
                    ));
                }
            }
            "content_block_start" => {
                let block = data.get("content_block");
                if block.and_then(|b| b.get("type")).and_then(|v| v.as_str()) == Some("tool_use") {
                    let a_idx = data.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                    let oa_idx = self.assign_tool_index(a_idx);
                    let id = block
                        .and_then(|b| b.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let name = block
                        .and_then(|b| b.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    chunks.push(self.chunk(
                        Delta {
                            tool_calls: Some(json!([{
                                "index": oa_idx,
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": "" }
                            }])),
                            ..Default::default()
                        },
                        None,
                    ));
                }
            }
            "content_block_delta" => {
                let delta = data.get("delta");
                match delta.and_then(|d| d.get("type")).and_then(|v| v.as_str()) {
                    Some("text_delta") => {
                        if let Some(t) = delta.and_then(|d| d.get("text")).and_then(|v| v.as_str()) {
                            chunks.push(self.chunk(
                                Delta {
                                    content: Some(t.to_string()),
                                    ..Default::default()
                                },
                                None,
                            ));
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(t) =
                            delta.and_then(|d| d.get("thinking")).and_then(|v| v.as_str())
                        {
                            chunks.push(self.chunk(
                                Delta {
                                    reasoning_content: Some(t.to_string()),
                                    ..Default::default()
                                },
                                None,
                            ));
                        }
                    }
                    Some("input_json_delta") => {
                        let a_idx = data.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                        let oa_idx = self.assign_tool_index(a_idx);
                        if let Some(pj) = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(|v| v.as_str())
                        {
                            chunks.push(self.chunk(
                                Delta {
                                    tool_calls: Some(json!([{
                                        "index": oa_idx,
                                        "function": { "arguments": pj }
                                    }])),
                                    ..Default::default()
                                },
                                None,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = data
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|v| v.as_str())
                {
                    let has_tool = !self.tool_index_map.is_empty();
                    self.finish_reason = map_finish_reason(Some(reason), has_tool);
                }
            }
            "message_stop" => {
                chunks.push(self.finish());
            }
            _ => {}
        }
        chunks
    }

    fn assign_tool_index(&mut self, a_idx: i64) -> i32 {
        if let Some(&idx) = self.tool_index_map.get(&a_idx) {
            idx
        } else {
            let idx = self.next_tool_index;
            self.next_tool_index += 1;
            self.tool_index_map.insert(a_idx, idx);
            idx
        }
    }

    /// 生成最终 chunk（携带 finish_reason）。幂等：只生效一次。
    pub fn finish(&mut self) -> ChatCompletionChunk {
        self.finished = true;
        let finish_reason = self
            .finish_reason
            .clone()
            .or_else(|| Some("stop".to_string()));
        self.chunk(Delta::default(), finish_reason)
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }
}
