//! 模拟 KV Cache（Prompt Cache）记录器
//!
//! 上游只返回 credits，不包含 prompt cache 命中信息。
//! 这里模拟「KV cache 可复用的前缀」概念：
//! - 将请求的 system/tools/messages 归一化后生成一串 prompt block hash
//! - 在本地保存一批历史 prompt（按 namespace 分组）
//! - 对新请求，寻找“最长前缀匹配”的历史 prompt 作为 cache read
//! - 默认将“最后一个 prompt block”视为 cache breakpoint 之后的动态输入：
//!   仅 breakpoint 之前的前缀参与 cache read / cache creation
//! - 统计结果写入 JSONL，供 Admin 面板展示与计费估算使用

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::types::{Message, SystemMessage, Tool};
use crate::token;

const KV_RECORDS_FILE: &str = "kiro_kv_cache_records.jsonl";
/// 小于该阈值的请求不参与 prompt cache（既不读也不写），避免小请求造成误判/噪声。
const MIN_CACHEABLE_INPUT_TOKENS: i32 = 1000;
/// KV cache 状态在内存中的 TTL（秒）
const KV_STATE_TTL_SECS: i64 = 3600;
/// 为避免每次请求都扫描全部 namespaces，做一次全量 prune 的最小间隔（秒）
const KV_STATE_PRUNE_INTERVAL_SECS: i64 = 30;
// v5: KV cache 记录与状态中的 input_tokens 口径采用“本地可见 prompt 的确定性估算 tokens”。
//
// 背景：Kiro 的 contextUsageEvent 只给出百分比，且该值在实践中可能受输出长度/内部实现影响，
// 用它推导出的 tokens 可能出现非单调（甚至减少），导致 cacheRead/cacheRatio 失真。
//
// 因此：
// - KV cache 统计（inputTokens/cachedTokens/cacheRatio/costUsd）统一使用本地估算 tokens
// - 对外返回的 usage.input_tokens 仍沿用原逻辑（如果有 contextUsageEvent 则使用它）
const KV_STATE_VERSION: u32 = 6;
const MAX_ENTRIES_PER_NAMESPACE: usize = 128;

#[derive(Debug, Clone)]
pub struct KvCacheRecordInput {
    pub endpoint: &'static str,
    pub model: String,
    pub stream: bool,
    /// 归一化后的 prompt block hash 列表（system/tools/messages）
    pub prompt_hashes: Vec<String>,
    /// 与 prompt_hashes 对齐的每个 block 的 tokens（本地估算，可复现）
    pub block_tokens: Vec<i32>,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub credits_used: f64,
    /// 本次请求触发的特殊设置（用于审计）
    pub special_settings: Vec<String>,
}

/// 模拟 KV cache 的命中结果（用于写入记录文件，以及回填到 API usage 字段）
#[derive(Debug, Clone)]
pub struct KvCacheSimResult {
    pub cache_key: String,
    pub cache_hit: bool,
    pub cache_creation_input_tokens: i32,
    pub cache_read_input_tokens: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KvPromptEntry {
    prompt_hashes: Vec<String>,
    block_tokens: Vec<i32>,
    input_tokens: i32,
    hit_count: u64,
    /// Unix timestamp（秒）
    last_seen_at: i64,
}

#[derive(Debug)]
struct KvInMemoryState {
    version: u32,
    namespaces: HashMap<String, Vec<KvPromptEntry>>,
    last_pruned_at: i64,
}

static KV_STATE: OnceLock<Mutex<KvInMemoryState>> = OnceLock::new();
static KV_RECORDS_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static KV_CONFIG: OnceLock<Mutex<(f64, i64)>> = OnceLock::new();

/// 设置 KV cache 的运行时配置（可多次调用，后续调用会更新值）
pub fn set_kv_cache_config(cache_read_efficiency: f64, kv_cache_ttl_secs: i64) {
    let val = (cache_read_efficiency.clamp(0.0, 1.0), kv_cache_ttl_secs.max(60));
    match KV_CONFIG.get() {
        Some(lock) => *lock.lock() = val,
        None => { let _ = KV_CONFIG.set(Mutex::new(val)); }
    }
}

fn get_cache_read_efficiency() -> f64 {
    KV_CONFIG.get().map(|l| l.lock().0).unwrap_or(0.87)
}

fn get_kv_cache_ttl_secs() -> i64 {
    KV_CONFIG.get().map(|l| l.lock().1).unwrap_or(3600)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct KvCacheRecord {
    recorded_at: String,
    request_id: String,
    endpoint: String,
    model: String,
    stream: bool,
    cache_key: String,
    cache_hit: bool,
    cache_creation_input_tokens: i32,
    cache_read_input_tokens: i32,
    input_tokens: i32,
    output_tokens: i32,
    credits_used: f64,
    special_settings: Vec<String>,
}

fn resolve_cache_dir(dir_hint: Option<PathBuf>) -> PathBuf {
    dir_hint.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn records_file_path(dir_hint: Option<PathBuf>) -> PathBuf {
    resolve_cache_dir(dir_hint).join(KV_RECORDS_FILE)
}

fn namespace_key(endpoint: &str, model: &str) -> String {
    format!("{}|{}", endpoint, model)
}

fn cache_key_for(prompt_hashes: &[String]) -> String {
    let mut hasher = Sha256::new();
    for (idx, h) in prompt_hashes.iter().enumerate() {
        if idx > 0 {
            hasher.update(b"\n");
        }
        hasher.update(h.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn append_record(path: &PathBuf, record: &KvCacheRecord) -> anyhow::Result<()> {
    // Serialize+append must be protected to avoid interleaved writes from concurrent requests.
    let write_lock = KV_RECORDS_WRITE_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = write_lock.lock();

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    file.write_all(&line)?;
    Ok(())
}

fn is_prefix(prefix: &[String], full: &[String]) -> bool {
    prefix.len() <= full.len() && prefix.iter().zip(full.iter()).all(|(a, b)| a == b)
}

fn upsert_prompt_entry(
    entries: &mut Vec<KvPromptEntry>,
    prompt_hashes: Vec<String>,
    block_tokens: Vec<i32>,
    tokens: i32,
    now_ts: i64,
) {
    let tokens = tokens.max(0);
    if let Some(existing) = entries
        .iter_mut()
        .find(|e| e.prompt_hashes == prompt_hashes)
    {
        existing.block_tokens = block_tokens
            .into_iter()
            .map(|t| t.max(0))
            .collect::<Vec<_>>();
        existing.input_tokens = tokens;
        existing.last_seen_at = now_ts;
        return;
    }

    entries.push(KvPromptEntry {
        prompt_hashes,
        block_tokens: block_tokens
            .into_iter()
            .map(|t| t.max(0))
            .collect::<Vec<_>>(),
        input_tokens: tokens,
        hit_count: 0,
        last_seen_at: now_ts,
    });
}

fn common_prefix_len(a: &[String], b: &[String]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0usize;
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

fn sum_prefix_tokens(tokens: &[i32], prefix_len: usize) -> i32 {
    let mut total = 0i32;
    for t in tokens.iter().take(prefix_len) {
        total = total.saturating_add((*t).max(0));
    }
    total
}

fn maybe_prune_state(state: &mut KvInMemoryState, now_ts: i64, ttl_secs: i64) {
    if now_ts <= 0 {
        return;
    }
    if now_ts.saturating_sub(state.last_pruned_at) < KV_STATE_PRUNE_INTERVAL_SECS {
        return;
    }
    state.last_pruned_at = now_ts;

    let deadline = now_ts.saturating_sub(ttl_secs);
    state.namespaces.retain(|_, entries| {
        entries.retain(|e| e.last_seen_at >= deadline);
        !entries.is_empty()
    });
}

fn record_impl(
    cache_dir_hint: Option<PathBuf>,
    input: KvCacheRecordInput,
) -> anyhow::Result<KvCacheSimResult> {
    let dir = resolve_cache_dir(cache_dir_hint.clone());
    fs::create_dir_all(&dir)?;

    let records_path = records_file_path(cache_dir_hint);

    let now_dt = Utc::now();
    let now = now_dt.to_rfc3339();
    let now_ts = now_dt.timestamp();
    let cache_read_efficiency = get_cache_read_efficiency();
    let kv_cache_ttl_secs = get_kv_cache_ttl_secs();
    let input_tokens = input.input_tokens.max(0);
    let prompt_hashes = input.prompt_hashes;
    let block_tokens = input
        .block_tokens
        .into_iter()
        .map(|t| t.max(0))
        .collect::<Vec<_>>();
    let cache_key = cache_key_for(&prompt_hashes);
    // 对齐 Anthropic 口径：把最后一个 block 视为 breakpoint 之后的动态输入，
    // 仅其之前的前缀参与缓存读写。
    let cacheable_prefix_len = prompt_hashes.len().saturating_sub(1);
    let cacheable_input_tokens = sum_prefix_tokens(&block_tokens, cacheable_prefix_len).max(0);
    let ns_key = namespace_key(input.endpoint, &input.model);
    let special_settings = input
        .special_settings
        .into_iter()
        .filter_map(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect::<Vec<_>>();

    let mut cache_read_input_tokens = 0i32;
    let mut cache_creation_input_tokens = 0i32;
    let mut cache_hit = false;

    // 小于阈值或无可缓存前缀的请求不参与 KV cache（既不读也不写）
    if cacheable_prefix_len > 0 && cacheable_input_tokens >= MIN_CACHEABLE_INPUT_TOKENS {
        let state = KV_STATE.get_or_init(|| {
            Mutex::new(KvInMemoryState {
                version: KV_STATE_VERSION,
                namespaces: HashMap::new(),
                last_pruned_at: 0,
            })
        });
        let mut state = state.lock();
        if state.version != KV_STATE_VERSION {
            state.version = KV_STATE_VERSION;
            state.namespaces.clear();
            state.last_pruned_at = 0;
        }
        maybe_prune_state(&mut state, now_ts, kv_cache_ttl_secs);

        let entries = state.namespaces.entry(ns_key).or_default();

        // 选择“最长共同前缀”的历史 prompt 作为 cache read
        //
        // KV cache 的可复用条件是：两次请求的前缀 tokens 完全一致。
        // 这并不要求“历史 prompt 必须是当前 prompt 的严格前缀”，
        // 只要共享的前缀足够长，就能复用该前缀的 K/V（直到第一个不一致的位置）。
        let mut best_idx: Option<usize> = None;
        let mut best_prefix_len = 0usize;
        let mut best_read_tokens = 0i32;
        for (idx, entry) in entries.iter().enumerate() {
            let entry_cacheable_prefix_len = entry.prompt_hashes.len().saturating_sub(1);
            let prefix_len = common_prefix_len(&entry.prompt_hashes, &prompt_hashes)
                .min(cacheable_prefix_len)
                .min(entry_cacheable_prefix_len);
            // 至少匹配 system + tools 两个基础块，避免把“空前缀”误当命中。
            if prefix_len < 2 {
                continue;
            }
            let read_tokens = sum_prefix_tokens(&entry.block_tokens, prefix_len);
            if read_tokens > best_read_tokens
                || (read_tokens == best_read_tokens && prefix_len > best_prefix_len)
            {
                best_idx = Some(idx);
                best_prefix_len = prefix_len;
                best_read_tokens = read_tokens;
            }
        }

        if let Some(idx) = best_idx {
            // 应用缓存效率折扣：模拟真实 KV cache 并非 100% 可复用
            let raw_read = best_read_tokens.min(cacheable_input_tokens);
            cache_read_input_tokens = (raw_read as f64 * cache_read_efficiency).round() as i32;
            entries[idx].hit_count = entries[idx].hit_count.saturating_add(1);
            entries[idx].last_seen_at = now_ts;
        }

        cache_creation_input_tokens = (cacheable_input_tokens - cache_read_input_tokens).max(0);
        cache_hit = cache_read_input_tokens > 0;

        // Upsert 本次完整 prompt（用于后续前缀匹配）
        upsert_prompt_entry(
            entries,
            prompt_hashes.clone(),
            block_tokens.clone(),
            input_tokens,
            now_ts,
        );

        // 控制单个 namespace 的状态大小
        if entries.len() > MAX_ENTRIES_PER_NAMESPACE {
            entries.sort_by(|a, b| a.last_seen_at.cmp(&b.last_seen_at));
            let drain_count = entries.len().saturating_sub(MAX_ENTRIES_PER_NAMESPACE);
            if drain_count > 0 {
                entries.drain(0..drain_count);
            }
        }
    }

    let record = KvCacheRecord {
        recorded_at: now,
        request_id: Uuid::new_v4().to_string(),
        endpoint: input.endpoint.to_string(),
        model: input.model,
        stream: input.stream,
        cache_key: cache_key.clone(),
        cache_hit,
        cache_creation_input_tokens,
        cache_read_input_tokens,
        input_tokens,
        output_tokens: input.output_tokens.max(0),
        credits_used: if input.credits_used.is_finite() {
            input.credits_used.max(0.0)
        } else {
            0.0
        },
        special_settings,
    };
    append_record(&records_path, &record)?;

    Ok(KvCacheSimResult {
        cache_key,
        cache_hit,
        cache_creation_input_tokens,
        cache_read_input_tokens,
    })
}

fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            let mut out = serde_json::Map::new();
            for key in keys {
                if let Some(v) = map.get(&key) {
                    out.insert(key, canonical_json(v));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(canonical_json).collect())
        }
        other => other.clone(),
    }
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn system_hash(system: &Option<Vec<SystemMessage>>) -> String {
    let text = system
        .as_ref()
        .map(|s| {
            s.iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    sha256_hex(text.as_bytes())
}

fn tools_hash(tools: &Option<Vec<Tool>>) -> String {
    let mut tool_strings: Vec<String> = tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| serde_json::to_value(t).ok())
                .map(|v| serde_json::to_string(&canonical_json(&v)).unwrap_or_default())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    tool_strings.sort();
    sha256_hex(tool_strings.join("\n").as_bytes())
}

fn message_hash(message: &Message) -> String {
    let obj = serde_json::json!({
        "role": message.role,
        "content": canonical_json(&message.content),
    });
    sha256_hex(serde_json::to_string(&obj).unwrap_or_default().as_bytes())
}

/// 将 Anthropic 请求的 system/tools/messages 归一化为 prompt block hash 列表
pub fn build_prompt_hashes(
    system: &Option<Vec<SystemMessage>>,
    messages: &[Message],
    tools: &Option<Vec<Tool>>,
) -> Vec<String> {
    let mut out = Vec::with_capacity(messages.len() + 2);
    out.push(system_hash(system));
    out.push(tools_hash(tools));
    out.extend(messages.iter().map(message_hash));
    out
}

/// 估算与 `build_prompt_hashes` 对齐的每个 prompt block 的 tokens
///
/// 口径与 `token::count_all_tokens_local` 保持一致（但按 block 拆开），用于：
/// - 计算最长共同前缀的可复用 tokens（更贴近 KV cache 的 token-level 语义）
/// - 保持统计稳定、可复现（不依赖上游 contextUsageEvent）
pub fn estimate_prompt_block_tokens(
    system: &Option<Vec<SystemMessage>>,
    messages: &[Message],
    tools: &Option<Vec<Tool>>,
) -> Vec<i32> {
    fn count_json_value_tokens(value: &serde_json::Value) -> u64 {
        match value {
            serde_json::Value::Null => 0,
            serde_json::Value::Bool(b) => token::count_tokens(if *b { "true" } else { "false" }),
            serde_json::Value::Number(n) => token::count_tokens(&n.to_string()),
            serde_json::Value::String(s) => token::count_tokens(s),
            serde_json::Value::Array(arr) => arr.iter().map(count_json_value_tokens).sum(),
            serde_json::Value::Object(obj) => {
                // Unknown shapes: serialize to keep estimate monotonic.
                let s = serde_json::to_string(obj).unwrap_or_default();
                token::count_tokens(&s)
            }
        }
    }

    fn count_content_block_tokens(block: &serde_json::Value) -> u64 {
        let obj = match block.as_object() {
            Some(obj) => obj,
            None => return count_json_value_tokens(block),
        };

        let mut total = 0u64;

        // Common text-bearing fields
        if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
            total += token::count_tokens(text);
        }
        if let Some(thinking) = obj.get("thinking").and_then(|v| v.as_str()) {
            total += token::count_tokens(thinking);
        }

        // Tool use: name + input
        if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
            total += token::count_tokens(name);
        }
        if let Some(input) = obj.get("input") {
            total += count_json_value_tokens(input);
        }

        // Tool result: content can be string / array / object
        if let Some(content) = obj.get("content") {
            total += count_json_value_tokens(content);
        }

        // Fallback: keep growth monotonic for structural blocks.
        if total == 0 {
            total = 1;
        }

        total
    }

    // block 0: system
    let system_text = system
        .as_ref()
        .map(|s| {
            s.iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let system_tokens = token::count_tokens(&system_text) as i32;

    // block 1: tools
    let mut tools_tokens = 0u64;
    if let Some(tools) = tools {
        for tool in tools {
            tools_tokens += token::count_tokens(&tool.name);
            tools_tokens += token::count_tokens(&tool.description);
            let input_schema_json = serde_json::to_string(&tool.input_schema).unwrap_or_default();
            tools_tokens += token::count_tokens(&input_schema_json);
        }
    }

    let mut out = Vec::with_capacity(messages.len() + 2);
    out.push(system_tokens.max(0));
    out.push((tools_tokens as i32).max(0));

    // blocks 2..: messages (1 block per message)
    for msg in messages {
        let mtoks = match &msg.content {
            serde_json::Value::String(s) => token::count_tokens(s),
            serde_json::Value::Array(arr) => arr.iter().map(count_content_block_tokens).sum(),
            other => count_json_value_tokens(other),
        } as i32;
        out.push(mtoks.max(0).max(1));
    }

    out
}

/// 记录一次模拟 KV 缓存统计（失败只记日志，不影响主请求）
pub fn record_simulated_kv_cache(
    cache_dir_hint: Option<PathBuf>,
    input: KvCacheRecordInput,
) -> KvCacheSimResult {
    let fallback = KvCacheSimResult {
        cache_key: cache_key_for(&input.prompt_hashes),
        cache_hit: false,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    };

    let result = if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| record_impl(cache_dir_hint, input))
    } else {
        record_impl(cache_dir_hint, input)
    };

    match result {
        Ok(sim) => sim,
        Err(err) => {
            tracing::warn!("记录模拟 KV 缓存失败: {}", err);
            fallback
        }
    }
}
