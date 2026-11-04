# Versioned Python compatibility inputs

`update_sdk_artifacts.py` owns these inputs alongside its existing schema
normalization and retained-wire compatibility passes. They supplement Python
bindings only; they are not server schemas or RPC registrations.

## Legacy thread goals: CLI 0.149.0

`thread_goals.cli-0.149.0.json` preserves the SDK's deliberate older-runtime
Goal API. The SDK currently pins `openai-codex-cli-bin==0.149.0`; the low-level
client explicitly diagnoses Goal method-not-found as requiring an older
compatible runtime. Current Rust has retired thread goals. Do not restore its
registrations, add these methods to current protocol unions, or remove public
Python helpers merely because the current runtime no longer supports them.

No configured legacy schema snapshot existed when this input was established.
Its provenance records the SHA-256 and complete model ranges of the retained
pre-refresh Python module plus the notification registry's identity. Definitions
were exported from those freshly loaded Pydantic models with wire aliases;
transitive Goal models were deduplicated, and RequestId references intentionally
reuse the current bounded protocol definition. This is a retained compatibility
contract, not a claim that the current server supports these methods.

Generation merges standalone Goal definitions into the Python schema input,
then uses the same datamodel-code-generator, normalization, staging, rollback
and fresh-module public-API owner. The two legacy notification mappings also
feed registry generation and turn-id metadata, so existing decoding/routing
continues to receive typed payloads. Current client/server unions remain based
solely on the current Rust export. The ergonomic api.py generator consumes the
same freshly generated module once; no stale generated module is imported.

Definition-name or notification-method overlap with the current schema fails
generation before publication, even for apparently identical definitions.
Reconcile the versioned owner explicitly if upstream reintroduces a Goal API.
Do not silently overwrite current contracts or copy generated classes after
generation. New compatibility surfaces require their own scoped provenance.
