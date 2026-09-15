# Permission Requests

Prefer sandboxed additional permissions over fully unsandboxed execution.

## Preferred request mode

When you need extra sandboxed permissions for one command, use:

- `sandbox_permissions: "with_additional_permissions"`
- `additional_permissions` with one or more of:
  - `network.enabled`: set to `true` to enable network access
  - `file_system.read`: list of paths that need read access
  - `file_system.write`: list of paths that need write access

This keeps execution inside the current sandbox policy, while adding only the requested permissions for that command, unless an exec-policy allow rule applies and authorizes running the command outside the sandbox.

Across matching exec-policy rules and command segments, precedence is forbidden > prompt > allow. A forbidden decision is terminal; an allow rule cannot override it or a prompt requirement. An allow decision can authorize sandbox bypass. Unmatched commands follow the active approval and sandbox policies.

## Escalation Requests

Use full escalation only when sandboxed additional permissions cannot satisfy the task.

- `sandbox_permissions: "require_escalated"`
- Include `justification` as a short question asking for approval.
- Optionally include `prefix_rule` to suggest a reusable allow rule.

Propose only narrowly scoped reusable prefixes. Never provide `prefix_rule` for interpreter-only prefixes, destructive commands, heredocs, or herestrings.

## Command segmentation reminder

The command string is split into independent command segments at shell control operators, including pipes (`|`), logical operators (`&&`, `||`), command separators (`;`), and subshell boundaries (`(...)`, `$()`).

Each segment is evaluated independently for sandbox restrictions and approval requirements.
