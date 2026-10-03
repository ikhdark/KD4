//! Stable projection of the known Desktop envelope. Unknown instructions stay eager.

use std::borrow::Cow;

pub(crate) const LOCATOR: &str = "context:desktop";

pub(crate) fn full(instructions: &str) -> Option<&str> {
    let start = instructions.find("<app-context>")?;
    let end = start + instructions[start..].find("</app-context>")? + "</app-context>".len();
    Some(&instructions[start..end])
}

pub(crate) fn project(instructions: &str) -> Cow<'_, str> {
    let Some(block) = full(instructions) else {
        return Cow::Borrowed(instructions);
    };
    // Do not interpret arbitrary XML/Markdown as the Desktop's supported envelope.
    if instructions.matches("<app-context>").count() != 1
        || instructions.matches("</app-context>").count() != 1
        || !block
            .trim_start_matches("<app-context>")
            .trim_start()
            .lines()
            .next()
            .is_some_and(|line| line == "# Codex desktop context")
        || block.contains("```")
        || block.contains("~~~")
    {
        return Cow::Borrowed(instructions);
    }
    let body = block
        .strip_prefix("<app-context>")
        .unwrap_or(block)
        .strip_suffix("</app-context>")
        .unwrap_or(block);
    let mut kept = String::new();
    let mut deferred = Vec::new();
    for (index, section) in body.split("\n### ").enumerate() {
        if index == 0 {
            kept.push_str(section);
            continue;
        }
        let heading = section.lines().next().unwrap_or_default().trim();
        // Keep the prefix independent of the current request. Feature-specific
        // guidance remains available through the locator before using a feature.
        if matches!(
            heading,
            "Automations"
                | "Thread Coordination"
                | "Worktrees"
                | "Sidebar Organization"
                | "Pull request diff links"
                | "Inline Code Comments"
                | "Inline Artifact Follow-Ups"
        ) {
            deferred.push(heading);
        } else {
            kept.push_str("\n### ");
            kept.push_str(section);
        }
    }
    if deferred.is_empty() {
        return Cow::Borrowed(instructions);
    }
    kept.push_str(&format!(
        "\n\nAdditional Desktop guidance ({}) is retained at `read_file(path=\"{LOCATOR}\")`. Before using any of these features, read its guidance there. Availability does not authorize actions.\n",
        deferred.join(", ")
    ));
    Cow::Owned(instructions.replacen(block, &format!("<app-context>{kept}</app-context>"), 1))
}
