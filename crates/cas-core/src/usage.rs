//! Read-only accounting of Codex's local JSONL rollouts.
//!
//! New rollouts persist one token_usage_record per completed response. Older
//! rollouts report token_count snapshots; those are used only when the file has
//! no per-response records, so the two formats are never added together.
//! No message bodies are collected, no network requests are made, and Codex
//! files are never written to.

use crate::pricing::{PricingSummary, ResponsePrice, fill_report_prices};
use crate::{CasError, CasPaths, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

const MAX_DIRECTORY_DEPTH: usize = 16;
const MAX_WARNINGS: usize = 25;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TokenCounts {
    /// The API's total input token count, including cached input tokens.
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    /// Included in output_tokens; this must never be billed a second time.
    pub reasoning_output_tokens: u64,
}

impl TokenCounts {
    pub fn fresh_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_sub(self.cached_input_tokens)
            .saturating_sub(self.cache_write_input_tokens)
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    fn has_usage(&self) -> bool {
        self.input_tokens != 0
            || self.output_tokens != 0
            || self.cached_input_tokens != 0
            || self.cache_write_input_tokens != 0
    }

    fn add_assign(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(other.cache_write_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(other.reasoning_output_tokens);
    }

    fn delta_from(&self, baseline: &Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(baseline.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(baseline.cached_input_tokens),
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .saturating_sub(baseline.cache_write_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(baseline.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_sub(baseline.reasoning_output_tokens),
        }
    }

    fn update_high_water(&mut self, current: &Self) {
        self.input_tokens = self.input_tokens.max(current.input_tokens);
        self.cached_input_tokens = self.cached_input_tokens.max(current.cached_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .max(current.cache_write_input_tokens);
        self.output_tokens = self.output_tokens.max(current.output_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .max(current.reasoning_output_tokens);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    TokenUsageRecord,
    LegacyTokenCount,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageRecord {
    pub source: UsageSource,
    pub model: String,
    /// Requested routing tier from the latest persisted thread settings.
    /// `priority` is Codex Fast; this does NOT prove what tier the server used.
    /// None means the rollout has no unambiguous tier setting at this point.
    pub requested_service_tier: Option<String>,
    pub timestamp: Option<String>,
    pub response_id: Option<String>,
    pub turn_id: Option<String>,
    pub tokens: TokenCounts,
    /// Number of unique tool calls emitted by this response, when the call
    /// event can be associated with its following token usage record.
    pub tool_calls: usize,
    #[serde(skip)]
    pub(crate) tool_call_ids: Vec<String>,
    /// Provider-reported input tokens for this individual response, including
    /// cached tokens. This is the input-side size for long-context pricing.
    /// None if legacy totals only allowed a cumulative-delta estimate.
    pub request_context_tokens: Option<u64>,
    /// Capacity, not the size of the prompt or the input tokens billed.
    pub model_context_window: Option<u64>,
    /// Local API-equivalent text token estimate for this response, if the
    /// exact model ID and the context needed for pricing are available.
    pub price: Option<ResponsePrice>,
    #[serde(skip)]
    pub(crate) dedup_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionUsage {
    pub path: PathBuf,
    pub session_id: Option<String>,
    /// ChatGPT account at session creation; not proof of subsequent requests.
    pub creator_account_id: Option<String>,
    pub records: Vec<UsageRecord>,
    pub malformed_lines: u64,
    /// Usage-shaped entries whose counters could not be parsed safely.
    pub invalid_usage_records: usize,
    pub incomplete_tail: bool,
    /// The file has per-response records, so legacy snapshots were not counted.
    pub ignored_legacy_records: usize,
    /// Legacy usage with no matching per-response record in the same rollout.
    pub unmatched_legacy_records: usize,
    /// Forked/resumed legacy files can contain inherited usage records.
    pub forked_or_parented: bool,
}

impl SessionUsage {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            session_id: None,
            creator_account_id: None,
            records: Vec::new(),
            malformed_lines: 0,
            invalid_usage_records: 0,
            incomplete_tail: false,
            ignored_legacy_records: 0,
            unmatched_legacy_records: 0,
            forked_or_parented: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelUsage {
    pub model: String,
    pub responses: usize,
    pub tokens: TokenCounts,
}

#[derive(Debug, Clone, Serialize)]
pub struct TierUsage {
    /// None means the requested service tier could not be reconstructed.
    pub requested_service_tier: Option<String>,
    pub responses: usize,
    pub tokens: TokenCounts,
}

/// Inclusive UTC limits for completed responses. Neither a session's file
/// path nor its creation time determines whether a response is in the range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageTimeRange {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl UsageTimeRange {
    pub fn new(start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Self> {
        if start > end {
            return Err(CasError::Verification(
                "usage range start must not be later than its end".into(),
            ));
        }
        Ok(Self { start, end })
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageReport {
    pub scan_roots: Vec<PathBuf>,
    pub time_range: Option<UsageTimeRange>,
    pub excluded_outside_range: usize,
    /// Untimestamped or malformed-timestamp responses are excluded rather
    /// than guessed into a bounded report.
    pub excluded_without_timestamp: usize,
    pub files_scanned: usize,
    pub files_with_usage: usize,
    pub responses: usize,
    /// Unique tool invocations associated with included, deduplicated token
    /// usage records; outputs and unmatched tool events are not counted.
    pub tool_calls: usize,
    pub duplicate_responses: usize,
    pub ignored_legacy_records: usize,
    pub unmatched_legacy_records: usize,
    pub malformed_lines: u64,
    pub invalid_usage_records: usize,
    pub incomplete_tails: usize,
    pub unknown_model_responses: usize,
    pub invalid_cache_breakdowns: usize,
    pub totals: TokenCounts,
    pub models: Vec<ModelUsage>,
    pub tiers: Vec<TierUsage>,
    pub pricing: PricingSummary,
    pub sessions: Vec<SessionUsage>,
    pub warnings: Vec<String>,
    pub warning_count: usize,
}

impl UsageReport {
    fn warn(&mut self, message: impl Into<String>) {
        self.warning_count += 1;
        if self.warnings.len() < MAX_WARNINGS {
            self.warnings.push(message.into());
        }
    }
}

/// Discover ~/.codex/sessions and ~/.codex/archived_sessions (honoring
/// CODEX_HOME via CasPaths). An explicit JSONL file or directory can override
/// the roots, including installations with a nonstandard archive layout.
pub fn scan_codex_usage(paths: &CasPaths, source: Option<&Path>) -> Result<UsageReport> {
    scan_codex_usage_in_range(paths, source, None)
}

/// Scan the same read-only rollouts, restricting all token and price
/// aggregations to the timestamps of individual completed responses.
pub fn scan_codex_usage_in_range(
    paths: &CasPaths,
    source: Option<&Path>,
    range: Option<UsageTimeRange>,
) -> Result<UsageReport> {
    if range.as_ref().is_some_and(|r| r.start > r.end) {
        return Err(CasError::Verification(
            "usage range start must not be later than its end".into(),
        ));
    }
    let roots = source.map_or_else(
        || {
            vec![
                paths.codex_home.join("sessions"),
                paths.codex_home.join("archived_sessions"),
            ]
        },
        |path| vec![path.to_path_buf()],
    );
    let mut report = UsageReport {
        scan_roots: roots.clone(),
        time_range: range.clone(),
        ..UsageReport::default()
    };
    let mut files = Vec::new();

    for root in &roots {
        if source.is_none()
            && fs::symlink_metadata(root).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            continue;
        }
        if source.is_some()
            && fs::symlink_metadata(root).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return Err(CasError::Verification(format!(
                "Codex JSONL path does not exist: {}",
                root.display()
            )));
        }
        collect_jsonl_files(root, 0, &mut files, &mut report)?;
    }
    files.sort();
    files.dedup();

    let mut seen = HashSet::new();
    let mut seen_tool_ids = HashSet::new();
    let mut by_model: BTreeMap<String, ModelUsage> = BTreeMap::new();
    let mut by_tier: BTreeMap<Option<String>, TierUsage> = BTreeMap::new();
    for path in files {
        report.files_scanned += 1;
        let mut session = match parse_codex_file(&path) {
            Ok(value) => value,
            Err(error) => {
                report.warn(format!("cannot read {}: {error}", path.display()));
                continue;
            }
        };

        report.malformed_lines += session.malformed_lines;
        report.invalid_usage_records += session.invalid_usage_records;
        if session.malformed_lines > 0 {
            report.warn(format!(
                "{}: {} malformed metadata line(s)",
                path.display(),
                session.malformed_lines
            ));
        }
        if session.invalid_usage_records > 0 {
            report.warn(format!(
                "{}: {} usage entry/entries have missing or invalid token counters",
                path.display(),
                session.invalid_usage_records
            ));
        }
        if session.incomplete_tail {
            report.incomplete_tails += 1;
            report.warn(format!("{}: incomplete final JSONL line", path.display()));
        }
        if session.ignored_legacy_records > 0 {
            report.ignored_legacy_records += session.ignored_legacy_records;
            report.unmatched_legacy_records += session.unmatched_legacy_records;
        }
        if session.unmatched_legacy_records > 0 {
            report.warn(format!(
                "{}: {} legacy token snapshot(s) have no matching per-response record and were excluded to avoid double counting; mixed-version history may be incomplete",
                path.display(),
                session.unmatched_legacy_records
            ));
        }
        if session.forked_or_parented
            && session
                .records
                .iter()
                .any(|r| r.source == UsageSource::LegacyTokenCount)
        {
            report.warn(format!(
                "{}: forked/parented legacy rollout may replay earlier usage",
                path.display()
            ));
        }

        if let Some(bounds) = &range {
            session.records.retain(|record| {
                let Some(timestamp) = record
                    .timestamp
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                else {
                    report.excluded_without_timestamp += 1;
                    return false;
                };
                let in_range = timestamp >= bounds.start && timestamp <= bounds.end;
                if !in_range {
                    report.excluded_outside_range += 1;
                }
                in_range
            });
        }

        session.records.retain(|record| {
            if seen.insert(record.dedup_id.clone()) {
                true
            } else {
                report.duplicate_responses += 1;
                false
            }
        });

        let thread = session
            .session_id
            .as_deref()
            .unwrap_or_else(|| path.to_str().unwrap_or("unidentified-rollout"));
        for record in &mut session.records {
            record.tool_calls = record
                .tool_call_ids
                .iter()
                .filter(|call_id| seen_tool_ids.insert(format!("{thread}:{call_id}")))
                .count();
            report.tool_calls += record.tool_calls;
        }

        if !session.records.is_empty() {
            report.files_with_usage += 1;
        }
        let unknown_in_file = session
            .records
            .iter()
            .filter(|record| record.model == "unknown")
            .count();
        report.unknown_model_responses += unknown_in_file;
        if unknown_in_file > 0 {
            report.warn(format!(
                "{}: {} response(s) have no model identifier (cannot be priced)",
                path.display(),
                unknown_in_file
            ));
        }
        let bad_cache_in_file = session
            .records
            .iter()
            .filter(|record| {
                record
                    .tokens
                    .cached_input_tokens
                    .saturating_add(record.tokens.cache_write_input_tokens)
                    > record.tokens.input_tokens
            })
            .count();
        report.invalid_cache_breakdowns += bad_cache_in_file;
        if bad_cache_in_file > 0 {
            report.warn(format!(
                "{}: {} response(s) have cache tokens exceeding total input tokens",
                path.display(),
                bad_cache_in_file
            ));
        }
        for record in &session.records {
            report.responses += 1;
            report.totals.add_assign(&record.tokens);
            let model = by_model
                .entry(record.model.clone())
                .or_insert_with(|| ModelUsage {
                    model: record.model.clone(),
                    responses: 0,
                    tokens: TokenCounts::default(),
                });
            model.responses += 1;
            model.tokens.add_assign(&record.tokens);
            let tier = by_tier
                .entry(record.requested_service_tier.clone())
                .or_insert_with(|| TierUsage {
                    requested_service_tier: record.requested_service_tier.clone(),
                    responses: 0,
                    tokens: TokenCounts::default(),
                });
            tier.responses += 1;
            tier.tokens.add_assign(&record.tokens);
        }
        report.sessions.push(session);
    }
    report.models = by_model.into_values().collect();
    report.tiers = by_tier.into_values().collect();
    if report.excluded_without_timestamp > 0 {
        report.warn(format!(
            "{} usage response(s) without a usable timestamp were excluded from the selected range",
            report.excluded_without_timestamp
        ));
    }
    fill_report_prices(&mut report);
    Ok(report)
}

fn collect_jsonl_files(
    path: &Path,
    depth: usize,
    files: &mut Vec<PathBuf>,
    report: &mut UsageReport,
) -> Result<()> {
    // A top-level sessions directory is often symlinked to another disk.
    // Follow only explicitly discovered/selected roots; nested symlinks are
    // excluded to avoid walking outside the chosen tree or symlink cycles.
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) => {
            report.warn(format!("cannot inspect {}: {error}", path.display()));
            return Ok(());
        }
    };
    if metadata.file_type().is_symlink() {
        if depth == 0 {
            match fs::canonicalize(path) {
                Ok(target) => collect_jsonl_files(&target, depth + 1, files, report)?,
                Err(error) => report.warn(format!("cannot resolve {}: {error}", path.display())),
            }
        } else {
            report.warn(format!("skipped nested symlink: {}", path.display()));
        }
    } else if metadata.is_file() {
        if path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(path.to_path_buf());
        } else if depth == 0 {
            return Err(CasError::Verification(format!(
                "expected a .jsonl file or directory: {}",
                path.display()
            )));
        }
    } else if metadata.is_dir() {
        if depth >= MAX_DIRECTORY_DEPTH {
            report.warn(format!("scan depth limit reached: {}", path.display()));
            return Ok(());
        }
        let entries = match fs::read_dir(path) {
            Ok(value) => value,
            Err(error) => {
                report.warn(format!("cannot list {}: {error}", path.display()));
                return Ok(());
            }
        };
        for entry in entries {
            match entry {
                Ok(value) => collect_jsonl_files(&value.path(), depth + 1, files, report)?,
                Err(error) => {
                    report.warn(format!("cannot list entry in {}: {error}", path.display()))
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawUsage {
    tokens: TokenCounts,
    reported_total: Option<u64>,
}

fn parse_tokens(value: &Value) -> Option<RawUsage> {
    let fields = value.as_object()?;
    // A total_tokens-only snapshot can be an estimated context size, not
    // billable usage; don't invent input/output amounts for it.
    let input = fields.get("input_tokens")?.as_u64()?;
    let output = fields.get("output_tokens")?.as_u64()?;
    let cached = fields
        .get("cached_input_tokens")
        .or_else(|| fields.get("cache_read_input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(RawUsage {
        tokens: TokenCounts {
            input_tokens: input,
            cached_input_tokens: cached,
            cache_write_input_tokens: fields
                .get("cache_write_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: output,
            reasoning_output_tokens: fields
                .get("reasoning_output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        },
        reported_total: fields.get("total_tokens").and_then(Value::as_u64),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LegacySignature {
    total: Option<RawUsage>,
    last: Option<RawUsage>,
}

fn parse_codex_file(path: &Path) -> Result<SessionUsage> {
    let mut session = SessionUsage::new(path);
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    let mut current_model = "unknown".to_owned();
    let mut current_requested_service_tier = None;
    let mut context_window = None;
    let mut modern = Vec::new();
    let mut legacy = Vec::new();
    let mut source_signatures: HashMap<Option<String>, LegacySignature> = HashMap::new();
    let mut previous_signature: Option<LegacySignature> = None;
    let mut high_water: Option<TokenCounts> = None;
    let file_identity = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut legacy_index = 0usize;
    let mut physical_line = 0usize;
    let mut pending_tool_calls: Vec<String> = Vec::new();

    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        physical_line += 1;
        let has_newline = line.ends_with(b"\n");
        // Read tool-call metadata but skip tool outputs and regular messages;
        // tool-call arguments are never inspected or retained.
        let tool_call_item = line
            .windows(b"\"response_item\"".len())
            .any(|window| window == b"\"response_item\"")
            && [
                b"\"function_call\"".as_slice(),
                b"\"custom_tool_call\"",
                b"\"web_search_call\"",
                b"\"file_search_call\"",
                b"\"computer_call\"",
            ]
            .iter()
            .any(|needle| line.windows(needle.len()).any(|window| window == *needle));
        if !tool_call_item
            && ![
                b"\"session_meta\"".as_slice(),
                b"\"turn_context\"",
                b"\"token_count\"",
                b"\"token_usage_record\"",
                b"\"thread_settings_applied\"",
            ]
            .iter()
            .any(|needle| line.windows(needle.len()).any(|window| window == *needle))
        {
            if !has_newline && serde_json::from_slice::<Value>(&line).is_err() {
                session.incomplete_tail = true;
            }
            continue;
        }
        let entry: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(_) if !has_newline => {
                session.incomplete_tail = true;
                break;
            }
            Err(_) => {
                session.malformed_lines += 1;
                continue;
            }
        };
        let event_type = entry.get("type").and_then(Value::as_str);
        let Some(payload) = entry.get("payload") else {
            continue;
        };
        let timestamp = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match event_type {
            Some("response_item")
                if is_tool_call_type(payload.get("type").and_then(Value::as_str)) =>
            {
                let call_id = payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("line:{file_identity}:{physical_line}"));
                pending_tool_calls.push(call_id);
            }
            Some("session_meta") => {
                session.session_id = payload
                    .get("id")
                    .or_else(|| payload.get("session_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(session.session_id);
                session.creator_account_id = payload
                    .get("creator_account_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(session.creator_account_id);
                session.forked_or_parented |=
                    payload.get("forked_from_id").is_some_and(|v| !v.is_null())
                        || payload
                            .get("parent_thread_id")
                            .is_some_and(|v| !v.is_null());
            }
            Some("turn_context") => {
                if let Some(model) = payload.get("model").and_then(Value::as_str) {
                    current_model = model.to_owned();
                }
            }
            Some("thread_settings_applied") | Some("event_msg")
                if event_type == Some("thread_settings_applied")
                    || payload.get("type").and_then(Value::as_str)
                        == Some("thread_settings_applied") =>
            {
                // Current Codex persists ThreadSettingsApplied as an event_msg
                // with nested `thread_settings`. Older files may serialize
                // it as a top-level item; both shapes have the same snapshot.
                let settings = payload.get("thread_settings").unwrap_or(payload);
                if let Some(model) = settings.get("model").and_then(Value::as_str) {
                    current_model = model.to_owned();
                }
                // Even explicit null must clear a previous Fast tier. Never
                // infer priority from the current config.toml: it may have
                // changed since this response was recorded.
                current_requested_service_tier = settings
                    .get("service_tier")
                    .and_then(Value::as_str)
                    .map(normalize_requested_tier);
            }
            Some("token_usage_record") => {
                // A tool invocation belongs to the model response that
                // emitted it. Codex persists the tool-call response_item
                // before the corresponding token_usage_record.
                let call_ids = std::mem::take(&mut pending_tool_calls);
                let Some(usage) = payload.get("usage").and_then(parse_tokens) else {
                    session.invalid_usage_records += 1;
                    continue;
                };
                if usage.tokens.has_usage() {
                    let response_id = payload
                        .get("response_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let turn_id = payload
                        .get("turn_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let thread_id = payload
                        .get("thread_id")
                        .and_then(Value::as_str)
                        .or(session.session_id.as_deref())
                        .unwrap_or(&file_identity);
                    let dedup_id = match response_id.as_deref() {
                        Some(id) if !id.is_empty() => format!("response:{thread_id}:{id}"),
                        _ => format!("line:{file_identity}:{physical_line}"),
                    };
                    modern.push(UsageRecord {
                        source: UsageSource::TokenUsageRecord,
                        model: current_model.clone(),
                        requested_service_tier: current_requested_service_tier.clone(),
                        timestamp,
                        response_id,
                        turn_id,
                        request_context_tokens: Some(usage.tokens.input_tokens),
                        tokens: usage.tokens,
                        tool_calls: 0,
                        tool_call_ids: call_ids,
                        model_context_window: context_window,
                        price: None,
                        dedup_id,
                    });
                }
            }
            Some("event_msg")
                if payload.get("type").and_then(Value::as_str) == Some("token_count") =>
            {
                let Some(info) = payload.get("info").filter(|v| v.is_object()) else {
                    continue;
                };
                if let Some(window) = info.get("model_context_window").and_then(Value::as_u64) {
                    context_window = Some(window);
                    // Usually token_usage_record precedes the corresponding
                    // token_count snapshot. Attribute its capacity only when
                    // the exact per-response token counts agree.
                    if let (Some(last_modern), Some(last_usage)) = (
                        modern.last_mut(),
                        info.get("last_token_usage").and_then(parse_tokens),
                    ) && billable_counts_match(&last_modern.tokens, &last_usage.tokens)
                        && last_modern.model_context_window.is_none()
                    {
                        last_modern.model_context_window = Some(window);
                    }
                }
                let snapshot = LegacySignature {
                    total: info.get("total_token_usage").and_then(parse_tokens),
                    last: info.get("last_token_usage").and_then(parse_tokens),
                };
                let unparsed = ["total_token_usage", "last_token_usage"]
                    .into_iter()
                    .any(|key| {
                        info.get(key)
                            .is_some_and(|entry| !entry.is_null() && parse_tokens(entry).is_none())
                    });
                if unparsed {
                    session.invalid_usage_records += 1;
                }
                if snapshot.total.is_none() && snapshot.last.is_none() {
                    continue;
                }
                let source = payload
                    .pointer("/rate_limits/limit_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let duplicated = snapshot.total.is_some()
                    && (source_signatures.get(&source) == Some(&snapshot)
                        || previous_signature.as_ref() == Some(&snapshot));
                if snapshot.total.is_some() {
                    source_signatures.insert(source, snapshot.clone());
                }
                previous_signature = Some(snapshot.clone());
                let (delta, request_context_tokens) = if duplicated {
                    (None, None)
                } else if let Some(ref last) = snapshot.last {
                    if last.tokens.has_usage() {
                        (Some(last.tokens.clone()), Some(last.tokens.input_tokens))
                    } else {
                        (
                            snapshot.total.as_ref().map(|total| {
                                total.tokens.delta_from(
                                    high_water.as_ref().unwrap_or(&TokenCounts::default()),
                                )
                            }),
                            None,
                        )
                    }
                } else {
                    (
                        snapshot.total.as_ref().map(|total| {
                            total
                                .tokens
                                .delta_from(high_water.as_ref().unwrap_or(&TokenCounts::default()))
                        }),
                        None,
                    )
                };
                if let Some(total) = snapshot.total {
                    high_water
                        .get_or_insert_with(TokenCounts::default)
                        .update_high_water(&total.tokens);
                }
                if let Some(tokens) = delta.filter(TokenCounts::has_usage) {
                    legacy_index += 1;
                    legacy.push(UsageRecord {
                        source: UsageSource::LegacyTokenCount,
                        model: current_model.clone(),
                        requested_service_tier: current_requested_service_tier.clone(),
                        timestamp,
                        response_id: None,
                        turn_id: None,
                        request_context_tokens,
                        tokens,
                        tool_calls: 0,
                        tool_call_ids: std::mem::take(&mut pending_tool_calls),
                        model_context_window: context_window,
                        price: None,
                        dedup_id: format!("legacy:{file_identity}:{legacy_index}"),
                    });
                }
            }
            _ => {}
        }
    }

    if !modern.is_empty() {
        session.ignored_legacy_records = legacy.len();
        // Modern Codex normally emits BOTH formats for the same completed
        // response. Match token signatures as a multiset: this distinguishes
        // expected duplicate legacy events from genuinely unmatched old
        // history without adding the two sources together.
        let mut modern_counts: HashMap<(String, u64, u64, u64, u64), usize> = HashMap::new();
        for record in &modern {
            *modern_counts.entry(usage_fingerprint(record)).or_default() += 1;
        }
        for record in &legacy {
            let remaining = modern_counts.entry(usage_fingerprint(record)).or_default();
            if *remaining > 0 {
                *remaining -= 1;
            } else {
                session.unmatched_legacy_records += 1;
            }
        }
        session.records = modern;
    } else {
        session.records = legacy;
    }
    // Include the logical thread identity in fallback keys: two unrelated
    // sessions can otherwise have the same basename and event ordinal.
    // A copied archive with the same thread ID and filename still deduplicates.
    let identity = session
        .session_id
        .as_deref()
        .unwrap_or_else(|| path.to_str().unwrap_or("unidentified-rollout"));
    for record in &mut session.records {
        if record.source == UsageSource::LegacyTokenCount || record.response_id.is_none() {
            record.dedup_id = format!("{identity}:{}", record.dedup_id);
        }
    }
    Ok(session)
}

fn is_tool_call_type(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some(
            "function_call"
                | "custom_tool_call"
                | "web_search_call"
                | "file_search_call"
                | "computer_call"
        )
    )
}

fn normalize_requested_tier(value: &str) -> String {
    // Codex writes runtime request values as `priority` but older configs and
    // some clients use `fast` for the same routing preference.
    if value.eq_ignore_ascii_case("fast") {
        "priority".to_owned()
    } else {
        value.to_ascii_lowercase()
    }
}

fn billable_counts_match(a: &TokenCounts, b: &TokenCounts) -> bool {
    // Legacy token_count messages may omit reasoning_output_tokens even when
    // the modern per-response record has that breakdown. It is already part
    // of output_tokens, so this is not a separate billing dimension.
    a.input_tokens == b.input_tokens
        && a.cached_input_tokens == b.cached_input_tokens
        && a.cache_write_input_tokens == b.cache_write_input_tokens
        && a.output_tokens == b.output_tokens
}

fn usage_fingerprint(record: &UsageRecord) -> (String, u64, u64, u64, u64) {
    (
        record.model.clone(),
        record.tokens.input_tokens,
        record.tokens.cached_input_tokens,
        record.tokens.cache_write_input_tokens,
        record.tokens.output_tokens,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn write_lines(path: &Path, lines: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = File::create(path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn meta(id: &str) -> Value {
        json!({"type":"session_meta","payload":{"id":id,"creator_account_id":"workspace-a"}})
    }

    fn model(name: &str) -> Value {
        json!({"type":"turn_context","payload":{"model":name}})
    }

    fn settings(model: &str, tier: Option<&str>) -> Value {
        json!({"type":"event_msg","payload":{
            "type":"thread_settings_applied",
            "thread_settings":{"model":model,"service_tier":tier}
        }})
    }

    fn modern(id: &str, input: u64, cached: u64, output: u64) -> Value {
        json!({"type":"token_usage_record","payload":{
            "thread_id":"thread-1", "turn_id":"turn-1", "response_id":id,
            "usage":{"input_tokens":input,"cached_input_tokens":cached,"output_tokens":output,"reasoning_output_tokens":5}
        }})
    }

    fn tool_call(kind: &str, id: &str) -> Value {
        json!({"type":"response_item","payload":{
            "type":kind,"call_id":id,"name":"functions.exec","input":"ignored"
        }})
    }

    fn legacy(total: (u64, u64, u64), last: (u64, u64, u64), limit_id: &str) -> Value {
        json!({"type":"event_msg","payload":{
            "type":"token_count", "rate_limits":{"limit_id":limit_id},
            "info":{"total_token_usage":{
                "input_tokens":total.0,"cached_input_tokens":total.1,"output_tokens":total.2
            },"last_token_usage":{
                "input_tokens":last.0,"cached_input_tokens":last.1,"output_tokens":last.2
            }}
        }})
    }

    fn paths(dir: &tempfile::TempDir) -> CasPaths {
        CasPaths::from_codex_home(dir.path().join(".codex"))
    }

    #[test]
    fn detects_standard_and_archived_layout_without_reading_other_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let active = paths
            .codex_home
            .join("sessions/2026/10/08/rollout-one.jsonl");
        let archived = paths.codex_home.join("archived_sessions/rollout-two.jsonl");
        write_lines(
            &active,
            &[meta("one"), model("gpt-6-sol"), modern("r1", 100, 25, 12)],
        );
        write_lines(
            &archived,
            &[meta("two"), model("gpt-6-sol"), modern("r2", 50, 10, 7)],
        );
        write_lines(
            &paths.codex_home.join("history.jsonl"),
            &[modern("r3", 500, 0, 100)],
        );
        let report = scan_codex_usage(&paths, None).unwrap();
        assert_eq!(report.files_scanned, 2);
        assert_eq!(report.responses, 2);
        assert_eq!(report.totals.input_tokens, 150);
        assert_eq!(report.totals.cached_input_tokens, 35);
        assert_eq!(report.totals.fresh_input_tokens(), 115);
        assert_eq!(report.models[0].model, "gpt-6-sol");
        assert_eq!(
            report.sessions[0].creator_account_id.as_deref(),
            Some("workspace-a")
        );
    }

    #[test]
    fn modern_record_wins_over_duplicate_legacy_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("session.jsonl");
        write_lines(
            &file,
            &[
                meta("one"),
                model("gpt-6-sol"),
                modern("r1", 130, 100, 40),
                legacy((130, 100, 40), (130, 100, 40), "one"),
            ],
        );
        let report = scan_codex_usage(&paths(&dir), Some(&file)).unwrap();
        assert_eq!(report.responses, 1);
        assert_eq!(report.ignored_legacy_records, 1);
        assert_eq!(report.unmatched_legacy_records, 0);
        assert_eq!(report.warning_count, 0);
        assert_eq!(report.totals.total_tokens(), 170);
        assert_eq!(report.totals.reasoning_output_tokens, 5);
        assert_eq!(
            report.sessions[0].records[0].request_context_tokens,
            Some(130)
        );
    }

    #[test]
    fn tool_averages_count_calls_not_outputs_and_ignore_orphan_calls() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tools.jsonl");
        write_lines(
            &file,
            &[
                meta("one"),
                model("gpt-6-sol"),
                tool_call("custom_tool_call", "call-one"),
                tool_call("function_call", "call-two"),
                json!({"type":"response_item","payload":{
                    "type":"custom_tool_call_output","call_id":"call-one","output":"ignored"
                }}),
                modern("resp1", 100, 80, 12),
                tool_call("custom_tool_call", "call-three"),
                modern("resp2", 200, 100, 8),
                tool_call("custom_tool_call", "orphan-without-token-record"),
            ],
        );
        let report = scan_codex_usage(&paths(&dir), Some(&file)).unwrap();
        assert_eq!(report.responses, 2);
        assert_eq!(report.tool_calls, 3);
        assert_eq!(report.sessions[0].records[0].tool_calls, 2);
        assert_eq!(report.sessions[0].records[1].tool_calls, 1);
        assert_eq!(report.totals.input_tokens, 300);
        assert_eq!(report.totals.cached_input_tokens, 180);
        assert_eq!(report.totals.output_tokens, 20);
    }

    #[test]
    fn tool_calls_are_deduplicated_across_archives_and_within_responses() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let originals = [
            meta("one"),
            model("gpt-6-sol"),
            tool_call("custom_tool_call", "same-tool"),
            tool_call("custom_tool_call", "same-tool"),
            modern("same-response", 100, 10, 2),
        ];
        write_lines(
            &paths.codex_home.join("sessions/2026/10/08/orig.jsonl"),
            &originals,
        );
        write_lines(
            &paths.codex_home.join("archived_sessions/orig.jsonl"),
            &originals,
        );
        let report = scan_codex_usage(&paths, None).unwrap();
        assert_eq!(report.responses, 1);
        assert_eq!(report.duplicate_responses, 1);
        assert_eq!(report.tool_calls, 1);
    }

    #[test]
    fn tool_count_and_token_total_use_the_same_response_time_filter() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("timed-tools.jsonl");
        let stamped = |mut value: Value, time: &str| {
            value["timestamp"] = json!(time);
            value
        };
        write_lines(
            &file,
            &[
                meta("one"),
                model("gpt-6-sol"),
                stamped(tool_call("custom_tool_call", "old"), "2026-10-07T21:00:00Z"),
                stamped(modern("old", 100, 90, 5), "2026-10-07T21:00:01Z"),
                stamped(
                    tool_call("custom_tool_call", "today"),
                    "2026-10-08T08:00:00Z",
                ),
                stamped(modern("today", 200, 150, 6), "2026-10-08T08:00:01Z"),
            ],
        );
        let bounds = UsageTimeRange::new(
            DateTime::parse_from_rfc3339("2026-10-08T00:00:00Z")
                .unwrap()
                .to_utc(),
            DateTime::parse_from_rfc3339("2026-10-09T00:00:00Z")
                .unwrap()
                .to_utc(),
        )
        .unwrap();
        let report = scan_codex_usage_in_range(&paths(&dir), Some(&file), Some(bounds)).unwrap();
        assert_eq!(report.responses, 1);
        assert_eq!(report.tool_calls, 1);
        assert_eq!(report.totals.input_tokens, 200);
        assert_eq!(report.totals.cached_input_tokens, 150);
        assert_eq!(report.totals.output_tokens, 6);
        assert_eq!(report.excluded_outside_range, 1);
    }

    #[test]
    fn requested_tier_and_model_follow_nested_thread_settings_by_response() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiers.jsonl");
        write_lines(
            &path,
            &[
                meta("one"),
                settings("gpt-6-sol", Some("priority")),
                modern("fast1", 280_100, 80_000, 35),
                settings("gpt-6-astra", Some("default")),
                modern("standard", 28_000, 3_000, 25),
                settings("gpt-6-astra", Some("fast")),
                modern("fast2", 4_000, 1_000, 15),
                settings("gpt-6-astra", None),
                modern("unknown", 800, 400, 10),
            ],
        );
        let report = scan_codex_usage(&paths(&dir), Some(&path)).unwrap();
        let records = &report.sessions[0].records;
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].model, "gpt-6-sol");
        assert_eq!(
            records[0].requested_service_tier.as_deref(),
            Some("priority")
        );
        assert_eq!(records[0].request_context_tokens, Some(280_100));
        assert_eq!(records[0].tokens.cached_input_tokens, 80_000);
        assert!(records[0].request_context_tokens.unwrap() > 272_000);
        assert_eq!(records[1].model, "gpt-6-astra");
        assert_eq!(
            records[1].requested_service_tier.as_deref(),
            Some("default")
        );
        assert_eq!(
            records[2].requested_service_tier.as_deref(),
            Some("priority")
        );
        assert_eq!(records[3].requested_service_tier, None);
        assert_eq!(report.tiers.iter().map(|x| x.responses).sum::<usize>(), 4);
        assert_eq!(
            report
                .tiers
                .iter()
                .find(|x| x.requested_service_tier.as_deref() == Some("priority"))
                .unwrap()
                .responses,
            2
        );
    }

    #[test]
    fn legacy_per_request_usage_can_recover_input_context_but_cumulative_cannot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.jsonl");
        write_lines(
            &path,
            &[
                settings("gpt-5", Some("priority")),
                legacy((300_000, 200_000, 30), (300_000, 200_000, 30), "tier"),
                json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":310_000,"output_tokens":50}}}}),
            ],
        );
        let report = scan_codex_usage(&paths(&dir), Some(&path)).unwrap();
        assert_eq!(report.responses, 2);
        let records = &report.sessions[0].records;
        assert_eq!(records[0].request_context_tokens, Some(300_000));
        assert_eq!(records[1].request_context_tokens, None);
        assert!(
            records
                .iter()
                .all(|r| r.requested_service_tier.as_deref() == Some("priority"))
        );
    }

    #[test]
    fn following_token_snapshot_sets_capacity_without_changing_request_context_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contexts.jsonl");
        write_lines(
            &path,
            &[
                settings("gpt-6-sol", Some("default")),
                modern("response", 120_000, 92_000, 450),
                json!({"type":"event_msg","payload":{
                    "type":"token_count","info":{
                        "model_context_window":400_000,
                        "total_token_usage":{"input_tokens":120_000,"cached_input_tokens":92_000,"output_tokens":450},
                        "last_token_usage":{"input_tokens":120_000,"cached_input_tokens":92_000,"output_tokens":450}
                    }
                }}),
            ],
        );
        let report = scan_codex_usage(&paths(&dir), Some(&path)).unwrap();
        let record = &report.sessions[0].records[0];
        assert_eq!(record.request_context_tokens, Some(120_000));
        assert_eq!(record.model_context_window, Some(400_000));
        assert_eq!(report.ignored_legacy_records, 1);
    }

    #[test]
    fn legacy_counts_exact_last_usage_and_skips_replayed_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("legacy.jsonl");
        write_lines(
            &file,
            &[
                meta("one"),
                model("gpt-5"),
                legacy((100, 20, 10), (100, 20, 10), "bucket1"),
                legacy((100, 20, 10), (100, 20, 10), "bucket2"),
                legacy((200, 40, 30), (100, 20, 20), "bucket1"),
            ],
        );
        let report = scan_codex_usage(&paths(&dir), Some(&file)).unwrap();
        assert_eq!(report.responses, 2);
        assert_eq!(report.totals.input_tokens, 200);
        assert_eq!(report.totals.cached_input_tokens, 40);
        assert_eq!(report.totals.output_tokens, 30);
        assert_eq!(report.models[0].model, "gpt-5");
    }

    #[test]
    fn mixed_legacy_history_is_flagged_instead_of_silently_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mixed.jsonl");
        write_lines(
            &file,
            &[
                model("gpt-6-sol"),
                legacy((10, 0, 2), (10, 0, 2), "bucket1"),
                modern("new", 20, 0, 4),
                legacy((30, 0, 6), (20, 0, 4), "bucket1"),
            ],
        );
        let report = scan_codex_usage(&paths(&dir), Some(&file)).unwrap();
        assert_eq!(report.responses, 1);
        assert_eq!(report.totals.input_tokens, 20);
        assert_eq!(report.ignored_legacy_records, 2);
        assert_eq!(report.unmatched_legacy_records, 1);
        assert_eq!(report.warning_count, 1);
    }

    #[test]
    fn legacy_total_only_uses_deltas_and_high_water() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("legacy.jsonl");
        write_lines(
            &file,
            &[
                model("gpt-5"),
                json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":150,"output_tokens":20}}}}),
                json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":190,"output_tokens":30}}}}),
            ],
        );
        let report = scan_codex_usage(&paths(&dir), Some(&file)).unwrap();
        assert_eq!(report.responses, 2);
        assert_eq!(report.totals.input_tokens, 190);
        assert_eq!(report.totals.output_tokens, 30);
    }

    #[test]
    fn skips_malformed_line_and_incomplete_live_tail() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("live.jsonl");
        write_lines(&file, &[model("gpt-6-sol"), modern("r1", 100, 10, 15)]);
        let mut handle = fs::OpenOptions::new().append(true).open(&file).unwrap();
        writeln!(handle, "{{\"type\":\"token_usage_record\",BAD}}").unwrap();
        write!(handle, "{{\"type\":\"token_usage_record\",\"payload\":").unwrap();
        let report = scan_codex_usage(&paths(&dir), Some(&file)).unwrap();
        assert_eq!(report.responses, 1);
        assert_eq!(report.malformed_lines, 1);
        assert_eq!(report.incomplete_tails, 1);
        assert!(report.warning_count >= 2);
    }

    #[test]
    fn reports_invalid_usage_and_unknown_models() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("partial.jsonl");
        write_lines(
            &file,
            &[
                modern("valid", 20, 0, 5),
                json!({"type":"token_usage_record","payload":{"response_id":"invalid","usage":{"input_tokens":30}}}),
            ],
        );
        let report = scan_codex_usage(&paths(&dir), Some(&file)).unwrap();
        assert_eq!(report.responses, 1);
        assert_eq!(report.invalid_usage_records, 1);
        assert_eq!(report.unknown_model_responses, 1);
        assert_eq!(report.warning_count, 2);
    }

    #[test]
    fn deduplicates_same_response_across_active_and_archived_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let active = paths.codex_home.join("sessions/2026/10/08/a.jsonl");
        let archived = paths.codex_home.join("archived_sessions/b.jsonl");
        for file in [&active, &archived] {
            write_lines(
                file,
                &[meta("one"), model("gpt-6-sol"), modern("r1", 100, 0, 25)],
            );
        }
        let report = scan_codex_usage(&paths, None).unwrap();
        assert_eq!(report.responses, 1);
        assert_eq!(report.duplicate_responses, 1);
    }

    #[test]
    fn rejects_missing_explicit_path_but_empty_default_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let missing = dir.path().join("missing.jsonl");
        assert!(scan_codex_usage(&paths, Some(&missing)).is_err());
        assert_eq!(scan_codex_usage(&paths, None).unwrap().responses, 0);
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_directory_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let outside = dir.path().join("outside");
        let sessions = paths.codex_home.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_lines(&outside.join("secrets.jsonl"), &[modern("r1", 10, 0, 2)]);
        std::os::unix::fs::symlink(&outside, sessions.join("linked")).unwrap();
        let report = scan_codex_usage(&paths, None).unwrap();
        assert_eq!(report.responses, 0);
        assert_eq!(report.warning_count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn follows_explicitly_selected_symlink_root() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let outside = dir.path().join("offloaded-sessions");
        write_lines(
            &outside.join("rollout.jsonl"),
            &[model("gpt-6-sol"), modern("r1", 10, 0, 2)],
        );
        fs::create_dir_all(&paths.codex_home).unwrap();
        std::os::unix::fs::symlink(&outside, paths.codex_home.join("sessions")).unwrap();
        let report = scan_codex_usage(&paths, None).unwrap();
        assert_eq!(report.responses, 1);
        assert_eq!(report.warning_count, 0);
    }

    #[test]
    fn legacy_same_filename_but_different_threads_are_not_merged() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        for (which, id) in [("2026/10/07", "first"), ("2026/10/08", "second")] {
            write_lines(
                &paths
                    .codex_home
                    .join(format!("sessions/{which}/same.jsonl")),
                &[meta(id), legacy((40, 0, 5), (40, 0, 5), "bucket")],
            );
        }
        let report = scan_codex_usage(&paths, None).unwrap();
        assert_eq!(report.responses, 2);
        assert_eq!(report.duplicate_responses, 0);
    }

    #[test]
    fn price_only_exact_ids_and_preserve_unpriced_requests() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("model-prices.jsonl");
        write_lines(
            &file,
            &[
                settings("gpt-5.2-2025-12-11", Some("default")),
                json!({"type":"token_usage_record","payload":{
                    "thread_id":"test-thread","turn_id":"turn-one","response_id":"old",
                    "usage":{"input_tokens":1_000_000,"cached_input_tokens":600_000,"cache_write_input_tokens":0,"output_tokens":100_000}
                }}),
                settings("gpt-6.1-sol-unreleased", Some("priority")),
                modern("not-priced", 100_000, 80_000, 9_000),
            ],
        );
        let report = scan_codex_usage(&paths(&dir), Some(&file)).unwrap();
        assert_eq!(report.responses, 2);
        assert_eq!(report.pricing.priced_responses, 1);
        assert_eq!(report.pricing.unpriced_responses, 1);
        assert_eq!(report.pricing.standard_usd, "2.205");
        assert_eq!(report.pricing.unpriced[0].model, "gpt-6.1-sol-unreleased");
        let records = &report.sessions[0].records;
        assert_eq!(records[0].price.as_ref().unwrap().price_model_id, "gpt-5.2");
        assert!(!records[0].price.as_ref().unwrap().long_context_surcharge);
        assert!(records[1].price.is_none());
    }

    #[test]
    fn timestamp_filter_includes_boundaries_and_converts_time_zones() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("one-long-running-session.jsonl");
        let stamped = |mut record: Value, timestamp: &str| {
            record["timestamp"] = json!(timestamp);
            record
        };
        write_lines(
            &file,
            &[
                meta("one"),
                model("gpt-6-sol"),
                stamped(modern("before", 100, 10, 1), "2026-10-07T23:59:59Z"),
                stamped(modern("start", 200, 20, 2), "2026-10-08T08:00:00+08:00"),
                stamped(modern("end", 300, 30, 3), "2026-10-08T01:00:00Z"),
                stamped(modern("after", 400, 40, 4), "2026-10-08T01:00:01Z"),
                stamped(modern("bad", 500, 50, 5), "not-a-date"),
                modern("missing", 600, 60, 6),
            ],
        );
        let utc = |stamp: &str| DateTime::parse_from_rfc3339(stamp).unwrap().to_utc();
        let range =
            UsageTimeRange::new(utc("2026-10-08T00:00:00Z"), utc("2026-10-08T01:00:00Z")).unwrap();
        let report =
            scan_codex_usage_in_range(&paths(&dir), Some(&file), Some(range.clone())).unwrap();
        assert_eq!(report.time_range, Some(range));
        assert_eq!(report.files_scanned, 1);
        assert_eq!(report.files_with_usage, 1);
        assert_eq!(report.responses, 2);
        assert_eq!(report.totals.input_tokens, 500);
        assert_eq!(report.excluded_outside_range, 2);
        assert_eq!(report.excluded_without_timestamp, 2);
        assert_eq!(report.warning_count, 1);
        assert_eq!(report.pricing.priced_responses, 2);
        assert!(
            report.sessions[0]
                .records
                .iter()
                .all(|r| matches!(r.response_id.as_deref(), Some("start" | "end")))
        );
    }

    #[test]
    fn timestamps_do_not_change_all_time_scans_and_invalid_ranges_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("timeless.jsonl");
        write_lines(
            &file,
            &[model("gpt-6-sol"), modern("missing-time", 10, 0, 2)],
        );
        let paths = paths(&dir);
        let entire_history = scan_codex_usage(&paths, Some(&file)).unwrap();
        assert_eq!(entire_history.responses, 1);
        assert_eq!(entire_history.excluded_without_timestamp, 0);
        assert!(entire_history.time_range.is_none());
        let from = DateTime::parse_from_rfc3339("2026-10-08T00:00:00Z")
            .unwrap()
            .to_utc();
        let until = DateTime::parse_from_rfc3339("2026-10-07T00:00:00Z")
            .unwrap()
            .to_utc();
        assert!(UsageTimeRange::new(from, until).is_err());
        let report = scan_codex_usage_in_range(
            &paths,
            Some(&file),
            Some(UsageTimeRange::new(until, from).unwrap()),
        )
        .unwrap();
        assert_eq!(report.responses, 0);
        assert_eq!(report.pricing.standard_usd, "0");
        assert_eq!(report.excluded_without_timestamp, 1);
        assert_eq!(report.files_with_usage, 0);
    }
}
