//! OpenAI Chat Completions API 类型定义
//!
//! 仅覆盖代理转换所需的字段，未知字段忽略。

use serde::{Deserialize, Serialize};

// ============ 请求类型 ============

/// OpenAI Chat Completions 请求体
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<OaMessage>,
    #[serde(default)]
    pub stream: bool,
    /// 旧字段
    pub max_tokens: Option<i32>,
    /// 新字段（o 系列 / 新版）
    pub max_completion_tokens: Option<i32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub tools: Option<Vec<OaTool>>,
    pub tool_choice: Option<serde_json::Value>,
    /// 推理力度（reasoning models），可选映射到 thinking
    pub reasoning_effort: Option<String>,
}

impl ChatCompletionRequest {
    /// 有效的最大输出 token 数（回退默认值）
    pub fn effective_max_tokens(&self) -> i32 {
        self.max_completion_tokens
            .or(self.max_tokens)
            .filter(|v| *v > 0)
            .unwrap_or(8192)
    }
}

/// OpenAI 消息
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct OaMessage {
    pub role: String,
    /// string | array(parts) | null
    #[serde(default)]
    pub content: serde_json::Value,
    #[serde(default)]
    pub tool_calls: Option<Vec<OaToolCall>>,
    /// tool 角色消息对应的 tool_call_id
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// OpenAI 工具调用
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct OaToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default = "default_tool_call_type")]
    pub call_type: String,
    pub function: OaFunctionCall,
    /// 流式增量时的索引
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<i32>,
}

fn default_tool_call_type() -> String {
    "function".to_string()
}

/// OpenAI 函数调用
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct OaFunctionCall {
    #[serde(default)]
    pub name: String,
    /// JSON 字符串形式的参数
    #[serde(default)]
    pub arguments: String,
}

/// OpenAI 工具定义
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct OaTool {
    #[serde(rename = "type", default)]
    pub tool_type: String,
    pub function: OaToolFunction,
}

/// OpenAI 工具函数定义
#[derive(Debug, Deserialize)]
pub struct OaToolFunction {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: serde_json::Value,
}

// ============ 响应类型（非流式） ============

/// Chat Completion 响应
#[derive(Debug, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
}

/// 单个 choice（非流式）
#[derive(Debug, Serialize)]
pub struct Choice {
    pub index: i32,
    pub message: RespMessage,
    pub finish_reason: Option<String>,
}

/// 响应消息
#[derive(Debug, Serialize)]
pub struct RespMessage {
    pub role: &'static str,
    pub content: Option<String>,
    /// 推理内容（thinking）；OpenAI 兼容客户端常用 reasoning_content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OaToolCall>>,
}

/// Token 用量
#[derive(Debug, Serialize)]
pub struct Usage {
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
}

// ============ 响应类型（流式） ============

/// Chat Completion 流式 chunk
#[derive(Debug, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
}

/// 流式 choice
#[derive(Debug, Serialize)]
pub struct ChunkChoice {
    pub index: i32,
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

/// 流式增量
#[derive(Debug, Serialize, Default)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// 增量工具调用（原始 JSON，以精确控制 OpenAI 流式分片形状）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<serde_json::Value>,
}
