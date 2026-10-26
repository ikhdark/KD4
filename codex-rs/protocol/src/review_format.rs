use crate::protocol::ReviewFinding;
use crate::protocol::ReviewOutputEvent;

// These helpers return plain strings that higher layers may style as needed.

fn format_location(item: &ReviewFinding) -> String {
    let path = item.code_location.absolute_file_path.display();
    let start = item.code_location.line_range.start;
    let end = item.code_location.line_range.end;
    format!("{path}:{start}-{end}")
}

/// Fallback text used when a review contains no displayable output.
pub const REVIEW_FALLBACK_MESSAGE: &str = "Reviewer failed to output a response.";

/// Format a full review findings block as plain text lines.
///
/// - When `selection` is `Some`, each item line includes a checkbox marker:
///   "[x]" for selected items and "[ ]" for unselected. Missing indices
///   default to selected.
/// - When `selection` is `None`, the marker is omitted and a simple bullet is
///   rendered ("- Title — path:start-end").
pub fn format_review_findings_block(
    findings: &[ReviewFinding],
    selection: Option<&[bool]>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(String::new());

    // Header
    if findings.len() > 1 {
        lines.push("Full review comments:".to_string());
    } else {
        lines.push("Review comment:".to_string());
    }

    for (idx, item) in findings.iter().enumerate() {
        lines.push(String::new());

        let title = canonical_review_title(&item.title, item.priority);
        let location = format_location(item);

        if let Some(flags) = selection {
            // Default to selected if index is out of bounds.
            let checked = flags.get(idx).copied().unwrap_or(true);
            let marker = if checked { "[x]" } else { "[ ]" };
            lines.push(format!("- {marker} {title} — {location}"));
        } else {
            lines.push(format!("- {title} — {location}"));
        }

        for body_line in item.body.lines() {
            lines.push(format!("  {body_line}"));
        }
    }

    lines.join("\n")
}

/// Render a human-readable review summary suitable for a user-facing message.
///
/// Preserve the explicit verdict even for historical results with no explanation.
pub fn render_review_output_text(output: &ReviewOutputEvent) -> String {
    let verdict = match output.overall_correctness.as_str() {
        "patch is correct" => "Review verdict: patch is correct.",
        "patch is incorrect" => "Review verdict: patch is incorrect.",
        _ => "Review verdict unavailable.",
    };
    let mut sections = vec![verdict.to_string()];
    let explanation = output.overall_explanation.trim();
    if !explanation.is_empty() {
        sections.push(explanation.to_string());
    }
    if !output.findings.is_empty() {
        let findings = format_review_findings_block(&output.findings, /*selection*/ None);
        let trimmed = findings.trim();
        if !trimmed.is_empty() {
            sections.push(trimmed.to_string());
        }
    }
    sections.join("\n\n")
}

/// Numeric priority is authoritative; never display a contradictory model marker.
pub fn canonical_review_title(title: &str, priority: i32) -> String {
    let title = title.trim();
    let substance = ["[P0]", "[P1]", "[P2]", "[P3]"]
        .into_iter()
        .find_map(|prefix| title.strip_prefix(prefix))
        .unwrap_or(title)
        .trim_start();
    if (0..=3).contains(&priority) {
        format!("[P{priority}] {substance}")
    } else {
        substance.to_string()
    }
}

