# Configuration documentation

See the current Codex configuration documentation:

- [Configuration reference](https://developers.openai.com/codex/config-file/config-reference)
- [Model Context Protocol (MCP)](https://developers.openai.com/codex/extend/mcp)

For the exact keys supported by this checkout, see the generated
[`core/config.schema.json`](core/config.schema.json).

`compact_prompt` applies to both initial and incremental local compaction summaries. Setting it
also selects local compaction when a provider supports the remote compaction endpoint, because the
remote endpoint cannot receive the custom prompt text.
