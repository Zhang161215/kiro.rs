//! 从 `kiro_kv_cache_records.jsonl` 聚合概览统计。
//!
//! 本仓库没有独立的 usage_log / 客户端 Key。请求结束时已经把 token 写入
//! KV 缓存明细文件，概览直接扫这份 JSONL，按小时/天桶查询。

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Timelike, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

const HOUR_BUCKETS: usize = 24 * 90;
const DAY_BUCKETS: usize = 400;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KvLogRow {
    recorded_at: String,
    model: String,
    #[serde(default)]
    credential_id: u64,
    #[serde(default)]
    cache_creation_input_tokens: i32,
    #[serde(default)]
    cache_read_input_tokens: i32,
    #[serde(default)]
    input_tokens: i32,
    #[serde(default)]
    output_tokens: i32,
    #[serde(default)]
    credits_used: f64,
}

#[derive(Debug, Clone)]
struct UsageRecord {
    ts: String,
    credential_id: u64,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    credits: f64,
}

impl UsageRecord {
    fn from_kv_row(row: KvLogRow) -> Self {
        let total = row.input_tokens.max(0) as u64;
        let cache_creation = row.cache_creation_input_tokens.max(0) as u64;
        let cache_read = row.cache_read_input_tokens.max(0) as u64;
        let input = total.saturating_sub(cache_creation.saturating_add(cache_read));
        Self {
            ts: row.recorded_at,
            credential_id: row.credential_id,
            model: row.model,
            input_tokens: input,
            output_tokens: row.output_tokens.max(0) as u64,
            cache_creation_tokens: cache_creation,
            cache_read_tokens: cache_read,
            credits: if row.credits_used.is_finite() {
                row.credits_used.max(0.0)
            } else {
                0.0
            },
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct BucketStats {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    calls: u64,
    errors: u64,
    credits: f64,
}

impl BucketStats {
    fn add(&mut self, rec: &UsageRecord) {
        self.input_tokens += rec.input_tokens;
        self.output_tokens += rec.output_tokens;
        self.cache_creation_tokens += rec.cache_creation_tokens;
        self.cache_read_tokens += rec.cache_read_tokens;
        self.credits += rec.credits;
        self.calls += 1;
    }
}

#[derive(Debug, Default, Clone)]
struct BucketEntry {
    ts: i64,
    overall: BucketStats,
    by_model: HashMap<String, BucketStats>,
    by_credential: HashMap<u64, BucketStats>,
}

pub struct UsageAggregator {
    inner: RwLock<AggregatorInner>,
}

struct AggregatorInner {
    hour_buckets: Vec<BucketEntry>,
    day_buckets: Vec<BucketEntry>,
}

#[derive(Debug, Clone, Copy)]
pub enum Range {
    Last24h,
    Last7d,
    Last30d,
}

impl Range {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "24h" => Some(Range::Last24h),
            "7d" => Some(Range::Last7d),
            "30d" => Some(Range::Last30d),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsGranularity {
    Hour,
    Day,
}

impl StatsGranularity {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hour" => Some(StatsGranularity::Hour),
            "day" => Some(StatsGranularity::Day),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StatsQueryWindow {
    pub start_ts: i64,
    pub end_ts: i64,
    pub granularity: StatsGranularity,
}

impl StatsQueryWindow {
    pub fn preset(range: Range, granularity: StatsGranularity) -> Self {
        let now = Utc::now().timestamp();
        let start_ts = match range {
            Range::Last24h => now - 24 * 3600,
            Range::Last7d => now - 7 * 24 * 3600,
            Range::Last30d => now - 30 * 24 * 3600,
        };
        Self {
            start_ts,
            end_ts: now,
            granularity,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeSeriesPoint {
    pub ts: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub calls: u64,
    pub errors: u64,
    pub credits: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDistribution {
    pub model: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialDistribution {
    pub credential_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub errors: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverviewStats {
    pub today_calls: u64,
    pub today_input_tokens: u64,
    pub today_output_tokens: u64,
    pub today_errors: u64,
    pub today_credits: f64,
    pub week_calls: u64,
    pub week_input_tokens: u64,
    pub week_output_tokens: u64,
    pub week_credits: f64,
}

impl UsageAggregator {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(AggregatorInner {
                hour_buckets: Vec::new(),
                day_buckets: Vec::new(),
            }),
        }
    }

    pub fn rebuild_from_kv_jsonl(&self, path: &Path) {
        {
            let mut inner = self.inner.write();
            inner.hour_buckets.clear();
            inner.day_buckets.clear();
        }
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!("读取请求明细失败 {}: {}", path.display(), e);
                return;
            }
        };
        let mut count = 0u64;
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { continue };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(row) = serde_json::from_str::<KvLogRow>(line) else {
                continue;
            };
            self.ingest(&UsageRecord::from_kv_row(row));
            count += 1;
        }
        if count > 0 {
            tracing::info!("概览统计已从 {} 装载 {} 条请求明细", path.display(), count);
        }
    }

    fn ingest(&self, rec: &UsageRecord) {
        let dt: DateTime<Utc> = DateTime::parse_from_rfc3339(&rec.ts)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let local = dt.with_timezone(&Local);
        let hour_ts = Local
            .with_ymd_and_hms(local.year(), local.month(), local.day(), local.hour(), 0, 0)
            .single()
            .map(|d| d.timestamp())
            .unwrap_or(0);
        let day_ts = Local
            .with_ymd_and_hms(local.year(), local.month(), local.day(), 0, 0, 0)
            .single()
            .map(|d| d.timestamp())
            .unwrap_or(0);
        let mut inner = self.inner.write();
        upsert_bucket(&mut inner.hour_buckets, hour_ts, rec, HOUR_BUCKETS);
        upsert_bucket(&mut inner.day_buckets, day_ts, rec, DAY_BUCKETS);
    }

    pub fn query_timeseries(&self, window: StatsQueryWindow) -> Vec<TimeSeriesPoint> {
        let inner = self.inner.read();
        let buckets = select_buckets(&inner, window.granularity);
        let mut points: Vec<TimeSeriesPoint> = buckets
            .iter()
            .filter(|b| bucket_in_window(b, window))
            .map(|b| TimeSeriesPoint {
                ts: ts_to_rfc3339(b.ts),
                input_tokens: b.overall.input_tokens,
                output_tokens: b.overall.output_tokens,
                cache_creation_tokens: b.overall.cache_creation_tokens,
                cache_read_tokens: b.overall.cache_read_tokens,
                calls: b.overall.calls,
                errors: b.overall.errors,
                credits: b.overall.credits,
            })
            .collect();
        points.sort_by_key(|p| p.ts.clone());
        points
    }

    pub fn query_by_model(&self, window: StatsQueryWindow) -> Vec<ModelDistribution> {
        let inner = self.inner.read();
        let buckets = select_buckets(&inner, window.granularity);
        let mut acc: HashMap<String, BucketStats> = HashMap::new();
        for b in buckets.iter().filter(|b| bucket_in_window(b, window)) {
            for (model, stats) in &b.by_model {
                let entry = acc.entry(model.clone()).or_default();
                entry.input_tokens += stats.input_tokens;
                entry.output_tokens += stats.output_tokens;
                entry.calls += stats.calls;
            }
        }
        let mut out: Vec<ModelDistribution> = acc
            .into_iter()
            .map(|(model, stats)| ModelDistribution {
                model,
                calls: stats.calls,
                input_tokens: stats.input_tokens,
                output_tokens: stats.output_tokens,
            })
            .collect();
        out.sort_by(|a, b| b.calls.cmp(&a.calls));
        out
    }

    pub fn query_by_credential(&self, window: StatsQueryWindow) -> Vec<CredentialDistribution> {
        let inner = self.inner.read();
        let buckets = select_buckets(&inner, window.granularity);
        let mut acc: HashMap<u64, BucketStats> = HashMap::new();
        for b in buckets.iter().filter(|b| bucket_in_window(b, window)) {
            for (id, stats) in &b.by_credential {
                if *id == 0 {
                    continue;
                }
                let entry = acc.entry(*id).or_default();
                entry.input_tokens += stats.input_tokens;
                entry.output_tokens += stats.output_tokens;
                entry.calls += stats.calls;
                entry.errors += stats.errors;
            }
        }
        let mut out: Vec<CredentialDistribution> = acc
            .into_iter()
            .map(|(id, stats)| CredentialDistribution {
                credential_id: id,
                email: None,
                calls: stats.calls,
                input_tokens: stats.input_tokens,
                output_tokens: stats.output_tokens,
                errors: stats.errors,
            })
            .collect();
        out.sort_by(|a, b| b.calls.cmp(&a.calls));
        out
    }

    pub fn overview(&self) -> OverviewStats {
        let inner = self.inner.read();
        let today_start = Local
            .with_ymd_and_hms(
                Local::now().year(),
                Local::now().month(),
                Local::now().day(),
                0,
                0,
                0,
            )
            .single()
            .map(|d| d.timestamp())
            .unwrap_or(0);
        let mut today = BucketStats::default();
        for b in inner.hour_buckets.iter().filter(|b| b.ts >= today_start) {
            today.input_tokens += b.overall.input_tokens;
            today.output_tokens += b.overall.output_tokens;
            today.calls += b.overall.calls;
            today.errors += b.overall.errors;
            today.credits += b.overall.credits;
        }
        let week_cutoff = Utc::now().timestamp() - 7 * 24 * 3600;
        let mut week = BucketStats::default();
        for b in inner.hour_buckets.iter().filter(|b| b.ts >= week_cutoff) {
            week.input_tokens += b.overall.input_tokens;
            week.output_tokens += b.overall.output_tokens;
            week.calls += b.overall.calls;
            week.credits += b.overall.credits;
        }
        OverviewStats {
            today_calls: today.calls,
            today_input_tokens: today.input_tokens,
            today_output_tokens: today.output_tokens,
            today_errors: today.errors,
            today_credits: today.credits,
            week_calls: week.calls,
            week_input_tokens: week.input_tokens,
            week_output_tokens: week.output_tokens,
            week_credits: week.credits,
        }
    }
}

fn upsert_bucket(buckets: &mut Vec<BucketEntry>, ts: i64, rec: &UsageRecord, max: usize) {
    if let Some(b) = buckets.iter_mut().find(|b| b.ts == ts) {
        add_record_to_bucket(b, rec);
        return;
    }
    let mut entry = BucketEntry {
        ts,
        ..Default::default()
    };
    add_record_to_bucket(&mut entry, rec);
    buckets.push(entry);
    buckets.sort_by_key(|b| b.ts);
    while buckets.len() > max {
        buckets.remove(0);
    }
}

fn add_record_to_bucket(bucket: &mut BucketEntry, rec: &UsageRecord) {
    bucket.overall.add(rec);
    bucket.by_model.entry(rec.model.clone()).or_default().add(rec);
    if rec.credential_id != 0 {
        bucket
            .by_credential
            .entry(rec.credential_id)
            .or_default()
            .add(rec);
    }
}

fn bucket_in_window(bucket: &BucketEntry, window: StatsQueryWindow) -> bool {
    bucket.ts >= window.start_ts && bucket.ts < window.end_ts
}

fn select_buckets(inner: &AggregatorInner, granularity: StatsGranularity) -> &[BucketEntry] {
    match granularity {
        StatsGranularity::Hour => &inner.hour_buckets,
        StatsGranularity::Day => &inner.day_buckets,
    }
}

fn ts_to_rfc3339(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

pub fn parse_stats_window(
    range: Option<&str>,
    start_date: Option<&str>,
    end_date: Option<&str>,
    granularity: Option<&str>,
) -> Result<StatsQueryWindow, String> {
    let granularity = granularity
        .and_then(StatsGranularity::parse)
        .ok_or_else(|| "granularity 必须是 hour 或 day".to_string())?;
    match (start_date, end_date) {
        (Some(start), Some(end)) => {
            let start_date = parse_stats_date(start, "startDate")?;
            let end_date = parse_stats_date(end, "endDate")?;
            if end_date < start_date {
                return Err("endDate 不能早于 startDate".to_string());
            }
            let start_ts = local_midnight_ts(start_date)?;
            let end_ts = local_midnight_ts(end_date + Duration::days(1))?;
            Ok(StatsQueryWindow {
                start_ts,
                end_ts,
                granularity,
            })
        }
        (None, None) => {
            let range = range
                .and_then(Range::parse)
                .ok_or_else(|| "range 必须是 24h、7d 或 30d".to_string())?;
            Ok(StatsQueryWindow::preset(range, granularity))
        }
        _ => Err("startDate 和 endDate 必须同时提供".to_string()),
    }
}

fn parse_stats_date(value: &str, name: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| format!("{} 必须使用 YYYY-MM-DD 格式", name))
}

fn local_midnight_ts(date: NaiveDate) -> Result<i64, String> {
    Local
        .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
        .single()
        .map(|d| d.timestamp())
        .ok_or_else(|| format!("日期 {} 无法转换为本地时间", date))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_and_query_today() {
        let agg = UsageAggregator::new();
        agg.ingest(&UsageRecord {
            ts: Utc::now().to_rfc3339(),
            credential_id: 7,
            model: "claude-sonnet".into(),
            input_tokens: 10,
            output_tokens: 20,
            cache_creation_tokens: 1,
            cache_read_tokens: 2,
            credits: 0.5,
        });
        let overview = agg.overview();
        assert_eq!(overview.today_calls, 1);
        assert_eq!(overview.today_input_tokens, 10);
        let window = StatsQueryWindow::preset(Range::Last24h, StatsGranularity::Hour);
        let models = agg.query_by_model(window);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model, "claude-sonnet");
        let creds = agg.query_by_credential(window);
        assert_eq!(creds[0].credential_id, 7);
    }

    #[test]
    fn parse_window_rejects_bad_granularity() {
        assert!(parse_stats_window(Some("24h"), None, None, Some("week")).is_err());
        assert!(parse_stats_window(Some("24h"), None, None, Some("hour")).is_ok());
    }
}
