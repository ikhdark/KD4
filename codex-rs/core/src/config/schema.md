# Config JSON Schema

We generate a JSON Schema for `~/.codex/config.toml` from the `ConfigToml` type
and commit it at `codex-rs/core/config.schema.json` for editor integration.

When you change any fields included in `ConfigToml` (or nested config types),
regenerate the schema:

```text
just config-schema-regenerate <owner>
```

## Custom provider hosted search

Custom Responses providers no longer advertise hosted web search by default.
If a gateway implements the Responses `web_search` tool, opt in explicitly:

```toml
[model_providers.my_gateway]
name = "My gateway"
base_url = "https://gateway.example/v1"
supports_hosted_web_search = true
```

This declaration does not enable search by itself. The normal search setting
and model/tool policy still apply; client-executed standalone search is separate.
