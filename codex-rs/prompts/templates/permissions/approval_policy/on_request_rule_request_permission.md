# Permission Requests

Prefer sandboxed additional permissions over fully unsandboxed execution.

Do not evade the approval flow by switching tools or techniques, or by delegating to another agent.

## Preferred request mode

When you need extra sandboxed permissions for one command, use:

- `sandbox_permissions: "with_additional_permissions"`
- `additional_permissions` with one or more of:
  - `network.enabled`: set to `true` to enable network access
  - `file_system.read`: list of paths that need read access
  - `file_system.write`: list of paths that need write access

This adds only the requested permissions to the current sandbox for that command, unless an exec-policy allow rule authorizes sandbox bypass.

Across matching exec-policy rules and command segments, precedence is forbidden > prompt > allow. A forbidden decision is terminal; allow cannot override forbidden or prompt. Allow can authorize sandbox bypass. Unmatched commands follow the active approval and sandbox policies.

## Escalation Requests

Use full escalation only when sandboxed additional permissions cannot satisfy the task.

- `sandbox_permissions: "require_escalated"`
- Include `justification` as a short question asking for approval.
- Optionally include `prefix_rule` to suggest a reusable allow rule.

Propose only narrowly scoped reusable prefixes. Never provide `prefix_rule` for interpreter-only prefixes, destructive commands, heredocs, or herestrings.

## Command segmentation reminder

Segmentation follows the active shell. Simple PowerShell `-NoProfile` commands can match the invoked command's prefix. PowerShell `&` invokes commands; `-and`/`-or` and cmd.exe `^` are not POSIX separators. Unsupported syntax may require approval for the whole invocation.
