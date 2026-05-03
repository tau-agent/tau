//! Historical session timing analysis.
//!
//! Reconstructs a wall-clock breakdown of past sessions purely from existing
//! `messages` rows in `tau.db` — no new telemetry, no schema changes. Adjacent
//! messages (ordered by `id` per session, using the in-JSON `timestamp` field)
//! are bucketed by the role transition:
//!
//! | Adjacency                 | Bucket                    |
//! |---------------------------|---------------------------|
//! | `user → assistant`        | `llm_first`               |
//! | `tool_result → assistant` | `llm_followup`            |
//! | `assistant → tool_result` | `tool:<tool_name>`        |
//! | `assistant → user`        | `user_thinking` (excluded from system perf — user reading) |
//! | anything else             | `other:<role><-<prev_role>` |
//!
//! In addition we synthesise an `llm_generation` bucket that estimates how
//! much of each assistant message's wall time was spent streaming tokens out
//! of the model. This matters because `AssistantMessage::timestamp` is set at
//! stream **start**, so the gap from `assistant → tool_result` includes both
//! LLM generation time AND tool execution time. Without this split, tools
//! that get long tool-call arguments generated for them (e.g. `task_create`
//! with multi-page specs) appear far slower than they really are.
//!
//! The estimate is `output_tokens / output_tps * 1000`, with `output_tps`
//! looked up per provider (60 for Anthropic, 50 for OpenAI, ∞ for mock/log).
//! See [`estimate_output_tps`].
//!
//! All work happens through a single SQL view `message_events`, created lazily
//! the first time a query runs. The view is read-only; this module never
//! mutates `messages` or any other application table.
//!
//! # Known limitations (inherent to the "no new telemetry" approach)
//!
//! - LLM phase split (Connecting / Thinking / Responding) is collapsed into
//!   one bucket (`llm_first` / `llm_followup`).
//! - Rate-limited time is included in LLM time. A heuristic detector
//!   (`dur_ms > 60000 AND tokens < 200`) is left as a follow-up.
//! - Compacting time is invisible: the `compaction_summary` row has no
//!   "started_at" pointer.
//! - Multiple tool calls in one assistant message collapse into a single
//!   `tool_result` gap and cannot be separated here.
//! - Ordering by `id` assumes monotonic insertion order, which is true today
//!   but not contractually guaranteed.
//! - `messages.created_at` and the in-JSON `timestamp` differ on rare
//!   occasions (sub-second usually). The view uses `timestamp` because that's
//!   when work actually happened, not when the row was persisted.
//! - `llm_generation` is an *estimate* derived from `usage.output / output_tps`.
//!   The TPS table is per-provider, not per-model, and ignores prompt-cache
//!   effects, thinking-token budgets, and provider load. Treat as ±25%.
//! - "Pure tool execution" residual time (gap minus LLM-gen estimate) can be
//!   negative if the TPS estimate runs slow relative to a fast model. The
//!   CLI clamps to zero for display.
//!
//! # Percentiles
//!
//! Bundled SQLite has no `percentile_cont`. We pull all `dur_ms` values for
//! the bucket into Rust, sort, and index. Cheap up to ~1M events; if that
//! ever stops being acceptable we can switch to an `NTILE`-based
//! approximation.

use rusqlite::{Connection, params};

use crate::db::Db;

/// Filter applied to profile queries.
#[derive(Debug, Clone, Default)]
pub struct ProfileFilter {
    /// Inclusive lower bound on event `ts` (ms since epoch).
    pub since_ms: Option<i64>,
    /// Inclusive upper bound on event `ts` (ms since epoch).
    pub until_ms: Option<i64>,
    /// Restrict to a single session.
    pub session_id: Option<String>,
    /// Restrict to a single project.
    pub project: Option<String>,
    /// Cap on result rows for "top N" queries (`slow_events`). 0 means
    /// unbounded.
    pub limit: usize,
    /// Per-event duration clamp (ms). Events with `dur_ms > max_event_ms`
    /// are excluded from aggregates and from `slow_events` output. The
    /// dropped count is tracked separately per bucket. `None` disables the
    /// clamp.
    ///
    /// Rationale: stale sessions accumulate huge gaps when an async info
    /// message lands hours/days after the parent went quiet. These gaps
    /// are not real "tool time" or "LLM time" and they swamp the
    /// rankings. A 1h cutoff is a reasonable default — almost any real
    /// tool/LLM gap is well under an hour, and almost any genuine "user
    /// walked away" gap exceeds it.
    pub max_event_ms: Option<i64>,
    /// When `true`, suppress `other:*` buckets from `buckets()` output and
    /// drop `other:*` events from `slow_events()`. The `other:*` buckets
    /// are inherently noisy — they catch every adjacency that isn't a
    /// clean role transition (info<-info, info<-user, etc.) and most are
    /// async-notification artifacts, not real perf signal.
    pub exclude_other: bool,
}

/// One row of an aggregated bucket leaderboard.
#[derive(Debug, Clone, PartialEq)]
pub struct BucketSummary {
    pub bucket: String,
    pub n: i64,
    pub total_ms: i64,
    pub mean_ms: f64,
    pub p50_ms: i64,
    pub p95_ms: i64,
    pub max_ms: i64,
    /// Count of events excluded from this bucket because their duration
    /// exceeded [`ProfileFilter::max_event_ms`]. Always `0` when the
    /// clamp is disabled.
    pub dropped_over_clamp: i64,
}

/// One slow event surfaced by [`slow_events`].
#[derive(Debug, Clone, PartialEq)]
pub struct SlowEvent {
    pub session_id: String,
    pub message_id: i64,
    pub bucket: String,
    pub dur_ms: i64,
    pub at_ms: i64,
    /// Tool input details, best-effort. v1 populates this for `tool:bash`
    /// (the bash command) and leaves it `None` for everything else.
    pub detail: Option<String>,
    /// Estimated LLM-generation time embedded in `dur_ms` for `tool:*`
    /// events. Computed from the triggering assistant message's
    /// `usage.output` divided by the session's per-provider TPS estimate
    /// (see [`estimate_output_tps`]). `None` for non-tool buckets and when
    /// the assistant lookup fails.
    pub llm_gen_ms: Option<i64>,
}

/// Create the `message_events` view if it doesn't already exist.
///
/// Idempotent — safe to call on every query.
pub fn ensure_view(conn: &Connection) -> crate::Result<()> {
    // SQLite has supported the `WINDOW` clause since 3.25 (2018); rusqlite's
    // bundled engine is much newer. Inline `OVER (PARTITION BY ...)` would
    // also work but the WITH form is easier to read.
    conn.execute_batch(
        "CREATE VIEW IF NOT EXISTS message_events AS
         WITH t AS (
             SELECT
                 session_id,
                 id,
                 json_extract(message_json,'$.role')               AS role,
                 json_extract(message_json,'$.tool_name')          AS tool_name,
                 json_extract(message_json,'$.timestamp')          AS ts,
                 json_extract(message_json,'$.usage.total_tokens') AS tokens,
                 json_extract(message_json,'$.usage.cost.total')   AS cost,
                 lag(json_extract(message_json,'$.role'))      OVER w AS prev_role,
                 lag(json_extract(message_json,'$.timestamp')) OVER w AS prev_ts
             FROM messages
             WINDOW w AS (PARTITION BY session_id ORDER BY id)
         )
         SELECT
             session_id, id, ts,
             CASE
                 WHEN role='assistant'   AND prev_role='user'        THEN 'llm_first'
                 WHEN role='assistant'   AND prev_role='tool_result' THEN 'llm_followup'
                 WHEN role='tool_result' AND prev_role='assistant'   THEN 'tool:' || COALESCE(tool_name,'?')
                 WHEN role='user'        AND prev_role IN ('assistant','tool_result') THEN 'user_thinking'
                 ELSE 'other:' || COALESCE(role,'?') || '<-' || COALESCE(prev_role,'start')
             END AS bucket,
             ts - prev_ts AS dur_ms,
             tokens, cost
         FROM t
         WHERE prev_ts IS NOT NULL AND ts > prev_ts;",
    )
    .map_err(|e| crate::Error::Io(format!("create message_events view: {}", e)))?;
    Ok(())
}

/// Bucket leaderboard. Returns one [`BucketSummary`] per distinct bucket
/// matching the filter, sorted by `total_ms` descending.
pub fn buckets(db: &Db, filter: &ProfileFilter) -> crate::Result<Vec<BucketSummary>> {
    let conn = db.conn();
    ensure_view(conn)?;

    // Pull (bucket, dur_ms) pairs for the filter, then aggregate in Rust so
    // we can compute percentiles without a SQL extension.
    let (sql, args) = build_event_query(
        "SELECT e.bucket, e.dur_ms FROM message_events e",
        filter,
        None,
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| crate::Error::Io(format!("prepare buckets: {}", e)))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), |row| {
            let bucket: String = row.get(0)?;
            let dur: i64 = row.get(1)?;
            Ok((bucket, dur))
        })
        .map_err(|e| crate::Error::Io(format!("query buckets: {}", e)))?;

    let mut by_bucket: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();
    let mut dropped_by_bucket: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    for r in rows {
        let (bucket, dur) = r.map_err(|e| crate::Error::Io(format!("row buckets: {}", e)))?;
        if filter.exclude_other && bucket.starts_with("other:") {
            continue;
        }
        if let Some(max) = filter.max_event_ms {
            if dur > max {
                *dropped_by_bucket.entry(bucket).or_default() += 1;
                continue;
            }
        }
        by_bucket.entry(bucket).or_default().push(dur);
    }

    // Synthetic `llm_generation` bucket: per-assistant-message estimate of
    // LLM streaming time, derived from `usage.output` and per-provider TPS.
    // This lets readers see how much of `tool:*` wall time is actually
    // LLM-bound. We *don't* subtract it from `tool:*` here — the renderer
    // can do that as a derived view if it wants.
    let gen_estimates = assistant_gen_estimates(db, filter)?;
    if !gen_estimates.is_empty() {
        let durs: Vec<i64> = gen_estimates.iter().map(|(_, gen_ms, _)| *gen_ms).collect();
        // Apply the clamp to llm_generation as well, for consistency.
        let (kept, dropped): (Vec<i64>, Vec<i64>) = if let Some(max) = filter.max_event_ms {
            durs.into_iter().partition(|d| *d <= max)
        } else {
            (durs, Vec::new())
        };
        if !dropped.is_empty() {
            dropped_by_bucket.insert("llm_generation".to_string(), dropped.len() as i64);
        }
        if !kept.is_empty() {
            by_bucket.insert("llm_generation".to_string(), kept);
        }
    }

    // Buckets that ONLY had drops (no surviving events) still need a row
    // so the user sees the noise count. Insert empty vecs for them.
    for bucket in dropped_by_bucket.keys() {
        by_bucket.entry(bucket.clone()).or_default();
    }
    let mut out: Vec<BucketSummary> = by_bucket
        .into_iter()
        .map(|(bucket, mut durs)| {
            let dropped = dropped_by_bucket.get(&bucket).copied().unwrap_or(0);
            let mut s = summarize(bucket, &mut durs);
            s.dropped_over_clamp = dropped;
            s
        })
        .collect();
    out.sort_by(|a, b| b.total_ms.cmp(&a.total_ms));
    Ok(out)
}

/// Per-session bucket breakdown — equivalent to [`buckets`] with
/// `filter.session_id = Some(session_id)` and no other filters set.
pub fn session_breakdown(db: &Db, session_id: &str) -> crate::Result<Vec<BucketSummary>> {
    let f = ProfileFilter {
        session_id: Some(session_id.to_string()),
        ..Default::default()
    };
    buckets(db, &f)
}

/// Individual events whose duration is at least `min_ms`. Sorted by duration
/// descending. `filter.limit` caps the result count (0 = unbounded).
pub fn slow_events(db: &Db, filter: &ProfileFilter, min_ms: i64) -> crate::Result<Vec<SlowEvent>> {
    let conn = db.conn();
    ensure_view(conn)?;

    let mut extras: Vec<String> = vec![format!("e.dur_ms >= {}", min_ms)];
    if let Some(max) = filter.max_event_ms {
        extras.push(format!("e.dur_ms <= {}", max));
    }
    if filter.exclude_other {
        extras.push("e.bucket NOT LIKE 'other:%'".to_string());
    }
    let extra = extras.join(" AND ");
    let order_limit = if filter.limit > 0 {
        format!(" ORDER BY e.dur_ms DESC LIMIT {}", filter.limit)
    } else {
        " ORDER BY e.dur_ms DESC".to_string()
    };
    let (sql, args) = build_event_query(
        "SELECT e.session_id, e.id, e.bucket, e.dur_ms, e.ts FROM message_events e",
        filter,
        Some(&extra),
    );
    let sql = sql + &order_limit;

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| crate::Error::Io(format!("prepare slow_events: {}", e)))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), |row| {
            Ok(SlowEvent {
                session_id: row.get(0)?,
                message_id: row.get(1)?,
                bucket: row.get(2)?,
                dur_ms: row.get(3)?,
                at_ms: row.get(4)?,
                detail: None,
                llm_gen_ms: None,
            })
        })
        .map_err(|e| crate::Error::Io(format!("query slow_events: {}", e)))?;

    let mut events: Vec<SlowEvent> = Vec::new();
    for r in rows {
        events.push(r.map_err(|e| crate::Error::Io(format!("row slow_events: {}", e)))?);
    }

    // Best-effort detail extraction for tool:bash events plus per-event
    // LLM-generation attribution. We look up the triggering assistant
    // message (highest id < tool_result.id in the same session) for both
    // the tool_call command and `usage.output`. The latter, divided by the
    // session's per-provider TPS estimate, gives the chunk of `dur_ms` that
    // is actually LLM streaming — critical for separating "slow tool" from
    // "slow LLM-generation-of-tool-args".
    if events.iter().any(|e| e.bucket.starts_with("tool:")) {
        let session_models = {
            let mut f = filter.clone();
            // Drop time/limit filters — we just need the model identity for
            // every session that produced an event.
            f.since_ms = None;
            f.until_ms = None;
            f.limit = 0;
            load_session_models(conn, &f)?
        };
        for ev in events.iter_mut() {
            if !ev.bucket.starts_with("tool:") {
                continue;
            }
            let model = session_models
                .get(&ev.session_id)
                .cloned()
                .unwrap_or_default();
            let (cmd, output_tokens) =
                preceding_assistant_info(conn, &ev.session_id, ev.message_id)?;
            if ev.bucket == "tool:bash" {
                ev.detail = cmd;
            }
            if let Some(tokens) = output_tokens {
                ev.llm_gen_ms = Some(estimate_gen_ms(tokens, &model.provider, &model.id));
            }
        }
    }

    Ok(events)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Build a `WHERE` clause matching the filter and append it to `base_select`.
/// Returns the full SQL plus the bound argument list.
fn build_event_query(
    base_select: &str,
    filter: &ProfileFilter,
    extra: Option<&str>,
) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;
    let mut sql = String::from(base_select);
    let mut args: Vec<Value> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();

    if filter.project.is_some() {
        sql.push_str(" JOIN sessions s ON s.id = e.session_id");
    }
    if let Some(s) = &filter.session_id {
        clauses.push("e.session_id = ?".to_string());
        args.push(Value::Text(s.clone()));
    }
    if let Some(p) = &filter.project {
        clauses.push("s.project_name = ?".to_string());
        args.push(Value::Text(p.clone()));
    }
    if let Some(t) = filter.since_ms {
        clauses.push("e.ts >= ?".to_string());
        args.push(Value::Integer(t));
    }
    if let Some(t) = filter.until_ms {
        clauses.push("e.ts <= ?".to_string());
        args.push(Value::Integer(t));
    }
    if let Some(extra) = extra {
        clauses.push(extra.to_string());
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    (sql, args)
}

/// Summarise a vector of durations into a [`BucketSummary`]. Sorts in place.
fn summarize(bucket: String, durs: &mut [i64]) -> BucketSummary {
    durs.sort_unstable();
    let n = durs.len() as i64;
    let total: i64 = durs.iter().sum();
    let mean = if n > 0 { total as f64 / n as f64 } else { 0.0 };
    let p50 = percentile(durs, 0.50);
    let p95 = percentile(durs, 0.95);
    let max = *durs.last().unwrap_or(&0);
    BucketSummary {
        bucket,
        n,
        total_ms: total,
        mean_ms: mean,
        p50_ms: p50,
        p95_ms: p95,
        max_ms: max,
        dropped_over_clamp: 0,
    }
}

/// Nearest-rank percentile on a pre-sorted slice. `q` in `[0.0, 1.0]`.
fn percentile(sorted: &[i64], q: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    // Nearest-rank: ceil(q * n), clamped to [1, n].
    let idx = ((q * n as f64).ceil() as usize).clamp(1, n) - 1;
    sorted[idx]
}

/// Look up the assistant message that triggered a `tool_result` event and
/// extract two facts:
///
/// - For `bash` tool_calls, the `command` argument (rendered as detail).
/// - The assistant message's `usage.output` (used to estimate LLM-gen time).
///
/// `tool_result_id` is the `messages.id` of the tool_result row. The
/// triggering assistant is normally the immediately previous row, but we
/// tolerate a handful of intervening rows just in case.
fn preceding_assistant_info(
    conn: &Connection,
    session_id: &str,
    tool_result_id: i64,
) -> crate::Result<(Option<String>, Option<u64>)> {
    // 1. Pull the tool_result's JSON to learn the tool_call_id.
    let tool_call_id: Option<String> = conn
        .query_row(
            "SELECT json_extract(message_json,'$.tool_call_id') \
             FROM messages WHERE id = ? AND session_id = ?",
            params![tool_result_id, session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|e| crate::Error::Io(format!("lookup tool_result {}: {}", tool_result_id, e)))?;
    let Some(tool_call_id) = tool_call_id else {
        return Ok((None, None));
    };

    // 2. Walk preceding messages in the same session.
    let mut stmt = conn
        .prepare(
            "SELECT message_json FROM messages \
             WHERE session_id = ? AND id < ? \
             ORDER BY id DESC LIMIT 8",
        )
        .map_err(|e| crate::Error::Io(format!("prepare assistant info: {}", e)))?;
    let rows = stmt
        .query_map(params![session_id, tool_result_id], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|e| crate::Error::Io(format!("query assistant info: {}", e)))?;

    for r in rows {
        let json = r.map_err(|e| crate::Error::Io(format!("row assistant info: {}", e)))?;
        let v: serde_json::Value = match serde_json::from_str(&json) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = v.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        // Look for the matching tool_call to confirm this is the right
        // assistant.
        let mut matched = false;
        let mut cmd = None;
        for entry in content {
            let is_tool_call = entry.get("type").and_then(|t| t.as_str()) == Some("tool_call");
            let id_match = entry.get("id").and_then(|i| i.as_str()) == Some(tool_call_id.as_str());
            if is_tool_call && id_match {
                matched = true;
                cmd = entry
                    .get("arguments")
                    .and_then(|a| a.get("command"))
                    .and_then(|c| c.as_str())
                    .map(|s| s.to_string());
                break;
            }
        }
        if !matched {
            continue;
        }
        let output = v
            .get("usage")
            .and_then(|u| u.get("output"))
            .and_then(|o| o.as_u64());
        return Ok((cmd, output));
    }
    Ok((None, None))
}

// ---------------------------------------------------------------------------
// `--since` / `--until` parsing helpers (used by the CLI)
// ---------------------------------------------------------------------------

/// Parse a "since-style" duration into a millisecond cutoff relative to
/// `now_ms`.
///
/// Accepts:
///
/// - `now` → returns `now_ms`.
/// - Plain integer (treated as a millisecond timestamp).
/// - Suffix forms: `30s`, `15m`, `24h`, `7d`, `4w` (case-insensitive).
/// - ISO-8601 dates (`YYYY-MM-DD`) and date-times
///   (`YYYY-MM-DDTHH:MM:SSZ`) — anything `chrono` can parse with
///   `from_str` for `DateTime<Utc>` plus the bare-date fallback.
pub fn parse_since(s: &str, now_ms: i64) -> crate::Result<i64> {
    let s = s.trim();
    if s.is_empty() {
        return Err(crate::Error::Parse("empty --since value".into()));
    }
    if s.eq_ignore_ascii_case("now") {
        return Ok(now_ms);
    }

    // Suffix form: digits + unit char.
    if let Some(last) = s.chars().last() {
        let unit_ms: Option<i64> = match last.to_ascii_lowercase() {
            's' => Some(1_000),
            'm' => Some(60 * 1_000),
            'h' => Some(60 * 60 * 1_000),
            'd' => Some(24 * 60 * 60 * 1_000),
            'w' => Some(7 * 24 * 60 * 60 * 1_000),
            _ => None,
        };
        if let Some(unit) = unit_ms {
            let head = &s[..s.len() - last.len_utf8()];
            if let Ok(n) = head.trim().parse::<i64>() {
                return Ok(now_ms - n.saturating_mul(unit));
            }
        }
    }

    // Plain integer → assume already a ms timestamp.
    if let Ok(n) = s.parse::<i64>() {
        return Ok(n);
    }

    // ISO date / datetime.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp_millis());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = d
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| crate::Error::Parse(format!("invalid date: {}", s)))?
            .and_utc();
        return Ok(dt.timestamp_millis());
    }

    Err(crate::Error::Parse(format!(
        "could not parse --since value: {}",
        s
    )))
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Misc helpers used by the CLI
// ---------------------------------------------------------------------------

/// Per-provider tokens-per-second estimate used to split LLM-generation time
/// out of `assistant → tool_result` gaps.
///
/// These are deliberately rough — the goal is to separate "obviously LLM-bound"
/// from "obviously tool-bound", not to produce a precise breakdown. A 25%
/// error is fine; a 10× attribution mistake is not, which is what happens if
/// we don't subtract LLM-gen at all (see s3255 `task_create` case).
pub fn estimate_output_tps(model_provider: &str, _model_id: &str) -> f64 {
    match model_provider.to_ascii_lowercase().as_str() {
        "anthropic" => 60.0,
        "openai" => 50.0,
        // Mock / log providers don't actually stream tokens — give them an
        // infinite TPS so the estimate collapses to 0.
        "mock" | "log" => f64::INFINITY,
        _ => 60.0,
    }
}

/// LLM-generation estimate (ms) for a single assistant message.
fn estimate_gen_ms(output_tokens: u64, provider: &str, model_id: &str) -> i64 {
    let tps = estimate_output_tps(provider, model_id);
    if !tps.is_finite() || tps <= 0.0 {
        return 0;
    }
    let ms = (output_tokens as f64 / tps) * 1000.0;
    ms.round() as i64
}

/// Per-session model identity, parsed once from `sessions.model_json`.
#[derive(Debug, Clone, Default)]
struct SessionModel {
    provider: String,
    id: String,
}

fn load_session_models(
    conn: &Connection,
    filter: &ProfileFilter,
) -> crate::Result<std::collections::HashMap<String, SessionModel>> {
    let mut sql = String::from(
        "SELECT id, json_extract(model_json,'$.provider'), json_extract(model_json,'$.id') \
         FROM sessions",
    );
    let mut clauses: Vec<String> = Vec::new();
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(s) = &filter.session_id {
        clauses.push("id = ?".into());
        args.push(rusqlite::types::Value::Text(s.clone()));
    }
    if let Some(p) = &filter.project {
        clauses.push("project_name = ?".into());
        args.push(rusqlite::types::Value::Text(p.clone()));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| crate::Error::Io(format!("prepare session models: {}", e)))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), |row| {
            let id: String = row.get(0)?;
            let provider: Option<String> = row.get(1)?;
            let mid: Option<String> = row.get(2)?;
            Ok((
                id,
                SessionModel {
                    provider: provider.unwrap_or_default(),
                    id: mid.unwrap_or_default(),
                },
            ))
        })
        .map_err(|e| crate::Error::Io(format!("query session models: {}", e)))?;

    let mut out = std::collections::HashMap::new();
    for r in rows {
        let (id, m) = r.map_err(|e| crate::Error::Io(format!("row session models: {}", e)))?;
        out.insert(id, m);
    }
    Ok(out)
}

/// Per-assistant-message LLM-generation estimate (ms), keyed by
/// (session_id, message_id). Used to add a synthetic `llm_generation`
/// bucket to the leaderboard and to enrich `SlowEvent`s.
fn assistant_gen_estimates(
    db: &Db,
    filter: &ProfileFilter,
) -> crate::Result<Vec<((String, i64), i64, i64)>> {
    // Returns Vec<((session_id, msg_id), gen_ms, ts_ms)>
    let conn = db.conn();
    let models = load_session_models(conn, filter)?;

    let mut sql = String::from(
        "SELECT m.session_id, m.id, \
                json_extract(m.message_json,'$.usage.output'), \
                json_extract(m.message_json,'$.timestamp') \
         FROM messages m",
    );
    let mut clauses: Vec<String> =
        vec!["json_extract(m.message_json,'$.role') = 'assistant'".into()];
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    if filter.project.is_some() {
        sql.push_str(" JOIN sessions s ON s.id = m.session_id");
    }
    if let Some(s) = &filter.session_id {
        clauses.push("m.session_id = ?".into());
        args.push(rusqlite::types::Value::Text(s.clone()));
    }
    if let Some(p) = &filter.project {
        clauses.push("s.project_name = ?".into());
        args.push(rusqlite::types::Value::Text(p.clone()));
    }
    if let Some(t) = filter.since_ms {
        clauses.push("json_extract(m.message_json,'$.timestamp') >= ?".into());
        args.push(rusqlite::types::Value::Integer(t));
    }
    if let Some(t) = filter.until_ms {
        clauses.push("json_extract(m.message_json,'$.timestamp') <= ?".into());
        args.push(rusqlite::types::Value::Integer(t));
    }
    sql.push_str(" WHERE ");
    sql.push_str(&clauses.join(" AND "));

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| crate::Error::Io(format!("prepare gen estimates: {}", e)))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), |row| {
            let sid: String = row.get(0)?;
            let mid: i64 = row.get(1)?;
            let out: Option<i64> = row.get(2)?;
            let ts: Option<i64> = row.get(3)?;
            Ok((sid, mid, out.unwrap_or(0), ts.unwrap_or(0)))
        })
        .map_err(|e| crate::Error::Io(format!("query gen estimates: {}", e)))?;

    let mut out = Vec::new();
    for r in rows {
        let (sid, mid, tokens, ts) =
            r.map_err(|e| crate::Error::Io(format!("row gen estimates: {}", e)))?;
        if tokens <= 0 {
            continue;
        }
        let model = models.get(&sid).cloned().unwrap_or_default();
        let gen_ms = estimate_gen_ms(tokens as u64, &model.provider, &model.id);
        if gen_ms > 0 {
            out.push(((sid, mid), gen_ms, ts));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------

/// Sum `usage.cost.total` over all messages in a session.
pub fn session_cost_total(db: &Db, session_id: &str) -> crate::Result<f64> {
    let conn = db.conn();
    let total: Option<f64> = conn
        .query_row(
            "SELECT SUM(json_extract(message_json,'$.usage.cost.total')) \
             FROM messages WHERE session_id = ?1",
            params![session_id],
            |row| row.get::<_, Option<f64>>(0),
        )
        .map_err(|e| crate::Error::Io(format!("sum session cost: {}", e)))?;
    Ok(total.unwrap_or(0.0))
}

// ---------------------------------------------------------------------------
// `tau profile tokens` — token / cost rollups.
// ---------------------------------------------------------------------------

/// Token + cost totals over an arbitrary scope (a session, a role, a task).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: f64,
}

impl TokenUsage {
    /// Sum of all four token counters. Cheap; called per row at print time.
    pub fn total_tokens(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }
}

/// One row of the [`token_leaderboard`] / [`task_token_breakdown`] output.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenRow {
    /// Group key — session id, role name, or stringified task id.
    pub group: String,
    /// Number of distinct sessions folded into this row. `1` when grouping
    /// by session.
    pub sessions: u64,
    pub tokens: TokenUsage,
    /// Distinct model identifiers seen in the group, sorted ascending.
    /// Empty when none of the contributing sessions had a recognisable
    /// `model_json.id`.
    pub models: Vec<String>,
}

/// Grouping axis for [`token_leaderboard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenGroupBy {
    /// One row per session.
    Session,
    /// One row per `task_sessions.role` (worker / reviewer / planner / …).
    /// Requires a `tasks_db` so the join can find the role of each session.
    /// Sessions that have no `task_sessions` row are skipped.
    Role,
    /// One row per task. Requires a `tasks_db`. Sessions that have no
    /// `task_sessions` row are skipped.
    Task,
}

/// Sort axis for the leaderboard. Default in the CLI is `Cost`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSort {
    Cost,
    Tokens,
    Input,
    Output,
}

impl Default for TokenSort {
    fn default() -> Self {
        TokenSort::Cost
    }
}

/// Per-session token totals pulled from `messages.message_json` under the
/// current `ProfileFilter`. Cheap shared helper for the three public
/// entry points below.
struct SessionTotals {
    session_id: String,
    tokens: TokenUsage,
    /// `model_json.id` from `sessions`, or empty string when missing.
    model_id: String,
    /// `sessions.project_name` (used only by callers that want to filter
    /// further; the SQL already restricts on it when the filter sets it).
    #[allow(dead_code)]
    project_name: Option<String>,
}

/// Inner SQL for the per-session aggregate. Returns one row per session
/// matching the project / since / until filters. The four token counters
/// and the cost sum come from `json_extract` over the assistant `usage`
/// blob. Messages without a `$.usage` (user, tool_result, info, …) are
/// transparently skipped — `json_extract` returns NULL and `SUM` ignores
/// NULLs.
fn collect_session_totals(db: &Db, filter: &ProfileFilter) -> crate::Result<Vec<SessionTotals>> {
    let conn = db.conn();

    let mut sql = String::from(
        "SELECT s.id, \
                COALESCE(SUM(json_extract(m.message_json,'$.usage.input')),       0) AS in_t, \
                COALESCE(SUM(json_extract(m.message_json,'$.usage.output')),      0) AS out_t, \
                COALESCE(SUM(json_extract(m.message_json,'$.usage.cache_read')),  0) AS cr_t, \
                COALESCE(SUM(json_extract(m.message_json,'$.usage.cache_write')), 0) AS cw_t, \
                COALESCE(SUM(json_extract(m.message_json,'$.usage.cost.total')),  0.0) AS cost, \
                json_extract(s.model_json,'$.id') AS model_id, \
                s.project_name \
         FROM sessions s \
         LEFT JOIN messages m \
             ON m.session_id = s.id \
            AND json_extract(m.message_json,'$.usage') IS NOT NULL",
    );
    let mut clauses: Vec<String> = Vec::new();
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(s) = &filter.session_id {
        clauses.push("s.id = ?".into());
        args.push(rusqlite::types::Value::Text(s.clone()));
    }
    if let Some(p) = &filter.project {
        clauses.push("s.project_name = ?".into());
        args.push(rusqlite::types::Value::Text(p.clone()));
    }
    // The since/until window is on message timestamps, not session
    // creation — this matches `buckets()` semantics and lets a long-lived
    // session contribute partial usage. The clause is wrapped so that
    // sessions with no matching messages still appear (they'll have
    // zeroed totals).
    if let Some(t) = filter.since_ms {
        clauses
            .push("(json_extract(m.message_json,'$.timestamp') IS NULL OR json_extract(m.message_json,'$.timestamp') >= ?)".into());
        args.push(rusqlite::types::Value::Integer(t));
    }
    if let Some(t) = filter.until_ms {
        clauses
            .push("(json_extract(m.message_json,'$.timestamp') IS NULL OR json_extract(m.message_json,'$.timestamp') <= ?)".into());
        args.push(rusqlite::types::Value::Integer(t));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" GROUP BY s.id");

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| crate::Error::Io(format!("prepare token totals: {}", e)))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), |row| {
            let sid: String = row.get(0)?;
            let in_t: i64 = row.get(1)?;
            let out_t: i64 = row.get(2)?;
            let cr_t: i64 = row.get(3)?;
            let cw_t: i64 = row.get(4)?;
            let cost: f64 = row.get(5)?;
            let model_id: Option<String> = row.get(6)?;
            let project_name: Option<String> = row.get(7)?;
            Ok(SessionTotals {
                session_id: sid,
                tokens: TokenUsage {
                    input: in_t.max(0) as u64,
                    output: out_t.max(0) as u64,
                    cache_read: cr_t.max(0) as u64,
                    cache_write: cw_t.max(0) as u64,
                    cost_usd: cost,
                },
                model_id: model_id.unwrap_or_default(),
                project_name,
            })
        })
        .map_err(|e| crate::Error::Io(format!("query token totals: {}", e)))?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| crate::Error::Io(format!("row token totals: {}", e)))?);
    }
    Ok(out)
}

/// Sort a vec of [`TokenRow`] in place by the requested axis, descending.
fn sort_token_rows(rows: &mut [TokenRow], sort: TokenSort) {
    match sort {
        TokenSort::Cost => rows.sort_by(|a, b| {
            b.tokens
                .cost_usd
                .partial_cmp(&a.tokens.cost_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        TokenSort::Tokens => {
            rows.sort_by(|a, b| b.tokens.total_tokens().cmp(&a.tokens.total_tokens()))
        }
        TokenSort::Input => rows.sort_by(|a, b| b.tokens.input.cmp(&a.tokens.input)),
        TokenSort::Output => rows.sort_by(|a, b| b.tokens.output.cmp(&a.tokens.output)),
    }
}

/// Helper: fold a [`TokenUsage`] delta into an accumulator.
fn fold_usage(acc: &mut TokenUsage, delta: &TokenUsage) {
    acc.input = acc.input.saturating_add(delta.input);
    acc.output = acc.output.saturating_add(delta.output);
    acc.cache_read = acc.cache_read.saturating_add(delta.cache_read);
    acc.cache_write = acc.cache_write.saturating_add(delta.cache_write);
    acc.cost_usd += delta.cost_usd;
}

/// Token leaderboard across all sessions matching `filter`, grouped by
/// `group_by`.
///
/// - `TokenGroupBy::Session` is pure sessions-DB and ignores `tasks_db`.
/// - `TokenGroupBy::Role` and `TokenGroupBy::Task` join on
///   `task_sessions` from `tasks_db`. Sessions that have no
///   `task_sessions` row are skipped (orchestrator / interactive
///   sessions usually fall here unless they were promoted to a task).
///
/// `role_filter` restricts Role/Task rollups to one role (e.g.
/// `"worker"`). Ignored for `TokenGroupBy::Session`.
///
/// Sort order is descending by `sort`. The CLI defaults to
/// [`TokenSort::Cost`].
pub fn token_leaderboard(
    db: &Db,
    filter: &ProfileFilter,
    group_by: TokenGroupBy,
    role_filter: Option<&str>,
    sort: TokenSort,
    tasks_db: Option<&tau_agent_plugin_tasks::tasks_db::TasksDb>,
) -> crate::Result<Vec<TokenRow>> {
    let totals = collect_session_totals(db, filter)?;

    match group_by {
        TokenGroupBy::Session => {
            let mut rows: Vec<TokenRow> = totals
                .into_iter()
                .map(|t| {
                    let models = if t.model_id.is_empty() {
                        Vec::new()
                    } else {
                        vec![t.model_id]
                    };
                    TokenRow {
                        group: t.session_id,
                        sessions: 1,
                        tokens: t.tokens,
                        models,
                    }
                })
                .collect();
            sort_token_rows(&mut rows, sort);
            Ok(rows)
        }
        TokenGroupBy::Role | TokenGroupBy::Task => {
            let tasks_db = tasks_db.ok_or_else(|| {
                crate::Error::Parse(
                    "token_leaderboard with Role/Task grouping requires a TasksDb".into(),
                )
            })?;
            // Build session_id -> (task_id, role) map. Use the project
            // filter when present to avoid pulling unrelated tasks.
            let session_map = load_task_session_map(tasks_db, filter.project.as_deref())?;
            let mut groups: std::collections::BTreeMap<
                String,
                (u64, TokenUsage, std::collections::BTreeSet<String>),
            > = std::collections::BTreeMap::new();
            // Track per-group seen sessions so a session counted under a
            // (task, role) is only folded once.
            let mut seen: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
                std::collections::BTreeMap::new();
            for st in totals {
                let Some(meta) = session_map.get(&st.session_id) else {
                    continue;
                };
                if let Some(rf) = role_filter {
                    if meta.role != rf {
                        continue;
                    }
                }
                let key = match group_by {
                    TokenGroupBy::Role => meta.role.clone(),
                    TokenGroupBy::Task => meta.task_id.to_string(),
                    TokenGroupBy::Session => unreachable!(),
                };
                let group_seen = seen.entry(key.clone()).or_default();
                if !group_seen.insert(st.session_id.clone()) {
                    // Already folded this session into this group (a session
                    // can have multiple task_sessions rows for the same
                    // task+role; record_session is idempotent on (task,
                    // session) but a session could in principle appear under
                    // multiple tasks — that's intentional under Role and
                    // distinguished under Task).
                    continue;
                }
                let entry = groups.entry(key).or_insert_with(|| {
                    (
                        0u64,
                        TokenUsage::default(),
                        std::collections::BTreeSet::new(),
                    )
                });
                entry.0 += 1;
                fold_usage(&mut entry.1, &st.tokens);
                if !st.model_id.is_empty() {
                    entry.2.insert(st.model_id.clone());
                }
            }
            let mut rows: Vec<TokenRow> = groups
                .into_iter()
                .map(|(k, (n, tokens, models))| TokenRow {
                    group: k,
                    sessions: n,
                    tokens,
                    models: models.into_iter().collect(),
                })
                .collect();
            sort_token_rows(&mut rows, sort);
            Ok(rows)
        }
    }
}

/// Sum the per-message `usage.{input,output,cache_read,cache_write}` and
/// `usage.cost.total` over all messages in `session_id`. Mirror of
/// [`session_cost_total`] but covering the four token counters too.
pub fn session_token_breakdown(db: &Db, session_id: &str) -> crate::Result<TokenUsage> {
    let conn = db.conn();
    let row = conn
        .query_row(
            "SELECT COALESCE(SUM(json_extract(message_json,'$.usage.input')),       0), \
                    COALESCE(SUM(json_extract(message_json,'$.usage.output')),      0), \
                    COALESCE(SUM(json_extract(message_json,'$.usage.cache_read')),  0), \
                    COALESCE(SUM(json_extract(message_json,'$.usage.cache_write')), 0), \
                    COALESCE(SUM(json_extract(message_json,'$.usage.cost.total')),  0.0) \
             FROM messages \
             WHERE session_id = ?1 \
               AND json_extract(message_json,'$.usage') IS NOT NULL",
            params![session_id],
            |row| {
                let in_t: i64 = row.get(0)?;
                let out_t: i64 = row.get(1)?;
                let cr_t: i64 = row.get(2)?;
                let cw_t: i64 = row.get(3)?;
                let cost: f64 = row.get(4)?;
                Ok(TokenUsage {
                    input: in_t.max(0) as u64,
                    output: out_t.max(0) as u64,
                    cache_read: cr_t.max(0) as u64,
                    cache_write: cw_t.max(0) as u64,
                    cost_usd: cost,
                })
            },
        )
        .map_err(|e| crate::Error::Io(format!("sum session tokens: {}", e)))?;
    Ok(row)
}

/// Per-task token rollup: returns one [`TokenRow`] per
/// `(task_session.role, task_session.session_id)` entry for `task_id`,
/// sorted by cost descending. The `group` field is the role (e.g.
/// `"worker"`); `sessions` is always `1`.
///
/// Multiple sessions sharing the same role on the same task each get
/// their own row — useful when a task was retried (two `worker` sessions)
/// or had a planner + a refiner.
pub fn task_token_breakdown(
    sessions_db: &Db,
    tasks_db: &tau_agent_plugin_tasks::tasks_db::TasksDb,
    task_id: i64,
) -> crate::Result<Vec<TokenRow>> {
    let task_sessions = tasks_db
        .get_sessions(task_id)
        .map_err(|e| crate::Error::Io(format!("load task_sessions for task {}: {}", task_id, e)))?;

    let mut rows: Vec<TokenRow> = Vec::with_capacity(task_sessions.len());
    for ts in task_sessions {
        let tokens = session_token_breakdown(sessions_db, &ts.session_id)?;
        // Pull the model id from the session row for the `models` column.
        let model_id: Option<String> = sessions_db
            .conn()
            .query_row(
                "SELECT json_extract(model_json,'$.id') FROM sessions WHERE id = ?1",
                params![&ts.session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|e| crate::Error::Io(format!("load session model: {}", e)))?;
        let models = match model_id {
            Some(s) if !s.is_empty() => vec![s],
            _ => Vec::new(),
        };
        rows.push(TokenRow {
            group: ts.role,
            sessions: 1,
            tokens,
            models,
        });
    }
    sort_token_rows(&mut rows, TokenSort::Cost);
    Ok(rows)
}

/// Per-session metadata pulled from `task_sessions`. Used to enrich the
/// per-session totals with role / task information.
struct TaskSessionMeta {
    task_id: i64,
    role: String,
}

/// Build a `session_id -> (task_id, role)` map from `task_sessions`.
///
/// When a session is referenced by multiple `task_sessions` rows (rare —
/// a session is typically owned by one task) we keep the first one. The
/// callers are aggregations, not point lookups, so collisions just
/// affect which group a session contributes to, not correctness of
/// totals.
fn load_task_session_map(
    tasks_db: &tau_agent_plugin_tasks::tasks_db::TasksDb,
    project: Option<&str>,
) -> crate::Result<std::collections::HashMap<String, TaskSessionMeta>> {
    let rows = tasks_db
        .list_task_session_roles(project)
        .map_err(|e| crate::Error::Io(format!("list task_sessions: {}", e)))?;
    let mut out: std::collections::HashMap<String, TaskSessionMeta> =
        std::collections::HashMap::new();
    for (task_id, session_id, role) in rows {
        out.entry(session_id)
            .or_insert(TaskSessionMeta { task_id, role });
    }
    Ok(out)
}

// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, StoredSession};
    use crate::types::{
        AssistantContent, AssistantMessage, InfoMessage, Message, Model, ModelCost, StopReason,
        TextContent, ToolCall, ToolResultContent, ToolResultMessage, Usage, UserContent,
        UserMessage,
    };

    fn test_model() -> Model {
        Model {
            id: "test".into(),
            name: "test".into(),
            api: "anthropic".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            thinking: Default::default(),
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 4_096,
            headers: Default::default(),
        }
    }

    fn make_session(db: &Db, id: &str, project: Option<&str>) {
        let s = StoredSession {
            id: id.into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 0,
            parent_id: None,
            child_budget: 16,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: project.map(|s| s.to_string()),
        };
        db.create_session(&s).expect("create session");
    }

    fn user_at(ts: u64, text: &str) -> Message {
        Message::User(UserMessage {
            content: vec![UserContent::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            timestamp: ts,
        })
    }

    fn anthropic_model() -> Model {
        Model {
            id: "claude-test".into(),
            name: "Claude Test".into(),
            api: "anthropic".into(),
            provider: "anthropic".into(),
            base_url: "http://localhost".into(),
            thinking: Default::default(),
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 4_096,
            headers: Default::default(),
        }
    }

    fn make_session_with_model(db: &Db, id: &str, model: Model) {
        let s = StoredSession {
            id: id.into(),
            model,
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 0,
            parent_id: None,
            child_budget: 16,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        };
        db.create_session(&s).expect("create session");
    }

    fn assistant_with_output(
        ts: u64,
        content: Vec<AssistantContent>,
        output_tokens: u64,
    ) -> Message {
        let mut usage = Usage::default();
        usage.output = output_tokens;
        usage.recompute_total();
        Message::Assistant(AssistantMessage {
            content,
            api: "anthropic".into(),
            provider: "anthropic".into(),
            model: "claude-test".into(),
            response_id: None,
            usage,
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: ts,
        })
    }

    fn assistant_at(ts: u64, content: Vec<AssistantContent>) -> Message {
        Message::Assistant(AssistantMessage {
            content,
            api: "anthropic".into(),
            provider: "test".into(),
            model: "test".into(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: ts,
        })
    }

    fn tool_call(id: &str, name: &str, args: serde_json::Value) -> AssistantContent {
        AssistantContent::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args,
        })
    }

    fn tool_result_at(ts: u64, call_id: &str, name: &str, text: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: call_id.into(),
            tool_name: name.into(),
            content: vec![ToolResultContent::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            details: None,
            is_error: false,
            timestamp: ts,
            duration_ms: None,
            summary: None,
            post_persist_actions: Vec::new(),
        })
    }

    #[test]
    fn percentile_basic() {
        let v = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(percentile(&v, 0.5), 5);
        assert_eq!(percentile(&v, 0.95), 10);
        assert_eq!(percentile(&v, 0.0), 1);
        assert_eq!(percentile(&[], 0.5), 0);
    }

    #[test]
    fn parse_since_forms() {
        assert_eq!(parse_since("now", 1000).expect("now"), 1000);
        assert_eq!(parse_since("1s", 10_000).expect("1s"), 9_000);
        assert_eq!(parse_since("1m", 60_001).expect("1m"), 1);
        assert_eq!(parse_since("2h", 7_200_000).expect("2h"), 0);
        assert_eq!(parse_since("1d", 86_400_000).expect("1d"), 0);
        assert_eq!(parse_since("1w", 7 * 86_400_000).expect("1w"), 0);
        assert_eq!(parse_since("12345", 0).expect("plain int"), 12345);
        let iso = parse_since("2024-01-01", 0).expect("iso date");
        assert!(iso > 1_700_000_000_000);
        assert!(parse_since("garbage", 0).is_err());
    }

    #[test]
    fn buckets_basic_sequence() {
        // Sequence: user@0, assistant@1000 (llm_first=1000),
        //           tool_result@1500 (tool:bash=500),
        //           assistant@2000 (llm_followup=500),
        //           user@2200 (user_thinking=200)
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "s1", Some("proj"));

        let msgs: Vec<Message> = vec![
            user_at(0, "hi"),
            assistant_at(
                1000,
                vec![tool_call(
                    "c1",
                    "bash",
                    serde_json::json!({"command": "ls"}),
                )],
            ),
            tool_result_at(1500, "c1", "bash", "ok"),
            assistant_at(2000, vec![]),
            user_at(2200, "thanks"),
        ];
        for m in &msgs {
            db.append_message("s1", m).expect("append");
        }

        let summaries = buckets(&db, &ProfileFilter::default()).expect("buckets");
        let by_name: std::collections::HashMap<_, _> =
            summaries.iter().map(|s| (s.bucket.clone(), s)).collect();

        let llm_first = by_name.get("llm_first").expect("llm_first");
        assert_eq!(llm_first.n, 1);
        assert_eq!(llm_first.total_ms, 1000);

        let bash = by_name.get("tool:bash").expect("tool:bash");
        assert_eq!(bash.n, 1);
        assert_eq!(bash.total_ms, 500);

        let llm_follow = by_name.get("llm_followup").expect("llm_followup");
        assert_eq!(llm_follow.n, 1);
        assert_eq!(llm_follow.total_ms, 500);

        let thinking = by_name.get("user_thinking").expect("user_thinking");
        assert_eq!(thinking.n, 1);
        assert_eq!(thinking.total_ms, 200);

        // Sorted by total_ms desc.
        let totals: Vec<_> = summaries.iter().map(|s| s.total_ms).collect();
        let mut sorted = totals.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(totals, sorted);
    }

    #[test]
    fn slow_events_filters_and_extracts_bash_detail() {
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "s1", None);

        let msgs: Vec<Message> = vec![
            user_at(0, "hi"),
            assistant_at(
                100,
                vec![tool_call(
                    "c1",
                    "bash",
                    serde_json::json!({"command": "sleep 5"}),
                )],
            ),
            // tool result lasts 5s
            tool_result_at(5_100, "c1", "bash", "done"),
            // assistant + tool_result with a short duration (filtered out)
            assistant_at(
                5_200,
                vec![tool_call(
                    "c2",
                    "bash",
                    serde_json::json!({"command": "echo hi"}),
                )],
            ),
            tool_result_at(5_250, "c2", "bash", "ok"),
        ];
        for m in &msgs {
            db.append_message("s1", m).expect("append");
        }

        let evs = slow_events(&db, &ProfileFilter::default(), 1_000).expect("slow_events");
        assert_eq!(evs.len(), 1, "only the 5s bash event passes min=1000");
        let ev = &evs[0];
        assert_eq!(ev.bucket, "tool:bash");
        assert_eq!(ev.dur_ms, 5_000);
        assert_eq!(ev.detail.as_deref(), Some("sleep 5"));

        // No matches with a higher threshold.
        let none = slow_events(&db, &ProfileFilter::default(), 60_000).expect("slow_events");
        assert!(none.is_empty());
    }

    #[test]
    fn session_breakdown_matches_filtered_buckets() {
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "a", None);
        make_session(&db, "b", None);

        let seq_a: Vec<Message> = vec![user_at(0, ""), assistant_at(2_000, vec![])];
        let seq_b: Vec<Message> = vec![user_at(0, ""), assistant_at(7_000, vec![])];
        for m in &seq_a {
            db.append_message("a", m).expect("a");
        }
        for m in &seq_b {
            db.append_message("b", m).expect("b");
        }

        let a = session_breakdown(&db, "a").expect("a");
        let by_filter = buckets(
            &db,
            &ProfileFilter {
                session_id: Some("a".into()),
                ..Default::default()
            },
        )
        .expect("by_filter");
        assert_eq!(a, by_filter);

        // Session a alone has 2_000ms of llm_first.
        let llm = a.iter().find(|s| s.bucket == "llm_first").expect("llm");
        assert_eq!(llm.total_ms, 2_000);

        // Cross-session aggregate sees both.
        let all = buckets(&db, &ProfileFilter::default()).expect("all");
        let llm_all = all
            .iter()
            .find(|s| s.bucket == "llm_first")
            .expect("llm all");
        assert_eq!(llm_all.n, 2);
        assert_eq!(llm_all.total_ms, 9_000);
    }

    #[test]
    fn project_filter_isolates_sessions() {
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "p1", Some("proj-a"));
        make_session(&db, "p2", Some("proj-b"));
        let seq: Vec<Message> = vec![user_at(0, ""), assistant_at(3_000, vec![])];
        for m in &seq {
            db.append_message("p1", m).expect("p1");
            db.append_message("p2", m).expect("p2");
        }

        let only_a = buckets(
            &db,
            &ProfileFilter {
                project: Some("proj-a".into()),
                ..Default::default()
            },
        )
        .expect("a");
        let llm = only_a
            .iter()
            .find(|s| s.bucket == "llm_first")
            .expect("llm a");
        assert_eq!(llm.n, 1);
        assert_eq!(llm.total_ms, 3_000);
    }

    #[test]
    fn ensure_view_is_idempotent() {
        let db = Db::open_memory().expect("open mem");
        ensure_view(db.conn()).expect("first");
        ensure_view(db.conn()).expect("second");
    }

    #[test]
    fn other_bucket_for_info_messages() {
        // Info messages don't fit any of the canonical adjacencies — they
        // should land in `other:`.
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "i", None);
        let info = Message::Info(InfoMessage {
            text: "hello".into(),
            timestamp: 500,
        });
        let user = user_at(1000, "");
        db.append_message("i", &info).expect("info");
        db.append_message("i", &user).expect("user");

        let s = buckets(
            &db,
            &ProfileFilter {
                session_id: Some("i".into()),
                ..Default::default()
            },
        )
        .expect("s");
        assert!(s.iter().any(|b| b.bucket.starts_with("other:")));
    }

    #[test]
    fn since_until_filter_window() {
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "w", None);
        let seq: Vec<Message> = vec![
            user_at(1_000, ""),
            assistant_at(2_000, vec![]), // event ts=2000, dur=1000
            user_at(10_000, ""),
            assistant_at(11_000, vec![]), // event ts=11000, dur=1000
        ];
        for m in &seq {
            db.append_message("w", m).expect("w");
        }

        // Only the second event has ts >= 5000.
        let later = buckets(
            &db,
            &ProfileFilter {
                since_ms: Some(5_000),
                ..Default::default()
            },
        )
        .expect("later");
        let llm = later.iter().find(|s| s.bucket == "llm_first").expect("llm");
        assert_eq!(llm.n, 1);

        // Both events have ts <= 12_000.
        let bounded = buckets(
            &db,
            &ProfileFilter {
                until_ms: Some(12_000),
                ..Default::default()
            },
        )
        .expect("bounded");
        let llm = bounded
            .iter()
            .find(|s| s.bucket == "llm_first")
            .expect("llm");
        assert_eq!(llm.n, 2);
    }

    #[test]
    fn llm_generation_bucket_and_slow_event_enrichment() {
        // An anthropic session: assistant produces 600 tokens, then a 5s
        // tool_result. With 60 tps that's a ~10s LLM-gen estimate, which
        // means the entire 5s tool gap is plausibly LLM-bound. The bucket
        // should reflect the full estimate; the slow_event should carry
        // `llm_gen_ms = 10000`.
        let db = Db::open_memory().expect("open mem");
        make_session_with_model(&db, "a", anthropic_model());

        let msgs: Vec<Message> = vec![
            user_at(0, "hi"),
            assistant_with_output(
                1_000,
                vec![tool_call("c1", "task_create", serde_json::json!({}))],
                600,
            ),
            tool_result_at(6_000, "c1", "task_create", "ok"),
        ];
        for m in &msgs {
            db.append_message("a", m).expect("append");
        }

        let summaries = buckets(&db, &ProfileFilter::default()).expect("buckets");
        let llm_gen = summaries
            .iter()
            .find(|s| s.bucket == "llm_generation")
            .expect("llm_generation bucket present");
        assert_eq!(llm_gen.n, 1);
        // 600 tokens / 60 tps = 10s
        assert_eq!(llm_gen.total_ms, 10_000);

        let evs = slow_events(&db, &ProfileFilter::default(), 1_000).expect("slow");
        let tool_ev = evs
            .iter()
            .find(|e| e.bucket == "tool:task_create")
            .expect("tool event");
        assert_eq!(tool_ev.llm_gen_ms, Some(10_000));
        assert_eq!(tool_ev.dur_ms, 5_000);
    }

    #[test]
    fn estimate_output_tps_known_providers() {
        assert_eq!(estimate_output_tps("anthropic", "claude-x"), 60.0);
        assert_eq!(estimate_output_tps("OpenAI", "gpt-4"), 50.0);
        assert!(estimate_output_tps("mock", "mock").is_infinite());
        assert!(estimate_output_tps("log", "log").is_infinite());
        // Default fallback.
        assert_eq!(estimate_output_tps("weird-new-provider", ""), 60.0);
    }

    #[test]
    fn llm_generation_skipped_for_mock_provider() {
        let db = Db::open_memory().expect("open mem");
        // Default `test_model()` has provider="test" — not in the
        // mock/log allowlist, so it gets the default 60 tps. Use a
        // distinct "mock" provider to verify the skip.
        let mut model = test_model();
        model.provider = "mock".into();
        make_session_with_model(&db, "m", model);
        db.append_message("m", &user_at(0, "hi")).expect("u");
        db.append_message("m", &assistant_with_output(1_000, vec![], 1_000_000))
            .expect("a");

        let summaries = buckets(&db, &ProfileFilter::default()).expect("buckets");
        // No llm_generation bucket because mock TPS is infinite => 0ms gen.
        assert!(
            !summaries.iter().any(|s| s.bucket == "llm_generation"),
            "summaries: {:?}",
            summaries
        );
    }

    #[test]
    fn unknown_session_provider_uses_default_tps() {
        // Confirm that without an anthropic_model the default 60 tps is
        // still applied (test_model has provider="test").
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "d", None);
        db.append_message("d", &user_at(0, "")).expect("u");
        db.append_message(
            "d",
            &assistant_with_output(
                1_000,
                vec![tool_call(
                    "c1",
                    "bash",
                    serde_json::json!({"command": "ls"}),
                )],
                300,
            ),
        )
        .expect("a");
        db.append_message("d", &tool_result_at(2_000, "c1", "bash", "ok"))
            .expect("tr");

        let summaries = buckets(&db, &ProfileFilter::default()).expect("buckets");
        let llm_gen = summaries
            .iter()
            .find(|s| s.bucket == "llm_generation")
            .expect("llm_generation");
        // 300 tokens / 60 tps = 5s
        assert_eq!(llm_gen.total_ms, 5_000);
    }

    #[test]
    fn clamp_drops_events_above_threshold() {
        // user@0, assistant@1_000 (llm_first=1000ms),
        // user@1_000_000 (user_thinking gap = 999_000ms = 16.65m),
        // assistant@1_001_000 (llm_first=1000ms).
        // With clamp = 60_000ms (60s), the 999_000ms user_thinking event
        // should be dropped, but the two 1_000ms llm_first events stay.
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "c", None);
        let msgs: Vec<Message> = vec![
            user_at(0, ""),
            assistant_at(1_000, vec![]),
            user_at(1_000_000, ""),
            assistant_at(1_001_000, vec![]),
        ];
        for m in &msgs {
            db.append_message("c", m).expect("append");
        }

        // Without clamp: user_thinking is the largest bucket.
        let unclamped = buckets(&db, &ProfileFilter::default()).expect("unclamped");
        let ut = unclamped
            .iter()
            .find(|s| s.bucket == "user_thinking")
            .expect("user_thinking present");
        assert_eq!(ut.n, 1);
        assert_eq!(ut.total_ms, 999_000);
        assert_eq!(ut.dropped_over_clamp, 0);

        // With a 60s clamp the user_thinking event is excluded but the
        // bucket row is still emitted with `dropped_over_clamp = 1`.
        let clamped = buckets(
            &db,
            &ProfileFilter {
                max_event_ms: Some(60_000),
                ..Default::default()
            },
        )
        .expect("clamped");
        let llm = clamped
            .iter()
            .find(|s| s.bucket == "llm_first")
            .expect("llm_first present");
        assert_eq!(llm.n, 2);
        assert_eq!(llm.total_ms, 2_000);
        assert_eq!(llm.dropped_over_clamp, 0);
        let ut = clamped
            .iter()
            .find(|s| s.bucket == "user_thinking")
            .expect("user_thinking row still emitted with drop count");
        assert_eq!(ut.n, 0);
        assert_eq!(ut.total_ms, 0);
        assert_eq!(ut.dropped_over_clamp, 1);
    }

    #[test]
    fn slow_events_respects_clamp_and_exclude_other() {
        // Build a session with an info adjacency that produces a long
        // `other:*` event, plus a real bash event.
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "s", None);
        // info -> info -> ... produces other:* buckets.
        let info1 = Message::Info(InfoMessage {
            text: "early".into(),
            timestamp: 0,
        });
        let info2 = Message::Info(InfoMessage {
            text: "much later (stale session noise)".into(),
            timestamp: 10 * 60 * 60 * 1_000, // 10h gap
        });
        db.append_message("s", &info1).expect("info1");
        db.append_message("s", &info2).expect("info2");
        // A normal user/assistant/tool sequence.
        let msgs: Vec<Message> = vec![
            user_at(11 * 60 * 60 * 1_000, "hi"),
            assistant_at(
                11 * 60 * 60 * 1_000 + 1_000,
                vec![tool_call(
                    "c1",
                    "bash",
                    serde_json::json!({"command": "ls"}),
                )],
            ),
            tool_result_at(11 * 60 * 60 * 1_000 + 6_000, "c1", "bash", "ok"),
        ];
        for m in &msgs {
            db.append_message("s", m).expect("append");
        }

        // Default (no clamp, include other): everything visible.
        let all = slow_events(&db, &ProfileFilter::default(), 1_000).expect("slow");
        assert!(all.iter().any(|e| e.bucket.starts_with("other:")));
        assert!(all.iter().any(|e| e.bucket == "tool:bash"));

        // exclude_other suppresses the other:* row.
        let no_other = slow_events(
            &db,
            &ProfileFilter {
                exclude_other: true,
                ..Default::default()
            },
            1_000,
        )
        .expect("slow no_other");
        assert!(no_other.iter().all(|e| !e.bucket.starts_with("other:")));
        assert!(no_other.iter().any(|e| e.bucket == "tool:bash"));

        // 1h clamp drops the 10h other:* event regardless of exclude_other.
        let clamped = slow_events(
            &db,
            &ProfileFilter {
                max_event_ms: Some(60 * 60 * 1_000),
                ..Default::default()
            },
            1_000,
        )
        .expect("slow clamped");
        assert!(clamped.iter().all(|e| e.dur_ms <= 60 * 60 * 1_000));
        assert!(clamped.iter().any(|e| e.bucket == "tool:bash"));
    }

    #[test]
    fn buckets_exclude_other_drops_other_buckets() {
        // Two info messages followed by a user — produces an `other:*`
        // bucket (info<-info, user<-info) plus nothing else.
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "o", None);
        let info1 = Message::Info(InfoMessage {
            text: "first".into(),
            timestamp: 100,
        });
        let info2 = Message::Info(InfoMessage {
            text: "second".into(),
            timestamp: 500,
        });
        let user = user_at(1_000, "");
        db.append_message("o", &info1).expect("info1");
        db.append_message("o", &info2).expect("info2");
        db.append_message("o", &user).expect("user");

        let with_other = buckets(&db, &ProfileFilter::default()).expect("with");
        assert!(with_other.iter().any(|b| b.bucket.starts_with("other:")));

        let without = buckets(
            &db,
            &ProfileFilter {
                exclude_other: true,
                ..Default::default()
            },
        )
        .expect("without");
        assert!(
            without.iter().all(|b| !b.bucket.starts_with("other:")),
            "buckets: {:?}",
            without
        );
    }

    #[test]
    fn drop_counter_is_zero_when_no_clamp() {
        // Without a clamp, every BucketSummary should report
        // dropped_over_clamp = 0.
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "z", None);
        let msgs: Vec<Message> = vec![user_at(0, ""), assistant_at(1_000, vec![])];
        for m in &msgs {
            db.append_message("z", m).expect("append");
        }
        let summaries = buckets(&db, &ProfileFilter::default()).expect("buckets");
        assert!(!summaries.is_empty());
        for s in &summaries {
            assert_eq!(
                s.dropped_over_clamp, 0,
                "bucket {} should have 0 drops without clamp",
                s.bucket
            );
        }
    }

    // ---------------------------------------------------------------
    // `tau profile tokens` — token / cost rollup tests.
    // ---------------------------------------------------------------

    use tau_agent_plugin_tasks::tasks_db::TasksDb;

    /// Build an assistant `Message` with a fully-populated `Usage`.
    fn assistant_with_usage(
        ts: u64,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        cost: f64,
    ) -> Message {
        let usage = Usage {
            input,
            output,
            cache_read,
            cache_write,
            total_tokens: input + output + cache_read + cache_write,
            cost: crate::types::Cost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: cost,
            },
        };
        Message::Assistant(AssistantMessage {
            content: vec![],
            api: "anthropic".into(),
            provider: "anthropic".into(),
            model: "claude-test".into(),
            response_id: None,
            usage,
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: ts,
        })
    }

    /// Make an in-memory tasks DB and seed `(task_id, role)` rows so the
    /// joins in `token_leaderboard` / `task_token_breakdown` find the
    /// caller's sessions.
    fn seed_tasks(
        rows: &[(i64, &str, &str, &str)], // (nominal task_id, project, role, session_id)
    ) -> (TasksDb, std::collections::HashMap<i64, i64>) {
        let tasks = TasksDb::open_memory().expect("open tasks mem");
        let mut id_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        for (nominal, project, _role, _sid) in rows {
            if id_map.contains_key(nominal) {
                continue;
            }
            let title = format!("task-{}", nominal);
            let task = tasks
                .create_task(
                    project,
                    &title,
                    None,
                    None,
                    None,
                    false,
                    "ready",
                    false,
                    None,
                    None,
                    false,
                    None,
                    false,
                    tau_agent_plugin_tasks::tasks_db::FiledBy::default(),
                )
                .expect("create task");
            id_map.insert(*nominal, task.id);
        }
        for (nominal, _project, role, sid) in rows {
            let real = id_map.get(nominal).copied().expect("id mapped");
            tasks
                .record_session(real, sid, role)
                .expect("record_session");
        }
        (tasks, id_map)
    }

    #[test]
    fn token_leaderboard_groups_by_role() {
        // Three sessions, two `worker`, one `reviewer`. The role rollup
        // should sum the two workers and report the reviewer separately.
        let db = Db::open_memory().expect("open mem");
        for sid in ["s-w1", "s-w2", "s-rv"] {
            make_session(&db, sid, Some("proj"));
        }
        // Each session contributes a single assistant message.
        db.append_message("s-w1", &assistant_with_usage(1_000, 100, 50, 10, 5, 0.10))
            .expect("w1");
        db.append_message("s-w2", &assistant_with_usage(1_000, 200, 80, 20, 0, 0.20))
            .expect("w2");
        db.append_message("s-rv", &assistant_with_usage(1_000, 50, 25, 0, 0, 0.05))
            .expect("rv");

        let (tasks, _ids) = seed_tasks(&[
            (1, "proj", "worker", "s-w1"),
            (2, "proj", "worker", "s-w2"),
            (1, "proj", "reviewer", "s-rv"),
        ]);

        let rows = token_leaderboard(
            &db,
            &ProfileFilter::default(),
            TokenGroupBy::Role,
            None,
            TokenSort::Cost,
            Some(&tasks),
        )
        .expect("leaderboard");

        assert_eq!(rows.len(), 2, "one row per role");
        let worker = rows
            .iter()
            .find(|r| r.group == "worker")
            .expect("worker row");
        assert_eq!(worker.sessions, 2);
        assert_eq!(worker.tokens.input, 300);
        assert_eq!(worker.tokens.output, 130);
        assert_eq!(worker.tokens.cache_read, 30);
        assert_eq!(worker.tokens.cache_write, 5);
        assert!((worker.tokens.cost_usd - 0.30).abs() < 1e-9);

        let reviewer = rows
            .iter()
            .find(|r| r.group == "reviewer")
            .expect("reviewer row");
        assert_eq!(reviewer.sessions, 1);
        assert_eq!(reviewer.tokens.input, 50);

        // Cost-desc default: worker > reviewer.
        assert_eq!(rows[0].group, "worker");
    }

    #[test]
    fn token_leaderboard_project_filter() {
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "a1", Some("alpha"));
        make_session(&db, "b1", Some("beta"));
        db.append_message("a1", &assistant_with_usage(0, 100, 0, 0, 0, 1.0))
            .expect("a1");
        db.append_message("b1", &assistant_with_usage(0, 999, 0, 0, 0, 9.99))
            .expect("b1");

        let rows = token_leaderboard(
            &db,
            &ProfileFilter {
                project: Some("alpha".into()),
                ..Default::default()
            },
            TokenGroupBy::Session,
            None,
            TokenSort::Cost,
            None,
        )
        .expect("leaderboard");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group, "a1");
        assert_eq!(rows[0].tokens.input, 100);
        assert!((rows[0].tokens.cost_usd - 1.0).abs() < 1e-9);
    }

    #[test]
    fn session_token_breakdown_sums_usage_fields() {
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "s1", None);
        // Three assistant messages plus one user (which has no usage and
        // must not poison the SUM).
        db.append_message("s1", &user_at(500, "hi")).expect("user");
        db.append_message("s1", &assistant_with_usage(1_000, 10, 5, 1, 2, 0.05))
            .expect("a1");
        db.append_message("s1", &assistant_with_usage(2_000, 20, 8, 3, 0, 0.10))
            .expect("a2");
        db.append_message("s1", &assistant_with_usage(3_000, 30, 12, 0, 4, 0.15))
            .expect("a3");

        let usage = session_token_breakdown(&db, "s1").expect("breakdown");
        assert_eq!(usage.input, 60);
        assert_eq!(usage.output, 25);
        assert_eq!(usage.cache_read, 4);
        assert_eq!(usage.cache_write, 6);
        assert!((usage.cost_usd - 0.30).abs() < 1e-9);
        assert_eq!(usage.total_tokens(), 60 + 25 + 4 + 6);
    }

    #[test]
    fn task_token_breakdown_lists_one_row_per_role() {
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "sw", Some("proj"));
        make_session(&db, "sr", Some("proj"));
        // Worker spends more than reviewer.
        db.append_message("sw", &assistant_with_usage(0, 1_000, 500, 0, 0, 5.00))
            .expect("sw");
        db.append_message("sr", &assistant_with_usage(0, 100, 50, 0, 0, 0.50))
            .expect("sr");

        let (tasks, ids) =
            seed_tasks(&[(42, "proj", "worker", "sw"), (42, "proj", "reviewer", "sr")]);
        let real_id = ids[&42];

        let rows = task_token_breakdown(&db, &tasks, real_id).expect("task breakdown");
        assert_eq!(rows.len(), 2);
        // Cost-desc.
        assert_eq!(rows[0].group, "worker");
        assert_eq!(rows[1].group, "reviewer");
        assert_eq!(rows[0].sessions, 1);
        assert!((rows[0].tokens.cost_usd - 5.00).abs() < 1e-9);
        assert!((rows[1].tokens.cost_usd - 0.50).abs() < 1e-9);
    }

    #[test]
    fn token_leaderboard_distinct_models_per_group() {
        // Two sessions with different model ids, both rolled up into the
        // same role. The `models` vec must list both, sorted, deduped.
        let db = Db::open_memory().expect("open mem");
        let mut m1 = test_model();
        m1.id = "claude-3-opus".into();
        let mut m2 = test_model();
        m2.id = "claude-3-sonnet".into();
        make_session_with_model(&db, "s-opus", m1);
        make_session_with_model(&db, "s-sonnet", m2);
        db.append_message("s-opus", &assistant_with_usage(0, 10, 1, 0, 0, 0.01))
            .expect("opus");
        db.append_message("s-sonnet", &assistant_with_usage(0, 10, 1, 0, 0, 0.01))
            .expect("sonnet");

        let (tasks, _ids) =
            seed_tasks(&[(1, "", "worker", "s-opus"), (2, "", "worker", "s-sonnet")]);

        let rows = token_leaderboard(
            &db,
            &ProfileFilter::default(),
            TokenGroupBy::Role,
            None,
            TokenSort::Cost,
            Some(&tasks),
        )
        .expect("leaderboard");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.group, "worker");
        assert_eq!(row.sessions, 2);
        assert_eq!(row.models, vec!["claude-3-opus", "claude-3-sonnet"]);
    }

    #[test]
    fn token_leaderboard_ignores_messages_without_usage() {
        // User and tool_result messages have no `$.usage` blob — the
        // SUM must skip them via the `IS NOT NULL` join filter so the
        // assistant totals are reported untouched. Regression for an
        // earlier draft that joined unconditionally and tripped over
        // SQLite's behaviour of folding NULLs into 0.
        let db = Db::open_memory().expect("open mem");
        make_session(&db, "s", None);
        db.append_message("s", &user_at(0, "hi")).expect("u1");
        db.append_message(
            "s",
            &assistant_with_usage(
                1_000, /*in*/ 100, /*out*/ 50, /*cr*/ 10, /*cw*/ 5, 0.42,
            ),
        )
        .expect("a");
        db.append_message("s", &tool_result_at(2_000, "call-1", "bash", "ok"))
            .expect("tr");
        db.append_message("s", &user_at(3_000, "thanks"))
            .expect("u2");

        let usage = session_token_breakdown(&db, "s").expect("breakdown");
        assert_eq!(usage.input, 100);
        assert_eq!(usage.output, 50);
        assert_eq!(usage.cache_read, 10);
        assert_eq!(usage.cache_write, 5);
        assert!((usage.cost_usd - 0.42).abs() < 1e-9);

        let rows = token_leaderboard(
            &db,
            &ProfileFilter::default(),
            TokenGroupBy::Session,
            None,
            TokenSort::Cost,
            None,
        )
        .expect("leaderboard");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tokens.input, 100);
        assert_eq!(rows[0].tokens.output, 50);
    }
}
