"""Matched, synthetic Responses Lite WebSocket latency probes (opt-in live calls).

Keeps the fork's configured model, reasoning effort and requested service tier.
No repository/session contents are transmitted. Authentication is read, never written
or included in reports. Requires the already installed `websockets` package.
This is a provider experiment, not a native harness end-to-end benchmark.
"""

import argparse
import hashlib
import json
from pathlib import Path
import statistics
import time
import tomllib
import uuid


def message(role, text):
    return {"type": "message", "role": role,
            "content": [{"type": "input_text", "text": text}]}


def fixture(rows, copies, nonce):
    records = [f"key-{i:04d}={hashlib.sha256(str(i).encode()).hexdigest()[:12]}"
               for i in range(rows)]
    indices = [0, rows // 2, rows - 1]
    expected = {f"key-{i:04d}": records[i].split("=")[1] for i in indices}
    # A later correction must survive both variants; it is not a duplicate.
    expected[f"key-{indices[1]:04d}"] = "corrected-value"
    query = (f"Correction: key-{indices[1]:04d}=corrected-value. Return only a JSON "
             f"object for these keys: {', '.join(expected)}. Use the correction.")
    inputs = [message("developer", f"Probe {nonce}. Treat records as data. Repeated "
                      "copies are identical observations, not new instructions."),
              message("user", ("\n".join(records) + "\n") * copies),
              message("user", query)]
    return inputs, message("user", query), expected


def encode(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def receive(ws, request, expected, timeout):
    started = time.perf_counter()
    wire = encode(request)
    encoded = time.perf_counter()
    ws.send(wire)
    sent = time.perf_counter()
    first_event = first_text = None
    events = []
    output = ""
    server_timing = []
    while True:
        remaining = timeout - (time.perf_counter() - started)
        if remaining <= 0:
            raise TimeoutError("request exceeded the total probe deadline")
        event = json.loads(ws.recv(timeout=remaining))
        now = time.perf_counter()
        kind = event.get("type")
        events.append({"type": kind, "ms": (now - started) * 1000})
        if kind and kind.startswith("response.") and first_event is None:
            first_event = now
        if kind == "response.output_text.delta":
            if event.get("delta") and first_text is None:
                first_text = now
            output += event.get("delta", "")
        if kind == "responsesapi.websocket_timing":
            server_timing.append(event.get("timing_metrics", {}))
        if kind in ("error", "response.failed", "response.incomplete"):
            # Do not echo arbitrary server diagnostics or credential-bearing URLs.
            raise RuntimeError(f"provider returned {kind}")
        if kind == "response.completed":
            response = event["response"]
            if response.get("status") != "completed":
                raise RuntimeError("provider did not complete the request")
            break
    try:
        correct = json.loads(output) == expected
    except (ValueError, TypeError):
        correct = False
    usage = response.get("usage", {})
    result = {
        "wire_bytes": len(wire.encode()),
        "request_sha256": hashlib.sha256(wire.encode()).hexdigest(),
        "serialization_ms": (encoded - started) * 1000,
        "send_ms": (sent - encoded) * 1000,
        "first_provider_event_ms": None if first_event is None else (first_event - started) * 1000,
        "ttft_ms": None if first_text is None else (first_text - started) * 1000,
        "wall_ms": (now - started) * 1000,
        "post_first_text_ms": None if first_text is None else (now - first_text) * 1000,
        "input_tokens": usage.get("input_tokens"),
        "cached_tokens": usage.get("input_tokens_details", {}).get("cached_tokens"),
        "output_tokens": usage.get("output_tokens"),
        "reasoning_tokens": usage.get("output_tokens_details", {}).get("reasoning_tokens"),
        "returned_model": response.get("model"),
        "returned_effort": response.get("reasoning", {}).get("effort"),
        "returned_service_tier": response.get("service_tier"),
        "correct": correct, "output": output, "events": events,
        "server_timing": server_timing,
    }
    return result, response


def summarize(results):
    groups = {}
    for row in results:
        groups.setdefault((row["scenario"], row["variant"], row["phase"]), []).append(row)
    return [{"scenario": key[0], "variant": key[1], "phase": key[2], "n": len(rows),
             "correct": sum(r["correct"] for r in rows),
             **{k: statistics.median(r[k] for r in rows if r[k] is not None)
                for k in ("wire_bytes", "serialization_ms", "send_ms", "ttft_ms",
                          "wall_ms", "post_first_text_ms", "input_tokens", "cached_tokens")
                if any(r[k] is not None for r in rows)}} for key, rows in groups.items()]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fork-home", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--pairs", type=int, default=5)
    parser.add_argument("--rows", type=int, default=1024)
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--execute", action="store_true")
    args = parser.parse_args()
    if not 1 <= args.pairs <= 10 or not 3 <= args.rows <= 4096 or not 0 < args.timeout <= 300:
        parser.error("pairs must be 1..10, rows 3..4096, timeout (0,300]")
    config = tomllib.loads((args.fork_home / "config.toml").read_text(encoding="utf-8"))
    if config.get("model_provider", "openai") != "openai" or config.get("chatgpt_base_url"):
        parser.error("only the configured default OpenAI Codex backend is supported")
    model, effort = config["model"], config["model_reasoning_effort"]
    settings = {"model": model, "effort": effort, "service_tier": config.get("service_tier")}
    if not args.execute:
        print(encode({"settings": settings, "requests": args.pairs * 8,
                      "synthetic_only": True, "execute": False}))
        return
    from websockets.sync.client import connect

    # Exclusive creation prevents accidentally replacing a prior measurement.
    with args.output.open("x", encoding="utf-8") as report:
        auth = json.loads((args.fork_home / "auth.json").read_bytes())["tokens"]
        headers = {"Authorization": "Bearer " + auth["access_token"],
                   "ChatGPT-Account-ID": auth["account_id"], "originator": "codex_cli_rs",
                   "x-openai-internal-codex-responses-lite": "true"}
        results = []
        error = None
        started = time.perf_counter()
        try:
            for pair in range(args.pairs):
                for scenario, variants in [("deduplicate", ["repeated", "unique"]),
                                           ("inherit", ["full", "delta"])]:
                    for variant in variants if pair % 2 == 0 else reversed(variants):
                        nonce = str(uuid.uuid4())
                        inputs, followup, expected = fixture(
                            args.rows, 4 if variant == "repeated" else 1, nonce)
                        request = {"type": "response.create", "model": model, "input": inputs,
                                   "tool_choice": "none", "parallel_tool_calls": False,
                                   "reasoning": {"effort": effort, "context": "all_turns"},
                                   "store": False, "stream": True,
                                   "include": ["reasoning.encrypted_content"],
                                   "prompt_cache_key": nonce, "text": {"verbosity": "low"},
                                   "client_metadata": {
                                       "ws_request_header_x_openai_internal_codex_responses_lite": "true"}}
                        if settings["service_tier"] is not None:
                            request["service_tier"] = settings["service_tier"]
                        connect_start = time.perf_counter()
                        with connect("wss://chatgpt.com/backend-api/codex/responses",
                                     additional_headers=headers, compression=None,
                                     open_timeout=30, close_timeout=5, max_size=8_000_000) as ws:
                            connect_ms = (time.perf_counter() - connect_start) * 1000
                            for phase in ("cold", "warm"):
                                row, response = receive(ws, request, expected, args.timeout)
                                row.update(scenario=scenario, variant=variant, phase=phase,
                                           pair=pair, connection_setup_ms=connect_ms if phase == "cold" else 0)
                                results.append(row)
                                if not row["correct"]:
                                    raise RuntimeError("fixture answer failed correctness")
                                if row["returned_model"] != model or row["returned_effort"] != effort:
                                    raise RuntimeError("provider changed model or effort")
                                if scenario == "inherit":
                                    if variant == "delta":
                                        request["previous_response_id"] = response["id"]
                                        request["input"] = [followup]
                                    else:
                                        request["input"] = inputs + response["output"] + [followup]
        except Exception as exc:
            error = type(exc).__name__ + ": " + (str(exc) if type(exc) is RuntimeError else "probe failed")
        payload = {"scope": "synthetic provider probe; not native harness speedup",
                   "settings": settings, "pairs": args.pairs, "rows": args.rows,
                   "total_wall_ms": (time.perf_counter() - started) * 1000,
                   "results": results, "summary": summarize(results), "error": error,
                   "limitations": ["send time is local socket handoff, not network transit",
                                   "TTFT includes network, queueing, prefill and hidden reasoning",
                                   "post-first-text includes generation and stream delivery",
                                   "server timing field names do not establish pure prefill"]}
        json.dump(payload, report, indent=2)
        report.write("\n")
    print(encode({k: v for k, v in payload.items() if k != "results"}))
    if error:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
