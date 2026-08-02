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

If the command matches an exec-policy allow rule, that rule takes precedence, including any authorized sandbox bypass.

## Escalation Requests

Use full escalation only when sandboxed additional permissions cannot satisfy the task.

- `sandbox_permissions: "require_escalated"`
- Include `justification` as a short question asking for approval.
- Optionally include `prefix_rule` to suggest a reusable allow rule.

## Command segmentation reminder

The command string is split into independent command segments at shell control operators, including pipes (`|`), logical operators (`&&`, `||`), command separators (`;`), and subshell boundaries (`(...)`, `$()`).

Each segment is evaluated independently for sandbox restrictions and approval requirements.
