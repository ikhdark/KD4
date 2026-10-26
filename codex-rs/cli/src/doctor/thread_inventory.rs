//! Doctor check that compares rollout files against the SQLite thread inventory.

use super::CheckStatus;
use super::Config;
use super::DoctorCheck;
use super::DoctorIssue;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_rollout::RolloutRecorder;
use codex_state::ThreadStateAuditRow;
use codex_utils_absolute_path::normalize_for_path_comparison;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;

const MAX_PARITY_SCAN_FILES: usize = 10_000;
const MAX_PARITY_SCAN_ENTRIES: usize = 50_000;
const SAMPLE_LIMIT: usize = 5;
const SUMMARY_LIMIT: usize = 8;
const CHECK_ID: &str = "state.rollout_db_parity";
const CHECK_CATEGORY: &str = "threads";

#[derive(Clone, Debug)]
struct RolloutAuditFile {
    path: PathBuf,
    key: PathBuf,
    archived: bool,
    thread_id: String,
}

#[derive(Default)]
struct RolloutScan {
    files: Vec<RolloutAuditFile>,
    visited_entries: usize,
    scan_errors: Vec<String>,
    malformed_names: Vec<PathBuf>,
    reached_scan_cap: bool,
}

enum RolloutThreadId {
    Id(String),
    MalformedName,
    Unusable(String),
}

impl RolloutScan {
    fn candidate_count(&self) -> usize {
        self.files.len() + self.malformed_names.len() + self.scan_errors.len()
    }

    fn reached_candidate_cap(&self) -> bool {
        self.candidate_count() >= MAX_PARITY_SCAN_FILES
    }

    fn record_malformed_name(&mut self, path: PathBuf) {
        if self.reached_candidate_cap() {
            self.reached_scan_cap = true;
            return;
        }
        self.malformed_names.push(path);
        self.reached_scan_cap = self.reached_candidate_cap();
    }

    fn record_scan_error(&mut self, message: String) {
        if self.reached_candidate_cap() {
            self.reached_scan_cap = true;
            return;
        }
        self.scan_errors.push(message);
        self.reached_scan_cap = self.reached_candidate_cap();
    }

    fn active_count(&self) -> usize {
        self.files.iter().filter(|file| !file.archived).count()
    }

    fn archived_count(&self) -> usize {
        self.files.iter().filter(|file| file.archived).count()
    }
}

pub(super) async fn thread_inventory_check(config: &Config) -> DoctorCheck {
    thread_inventory_check_for_roots(
        config.codex_home.as_path(),
        config.sqlite_home.as_path(),
        config.model_provider_id.as_str(),
    )
    .await
}

async fn thread_inventory_check_for_roots(
    codex_home: &Path,
    sqlite_home: &Path,
    default_provider: &str,
) -> DoctorCheck {
    let scan = scan_rollout_files(codex_home).await;
    let state_db_path = codex_state::state_db_path(sqlite_home);

    let mut details = vec![
        format!("default model provider: {default_provider}"),
        format!("rollout DB active files: {}", scan.active_count()),
        format!("rollout DB archived files: {}", scan.archived_count()),
        format!("rollout DB scan errors: {}", scan.scan_errors.len()),
        format!(
            "rollout DB malformed file names: {}",
            scan.malformed_names.len()
        ),
        format!("rollout DB scan cap reached: {}", scan.reached_scan_cap),
    ];
    push_samples(
        &mut details,
        "rollout DB scan error sample",
        scan.scan_errors.iter().map(String::as_str),
    );
    push_samples(
        &mut details,
        "rollout DB malformed file sample",
        scan.malformed_names
            .iter()
            .map(|path| path.display().to_string()),
    );

    if !state_db_path.is_file() {
        details.push("rollout DB rows: skipped (state DB missing)".to_string());
        return missing_state_db_check(scan, details);
    }

    let rows = match codex_state::read_thread_state_audit_rows(&state_db_path).await {
        Ok(rows) => rows,
        Err(err) => {
            details.push(format!("rollout DB read error: {err}"));
            return DoctorCheck::new(
                CHECK_ID,
                CHECK_CATEGORY,
                CheckStatus::Warning,
                "state database thread inventory could not be read",
            )
            .details(details)
            .issue(
                DoctorIssue::new(
                    CheckStatus::Warning,
                    "state DB thread rows could not be queried",
                )
                .measured(err.to_string())
                .expected("readable threads table"),
            );
        }
    };

    let codex_home = codex_home.to_path_buf();
    tokio::task::spawn_blocking(move || {
        parity_check_from_scan_and_rows(&codex_home, scan, rows, details)
    })
    .await
    .unwrap_or_else(|err| {
        DoctorCheck::new(
            CHECK_ID,
            CHECK_CATEGORY,
            CheckStatus::Warning,
            "rollout/state DB comparison is incomplete",
        )
        .detail(format!("comparison error: {err}"))
    })
}

fn missing_state_db_check(scan: RolloutScan, details: Vec<String>) -> DoctorCheck {
    if scan.files.is_empty()
        && scan.scan_errors.is_empty()
        && scan.malformed_names.is_empty()
        && !scan.reached_scan_cap
    {
        return DoctorCheck::new(
            CHECK_ID,
            CHECK_CATEGORY,
            CheckStatus::Ok,
            "no rollout/state DB inventory to compare",
        )
        .details(details);
    }

    let summary = if scan.files.is_empty() {
        "rollout scan was incomplete or found bad files"
    } else {
        "state DB is missing while rollout files exist"
    };
    let mut check =
        DoctorCheck::new(CHECK_ID, CHECK_CATEGORY, CheckStatus::Warning, summary).details(details);

    if !scan.files.is_empty() {
        check = check
            .issue(
                DoctorIssue::new(
                    CheckStatus::Warning,
                    "rollout files exist but the state DB is missing",
                )
                .measured(format!("{} rollout files", scan.files.len()))
                .expected("state DB contains matching thread rows")
                .remedy("Start Codex with no state DB present so startup backfill can create it from rollout files."),
        )
            .remediation(
                "Start Codex with no state DB present so startup backfill can create it from rollout files.",
            );
    }
    if !scan.scan_errors.is_empty() || !scan.malformed_names.is_empty() || scan.reached_scan_cap {
        check = check.issue(
            DoctorIssue::new(
                CheckStatus::Warning,
                "rollout scan was incomplete or found bad files",
            )
            .measured(format!(
                "{} scan errors, {} malformed names, scan cap reached: {}",
                scan.scan_errors.len(),
                scan.malformed_names.len(),
                scan.reached_scan_cap
            ))
            .expected("rollout directories are fully scannable")
            .remedy("Check file permissions and unexpected files under CODEX_HOME sessions."),
        );
    }
    check
}

fn parity_check_from_scan_and_rows(
    codex_home: &Path,
    scan: RolloutScan,
    rows: Vec<ThreadStateAuditRow>,
    mut details: Vec<String>,
) -> DoctorCheck {
    let rollout_by_key = scan
        .files
        .iter()
        .map(|file| (file.key.clone(), file))
        .collect::<HashMap<_, _>>();
    let row_keys = rows
        .iter()
        .map(|row| path_key(&row.rollout_path))
        .collect::<Vec<_>>();
    let mut rows_by_key: HashMap<PathBuf, Vec<&ThreadStateAuditRow>> = HashMap::new();
    for (row, key) in rows.iter().zip(&row_keys) {
        rows_by_key.entry(key.clone()).or_default().push(row);
    }
    let missing_active = missing_rollout_paths(&scan.files, &rows_by_key, false);
    let missing_archived = missing_rollout_paths(&scan.files, &rows_by_key, true);
    let scan_complete = !scan.reached_scan_cap && scan.scan_errors.is_empty();
    let mut stale_rows = Vec::new();
    let mut unverified_rows = Vec::new();
    let mut archive_mismatches = Vec::new();
    let active_root = path_key(&codex_home.join("sessions"));
    let archived_root = path_key(&codex_home.join("archived_sessions"));
    for (row, key) in rows.iter().zip(&row_keys) {
        if let Some(file) = rollout_by_key.get(key) {
            if row.id != file.thread_id {
                stale_rows.push(row);
            }
            if row.archived != file.archived {
                archive_mismatches.push(row);
            }
        } else if scan_complete {
            if key.starts_with(&active_root)
                || key.starts_with(&archived_root)
                || !row.rollout_path.is_file()
            {
                stale_rows.push(row);
            } else {
                // External references were not scanned; existence alone cannot prove identity.
                unverified_rows.push(row);
            }
        }
    }
    let duplicate_rollout_thread_ids = duplicate_rollout_thread_ids(&scan.files);
    let duplicate_db_paths = duplicate_db_paths(&rows_by_key);
    let archived_rows = rows.iter().filter(|row| row.archived).count();
    let active_rows = rows.len() - archived_rows;

    details.extend([
        format!("rollout DB rows: {}", rows.len()),
        format!("rollout DB unverified rows: {}", unverified_rows.len()),
        format!("rollout DB active rows: {active_rows}"),
        format!("rollout DB archived rows: {archived_rows}"),
        format!("rollout DB missing active rows: {}", missing_active.len()),
        format!(
            "rollout DB missing archived rows: {}",
            missing_archived.len()
        ),
        format!(
            "rollout DB stale rows: {}",
            count_or_skipped(stale_rows.len(), scan_complete)
        ),
        format!(
            "rollout DB archive mismatches: {}",
            count_or_skipped(archive_mismatches.len(), scan_complete)
        ),
        format!(
            "rollout DB duplicate rollout thread ids: {}",
            duplicate_rollout_thread_ids.len()
        ),
        format!(
            "rollout DB duplicate DB paths: {}",
            duplicate_db_paths.len()
        ),
        format!(
            "rollout DB model providers: {}",
            count_summary(rows.iter().map(|row| row.model_provider.as_str()))
        ),
        format!(
            "rollout DB sources: {}",
            count_summary(rows.iter().map(|row| source_category(&row.source)))
        ),
    ]);
    push_path_samples(
        &mut details,
        "rollout DB missing active sample",
        missing_active.iter().copied(),
    );
    push_path_samples(
        &mut details,
        "rollout DB missing archived sample",
        missing_archived.iter().copied(),
    );
    push_path_samples(
        &mut details,
        "rollout DB stale row sample",
        stale_rows.iter().map(|row| row.rollout_path.as_path()),
    );
    push_path_samples(
        &mut details,
        "rollout DB archive mismatch sample",
        archive_mismatches
            .iter()
            .map(|row| row.rollout_path.as_path()),
    );
    push_samples(
        &mut details,
        "rollout DB duplicate rollout thread id sample",
        duplicate_rollout_thread_ids.iter().map(String::as_str),
    );
    push_path_samples(
        &mut details,
        "rollout DB duplicate DB path sample",
        duplicate_db_paths.iter().map(PathBuf::as_path),
    );

    let status = if scan.scan_errors.is_empty()
        && scan.malformed_names.is_empty()
        && !scan.reached_scan_cap
        && missing_active.is_empty()
        && missing_archived.is_empty()
        && unverified_rows.is_empty()
        && stale_rows.is_empty()
        && archive_mismatches.is_empty()
        && duplicate_rollout_thread_ids.is_empty()
        && duplicate_db_paths.is_empty()
    {
        CheckStatus::Ok
    } else {
        CheckStatus::Warning
    };

    let summary = if status == CheckStatus::Ok {
        "rollout files and state DB thread inventory agree"
    } else if !missing_active.is_empty()
        || !missing_archived.is_empty()
        || !stale_rows.is_empty()
        || !archive_mismatches.is_empty()
        || !duplicate_rollout_thread_ids.is_empty()
        || !duplicate_db_paths.is_empty()
    {
        "rollout files and state DB thread inventory differ"
    } else {
        "rollout/state DB comparison is incomplete"
    };
    let mut check = DoctorCheck::new(CHECK_ID, CHECK_CATEGORY, status, summary).details(details);

    if !missing_active.is_empty() || !missing_archived.is_empty() {
        check = check.issue(
            DoctorIssue::new(
                CheckStatus::Warning,
                "rollout files are missing from the state DB",
            )
            .measured(format!(
                "{} active, {} archived",
                missing_active.len(),
                missing_archived.len()
            ))
            .expected("every rollout file has a matching threads row"),
        );
    }
    if !unverified_rows.is_empty() {
        check = check.issue(
            DoctorIssue::new(
                CheckStatus::Warning,
                "state DB references outside the scanned rollout roots are unverified",
            )
            .measured(format!("{} unverified rows", unverified_rows.len()))
            .expected("verified rollout identity for each state DB reference"),
        );
    }
    if !stale_rows.is_empty() {
        check = check.issue(
            DoctorIssue::new(
                CheckStatus::Warning,
                "state DB rows point at missing or unusable rollout files",
            )
            .measured(format!("{} stale rows", stale_rows.len()))
            .expected("every state DB rollout path contains the matching thread identity"),
        );
    }
    if !archive_mismatches.is_empty() {
        check = check.issue(
            DoctorIssue::new(
                CheckStatus::Warning,
                "state DB archive flags disagree with rollout file locations",
            )
            .measured(format!("{} mismatched rows", archive_mismatches.len()))
            .expected(
                "rows under archived_sessions are archived and rows under sessions are active",
            ),
        );
    }
    if !duplicate_rollout_thread_ids.is_empty() || !duplicate_db_paths.is_empty() {
        check = check.issue(
            DoctorIssue::new(
                CheckStatus::Warning,
                "duplicate thread inventory entries found",
            )
            .measured(format!(
                "{} duplicate rollout thread ids, {} duplicate DB paths",
                duplicate_rollout_thread_ids.len(),
                duplicate_db_paths.len()
            ))
            .expected("one rollout path and thread id per thread")
            .remedy("Attach the doctor report to a bug report so support can inspect samples."),
        );
    }
    if !scan.scan_errors.is_empty() || !scan.malformed_names.is_empty() || scan.reached_scan_cap {
        check = check.issue(
            DoctorIssue::new(
                CheckStatus::Warning,
                "rollout scan was incomplete or found bad files",
            )
            .measured(format!(
                "{} scan errors, {} malformed names, scan cap reached: {}",
                scan.scan_errors.len(),
                scan.malformed_names.len(),
                scan.reached_scan_cap
            ))
            .expected("rollout directories are fully scannable")
            .remedy("Check file permissions and unexpected files under CODEX_HOME sessions."),
        );
    }
    check
}

async fn scan_rollout_files(codex_home: &Path) -> RolloutScan {
    let mut scan = RolloutScan::default();
    scan_rollout_root(
        &codex_home.join("sessions"),
        /*archived*/ false,
        &mut scan,
    )
    .await;
    scan_rollout_root(
        &codex_home.join("archived_sessions"),
        /*archived*/ true,
        &mut scan,
    )
    .await;
    scan
}

async fn scan_rollout_root(root: &Path, archived: bool, scan: &mut RolloutScan) {
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        if scan.reached_scan_cap {
            return;
        }
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                scan.record_scan_error(format!("{} ({err})", dir.display()));
                continue;
            }
        };
        loop {
            if scan.reached_scan_cap {
                return;
            }
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(err) => {
                    scan.record_scan_error(format!("{} ({err})", dir.display()));
                    continue;
                }
            };
            if scan.visited_entries >= MAX_PARITY_SCAN_ENTRIES {
                scan.reached_scan_cap = true;
                return;
            }
            scan.visited_entries += 1;
            let path = entry.path();
            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                Err(err) => {
                    scan.record_scan_error(format!("{} ({err})", path.display()));
                    continue;
                }
            };
            if file_type.is_dir() {
                dirs.push(path);
                continue;
            }
            if !file_type.is_file() || !is_rollout_file(&path) {
                continue;
            }
            if scan.candidate_count() >= MAX_PARITY_SCAN_FILES {
                scan.reached_scan_cap = true;
                return;
            }
            let thread_id = match thread_id_from_rollout(&path).await {
                RolloutThreadId::Id(thread_id) => thread_id,
                RolloutThreadId::MalformedName => {
                    scan.record_malformed_name(path.clone());
                    continue;
                }
                RolloutThreadId::Unusable(reason) => {
                    scan.record_scan_error(format!("{} ({reason})", path.display()));
                    continue;
                }
            };
            scan.files.push(RolloutAuditFile {
                key: path_key(&path),
                path,
                archived,
                thread_id,
            });
        }
    }
}

async fn thread_id_from_rollout(path: &Path) -> RolloutThreadId {
    // Identity comes from the first session metadata line, so avoid parsing the rest of the
    // rollout. Legacy rollouts without metadata, and unreadable heads, use the full scan below.
    if let Ok(meta) = codex_rollout::read_session_meta_line(path).await
        && let Some(builder) = codex_rollout::builder_from_items(
            &[codex_protocol::protocol::RolloutItem::SessionMeta(meta)],
            path,
        )
    {
        return RolloutThreadId::Id(builder.id.to_string());
    }

    let mut saw_item = false;
    let mut first_meta = Vec::new();
    let result = RolloutRecorder::for_each_rollout_item(path, |item| {
        saw_item = true;
        if first_meta.is_empty()
            && matches!(item, codex_protocol::protocol::RolloutItem::SessionMeta(_))
        {
            first_meta.push(item);
        }
    })
    .await;
    if let Err(err) = result {
        return RolloutThreadId::Unusable(err.to_string());
    }
    if !saw_item {
        return RolloutThreadId::Unusable("no parseable rollout items".to_string());
    }
    codex_rollout::builder_from_items(&first_meta, path)
        .map(|builder| RolloutThreadId::Id(builder.id.to_string()))
        .unwrap_or(RolloutThreadId::MalformedName)
}

fn is_rollout_file(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("jsonl"))
        && path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with("rollout-"))
}

fn count_or_skipped(count: usize, complete: bool) -> String {
    if complete {
        count.to_string()
    } else {
        "skipped (scan incomplete)".to_string()
    }
}

fn path_key(path: &Path) -> PathBuf {
    normalize_for_path_comparison(path).unwrap_or_else(|_| path.to_path_buf())
}

fn missing_rollout_paths<'a>(
    files: &'a [RolloutAuditFile],
    rows_by_key: &HashMap<PathBuf, Vec<&ThreadStateAuditRow>>,
    archived: bool,
) -> Vec<&'a Path> {
    files
        .iter()
        .filter(|file| file.archived == archived && !has_matching_thread_row(file, rows_by_key))
        .map(|file| file.path.as_path())
        .collect()
}

fn has_matching_thread_row(
    file: &RolloutAuditFile,
    rows_by_key: &HashMap<PathBuf, Vec<&ThreadStateAuditRow>>,
) -> bool {
    let Some(rows) = rows_by_key.get(&file.key) else {
        return false;
    };
    rows.iter().any(|row| row.id == file.thread_id.as_str())
}

fn duplicate_rollout_thread_ids(files: &[RolloutAuditFile]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut duplicates = HashSet::new();
    for thread_id in files.iter().map(|file| file.thread_id.as_str()) {
        if !seen.insert(thread_id) {
            duplicates.insert(thread_id.to_string());
        }
    }
    let mut duplicates = duplicates.into_iter().collect::<Vec<_>>();
    duplicates.sort();
    duplicates
}

fn duplicate_db_paths(rows_by_key: &HashMap<PathBuf, Vec<&ThreadStateAuditRow>>) -> Vec<PathBuf> {
    let mut paths = rows_by_key
        .iter()
        .filter(|(_, rows)| rows.len() > 1)
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn source_category(source: &str) -> &'static str {
    let parsed = serde_json::from_str::<SessionSource>(source)
        .or_else(|_| serde_json::from_value(serde_json::Value::String(source.to_string())));
    let Ok(source) = parsed else {
        return "unparsable";
    };

    match source {
        SessionSource::Cli => "cli",
        SessionSource::VSCode => "vscode",
        SessionSource::Exec => "exec",
        SessionSource::Mcp => "mcp",
        SessionSource::Custom(_) => "custom",
        SessionSource::Internal(InternalSessionSource::MemoryConsolidation) => {
            "internal:memory_consolidation"
        }
        SessionSource::SubAgent(SubAgentSource::Review) => "subagent:review",
        SessionSource::SubAgent(SubAgentSource::Compact) => "subagent:compact",
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }) => "subagent:thread_spawn",
        SessionSource::SubAgent(SubAgentSource::MemoryConsolidation) => {
            "subagent:memory_consolidation"
        }
        SessionSource::SubAgent(SubAgentSource::Other(_)) => "subagent:other",
        SessionSource::Unknown => "unknown",
    }
}

fn count_summary<I, V>(values: I) -> String
where
    I: Iterator<Item = V>,
    V: Into<String>,
{
    let mut counts = BTreeMap::<String, usize>::new();
    for value in values {
        *counts.entry(value.into()).or_default() += 1;
    }
    if counts.is_empty() {
        return "none".to_string();
    }

    let mut entries = counts.into_iter().collect::<Vec<_>>();
    entries.sort_by(|(left_value, left_count), (right_value, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| left_value.cmp(right_value))
    });
    let omitted_categories = entries.len().saturating_sub(SUMMARY_LIMIT);
    let omitted_rows = entries
        .iter()
        .skip(SUMMARY_LIMIT)
        .map(|(_, count)| count)
        .sum::<usize>();
    let mut parts = entries
        .into_iter()
        .take(SUMMARY_LIMIT)
        .map(|(value, count)| format!("{value}={count}"))
        .collect::<Vec<_>>();
    if omitted_categories > 0 {
        parts.push(format!(
            "other={omitted_rows} across {omitted_categories} categories"
        ));
    }
    parts.join(", ")
}

fn push_path_samples<'a>(
    details: &mut Vec<String>,
    label: &str,
    paths: impl Iterator<Item = &'a Path>,
) {
    push_samples(details, label, paths.map(|path| path.display().to_string()));
}

fn push_samples<I, V>(details: &mut Vec<String>, label: &str, values: I)
where
    I: Iterator<Item = V>,
    V: ToString,
{
    for value in values.take(SAMPLE_LIMIT) {
        details.push(format!("{label}: {}", value.to_string()));
    }
}

