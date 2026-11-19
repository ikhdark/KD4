# Local inference presets

These opt-in presets configure an existing local llama.cpp server, not KD4's
bundled client or hosted OpenAI inference. They do not install an engine, change
the active model, route traffic automatically, or combine optimizations.

Measured setup: Windows Vulkan x64 [llama.cpp b12532][engine] (`08c252e22`),
RTX 4070 Ti, driver 610.47, and [Qwen2.5-Coder-1.5B-Instruct GGUF][weights]
revision `f86cb2c1fa58255f8052cc32aeede1b7482d4361`. Use the matching release
and model files. In the portable engine directory, name the Q8_0 GGUF
`target-q8_0.gguf` and the Q4_K_M GGUF `target-q4_k_m.gguf`. Their SHA-256
digests are, respectively,
`507de59046601282ba768a9789900e6ccf60ed93ddf346730b7c68eb0715bc47` and
`cc324af070c2ecbfd324a30884d2f951a7ff756aba85cb811a6ec436933bb046`.

| Preset | When to use it | Limits |
| --- | --- | --- |
| `SharedFanout` | Concurrent compatible requests when finishing the full wave matters. | Keeps the engine's default continuous batching; the first request took longer under contention. |
| `SourceCopy` | Long outputs that repeat substantial source text, such as a small edit delivered as a full rewrite. | No demonstrated end-to-end win for short-output extraction. Not a default for arbitrary or tool-heavy prompts. |
| `CompactWeights` | Short copy/edit and literal lookup workloads that pass an exact output check. | Q4_K_M changes model weights; it was not qualified as a general replacement for Q8_0. Don't reuse KV across weights. |
| `KvCapacity` | A tight KV memory budget where two long lookups need to coexist. | Q8 KV trades some speed for room; prefer f16 KV when capacity is not limiting. |
| `HostRestore` | A single slot alternating between long prefixes that are likely to recur. | Up to 384 MiB of host prompt cache. Does not signal tool lifecycle, prefetch, or share cache across instances. |

From the KD4 root, load one preset in PowerShell. Use a private API-key file
with one nonempty key per line, outside the checkout; pass its path, not the
key itself. Do not expose this server beyond loopback.

```powershell
$engine = 'C:\path\to\portable-llama'
$keyFile = 'C:\private\llama-api-keys.txt'
$presets = Import-PowerShellDataFile .\scripts\inference\llama-b12532-profiles.psd1
$serverArgs = $presets.Common + $presets.Profiles.SourceCopy
Push-Location -LiteralPath $engine
try {
    & .\llama-server.exe @serverArgs --port 9931 --api-key-file $keyFile
} finally {
    Pop-Location
}
```

The server exposes an HTTP Responses endpoint at
`http://127.0.0.1:9931/v1/responses`. The engine presets have narrower context,
model-quality, and lifecycle coverage than a full KD4 session. Do not set one as
the default provider on the strength of the engine benchmarks.

`SourceCopy` was also measured through the installed KD4 CLI using an isolated
profile, direct tools, greedy generation, and short editor instructions. The
45,056-token server allocation accommodates that harness's context budgeting;
the largest measured input was 31,585 tokens. It is not a model context-length
qualification. Warm exact-copy edits took a median 0.94 s versus 2.18 s without
speculation, with six correct completions per arm across warm and cold runs.

With the same preset serving four concurrent exact-copy edits, prompt lookup
reduced median wave completion from 1.72 s to 0.77 s, with 24/24 exact results
in each arm. When a cold lookup joined an edit, the pair finished sooner, but
the lookup itself slowed from 0.77 s to 0.82 s. Keep this an opt-in for known
copy-heavy output, not a default for mixed traffic or latency-critical lookups.

## Explicit precision routing

[The precision-router preset](llama-b12532-precision-router.ini) serves two
aliases of the same Qwen2.5 checkpoint. Use `coder-reference` by default and
`coder-copy-compact` only for an explicitly qualified, stateless source-copy
request. Set `model` in the Responses request. Keep in-progress work on the
same alias; neither the preset nor KD4 auto-selects precision or migrates KV.

From the KD4 root, resolve the preset path. Then start the router from the
portable engine directory with the private API-key file described above:

```powershell
$routerPreset = (Resolve-Path -LiteralPath '.\scripts\inference\llama-b12532-precision-router.ini').Path
Push-Location -LiteralPath $engine
try {
    & .\llama-server.exe --host 127.0.0.1 --port 9931 --no-ui `
        --models-preset $routerPreset --models-max 2 --no-models-autoload `
        --api-key-file $keyFile
} finally {
    Pop-Location
}
```

Explicitly load each needed alias with the authenticated `POST /models/load`
endpoint and wait for `loaded` in `GET /models` before admitting it. If the
server says the compact alias is not loaded, use the reference alias for a
new stateless request. Don't retry unrelated invalid requests under another
alias.

In the measured closed workload (copies, unclassified copies, and long
lookups), median time for 12 sequential requests was 11.76 s versus 14.56 s
over two launches per arm, with all outputs correct. The second resident
model used about 1.25 GiB more GPU
memory. The router preset keeps Flash Attention off: the Flash Attention-on
reference arm returned incorrect outputs on this Vulkan setup.

[engine]: https://github.com/ggml-org/llama.cpp/releases/tag/b12532
[weights]: https://huggingface.co/Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF/tree/f86cb2c1fa58255f8052cc32aeede1b7482d4361
