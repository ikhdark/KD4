//! Stable projection of the known Desktop envelope. Unknown instructions stay eager.

use std::borrow::Cow;

pub(crate) const LOCATOR: &str = "context:desktop";

fn is_deferred_heading(heading: &str) -> bool {
    matches!(heading, "Automations" | "Thread Coordination" | "Worktrees"
        | "Sidebar Organization" | "Pull request diff links"
        | "Inline Code Comments" | "Inline Artifact Follow-Ups")
}

/// Published with the router's existing inventory notice, not another inventory
/// lookup or persistent context owner. Include deferred tools as available too.
pub(crate) fn unavailable_tools(instructions: &str, available: &std::collections::HashSet<&str>) -> String {
    if matches!(project(instructions), Cow::Borrowed(_)) { return String::new(); }
    let Some(block) = full(instructions) else { return String::new(); };
    let mut notes = Vec::new();
    for section in block.split("\n### ").skip(1) {
        let heading = section.lines().next().unwrap_or_default().trim();
        if !is_deferred_heading(heading) { continue; }
        let mut missing = std::collections::BTreeSet::new();
        for name in section.split('`').skip(1).step_by(2) {
            // Desktop's callable references are backticked snake_case names,
            // not arbitrary code snippets, arguments, paths or directives.
            if name.contains('_') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !available.contains(name)
            {
                missing.insert(name);
            }
        }
        if !missing.is_empty() {
            notes.push(format!("{heading}: {}", missing.into_iter().collect::<Vec<_>>().join(", ")));
        }
    }
    if notes.is_empty() { return String::new(); }
    format!("Deferred Desktop guidance references tools absent from the current inventory (including deferred tools): {}. These names cannot be discovered with tool_search in this inventory; report unavailable capabilities rather than searching for them.", notes.join("; "))
}

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
        if is_deferred_heading(heading) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_desktop_tools_are_scoped_to_deferred_guidance_and_live_inventory() {
        let instructions = "<app-context>\n# Codex desktop context\n### Thread Coordination\nUse `list_threads` and `send_message_to_thread`.\n### Images/Visuals/Files\nUse `other_tool`.\n</app-context>";
        let available = std::collections::HashSet::from(["list_threads"]);
        let notice = unavailable_tools(instructions, &available);
        assert!(notice.contains("Thread Coordination: send_message_to_thread"));
        assert!(!notice.contains("list_threads") && !notice.contains("other_tool"));
        assert!(unavailable_tools(instructions, &std::collections::HashSet::from(["list_threads", "send_message_to_thread"])).is_empty());
        assert!(unavailable_tools("unknown envelope", &available).is_empty());
        assert!(project(instructions).contains(LOCATOR));
    }
}
