//! Converts raw doctor detail strings into human-oriented rows.
//!
//! Checks intentionally store details as simple redacted `label: value` strings
//! so JSON serialization and human rendering share the same source data. This
//! module owns the presentation-only transformations: collapsing noisy booleans,
//! truncating long paths for terminal output, grouping repeated values, and
//! keeping the `--all` expansion behavior out of check construction.

use std::collections::BTreeSet;
use std::env;

use super::DoctorCheck;
use super::HumanOutputOptions;
use super::redact_detail;

const LIST_LIMIT: usize = 7;
const PATH_LIMIT: usize = 48;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum HumanDetail {
    Row {
        label: String,
        value: String,
        expected: Option<String>,
    },
    Continuation(String),
    Bullet(String),
    Remedy(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedDetail {
    label: String,
    value: String,
    expected: Option<String>,
}

pub(super) fn detail_lines(check: &DoctorCheck, options: HumanOutputOptions) -> Vec<HumanDetail> {
    let parsed = parsed_details(check, options);
    let details = match check.category.as_str() {
        "system" => system_details(&parsed),
        "runtime" => runtime_details(&parsed),
        "install" => install_details(&parsed, options),
        "git" => git_details(&parsed, options),
        "title" => title_details(&parsed),
        "config" => config_details(&parsed, options),
        "state" => state_details(&parsed, options),
        _ => generic_details(&parsed),
    };
    let mut details = details.into_iter().collect::<Vec<_>>();
    details.extend(issue_remedies(check));
    details
}

fn system_details(parsed: &[ParsedDetail]) -> Vec<HumanDetail> {
    let mut out = Vec::new();
    push_row_if_present(&mut out, parsed, "os", "os");
    push_row_if_present(&mut out, parsed, "os language", "OS language");
    push_row_if_present(&mut out, parsed, "LC_ALL", "LC_ALL");
    push_row_if_present(&mut out, parsed, "LC_CTYPE", "LC_CTYPE");
    push_row_if_present(&mut out, parsed, "LANG", "LANG");
    push_row_if_present(&mut out, parsed, "VISUAL", "VISUAL");
    push_row_if_present(&mut out, parsed, "EDITOR", "EDITOR");
    push_row_if_present(&mut out, parsed, "PAGER", "PAGER");
    push_row_if_present(&mut out, parsed, "GIT_PAGER", "GIT_PAGER");
    push_row_if_present(&mut out, parsed, "GH_PAGER", "GH_PAGER");
    push_row_if_present(&mut out, parsed, "LESS", "LESS");
    push_remaining(
        &mut out,
        parsed,
        &[
            "os",
            "os type",
            "os version",
            "os language",
            "LC_ALL",
            "LC_CTYPE",
            "LANG",
            "VISUAL",
            "EDITOR",
            "PAGER",
            "GIT_PAGER",
            "GH_PAGER",
            "LESS",
        ],
        &[],
    );
    out
}

pub(super) fn detail_value(check: &DoctorCheck, label: &str) -> Option<String> {
    check
        .details
        .iter()
        .find(|line| line.split_once(": ").is_some_and(|(key, _)| key == label))
        .and_then(|line| {
            redact_detail(line)
                .split_once(": ")
                .map(|(_, value)| value.to_string())
        })
}

pub(super) fn rollout_summary(value: &str, options: HumanOutputOptions) -> Option<String> {
    let sep = super::inline_separator(options);
    let (files, rest) = value.split_once(" files, ")?;
    let (total_bytes, rest) = rest.split_once(" total bytes, ")?;
    let (average_bytes, _) = rest.split_once(" average bytes")?;
    let files = files.trim().parse::<u64>().ok()?;
    let total_bytes = total_bytes.trim().parse::<u64>().ok()?;
    let average_bytes = average_bytes.trim().parse::<u64>().ok()?;
    Some(format!(
        "{} files{sep}{} (avg {})",
        format_count(files),
        format_bytes(total_bytes),
        format_bytes(average_bytes)
    ))
}

pub(super) fn rollout_files_and_bytes(value: &str) -> Option<(u64, u64)> {
    let (files, rest) = value.split_once(" files, ")?;
    let (total_bytes, _) = rest.split_once(" total bytes, ")?;
    Some((
        files.trim().parse::<u64>().ok()?,
        total_bytes.trim().parse::<u64>().ok()?,
    ))
}

pub(super) fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.2} GB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.2} MB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.2} KB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
    }
}

pub(super) fn format_count(count: u64) -> String {
    let mut digits = count.to_string();
    let mut out = String::new();
    while digits.len() > 3 {
        let tail = digits.split_off(digits.len() - 3);
        if out.is_empty() {
            out = tail;
        } else {
            out = format!("{tail},{out}");
        }
    }
    if out.is_empty() {
        digits
    } else {
        format!("{digits},{out}")
    }
}

fn parsed_details(check: &DoctorCheck, options: HumanOutputOptions) -> Vec<ParsedDetail> {
    check
        .details
        .iter()
        .map(|detail| redact_detail(detail))
        .map(|detail| {
            detail
                .split_once(": ")
                .map(|(label, value)| ParsedDetail {
                    label: label.to_string(),
                    value: humanize_source_value(label, value, options),
                    expected: issue_expected_for_label(check, label),
                })
                .unwrap_or_else(|| ParsedDetail {
                    label: String::new(),
                    value: detail,
                    expected: None,
                })
        })
        .collect()
}

fn runtime_details(parsed: &[ParsedDetail]) -> Vec<HumanDetail> {
    let mut out = Vec::new();
    push_row_if_present(&mut out, parsed, "version", "version");
    push_row_if_present(&mut out, parsed, "install method", "install method");
    push_row_if_present(&mut out, parsed, "commit", "commit");
    push_row_if_present(&mut out, parsed, "current executable", "executable");
    push_remaining(
        &mut out,
        parsed,
        &[
            "version",
            "platform",
            "install method",
            "commit",
            "current executable",
        ],
        &[],
    );
    out
}

fn install_details(parsed: &[ParsedDetail], options: HumanOutputOptions) -> Vec<HumanDetail> {
    let sep = super::inline_separator(options);
    let mut out = Vec::new();
    push_row_if_present(&mut out, parsed, "install context", "context");
    if parsed.iter().any(|detail| {
        detail.value == "ignored inherited package-manager launch env for cargo-built binary"
    }) {
        out.push(HumanDetail::Bullet(
            "ignored inherited package-manager launch env for cargo-built binary".to_string(),
        ));
    }

    let managed_by_npm = value(parsed, "managed by npm").unwrap_or("false");
    let managed_by_bun = value(parsed, "managed by bun").unwrap_or("false");
    let managed_by_pnpm = value(parsed, "managed by pnpm").unwrap_or("false");
    let package_root = value(parsed, "managed package root").unwrap_or("not set");
    out.push(HumanDetail::Row {
        label: "managed by".to_string(),
        value: format!(
            "npm: {}{sep}bun: {}{sep}pnpm: {}{sep}package root {}",
            yes_no(managed_by_npm),
            yes_no(managed_by_bun),
            yes_no(managed_by_pnpm),
            if is_falsy(package_root) {
                (if options.ascii { "-" } else { "—" }).to_string()
            } else {
                package_root.to_string()
            }
        ),
        expected: expected(
            parsed,
            &[
                "managed by npm",
                "managed by bun",
                "managed by pnpm",
                "managed package root",
            ],
        ),
    });

    let path_entries = numbered_values(parsed, "PATH codex #");
    if !path_entries.is_empty() {
        let total = path_entries.len();
        let shown = if options.show_all {
            total
        } else {
            total.min(3)
        };
        out.push(HumanDetail::Row {
            label: format!("PATH entries ({total})"),
            value: path_entries[0].clone(),
            expected: expected(
                parsed,
                &parsed
                    .iter()
                    .filter(|detail| {
                        detail.label == "PATH codex entries"
                            || detail.label.starts_with("PATH codex #")
                    })
                    .map(|detail| detail.label.as_str())
                    .collect::<Vec<_>>(),
            ),
        });
        out.extend(
            path_entries
                .iter()
                .skip(1)
                .take(shown.saturating_sub(1))
                .cloned()
                .map(HumanDetail::Continuation),
        );
        if shown < total {
            out.push(HumanDetail::Continuation(
                (if options.ascii {
                    "... (full list with --all)"
                } else {
                    "… (full list with --all)"
                })
                .to_string(),
            ));
        }
    }

    push_remaining(
        &mut out,
        parsed,
        &[
            "current executable",
            "install context",
            "managed by npm",
            "managed by bun",
            "managed by pnpm",
            "managed package root",
            "PATH codex entries",
        ],
        &["PATH codex #"],
    );
    out
}

fn git_details(parsed: &[ParsedDetail], options: HumanOutputOptions) -> Vec<HumanDetail> {
    let mut out = Vec::new();
    push_row_if_present(&mut out, parsed, "selected git", "selected git");
    push_row_if_present(&mut out, parsed, "git version", "version");
    push_row_if_present(&mut out, parsed, "git exec path", "exec path");
    push_row_if_present(&mut out, parsed, "repo detected", "repo detected");
    push_row_if_present(&mut out, parsed, "repo root", "repo root");
    push_row_if_present(&mut out, parsed, ".git entry", ".git entry");
    push_row_if_present(&mut out, parsed, "git branch", "branch");
    push_row_if_present(&mut out, parsed, "core.fsmonitor", "core.fsmonitor");

    let path_entries = numbered_values(parsed, "PATH git #");
    if !path_entries.is_empty() {
        let total = path_entries.len();
        let shown = if options.show_all {
            total
        } else {
            total.min(3)
        };
        out.push(HumanDetail::Row {
            label: format!("PATH entries ({total})"),
            value: path_entries[0].clone(),
            expected: expected(
                parsed,
                &parsed
                    .iter()
                    .filter(|detail| {
                        detail.label == "PATH git entries" || detail.label.starts_with("PATH git #")
                    })
                    .map(|detail| detail.label.as_str())
                    .collect::<Vec<_>>(),
            ),
        });
        out.extend(
            path_entries
                .iter()
                .skip(1)
                .take(shown.saturating_sub(1))
                .cloned()
                .map(HumanDetail::Continuation),
        );
        if shown < total {
            out.push(HumanDetail::Continuation(
                (if options.ascii {
                    "... (full list with --all)"
                } else {
                    "… (full list with --all)"
                })
                .to_string(),
            ));
        }
    }

    push_remaining(
        &mut out,
        parsed,
        &[
            "selected git",
            "PATH git entries",
            "git version",
            "git exec path",
            "git build options",
            "repo detected",
            "repo root",
            ".git entry",
            "git branch",
            "core.fsmonitor",
        ],
        &["PATH git #"],
    );
    out
}

fn title_details(parsed: &[ParsedDetail]) -> Vec<HumanDetail> {
    let mut out = Vec::new();
    push_row_if_present(&mut out, parsed, "terminal title source", "title source");
    push_row_if_present(&mut out, parsed, "terminal title items", "title items");
    push_row_if_present(&mut out, parsed, "terminal title activity", "activity item");
    push_row_if_present(
        &mut out,
        parsed,
        "terminal title project source",
        "project source",
    );
    push_row_if_present(
        &mut out,
        parsed,
        "terminal title project value",
        "project value",
    );
    push_remaining(
        &mut out,
        parsed,
        &[
            "terminal title source",
            "terminal title items",
            "terminal title activity",
            "terminal title project source",
            "terminal title project value",
        ],
        &[],
    );
    out
}

fn config_details(parsed: &[ParsedDetail], options: HumanOutputOptions) -> Vec<HumanDetail> {
    let sep = super::inline_separator(options);
    let mut out = Vec::new();
    if let Some(model) = value(parsed, "model") {
        let value = value(parsed, "model provider").map_or_else(
            || model.to_string(),
            |provider| format!("{model}{sep}{provider}"),
        );
        out.push(HumanDetail::Row {
            label: "model".to_string(),
            value,
            expected: expected(parsed, &["model", "model provider"]),
        });
    }
    push_row_if_present(&mut out, parsed, "cwd", "cwd");
    push_row_if_present(&mut out, parsed, "config.toml", "config.toml");
    push_row_if_present(&mut out, parsed, "config.toml parse", "config.toml parse");
    push_row_if_present(&mut out, parsed, "config.toml read", "config.toml read");
    push_row_if_present(&mut out, parsed, "mcp servers", "MCP servers");
    push_feature_flags(&mut out, parsed, options);

    for detail in parsed
        .iter()
        .filter(|detail| detail.label == "legacy feature flag")
    {
        out.push(HumanDetail::Row {
            label: "legacy alias".to_string(),
            value: detail.value.clone(),
            expected: detail.expected.clone(),
        });
    }

    push_remaining(
        &mut out,
        parsed,
        &[
            "CODEX_HOME",
            "cwd",
            "model",
            "model provider",
            "log dir",
            "sqlite home",
            "mcp servers",
            "feature flags enabled",
            "enabled feature flags",
            "feature flag overrides",
            "legacy feature flag",
            "config.toml",
            "config.toml parse",
            "config.toml read",
        ],
        &[],
    );
    out
}

fn state_details(parsed: &[ParsedDetail], options: HumanOutputOptions) -> Vec<HumanDetail> {
    let mut out = Vec::new();
    push_row_if_present(&mut out, parsed, "CODEX_HOME", "CODEX_HOME");
    push_row_if_present(&mut out, parsed, "log dir", "log dir");
    push_row_if_present(&mut out, parsed, "sqlite home", "sqlite home");
    push_database_row(&mut out, parsed, "state DB", options);
    push_database_row(&mut out, parsed, "log DB", options);
    push_database_row(&mut out, parsed, "goals DB", options);
    push_database_row(&mut out, parsed, "memories DB", options);
    for (source, label) in [
        ("active rollout files", "active rollouts"),
        ("archived rollout files", "archived rollouts"),
    ] {
        if let Some(value) = value(parsed, source) {
            out.push(HumanDetail::Row {
                label: label.to_string(),
                value: rollout_summary(value, options).unwrap_or_else(|| value.to_string()),
                expected: expected(parsed, &[source]),
            });
        }
    }

    push_remaining(
        &mut out,
        parsed,
        &[
            "CODEX_HOME",
            "log dir",
            "sqlite home",
            "state DB",
            "log DB",
            "goals DB",
            "state DB integrity",
            "log DB integrity",
            "goals DB integrity",
            "memories DB",
            "memories DB integrity",
            "active rollout files",
            "archived rollout files",
        ],
        &[],
    );
    out
}

fn generic_details(parsed: &[ParsedDetail]) -> Vec<HumanDetail> {
    parsed
        .iter()
        .map(|detail| {
            if detail.label.is_empty() {
                HumanDetail::Bullet(detail.value.clone())
            } else {
                HumanDetail::Row {
                    label: display_label(&detail.label),
                    value: detail.value.clone(),
                    expected: detail.expected.clone(),
                }
            }
        })
        .collect()
}

fn push_feature_flags(
    out: &mut Vec<HumanDetail>,
    parsed: &[ParsedDetail],
    options: HumanOutputOptions,
) {
    let sep = super::inline_separator(options);
    let enabled_count = value(parsed, "feature flags enabled")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default();
    let overrides = list_items(value(parsed, "feature flag overrides").unwrap_or("none"));
    let override_count = overrides.len();
    let hint = if !options.show_all && enabled_count > 0 {
        " (full list with --all)"
    } else {
        ""
    };
    out.push(HumanDetail::Row {
        label: "feature flags".to_string(),
        value: format!("{enabled_count} enabled{sep}{override_count} overridden{hint}"),
        expected: expected(
            parsed,
            &[
                "feature flags enabled",
                "feature flag overrides",
                "enabled feature flags",
            ],
        ),
    });

    if !overrides.is_empty() {
        push_list_row(
            out,
            "overrides",
            &overrides,
            options,
            expected(parsed, &["feature flag overrides"]),
        );
    }
    if options.show_all {
        let enabled = list_items(value(parsed, "enabled feature flags").unwrap_or("none"));
        if !enabled.is_empty() {
            push_list_row(
                out,
                "enabled flags",
                &enabled,
                options,
                expected(parsed, &["enabled feature flags"]),
            );
        }
    }
}

fn push_list_row(
    out: &mut Vec<HumanDetail>,
    label: &str,
    items: &[String],
    options: HumanOutputOptions,
    expected: Option<String>,
) {
    let limit = if options.show_all {
        items.len()
    } else {
        items.len().min(LIST_LIMIT)
    };
    let mut value = items
        .iter()
        .take(limit)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if limit < items.len() {
        value.push_str(if options.ascii {
            ", ... (full list with --all)"
        } else {
            ", … (full list with --all)"
        });
    }
    out.push(HumanDetail::Row {
        label: label.to_string(),
        value,
        expected,
    });
}

fn push_database_row(
    out: &mut Vec<HumanDetail>,
    parsed: &[ParsedDetail],
    label: &str,
    options: HumanOutputOptions,
) {
    let sep = super::inline_separator(options);
    let Some(path) = value(parsed, label) else {
        return;
    };
    let integrity = value(parsed, &format!("{label} integrity"));
    let value = integrity.map_or_else(
        || path.to_string(),
        |integrity| format!("{path}{sep}integrity {integrity}"),
    );
    out.push(HumanDetail::Row {
        label: label.to_string(),
        value,
        expected: expected(parsed, &[label, &format!("{label} integrity")]),
    });
}

fn push_row_if_present(
    out: &mut Vec<HumanDetail>,
    parsed: &[ParsedDetail],
    source_label: &str,
    display_label: &str,
) {
    if let Some(value) = value(parsed, source_label) {
        out.push(HumanDetail::Row {
            label: display_label.to_string(),
            value: value.to_string(),
            expected: expected(parsed, &[source_label]),
        });
    }
}

fn push_remaining(
    out: &mut Vec<HumanDetail>,
    parsed: &[ParsedDetail],
    consumed_labels: &[&str],
    consumed_prefixes: &[&str],
) {
    for detail in parsed {
        if detail.value == "ignored inherited package-manager launch env for cargo-built binary" {
            continue;
        }
        if consumed_labels.contains(&detail.label.as_str())
            || consumed_prefixes
                .iter()
                .any(|prefix| detail.label.starts_with(prefix))
        {
            continue;
        }
        if detail.label.is_empty() {
            out.push(HumanDetail::Bullet(detail.value.clone()));
        } else {
            out.push(HumanDetail::Row {
                label: display_label(&detail.label),
                value: detail.value.clone(),
                expected: detail.expected.clone(),
            });
        }
    }
}

fn expected(parsed: &[ParsedDetail], sources: &[&str]) -> Option<String> {
    let values = parsed
        .iter()
        .filter(|detail| sources.contains(&detail.label.as_str()))
        .filter_map(|detail| detail.expected.clone())
        .collect::<BTreeSet<_>>();
    (!values.is_empty()).then(|| values.into_iter().collect::<Vec<_>>().join("; "))
}

fn issue_expected_for_label(check: &DoctorCheck, label: &str) -> Option<String> {
    check
        .issues
        .iter()
        .find(|issue| issue.fields.iter().any(|field| field == label))
        .and_then(|issue| issue.expected.as_deref().map(redact_detail))
}

fn issue_remedies(check: &DoctorCheck) -> Vec<HumanDetail> {
    let mut seen = BTreeSet::new();
    check
        .issues
        .iter()
        .filter_map(|issue| issue.remedy.as_ref())
        .filter(|remedy| seen.insert((*remedy).clone()))
        .map(|remedy| redact_detail(remedy))
        .map(HumanDetail::Remedy)
        .collect()
}

fn humanize_source_value(label: &str, value: &str, options: HumanOutputOptions) -> String {
    let path_field = matches!(
        label,
        "current executable"
            | "selected git"
            | "git exec path"
            | "repo root"
            | "cwd"
            | "config.toml"
            | "CODEX_HOME"
            | "log dir"
            | "sqlite home"
            | "state DB"
            | "log DB"
            | "goals DB"
            | "memories DB"
            | "managed package root"
    ) || label.starts_with("PATH codex #")
        || label.starts_with("PATH git #");
    if path_field && looks_like_path(value) {
        let value = home_shortened_path(value);
        if options.show_all {
            return value;
        }
        return middle_truncate(&value, PATH_LIMIT, options);
    }
    // Keep timestamps and arbitrary diagnostic values intact instead of guessing their syntax.
    value.to_string()
}

fn home_shortened_path(path: &str) -> String {
    let Some(home) = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .and_then(|home| home.into_string().ok())
    else {
        return path.to_string();
    };
    if path == home {
        "~".to_string()
    } else {
        path.strip_prefix(&format!("{home}/"))
            .or_else(|| path.strip_prefix(&format!("{home}\\")))
            .map_or_else(|| path.to_string(), |tail| format!("~/{tail}"))
    }
}

fn middle_truncate(value: &str, max_chars: usize, options: HumanOutputOptions) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }
    let head_len = max_chars / 2;
    let marker = if options.ascii { "..." } else { "…" };
    let tail_len = max_chars.saturating_sub(head_len + marker.chars().count());
    let head_end = value
        .char_indices()
        .nth(head_len)
        .map_or(value.len(), |(index, _)| index);
    let tail_start = value
        .char_indices()
        .rev()
        .nth(tail_len.saturating_sub(1))
        .map_or(0, |(index, _)| index);
    let head = &value[..head_end];
    let tail = if tail_len == 0 {
        ""
    } else {
        &value[tail_start..]
    };
    format!("{head}{marker}{tail}")
}

fn looks_like_path(value: &str) -> bool {
    value.starts_with("\\\\")
        || (value.as_bytes().get(1) == Some(&b':')
            && value
                .as_bytes()
                .get(2)
                .is_some_and(|byte| *byte == b'\\' || *byte == b'/'))
        || value.starts_with('/')
        || value.starts_with("~/")
        || value.starts_with("./")
        || value.starts_with("../")
}

fn numbered_values(parsed: &[ParsedDetail], prefix: &str) -> Vec<String> {
    parsed
        .iter()
        .filter(|detail| detail.label.starts_with(prefix))
        .map(|detail| detail.value.clone())
        .collect()
}

fn value<'a>(parsed: &'a [ParsedDetail], label: &str) -> Option<&'a str> {
    parsed
        .iter()
        .find(|detail| detail.label == label)
        .map(|detail| detail.value.as_str())
}

fn display_label(label: &str) -> String {
    match label {
        "optional reachability failed" => "optional reachability",
        "check for update on startup" => "startup update check",
        other => other,
    }
    .to_string()
}

fn list_items(value: &str) -> Vec<String> {
    if is_falsy(value) {
        return Vec::new();
    }
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn yes_no(value: &str) -> &'static str {
    if value == "true" { "yes" } else { "no" }
}

pub(super) fn is_falsy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "false" | "none" | "not set" | "unknown" | "missing" | "absent" | "no" | "—" | "-"
    )
}
