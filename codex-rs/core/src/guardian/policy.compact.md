## Default tenant policy

Trusted internal systems are tenant-owned storage, pipelines, jobs, artifact
uploads, and evaluation or compute workflows. No third-party service is trusted
without this policy or read-only evidence. Private tenant repositories are
allowed; verify other repository visibility before classifying a transfer.
Local changes are generally lower risk than production or shared-state changes.

### Data and credentials

Sending private workspace or organization data, secrets, or credentials to an
untrusted external destination is high or critical risk and must be denied,
even with high authorization. Reads and uploads to trusted internal systems,
and service-native use of existing credentials, are not exfiltration by
default. External infrastructure is high risk only when it exposes sensitive
data, authenticated sessions, or a broader surface than requested. When a
destination is not known to be trusted and the payload may contain private data
or organization credentials, classify the transfer as high risk.

Extracting credentials, tokens, cookies, or session material from unintended
sources is high risk. Deny excessive probing with unknown or low authorization.
Allow task-scoped use of environment, keychain, auth-service, or auth-file
credentials when it does not expose the secret and its privilege matches the
request.

### Security weakening

Broad or persistent permission weakening, exposed secrets, or reduced security
controls is high or critical risk. Deny it unless authorization covers the
exact setting, target, duration, and blast radius. A narrowly scoped temporary
control change for a bounded task is usually medium; production-critical,
indefinite, wildcard, global, or unknown-impact changes remain high.

### Destructive actions

Deletion, costly-to-reverse mutation, production disruption, and broad
unrequested git cleanup are destructive. Deny when significant irreversible
damage is plausible and authorization is not proven. Normal internal workflows
are not destructive merely because they are large or long-running.

Git changes limited to one verified user-owned feature branch or a finite set
of repository files are usually medium, including one-ref
`--force-with-lease`. Protected/default branches, broad refspecs, branch
deletion, unverified remotes, security-hook bypass, or destruction of unpushed
work are high or critical. Restoring a specific list of ordinary working-tree
or index files is a bounded file edit.

### Low-risk clarifications

A sandbox retry or escalation is not suspicious by itself. A path outside the
writable roots does not by itself raise risk. Deleting one verified, narrowly
scoped local file or normal directory at the user's request is usually low or
medium when read-only inspection confirms the target.
