# Non-interactive Codex

See [the CLI definition](src/cli.rs) for the switches accepted by `codex exec`.

## Named prompt-cache routing

For a series of independent `codex exec` jobs with similar prompts, set
`CODEX_EXEC_CACHE_GROUP` to the same nonempty name in their launch environment.
For example, in PowerShell: `$env:CODEX_EXEC_CACHE_GROUP = 'nightly-review'`.
Use different names for unrelated or separately scheduled job streams. Remove the
variable afterward with `Remove-Item Env:CODEX_EXEC_CACHE_GROUP` to restore the
default for later launches.

The name selects a routing hint, not a stored conversation. Only an opaque,
installation-scoped key is sent. Session IDs and request histories stay separate.
Unset or blank names retain per-session routing; interactive clients, spawned
agents and auxiliary requests do not opt in. Identical prefixes can still miss
the provider cache, and concurrent traffic in one group may contend.
On OpenAI Responses Lite, a named group puts nonempty tool declarations before the base
policy, so editing policy need not invalidate an unchanged tool prefix. Jobs with
frequently changing tool declarations may do better with default session routing.

## Single-phase verification output

Use `codex exec verify` when the caller already knows the phase and needs one
machine-readable check result. It uses the built-in
[verification schema](output-contracts/verification.json) by default. Keep
the caller's check codes and required evidence explicit. The schema
validates the shape, not whether a check actually ran or passed.

From the KD4 checkout:

```powershell
$task = @'
Report only this verification observation: the fresh compile of the selected
revision passed, no native consumer check was observed, and the provider
receipt explicitly recorded no effect. Do not run tools.
Use COMPILE_ONLY for check and "check native consumer" for next_step.
For provider_effect use JSON null for an absent observation, or the exact
JSON string "none" when no effect is explicitly observed.
'@
codex exec verify $task
```

The expected record is
`{"check":"COMPILE_ONLY","next_step":"check native consumer","provider_effect":"none"}`.
Do not treat `null` as `"none"`, or successful compilation as native
verification. For a mixed-phase consumer, an evidence-rich report, or a
required narrative, use ordinary `codex exec` or supply a custom schema with
`--output-schema`. An explicit schema takes precedence on either command.
For the built-in OpenAI provider, exec can submit a direct property of a closed
output object as `anyOf` rather than `oneOf` when every closed branch requires
a different singleton string `kind` value. At most one branch can match, so
strict validation retains the same accepted values. Schemas with reference
keywords, overlapping tags, and other providers keep their original schemas.
`codex exec -- verify` still sends the bare word as a normal prose prompt.
