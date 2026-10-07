use crate::validation::ValidationClassification;
use crate::validation::classify_validation_script;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::fmt::Write as _;

const DEFAULT_SUMMARY_AFTER_BYTES: usize = 48 * 1024;
const DEFAULT_SUMMARY_AFTER_LINES: usize = 600;
const VALIDATION_SUCCESS_SUMMARY_AFTER_LINES: usize = 80;
const SUMMARY_MAX_BYTES: usize = 32 * 1024;
const SUMMARY_MAX_LINES: usize = 240;
// Reserve room for the retention counts and an explicit cap marker.
const SUMMARY_FOOTER_BYTES: usize = 128;
const SUMMARY_FOOTER_LINES: usize = 3;
const SUCCESS_HEAD_LINES: usize = 24;
const SUCCESS_TAIL_LINES: usize = 64;
const VALIDATION_SUCCESS_TAIL_LINES: usize = 16;
const FAILURE_TAIL_LINES: usize = 140;
/// A successful output this short exceeds its budget through long lines, not
/// many lines: keep every line and abbreviate the long ones instead of ranking
/// an excerpt that sends the model back to re-read the omitted lines.
const COMPLETE_SUCCESS_MAX_LINES: usize = 160;
/// Narrowest per-line excerpt used before whole lines are omitted.
const MIN_COMPLETE_LINE_BYTES: usize = 240;
pub(crate) const FOCUS_CONTEXT_LINES: usize = 3;
// Keep the selected ranges below the summary line ceiling even when every
// match is disjoint and receives the full context window. This leaves room for
// the failure tail, final statuses, separators, and summary metadata.
const MAX_FOCUS_MATCHES: usize = 8;
const MAX_STATUS_MATCHES: usize = 8;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ShellOutputSummaryOptions<'a> {
    pub(crate) enabled: bool,
    /// Summarize when output exceeds the actual projection budget, including
    /// outputs below the default large-output threshold.
    pub(crate) applied_token_limit: Option<usize>,
    /// Optional command text may classify output shape, such as validation/build
    /// output. Do not add extra plumbing just to carry this value.
    pub(crate) command_text: Option<&'a str>,
}

#[cfg(test)]
pub(crate) fn summarize_shell_output_for_model(
    output: &str,
    exit_code: i32,
    timed_out: bool,
    options: ShellOutputSummaryOptions<'_>,
) -> Option<String> {
    summarize_shell_output_for_model_with_streams(output, exit_code, timed_out, options, None)
}

/// `streams` must be complete, producer-owned stdout/stderr for this output,
/// not a cumulative snapshot paired with a later polling chunk.
pub(crate) fn summarize_shell_output_for_model_with_streams(
    output: &str,
    exit_code: i32,
    timed_out: bool,
    options: ShellOutputSummaryOptions<'_>,
    streams: Option<(&str, &str)>,
) -> Option<String> {
    if !options.enabled {
        return None;
    }
    let exceeds_token_budget = options
        .applied_token_limit
        .is_some_and(|limit| codex_utils_string::approx_token_count_exceeds(output, limit));
    // Below even the smaller success threshold, avoid parsing a command whose
    // output cannot need a summary. This is the common short-command path.
    if !exceeds_token_budget
        && output.len() <= DEFAULT_SUMMARY_AFTER_BYTES
        && output
            .lines()
            .take(VALIDATION_SUCCESS_SUMMARY_AFTER_LINES)
            .count()
            < VALIDATION_SUCCESS_SUMMARY_AFTER_LINES
    {
        return None;
    }
    let failed = timed_out || exit_code != 0;
    let validation = options.command_text.is_some_and(|command| {
        matches!(
            classify_validation_script(command),
            ValidationClassification::Validation { leaves, .. } if !leaves.is_empty()
        )
    });
    let summary_after_lines = if validation && !failed {
        VALIDATION_SUCCESS_SUMMARY_AFTER_LINES
    } else {
        DEFAULT_SUMMARY_AFTER_LINES
    };
    if !exceeds_token_budget
        && output.len() <= DEFAULT_SUMMARY_AFTER_BYTES
        && output.lines().take(summary_after_lines).count() < summary_after_lines
    {
        return None;
    }
    if options.command_text.is_some_and(|command| {
        is_read_only_command(command)
            || powershell_command_segments(command).is_some_and(|segments| {
                segments
                    .into_iter()
                    .any(|segment| source_read_output_budget(segment).is_some())
            })
    }) {
        if let Some(command) = options.command_text
            && let Some(summary) = rg_file_summary(output, command, options.applied_token_limit)
        {
            return Some(summary);
        }
        // Preserve the requested source order and the existing truncation/raw
        // artifact recovery path instead of ranking code as diagnostic prose.
        // A later failing command does not make earlier source reads diagnostics.
        // Without per-segment output boundaries, keep the entire mixed stream.
        return None;
    }

    // Replace (never append to) the prose excerpt for recognized compiler JSON.
    // Raw output remains the canonical artifact owned by the caller.
    if let Some(summary) = structured_compiler_summary_with_streams(output, exit_code, timed_out, options.applied_token_limit, streams) {
        return Some(summary);
    }

    // A successful command with no diagnostic line has nothing to rank, so the
    // head/warning/tail policy degenerates to positional truncation that drops
    // the middle of a flat list. A summary also reads as complete in a way a
    // truncation notice does not. Leave those to ordinary truncation, which
    // retains the artifact and reports exactly what it withheld. This holds
    // when the output exceeds the caller's token budget as well: the budget is
    // still enforced by that truncation, and a ranked excerpt of source or
    // listing text only sends the model back to re-read what it withheld.
    if !failed
        && !validation
        && !output.lines().any(|line| {
            // Unknown successful scripts often print source signatures or
            // inventory fields containing these words. Require a diagnostic
            // label, not an incidental occurrence inside source evidence.
            let line = line.trim_start();
            ["error:", "error[", "warning:", "warning[", "fatal:", "npm err!"].iter()
                .any(|prefix| starts_with_ascii_case(line, prefix))
        })
    {
        return None;
    }

    let line_count = output.lines().count();
    let selection = select_lines(output, line_count, failed, validation);
    let selection_policy = if validation {
        "source-ordered failure-focused lines, final status lines, tail"
    } else if failed {
        "source-ordered failure-focused lines, tail"
    } else {
        "source-ordered head, warning/error lines, tail"
    };

    let mut builder = SummaryBuilder::new();
    builder.push_line("Shell output summary:");
    builder.push_line(format!("- original_lines: {line_count}"));
    builder.push_line(format!("- original_bytes: {}", output.len()));
    builder.push_line(format!("- exit_code: {exit_code}"));
    if timed_out {
        builder.push_line("- timed_out: true");
    }
    builder.push_line(format!("- selection_policy: {selection_policy}"));
    if selection.collapsed_progress_lines > 0 {
        builder.push_line(format!("- collapsed_progress_lines: {} (latest sample retained; raw output unchanged)", selection.collapsed_progress_lines));
    }
    if selection.omitted_groups > 0 {
        let qualifier = if selection.groups_overflowed {
            " (group inventory also capped)"
        } else {
            ""
        };
        builder.push_line(format!("- omitted_diagnostic_groups: at least {} (selection-stage lower bound{qualifier}); final fitting may omit additional diagnostics; inspect the raw output", selection.omitted_groups));
    }
    builder.push_line("");
    builder.push_line("Selected output lines:");

    // Candidate quotas reserve space for actionable and final regions before
    // rendering. Emit the selected lines in source order so a diagnostic stays
    // attached to the context that explains it.
    // Borrow only the bounded selection; never copy oversized source lines.
    let mut lines = output.lines();
    let mut next_index = 0;
    let mut selected = selection
        .indexes
        .iter()
        .filter_map(|&index| {
            let line = lines.nth(index - next_index)?;
            next_index = index + 1;
            Some((index, line))
        })
        .collect::<Vec<_>>();
    let gap_bytes: usize = selected
        .windows(2)
        .filter(|pair| pair[1].0 != pair[0].0 + 1)
        .map(|pair| format!("\n... [{} lines omitted]", pair[1].0 - pair[0].0 - 1).len())
        .sum();
    let prefixes: usize = selected
        .iter()
        .map(|(index, _)| format!("{:>5}: ", index + 1).len())
        .sum();
    let available = SUMMARY_MAX_BYTES.saturating_sub(
        SUMMARY_FOOTER_BYTES + builder.text.len() + gap_bytes + prefixes + selected.len(),
    );
    let mut low = 0;
    let mut high = available;
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if selected
            .iter()
            .map(|(_, line)| line.len().min(mid))
            .sum::<usize>()
            <= available
        {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let mut line_budget = low;
    let mut summary = render_selected_lines(builder.clone(), &selected, line_budget, line_count)?;
    if let Some(limit) = options.applied_token_limit
        && codex_utils_string::approx_token_count_exceeds(&summary, limit)
    {
        if !(failed || validation) && line_count <= COMPLETE_SUCCESS_MAX_LINES {
            // Every line is selected; abbreviate the long ones further before
            // omitting any whole line.
            let fits = |budget| {
                render_selected_lines(builder.clone(), &selected, budget, line_count).filter(
                    |rendered| !codex_utils_string::approx_token_count_exceeds(rendered, limit),
                )
            };
            if let Some(minimum) = fits(MIN_COMPLETE_LINE_BYTES.min(line_budget)) {
                let mut low = MIN_COMPLETE_LINE_BYTES.min(line_budget);
                let mut high = line_budget;
                summary = minimum;
                while low < high {
                    let mid = low + (high - low).div_ceil(2);
                    match fits(mid) {
                        Some(rendered) => {
                            low = mid;
                            summary = rendered;
                        }
                        None => high = mid - 1,
                    }
                }
                return (summary.len() < output.len()).then_some(summary);
            }
        }
        // The byte ceiling does not honor a caller's smaller token budget (or
        // punctuation-dense diagnostics). Fit the entire selected summary so
        // downstream truncation cannot discard a middle diagnostic region.
        // Give diagnostic context priority over extra head/tail lines before
        // shortening diagnostics themselves. Every omission remains explicit.
        let diagnostic_indexes = selected
            .iter()
            .filter_map(|(index, line)| {
                let kind = classify_line(line);
                (kind.critical || kind.advisory || kind.status).then_some(*index)
            })
            .collect::<Vec<_>>();
        let (kept, mut pruned): (Vec<_>, Vec<_>) = std::mem::take(&mut selected)
            .into_iter()
            .partition(|(index, _)| {
                *index < FOCUS_CONTEXT_LINES
                    || *index >= line_count.saturating_sub(FOCUS_CONTEXT_LINES)
                    || diagnostic_indexes
                        .iter()
                        .any(|diagnostic| index.abs_diff(*diagnostic) <= FOCUS_CONTEXT_LINES)
            });
        selected = kept;
        summary = render_selected_lines(builder.clone(), &selected, line_budget, line_count)?;
        if !codex_utils_string::approx_token_count_exceeds(&summary, limit) {
            // Pruning is coarse. Return the remaining budget to the pruned
            // lines, nearest the tail first, so an output slightly over the
            // budget does not collapse to its diagnostics alone.
            pruned.reverse();
            let mut low = 0;
            let mut high = pruned.len();
            while low < high {
                let mid = low + (high - low).div_ceil(2);
                let mut candidate = selected.clone();
                candidate.extend_from_slice(&pruned[..mid]);
                candidate.sort_unstable_by_key(|(index, _)| *index);
                let rendered =
                    render_selected_lines(builder.clone(), &candidate, line_budget, line_count)?;
                if codex_utils_string::approx_token_count_exceeds(&rendered, limit) {
                    high = mid - 1;
                } else {
                    low = mid;
                    summary = rendered;
                }
            }
            return (summary.len() < output.len()).then_some(summary);
        }
        let minimum = render_selected_lines(builder.clone(), &selected, 0, line_count)?;
        if codex_utils_string::approx_token_count_exceeds(&minimum, limit) {
            // Even metadata does not fit. Use ordinary truncation/recovery.
            return None;
        }
        let mut low = 0;
        let mut high = line_budget;
        summary = minimum;
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            let candidate = render_selected_lines(builder.clone(), &selected, mid, line_count)?;
            if codex_utils_string::approx_token_count_exceeds(&candidate, limit) {
                high = mid - 1;
            } else {
                low = mid;
                summary = candidate;
            }
        }
        line_budget = low;
        // An empty line selection is less useful than ordinary truncation.
        if line_budget == 0 {
            return None;
        }
    }
    (summary.len() < output.len()).then_some(summary)
}

/// Cargo/rustc JSON carries diagnostics inside escaped single-line records.
/// Ranking/truncating those raw lines can hide their message or location.
/// Cargo mixes JSON stdout with plain stderr; retain that context verbatim.
/// Malformed/unknown JSON or excessive text still uses the existing text path.
#[cfg(test)]
fn structured_compiler_summary(
    output: &str,
    exit_code: i32,
    timed_out: bool,
    token_limit: Option<usize>,
) -> Option<String> {
    structured_compiler_summary_with_streams(output, exit_code, timed_out, token_limit, None)
}

fn structured_compiler_summary_with_streams(
    output: &str,
    exit_code: i32,
    timed_out: bool,
    token_limit: Option<usize>,
    streams: Option<(&str, &str)>,
) -> Option<String> {
    let compiler_output = streams.map_or(output, |(stdout, _)| stdout);
    let stderr = streams.map(|(_, stderr)| stderr);
    let recovery_selector = |source_line: usize| {
        let range = compiler_output.lines().nth(source_line.saturating_sub(1))
            .and_then(|line| output.find(line).map(|start| (start, start + line.len())))
            .unwrap_or((0, output.len()));
        serde_json::json!({"kind":"bytes", "start":range.0, "end":range.1})
    };
    // Keep stderr independently and verbatim. If it cannot fit, fall back to
    // ordinary output projection/recovery rather than silently dropping it.
    if stderr.is_some_and(|stderr| stderr.len() > SUMMARY_MAX_BYTES / 4) {
        return None;
    }
    let mut diagnostics = Vec::new();
    // Evict the latest lowest-priority complete group. This keeps actionable
    // errors ahead of warning floods and preserves source order within ties.
    let lowest_priority_index = |diagnostics: &[serde_json::Value]| {
        diagnostics.iter().enumerate().max_by_key(|(index, item)| {
            let diagnostic = &item["diagnostic"];
            let summary = diagnostic["message"].as_str().is_some_and(|message|
                message.starts_with("aborting due to "));
            let priority = match diagnostic["level"].as_str() {
                Some("error" | "fatal") if !summary => 0,
                Some("warning") => 1,
                _ => 2,
            };
            (priority, *index)
        }).map(|(index, _)| index)
    };
    let mut text_lines = Vec::new();
    let mut text_bytes = stderr.map_or(0, str::len);
    let mut total = 0usize;
    let mut records = 0usize;
    let mut omitted = 0usize;
    let mut errors = 0usize;
    let mut warnings = 0usize;
    let mut build_success = None;
    for (index, line) in compiler_output.lines().enumerate() {
        if line.trim().is_empty() { continue; }
        if !line.trim_start().starts_with('{') {
            text_bytes = text_bytes.saturating_add(line.len());
            if text_bytes > SUMMARY_MAX_BYTES / 4 { return None; }
            text_lines.push(serde_json::json!({"source_line": index + 1, "text": line}));
            continue;
        }
        let mut record: serde_json::Value = serde_json::from_str(line).ok()?;
        records += 1;
        let diagnostic = match record["reason"].as_str() {
            Some("compiler-message") => record.get_mut("message")?,
            Some("compiler-artifact" | "build-script-executed") => continue,
            Some("build-finished") => {
                // A later success cannot erase an earlier failed build.
                let success = record["success"].as_bool()?;
                build_success = Some(build_success.unwrap_or(true) && success);
                continue;
            }
            None if record["$message_type"] == "diagnostic" => &mut record,
            _ => return None,
        };
        let level = diagnostic["level"].as_str()?;
        let message = diagnostic["message"].as_str()?;
        let is_abort_summary = message.starts_with("aborting due to ")
            && (message.contains(" previous error") || message.contains(" previous warning"))
            && diagnostic["spans"].as_array().is_some_and(Vec::is_empty);
        errors += usize::from(matches!(level, "error" | "fatal") && !is_abort_summary);
        warnings += usize::from(level == "warning");
        diagnostic["message"].as_str()?;
        diagnostic["spans"].as_array()?;
        diagnostic["children"].as_array()?;
        total += 1;
        // Keep locations, source excerpts, children and fix suggestions exact.
        // Only redundant rendered prose and the general error-code manual go.
        diagnostic.as_object_mut()?.remove("rendered");
        if let Some(code) = diagnostic.get_mut("code").and_then(serde_json::Value::as_object_mut) {
            code.remove("explanation");
        }
        let mut item = serde_json::json!({"source_line": index + 1, "diagnostic": diagnostic});
        if item.to_string().len() > SUMMARY_MAX_BYTES / 2
            && compact_compiler_diagnostic(&mut item["diagnostic"])
        {
            item["details_omitted"] = true.into();
            item["recovery_selector"] = recovery_selector(index + 1);
        }
        diagnostics.push(item);
        if diagnostics.len() > MAX_DIAGNOSTIC_GROUPS {
            diagnostics.remove(lowest_priority_index(&diagnostics)?);
            omitted += 1;
        }
    }
    if total == 0 { return None; }
    loop {
        let summary = serde_json::json!({
            "format": "compiler_diagnostics",
            "exit_code": exit_code,
            "timed_out": timed_out,
            "build_success": build_success,
            "original_lines": output.lines().count(),
            "original_bytes": output.len(),
            "records": records,
            "diagnostic_count": total,
            "error_count": errors,
            "warning_count": warnings,
            "omitted_diagnostics": omitted,
            "diagnostics_complete": omitted == 0 && diagnostics.iter().all(|item| item["details_omitted"] != true),
            "text_lines": text_lines,
            "diagnostic_stream": if streams.is_some() { "stdout" } else { "aggregated" },
            "stderr": stderr,
            "projection": "Redundant rendered text, code explanations and build artifacts omitted. details_omitted marks abbreviated prose/excerpts or omitted children without locations or fixes. Omitted whole diagnostics may include locations and fixes; recover them with recovery_selector against the retained aggregate bytes.",
            "recovery_selector": (omitted > 0).then(|| serde_json::json!({"kind":"bytes", "start":0, "end":output.len()})),
            "diagnostics": diagnostics,
        }).to_string();
        if summary.len() <= SUMMARY_MAX_BYTES && token_limit.is_none_or(|limit|
            !codex_utils_string::approx_token_count_exceeds(&summary, limit)) {
            return (summary.len() < output.len()).then_some(summary);
        }
        let index = lowest_priority_index(&diagnostics)?;
        let item = &mut diagnostics[index];
        // Warnings/notes yield before actionable error detail. Once only errors
        // remain, shed bulky excerpts before evicting their locations and fixes.
        if matches!(item["diagnostic"]["level"].as_str(), Some("error" | "fatal"))
            && compact_compiler_diagnostic(&mut item["diagnostic"]) {
            item["details_omitted"] = true.into();
            item["recovery_selector"] = recovery_selector(item["source_line"].as_u64()? as usize);
            continue;
        }
        diagnostics.remove(index);
        omitted += 1;
    }
}

/// Drop source excerpts (not span coordinates, labels, or fixes) and bound long
/// diagnostic prose. The canonical compiler output remains the recovery owner.
fn compact_compiler_diagnostic(diagnostic: &mut serde_json::Value) -> bool {
    let mut changed = false;
    if let Some(message) = diagnostic.get_mut("message")
        && let Some(text) = message.as_str()
        && text.len() > 1_024
    {
        *message = summarize_oversized_line(text, 1_024).into_owned().into();
        changed = true;
    }
    if let Some(spans) = diagnostic.get_mut("spans").and_then(serde_json::Value::as_array_mut) {
        for span in spans {
            if let Some(span) = span.as_object_mut() {
                changed |= span.remove("text").is_some();
                changed |= span.remove("expansion").is_some();
            }
        }
    }
    if let Some(children) = diagnostic.get_mut("children").and_then(serde_json::Value::as_array_mut) {
        for child in children {
            changed |= compact_compiler_diagnostic(child);
        }
    }
    if diagnostic.to_string().len() > SUMMARY_MAX_BYTES / 2 {
        if let Some(children) = diagnostic.get_mut("children").and_then(serde_json::Value::as_array_mut)
            && !children.is_empty()
        {
            let before = children.len();
            children.retain(compiler_diagnostic_has_locations);
            changed |= children.len() != before;
        }
    }
    changed
}

fn compiler_diagnostic_has_locations(diagnostic: &serde_json::Value) -> bool {
    diagnostic["spans"].as_array().is_some_and(|spans| !spans.is_empty())
        || diagnostic["children"].as_array().is_some_and(|children| {
            children.iter().any(compiler_diagnostic_has_locations)
        })
}

fn render_selected_lines(
    mut builder: SummaryBuilder,
    selected: &[(usize, &str)],
    line_budget: usize,
    line_count: usize,
) -> Option<String> {
    let mut previous = None;
    let mut emitted_source_lines = 0;
    for &(index, line) in selected {
        let before_gap = (builder.text.len(), builder.lines);
        if let Some(previous_index) = previous
            && index != previous_index + 1
            && !builder.push_line(format!(
                "... [{} lines omitted]",
                index - previous_index - 1
            ))
        {
            builder.text.truncate(before_gap.0);
            builder.lines = before_gap.1;
            break;
        }
        {
            builder.capped |= line.len() > line_budget;
            let line = summarize_oversized_line(line, line_budget);
            if !builder.push_line(format!("{:>5}: {line}", index + 1)) {
                // A gap describes the next retained line. Keep the pair atomic
                // when the gap uses the last available byte or line slot.
                builder.text.truncate(before_gap.0);
                builder.lines = before_gap.1;
                break;
            }
            emitted_source_lines += 1;
        }
        previous = Some(index);
    }
    builder.finish(emitted_source_lines, line_count)
}

/// Count path-prefixed rg records before selecting a source-ordered excerpt.
fn rg_file_summary(output: &str, command: &str, token_limit: Option<usize>) -> Option<String> {
    if !command.split(|c: char| !c.is_alphanumeric() && c != '_').any(|word| word == "rg") {
        return None;
    }
    static HIT: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(||
        regex_lite::Regex::new(r"^(.+?):[0-9]+:").expect("rg path and line prefix"));
    let mut files = std::collections::BTreeMap::<&str, usize>::new();
    let mut hits = 0usize;
    let mut other_lines = 0usize;
    // Count every visible hit, never just the excerpt selected below.
    for line in output.lines() {
        if let Some(hit) = HIT.captures(line) {
            *files.entry(hit.get(1)?.as_str()).or_default() += 1;
            hits += 1;
        } else {
            other_lines += 1;
        }
    }
    if files.is_empty() { return None; }
    let limit = token_limit.unwrap_or(8_000).min(8_000);
    let mut ranked = files.iter().collect::<Vec<_>>();
    ranked.sort_by(|(left_path, left_count), (right_path, right_count)|
        right_count.cmp(left_count).then_with(|| left_path.cmp(right_path)));
    let mut summary = format!(
        "rg output summary (path:line records in this output, not unique matches): {hits} hits across {} files; {other_lines} other lines. Raw output is unchanged and recoverable.\nPer-file hit counts:\n",
        files.len(),
    );
    let mut listed = 0;
    for (path, count) in ranked.into_iter().take(64) {
        let line = format!("- {}: {count}\n", serde_json::to_string(path).ok()?);
        if codex_utils_string::approx_token_count(&summary) + codex_utils_string::approx_token_count(&line) > limit / 2 { break; }
        summary.push_str(&line);
        listed += 1;
    }
    if listed < files.len() {
        let _ = writeln!(summary, "- {} additional files omitted from this directory", files.len() - listed);
    }
    summary.push_str("\nSource-ordered excerpt (not complete):\n");
    let remaining = limit.saturating_sub(codex_utils_string::approx_token_count(&summary) + 64);
    if listed == 0 || remaining == 0 { return None; }
    summary.push_str(&codex_utils_output_truncation::formatted_truncate_text(
        output, codex_utils_output_truncation::TruncationPolicy::Tokens(remaining),
    ));
    (summary.len() < output.len()).then_some(summary)
}

/// Reads, searches, and listings are source material, not diagnostics: ranking
/// their lines hoists incidental "error" text above the code around it, and
/// the model then re-reads the file to recover the order. Bash scripts are
/// classified by the shared parser; PowerShell scripts, which that parser does
/// not understand, are accepted only when every command position is a known
/// read-only cmdlet, alias, or control-flow keyword.
pub(crate) fn source_read_output_budget(command: &str) -> Option<usize> {
    let has_source_reader = command
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .any(|word| {
            matches!(
                word.to_ascii_lowercase().as_str(),
                "rg" | "grep"
                    | "cat"
                    | "head"
                    | "tail"
                    | "sed"
                    | "ls"
                    | "find"
                    | "get-content"
                    | "gc"
                    | "select-string"
                    | "sls"
                    | "get-childitem"
                    | "gci"
                    | "dir"
            )
        });
    (has_source_reader && is_read_only_command(command)).then_some(10_000)
}

pub(crate) fn is_read_only_command(command: &str) -> bool {
    crate::turn_diff_tracker::script_is_read_only(command)
}

/// Splits a PowerShell script at every position that can start a command:
/// statement separators, pipes, conditional chains, script blocks, and
/// sub-expressions. Quoted text and comments are data, so the `|` in
/// `rg -n 'fn |impl '` does not start a command. Returns `None` for syntax this
/// scanner does not model: here-strings, block comments, typographic quotes,
/// and `$(...)` inside double quotes, which runs a command.
fn powershell_command_segments(script: &str) -> Option<Vec<&str>> {
    if script.contains("@'")
        || script.contains("@\"")
        || script.contains("<#")
        || script.contains(['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'])
        || script.contains(['\u{201C}', '\u{201D}', '\u{201E}'])
    {
        return None;
    }
    let mut segments = Vec::new();
    let mut start = 0;
    let mut token_start = true;
    let mut characters = script.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        let at_token_start = token_start;
        token_start = character.is_whitespace();
        match character {
            '\'' => {
                // A doubled quote is a literal quote inside a verbatim string.
                while let Some((_, next)) = characters.next() {
                    if next == '\'' && characters.next_if(|&(_, c)| c == '\'').is_none() {
                        break;
                    }
                }
            }
            '"' => {
                while let Some((_, next)) = characters.next() {
                    match next {
                        '`' => {
                            characters.next();
                        }
                        '$' if characters.peek().is_some_and(|&(_, c)| c == '(') => return None,
                        '"' if characters.next_if(|&(_, c)| c == '"').is_none() => break,
                        _ => {}
                    }
                }
            }
            '`' => {
                characters.next();
            }
            '#' if at_token_start => {
                segments.push(&script[start..index]);
                while characters
                    .next_if(|&(_, c)| c != '\n' && c != '\r')
                    .is_some()
                {}
                start = characters.peek().map_or(script.len(), |&(next, _)| next);
            }
            ';' | '|' | '\n' | '\r' | '{' | '(' => {
                segments.push(&script[start..index]);
                start = index + character.len_utf8();
                token_start = true;
            }
            '&' if characters.next_if(|&(_, c)| c == '&').is_some() => {
                segments.push(&script[start..index]);
                start = index + 2;
                token_start = true;
            }
            _ => {}
        }
    }
    segments.push(&script[start..]);
    Some(segments)
}

/// Whether exit 1 can describe an empty search for the whole command.
/// Compound commands and pipelines may contain earlier matches or failures;
/// their final search's exit code cannot classify the aggregate output.
pub(crate) fn ends_with_native_search(command: &str) -> bool {
    if command.contains("&&") || command.contains("||") {
        return false;
    }
    let Some(last) = powershell_command_segments(command).and_then(|segments| {
        let mut commands = segments.into_iter().map(str::trim).filter(|segment| !segment.is_empty());
        let only = commands.next()?;
        commands.next().is_none().then_some(only)
    }) else {
        return false;
    };
    if last.contains(['}', ')']) {
        return false;
    }
    let program = last
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(['"', '\'']);
    let program = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    matches!(
        program.strip_suffix(".exe").unwrap_or(&program),
        "rg" | "grep" | "findstr"
    )
}

#[derive(Clone, Copy, Default)]
struct LineClassification {
    critical: bool,
    advisory: bool,
    status: bool,
}

pub(crate) fn is_critical_output_line(line: &str) -> bool {
    classify_line(line).critical
}

fn classify_line(line: &str) -> LineClassification {
    let trimmed = line.trim_start();
    LineClassification {
        critical: starts_with_diagnostic_label_ascii_case(trimmed, "error")
            || starts_with_diagnostic_label_ascii_case(trimmed, "failed")
            || starts_with_diagnostic_label_ascii_case(trimmed, "failure")
            || starts_with_diagnostic_label_ascii_case(trimmed, "panic")
            || starts_with_diagnostic_label_ascii_case(trimmed, "fatal")
            || starts_with_ascii_case(trimmed, "fail [")
            || starts_with_ascii_case(trimmed, "--- fail:")
            || strip_prefix_ascii_case(trimmed, "try ").is_some_and(|retry| {
                find_ascii_case(retry, " fail [").is_some_and(|separator| {
                    let attempt = &retry[..separator];
                    !attempt.is_empty() && attempt.bytes().all(|byte| byte.is_ascii_digit())
                })
            })
            || starts_with_ascii_case(trimmed, "failures:")
            // Failed suites must survive later passing status lines in a
            // multi-package run, even when their detailed errors were omitted.
            || contains_ascii_case(line, "test result: failed")
            || starts_with_ascii_case(trimmed, "panicked at ")
            || contains_ascii_case(line, " panicked at ")
            || contains_ascii_case(line, " error:")
            // TypeScript diagnostics use "error TS<digits>:" after a location.
            || find_ascii_case(line, "error ts").is_some_and(|start| {
                line[start + "error ts".len()..].split_once(':').is_some_and(|(number, _)| {
                    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                })
            })
            || contains_ascii_case(line, "npm err!"),
        advisory: contains_word_ascii_case(line, "warning")
            || trimmed.starts_with("-->")
            || starts_with_ascii_case(trimmed, "note:")
            || starts_with_ascii_case(trimmed, "help:"),
        status: contains_ascii_case(line, "test result:")
            || contains_ascii_case(line, "failures:")
            || contains_ascii_case(line, "failed.")
            || (contains_word_ascii_case(line, "passed")
                && (starts_with_ascii_case(trimmed, "test ")
                    || starts_with_ascii_case(trimmed, "tests ")
                    || starts_with_ascii_case(trimmed, "suite ")
                    || find_ascii_case(trimmed, " passed").is_some_and(|separator| {
                        let count = &trimmed[..separator];
                        !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit())
                    })))
            || contains_ascii_case(line, "finished ")
            || starts_with_diagnostic_label_ascii_case(trimmed, "error")
            || starts_with_ascii_case(trimmed, "summary:")
            || starts_with_ascii_case(trimmed, "summary ["),
    }
}

fn contains_word_ascii_case(line: &str, word: &str) -> bool {
    line.split(|character: char| {
        !character.is_alphanumeric() && character != '_' && character != '-'
    })
    .any(|candidate| candidate.eq_ignore_ascii_case(word))
}

fn starts_with_diagnostic_label_ascii_case(line: &str, label: &str) -> bool {
    strip_prefix_ascii_case(line, label).is_some_and(|remainder| {
        matches!(
            remainder.as_bytes().first(),
            None | Some(b':') | Some(b'[') | Some(b'.') | Some(b' ')
        )
    })
}

fn starts_with_ascii_case(line: &str, prefix: &str) -> bool {
    strip_prefix_ascii_case(line, prefix).is_some()
}

fn strip_prefix_ascii_case<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    line.as_bytes()
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix.as_bytes()))
        .then(|| &line[prefix.len()..])
}

fn contains_ascii_case(line: &str, needle: &str) -> bool {
    find_ascii_case(line, needle).is_some()
}

fn find_ascii_case(line: &str, needle: &str) -> Option<usize> {
    find_ascii_case_bytes(line.as_bytes(), needle.as_bytes())
}

pub(crate) fn find_ascii_case_bytes(line: &[u8], needle: &[u8]) -> Option<usize> {
    let (&first, _) = needle.split_first()?;
    memchr::memchr2_iter(
        first.to_ascii_lowercase(),
        first.to_ascii_uppercase(),
        line,
    )
    .find(|&index| {
        line.get(index..index + needle.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(needle))
    })
}

// The selector retains at most 64 diagnostic groups and a fixed number of
// indexes, regardless of log length. Text is borrowed from the captured output.
const MAX_DIAGNOSTIC_GROUPS: usize = 64;

struct LineSelection {
    indexes: BTreeSet<usize>,
    omitted_groups: usize,
    groups_overflowed: bool,
    collapsed_progress_lines: usize,
}

fn progress_line_key(line: &str) -> Option<String> {
    let kind = classify_line(line);
    // Never normalize diagnostic identities, file inventories, or JSON records.
    if kind.critical || kind.advisory || kind.status
        || (line.trim_start().starts_with(['{', '['])
            && serde_json::from_str::<serde_json::Value>(line).is_ok())
        || !(contains_word_ascii_case(line, "elapsed")
            || contains_word_ascii_case(line, "progress"))
        || !line.bytes().any(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    static COUNTERS: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(|| {
        regex_lite::Regex::new(concat!(
            r"(?i)\b(?P<elapsed>elapsed\s+)\d+(?:\.\d+)?(?P<unit>ms|s)\b",
            r"|\b(?P<progress>progress\s+)\d+(?P<total>/\d+|%)"
        )).expect("valid progress counters")
    });
    COUNTERS.is_match(line).then(|| COUNTERS.replace_all(line,
        "${elapsed}${progress}<counter>${unit}${total}").into_owned())
}

fn select_lines(output: &str, line_count: usize, failed: bool, validation: bool) -> LineSelection {
    let mut progress = std::collections::BTreeMap::new();
    let mut collapsed_progress_lines = 0;
    for (index, line) in output.lines().enumerate() {
        if let Some(key) = progress_line_key(line) {
            // Bound metadata even for a pathological stream of distinct progress.
            if (progress.len() < MAX_DIAGNOSTIC_GROUPS || progress.contains_key(&key))
                && progress.insert(key, index).is_some()
            {
                collapsed_progress_lines += 1;
            }
        }
    }
    let mut groups: Vec<(Option<String>, usize)> = Vec::new();
    let mut groups_overflowed = false;
    let mut lines = output.lines().enumerate().peekable();
    while let Some((index, line)) = lines.next() {
        if !classify_line(line).critical { continue; }
        // Headlines are not block identities. Keep the complete bounded
        // assertion/owner context; unknown or oversized blocks stay distinct.
        let recognized = ["error:", "error[", "thread '", "FAILED ", "--- FAIL:"].iter()
            .any(|prefix| line.trim_start().starts_with(prefix));
        let mut bounded = recognized && line.len() <= 4096;
        let mut block = if bounded { line.to_string() } else { String::new() };
        for (offset, (_, next)) in lines.clone().enumerate() {
            if !bounded { break; }
            if next.is_empty() || classify_line(next).critical { break; }
            if offset >= 64 || block.len() + next.len() + 1 > 4096 {
                bounded = false;
                break;
            }
            block.push('\n');
            block.push_str(next);
        }
        let identity = bounded.then_some(block);
        if identity.is_none() || !groups.iter().any(|(text, _)| *text == identity) {
            if groups.len() == MAX_DIAGNOSTIC_GROUPS {
                groups_overflowed = true;
                // Retain the latest distinct group as well as the stable prefix.
                groups.pop();
            }
            groups.push((identity, index));
        }
    }
    let group_count = groups.len();
    let critical: Vec<usize> = if group_count <= MAX_FOCUS_MATCHES {
        groups.iter().map(|(_, index)| *index).collect()
    } else {
        groups[..MAX_FOCUS_MATCHES / 2]
            .iter()
            .chain(groups[group_count - MAX_FOCUS_MATCHES / 2..].iter())
            .map(|(_, index)| *index)
            .collect()
    };
    let mut indexes = BTreeSet::new();
    for &index in &critical {
        indexes.extend(
            index.saturating_sub(FOCUS_CONTEXT_LINES)
                ..(index + FOCUS_CONTEXT_LINES + 1).min(line_count),
        );
    }
    let mut advisory_slots = MAX_FOCUS_MATCHES - critical.len();
    let mut statuses = VecDeque::new();
    for (index, line) in output.lines().enumerate() {
        let classification = classify_line(line);
        if classification.advisory && advisory_slots > 0 && !indexes.contains(&index) {
            indexes.extend(
                index.saturating_sub(FOCUS_CONTEXT_LINES)
                    ..(index + FOCUS_CONTEXT_LINES + 1).min(line_count),
            );
            advisory_slots -= 1;
        }
        if (failed || validation) && classification.status && !indexes.contains(&index) {
            if statuses.len() == MAX_STATUS_MATCHES {
                statuses.pop_front();
            }
            statuses.push_back(index);
        }
    }
    indexes.extend(statuses);
    if !(failed || validation) || indexes.is_empty() {
        indexes.extend(0..SUCCESS_HEAD_LINES.min(line_count));
    }
    let tail = if failed {
        FAILURE_TAIL_LINES
    } else if validation {
        VALIDATION_SUCCESS_TAIL_LINES
    } else {
        SUCCESS_TAIL_LINES
    };
    indexes.extend(line_count.saturating_sub(tail)..line_count);
    if !(failed || validation) && line_count <= COMPLETE_SUCCESS_MAX_LINES {
        indexes.extend(0..line_count);
    }
    indexes.extend(progress.values().copied());
    for (index, line) in output.lines().enumerate() {
        if let Some(key) = progress_line_key(line)
            && progress.get(&key).is_some_and(|latest| *latest != index)
        {
            indexes.remove(&index);
        }
    }
    // Count groups whose representative is absent, including context/tail
    // coverage. After overflow this is a lower bound, never a claim of completeness.
    let omitted_groups = groups
        .iter()
        .filter(|(_, index)| !indexes.contains(index))
        .count()
        + usize::from(groups_overflowed);
    LineSelection {
        indexes,
        omitted_groups,
        groups_overflowed,
        collapsed_progress_lines,
    }
}

#[derive(Clone)]
struct SummaryBuilder {
    text: String,
    lines: usize,
    capped: bool,
}

impl SummaryBuilder {
    fn new() -> Self {
        Self {
            text: String::new(),
            lines: 0,
            capped: false,
        }
    }

    fn push_line(&mut self, line: impl AsRef<str>) -> bool {
        if self.is_full() {
            self.capped = true;
            return false;
        }

        let line = line.as_ref();
        let separator_bytes = usize::from(!self.text.is_empty());
        let remaining = SUMMARY_MAX_BYTES
            .saturating_sub(SUMMARY_FOOTER_BYTES)
            .saturating_sub(self.text.len())
            .saturating_sub(separator_bytes);
        if remaining == 0 {
            self.capped = true;
            return false;
        }
        let rendered = if line.len() > remaining {
            self.capped = true;
            summarize_oversized_line(line, remaining)
        } else {
            std::borrow::Cow::Borrowed(line)
        };

        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(&rendered);
        self.lines += 1;
        true
    }

    fn is_full(&self) -> bool {
        self.lines >= SUMMARY_MAX_LINES - SUMMARY_FOOTER_LINES
            || self.text.len() >= SUMMARY_MAX_BYTES - SUMMARY_FOOTER_BYTES
    }

    fn finish(mut self, emitted_source_lines: usize, original_lines: usize) -> Option<String> {
        if self.text.trim().is_empty() {
            return None;
        }
        let _ = write!(
            self.text,
            "\n- emitted_source_lines: {emitted_source_lines}\n- omitted_source_lines: {}",
            original_lines.saturating_sub(emitted_source_lines),
        );
        if self.capped && !self.text.ends_with("[summary capped]") {
            if !self.text.is_empty() {
                self.text.push('\n');
            }
            self.text.push_str("[summary capped]");
        }
        Some(self.text)
    }
}

fn summarize_oversized_line(line: &str, max_bytes: usize) -> std::borrow::Cow<'_, str> {
    const MARKER: &str = " ... [line truncated] ... ";
    if line.len() <= max_bytes {
        return std::borrow::Cow::Borrowed(line);
    }
    if max_bytes <= MARKER.len() {
        return std::borrow::Cow::Borrowed(&line[..line.floor_char_boundary(max_bytes)]);
    }

    let payload_bytes = max_bytes - MARKER.len();
    let head_bytes = payload_bytes / 2;
    let tail_bytes = payload_bytes - head_bytes;
    let head = &line[..line.floor_char_boundary(head_bytes)];
    let tail_start = line.ceil_char_boundary(line.len().saturating_sub(tail_bytes));
    let tail = &line[tail_start..];
    std::borrow::Cow::Owned(format!("{head}{MARKER}{tail}"))
}

#[cfg(test)]
mod optimization_tests {
    use super::*;

    #[test]
    fn diagnostic_search_preserves_ascii_case_and_utf8_offsets() {
        for (line, needle, expected) in [
            ("界 ERROR: failed", "error:", Some(4)),
            ("error er", "error:", None),
            ("x eRrOr: y ERROR:", "error:", Some(2)),
            ("ordinary output", "npm err!", None),
        ] {
            assert_eq!(find_ascii_case(line, needle), expected);
        }
    }

    #[test]
    fn selection_storage_is_bounded_for_large_logs() {
        let output = "error: repeated failure\nordinary context\n".repeat(100_000);
        let selection = select_lines(&output, 200_000, true, true);
        assert!(
            selection.indexes.len()
                <= FAILURE_TAIL_LINES + MAX_FOCUS_MATCHES * 7 + MAX_STATUS_MATCHES
        );
        assert!(selection.indexes.contains(&0));
        assert!(selection.indexes.contains(&199_999));
        assert_eq!(selection.omitted_groups, 0);
    }

    #[test]
    fn line_classification_is_case_insensitive_without_retained_scratch() {
        let classification = classify_line("  ERROR: warning; tests PASSED");
        assert!(classification.critical);
        assert!(classification.advisory);
        assert!(classification.status);
        let classification = classify_line("ordinary output");
        assert!(!classification.critical);
        assert!(!classification.advisory);
        assert!(!classification.status);
    }

    #[test]
    fn selection_flags_emit_each_index_once_in_source_order() {
        let lines = [
            "warning: advisory",
            "before",
            "error: exact failure",
            "after one",
            "after two",
            "after three",
            "tail one",
            "tail two",
        ];
        let selection = select_lines(&lines.join("\n"), lines.len(), true, true);
        let ordered = selection.indexes.into_iter().collect::<Vec<_>>();

        assert_eq!(ordered, (0..lines.len()).collect::<Vec<_>>());
        let mut sorted = ordered.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ordered.len());
    }
}

#[cfg(test)]
#[path = "shell_output_summary_tests.rs"]
mod tests;
