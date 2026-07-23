//! OpenAI 兼容层
//!
//! 提供 `/v1/chat/completions` OpenAI 格式端点，内部转换为 Anthropic 请求并复用
//! 现有 Kiro 管线，再把输出翻译回 OpenAI 格式。

pub mod convert;
pub mod handlers;
pub mod responses;
pub mod types;
