//! 记录“工具调用失败 / malformed request”对应的 prompt，便于后续排障

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use chrono::Utc;
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const PROMPT_FAILURE_RECORDS_FILE: &str = "kiro_prompt_failure_records.jsonl";
const MAX_PROMPT_CHARS: usize = 20_000;
const MAX_ERROR_CHARS: usize = 4_000;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptFailureRecord {
    recorded_at: String,
    request_id: String,
    failure_type: String,
    source: String,
    endpoint: String,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_names: Vec<String>,
    error: String,
    prompt: String,
    request_body_size: usize,
    request_body_sha256: String,
}

#[derive(Debug, Default)]
struct PromptDetails {
    conversation_id: Option<String>,
    model: Option<String>,
    prompt: Option<String>,
    tool_names: Vec<String>,
}

fn resolve_cache_dir(dir_hint: Option<PathBuf>) -> PathBuf {
    // 与 kv_cache 保持一致：显式 hint > 全局记录目录（凭据目录）> 进程 cwd
    dir_hint
        .or_else(|| super::kv_cache::records_dir())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn records_file_path(dir_hint: Option<PathBuf>) -> PathBuf {
    resolve_cache_dir(dir_hint).join(PROMPT_FAILURE_RECORDS_FILE)
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated = text.chars().take(max_chars).collect::<String>();
    truncated.push_str("...[truncated]");
    truncated
}

fn classify_failure(error_text: &str) -> Option<&'static str> {
    let normalized = error_text.to_lowercase();

    if normalized.contains("improperly formed request")
        || normalized.contains("malformed request")
        || normalized.contains("misformed request")
        || (normalized.contains("improper") && normalized.contains("formed request"))
    {
        return Some("malformed_request");
    }

    if normalized.contains("tool call failed")
        || normalized.contains("failed to call tool")
        || normalized.contains("tool invocation failed")
        || normalized.contains("tool execution failed")
        || normalized.contains("mcp 请求失败")
        || normalized.contains("mcp request failed")
        || normalized.contains("mcp error")
        || (normalized.contains("tool") && normalized.contains("failed"))
    {
        return Some("tool_call_failed");
    }

    None
}

fn extract_prompt_details(request_body: &str) -> PromptDetails {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(request_body) else {
        return PromptDetails::default();
    };

    let conversation_id = value
        .pointer("/conversationState/conversationId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let model = value
        .pointer("/conversationState/currentMessage/userInputMessage/modelId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let prompt = value
        .pointer("/conversationState/currentMessage/userInputMessage/content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty());

    let mut tool_names = Vec::new();
    if let Some(tools) = value
        .pointer("/conversationState/currentMessage/userInputMessage/userInputMessageContext/tools")
        .and_then(|v| v.as_array())
    {
        for tool in tools {
            if let Some(name) = tool
                .pointer("/toolSpecification/name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                tool_names.push(name.to_string());
            }
        }
    }

    PromptDetails {
        conversation_id,
        model,
        prompt,
        tool_names,
    }
}

fn append_record(path: &PathBuf, record: &PromptFailureRecord) -> anyhow::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(record)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

pub fn maybe_record_failure_prompt(
    cache_dir_hint: Option<PathBuf>,
    endpoint: &str,
    model_hint: &str,
    request_body: &str,
    source: &str,
    error_text: &str,
) -> bool {
    let Some(failure_type) = classify_failure(error_text) else {
        return false;
    };

    let details = extract_prompt_details(request_body);
    let model = details
        .model
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| model_hint.to_string());
    let prompt = details.prompt.unwrap_or_else(|| {
        format!(
            "[无法从请求体提取 current prompt，request_body 前 300 字符]: {}",
            truncate_text(request_body, 300)
        )
    });

    let mut hasher = Sha256::new();
    hasher.update(request_body.as_bytes());
    let request_body_sha256 = format!("{:x}", hasher.finalize());

    let record = PromptFailureRecord {
        recorded_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        request_id: Uuid::new_v4().to_string(),
        failure_type: failure_type.to_string(),
        source: source.to_string(),
        endpoint: endpoint.to_string(),
        model: truncate_text(&model, 200),
        conversation_id: details.conversation_id.map(|s| truncate_text(&s, 200)),
        tool_names: details
            .tool_names
            .into_iter()
            .map(|s| truncate_text(&s, 200))
            .collect(),
        error: truncate_text(error_text, MAX_ERROR_CHARS),
        prompt: truncate_text(&prompt, MAX_PROMPT_CHARS),
        request_body_size: request_body.len(),
        request_body_sha256,
    };

    let path = records_file_path(cache_dir_hint);
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "创建故障 prompt 日志目录失败"
            );
            return true;
        }
    }

    if let Err(e) = append_record(&path, &record) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "写入故障 prompt 日志失败"
        );
        return true;
    }

    tracing::warn!(
        path = %path.display(),
        failure_type = %record.failure_type,
        source = %record.source,
        endpoint = %record.endpoint,
        model = %record.model,
        "已记录故障 prompt"
    );

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_malformed_request() {
        let text = "400 Bad Request: Improperly formed request";
        assert_eq!(classify_failure(text), Some("malformed_request"));
    }

    #[test]
    fn test_classify_tool_failure() {
        let text = "tool call failed: unable to execute";
        assert_eq!(classify_failure(text), Some("tool_call_failed"));
    }

    #[test]
    fn test_extract_prompt_details() {
        let request_body = r#"{
            "conversationState": {
                "conversationId": "conv-1",
                "currentMessage": {
                    "userInputMessage": {
                        "content": "hello",
                        "modelId": "claude-sonnet-4.6",
                        "userInputMessageContext": {
                            "tools": [
                                {
                                    "toolSpecification": {
                                        "name": "read_file"
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        }"#;

        let details = extract_prompt_details(request_body);
        assert_eq!(details.conversation_id.as_deref(), Some("conv-1"));
        assert_eq!(details.model.as_deref(), Some("claude-sonnet-4.6"));
        assert_eq!(details.prompt.as_deref(), Some("hello"));
        assert_eq!(details.tool_names, vec!["read_file"]);
    }
}
