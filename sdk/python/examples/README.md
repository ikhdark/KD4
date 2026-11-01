# Python SDK Examples

Most example folders contain both runnable versions:

- `sync.py` (public sync surface: `Codex`)
- `async.py` (public async surface: `AsyncCodex`)

`09_async_parity` contains only `sync.py`; the other folders include both files.

All examples intentionally use only public SDK exports from `openai_codex`
and `openai_codex.types`.

Examples use plain strings for text-only turns and typed input objects for
multimodal or structured input lists.

## Prerequisites

- Windows and Python `>=3.10`
- Install the SDK for the same Python interpreter you will use to run examples

This local KD4 checkout is not the SDK distribution on the public package index.
See [local SDK setup](../docs/getting-started.md) for source-only use with an
explicit `CodexConfig.codex_bin`. A dependency-resolving SDK install requests the
`openai-codex-cli-bin` version pinned by `sdk/python/pyproject.toml`.

## Run From A Checkout

Contributors using these checked-in scripts should install development
dependencies from `sdk/python`:

```powershell
uv sync --group dev
.\.venv\Scripts\Activate.ps1
```

The examples bootstrap local SDK imports from `sdk/python/src`. If the pinned
runtime is not already installed, the bootstrap installs the matching runtime
package for the active interpreter from a KD4 release archive and cleans up
temporary staging files afterward. This can download artifacts and install a
package; it does not select a locally rebuilt executable. Do not run these
examples in a no-install or offline-validation task. For local-runtime control,
use the explicit `CodexConfig` pattern in the setup guide instead.

These examples may authenticate and make live model requests. Running them is not
a test-free, offline documentation check.

## Run examples

From `sdk/python`:

```bash
python examples/<example-folder>/sync.py
python examples/<example-folder>/async.py
```

The checked-in examples use the local SDK source tree automatically.

## Recommended first run

```bash
python examples/01_quickstart_constructor/sync.py
python examples/01_quickstart_constructor/async.py
```

## Index

- `01_quickstart_constructor/`
  - first run / sanity check
- `02_turn_run/`
  - inspect full turn output fields
- `03_turn_stream_events/`
  - stream a turn with a small curated event view
- `04_models_and_metadata/`
  - discover visible models for the connected runtime
- `05_existing_thread/`
  - resume a real existing thread (created in-script)
- `06_thread_lifecycle_and_controls/`
  - thread lifecycle + control calls
- `07_image_and_text/`
  - image data URL + text multimodal turn
- `08_local_image_and_text/`
  - local image + text multimodal turn using a generated temporary sample image
- `09_async_parity/`
  - parity-style sync flow (see async parity in other examples)
- `10_error_handling_and_retry/`
  - overload retry pattern + typed error handling structure
- `11_cli_mini_app/`
  - interactive chat loop
- `12_turn_params_kitchen_sink/`
  - structured output with a curated advanced `turn(...)` configuration
- `13_model_select_and_turn_params/`
  - list models, pick highest model + highest supported reasoning effort, run turns, print message
    and usage
- `14_turn_controls/`
  - separate `steer()` and `interrupt()` demos with concise summaries
- `15_login_and_account/`
  - browser-login handle lifecycle, cancellation, and account inspection
