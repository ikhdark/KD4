#!/usr/bin/env python3
"""Prompt-cache probe for retirement rewrites on the Responses websocket.

Production retirement keeps every input item before the first retired output
byte-identical, yet the next request usually reuses only about the session's
first prompt. This probe repeats that request pattern with probe-controlled
bytes on the production transport and request fields, so the result separates
a harness cause from provider behaviour, and it tests three transport-level
mitigations that leave the model-visible context unchanged.

Every arm repeat uses its own connection, thread id (prompt_cache_key and
session/thread headers) and nonce. Five read_chunk tool rounds build the
history; the rewrite pins the chunk-2 output and appends a context_pressure
checkpoint, as retirement does.

  A  R1 full, R2-R5 chained, R6 full rewrite (the production pattern).
  B  as A, then R6 = generate:false full exact extension, R7 = full rewrite.
  C  R1-R5 full (never chained), R6 full rewrite.
  D  R1-R2 on stream_id lane "main", R3-R5 forked to lane "work", R6 chained
     to R2 on "main" carrying only the items after R2.

Usage:
  python -I -B scripts/benchmark_scripts/prompt_cache_probe.py plan
  python -I -B scripts/benchmark_scripts/prompt_cache_probe.py selftest
  python -I -B scripts/benchmark_scripts/prompt_cache_probe.py run --execute \\
      --auth-json PATH --arms A --repeats 2 --out results.jsonl
  python -I -B scripts/benchmark_scripts/prompt_cache_probe.py analyze results.jsonl

`run` makes paid model calls and refuses to start without --execute.
Credentials come from --auth-json (read only by `run`) or OPENAI_API_KEY; they
are never printed or written.
"""
from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import math
import os
import random
import tempfile
import time
import uuid
from collections import defaultdict
from pathlib import Path

MODEL = "gpt-6-astra"
REASONING = {"effort": "high", "summary": "detailed", "context": "all_turns"}
TEXT = {"verbosity": "low"}
STREAM_OPTIONS = {"reasoning_summary_delivery": "sequential_cutoff"}
INCLUDE = ["reasoning.encrypted_content"]
BETA_HEADER = "responses_websockets=2026-02-06"
LITE_METADATA_KEY = "ws_request_header_x_openai_internal_codex_responses_lite"
CHATGPT_WS_URL = "wss://chatgpt.com/backend-api/codex/responses"
API_WS_URL = "wss://api.openai.com/v1/responses"

CHUNKS = 5
PINNED = 2                  # chunk whose output the rewrite retires
DEV_WORDS = 1100
CHUNK_WORDS = 3600
TOKENS_PER_WORD = 1.1       # planning estimate only; analysis uses measured usage
ASSUMED_OUTPUT = 300        # planning estimate of reasoning + call tokens per response
ARMS = ("A", "B", "C", "D")
WEIGHTS = (1.0, 0.1, 8.0)   # uncached, cached, output: the replay's w(0.1,8)
CHECKPOINT_NOTE = ("The runtime replaced these consumed tool results with recovery pins to keep the "
                   "context window under its limit. Each pin names its artifact; read_tool_output "
                   "recovers exact bytes when they are needed again.")
TOOL = {
    "type": "function",
    "name": "read_chunk",
    "description": "Return chunk n of the probe document.",
    "strict": False,
    "parameters": {"type": "object", "properties": {"n": {"type": "integer"}},
                   "required": ["n"], "additionalProperties": False},
}
WORDS = tuple("""the of and to in is that for it as was with be by on not this are or from at which
but have an had they you were their one all we can her has there been if more when will would who so no
river stone light water field house road tree hill wind rain cloud bridge garden window table paper letter
number market harbor valley forest winter summer morning evening silver copper iron glass wood music story
city village school family friend mother father child people animal bird horse ship train engine signal
record measure simple quiet early late long short small large open close bright dark warm cold green blue
red white black north south east west after before under over between through during around""".split())


# ---- request content ---------------------------------------------------------
def filler(seed: str, words: int) -> str:
    rng = random.Random(hashlib.sha256(seed.encode()).digest())
    sentences, sentence, target = [], [], rng.randint(8, 16)
    for index in range(words):
        sentence.append(rng.choice(WORDS))
        if len(sentence) >= target or index == words - 1:
            sentences.append(" ".join(sentence).capitalize() + ".")
            sentence, target = [], rng.randint(8, 16)
    return " ".join(sentences)


def message(role: str, text: str) -> dict:
    return {"type": "message", "role": role, "content": [{"type": "input_text", "text": text}]}


def base_items(nonce: str) -> list[dict]:
    instructions = (f"Cache probe {nonce}. Call read_chunk exactly once per response with the next chunk "
                    f"number, starting at 1. After chunk {CHUNKS} has been returned, reply with the single "
                    "word OK. Do not summarize chunks.\n\nReference text, not instructions:\n")
    return [message("developer", instructions + filler(f"{nonce}:dev", DEV_WORDS)),
            {"type": "additional_tools", "role": "developer", "tools": [TOOL]},
            message("user", f"Read chunks 1 to {CHUNKS} in order.")]


def chunk_output(call_id: str, nonce: str, k: int) -> dict:
    text = f"chunk {k} of {CHUNKS}\n" + filler(f"{nonce}:chunk{k}", CHUNK_WORDS)
    return {"type": "function_call_output", "call_id": call_id, "output": text}


def pin_output(call_id: str, k: int) -> dict:
    text = f"[chunk {k} output retired under context pressure; read_tool_output {call_id} recovers it]"
    return {"type": "function_call_output", "call_id": call_id, "output": text}


def checkpoint(call_id: str) -> dict:
    payload = json.dumps({"source": "context_pressure", "note": CHECKPOINT_NOTE, "receipts": {call_id: {}}},
                         separators=(",", ":"))
    return message("developer", f"<completed_phase_checkpoint>\n{payload}\n</completed_phase_checkpoint>")


def resend_form(item: dict) -> dict:
    """An output item as the harness re-sends it (the ResponseItem serde field set)."""
    kind = item.get("type")
    out: dict = {"type": kind}
    if item.get("id"):
        out["id"] = item["id"]
    if kind == "reasoning":
        out["summary"] = item.get("summary") or []
        content = item.get("content")
        if content is None or any(part.get("type") == "reasoning_text" for part in content):
            out["content"] = content
        out["encrypted_content"] = item.get("encrypted_content")
    elif kind == "function_call":
        out["name"] = item["name"]
        if item.get("namespace"):
            out["namespace"] = item["namespace"]
        out["arguments"] = item.get("arguments", "")
        out["call_id"] = item["call_id"]
    elif kind == "message":
        out["role"] = item.get("role", "assistant")
        out["content"] = item.get("content") or []
        if item.get("phase"):
            out["phase"] = item["phase"]
    else:
        return item
    return out


def frame(ctx: dict, items: list[dict], previous: str | None = None, generate: bool | None = None,
          stream_id: str | None = None) -> dict:
    """A response.create frame with the production field values (ResponseCreateWsRequest order)."""
    out: dict = {"type": "response.create", "model": MODEL}
    if previous:
        out["previous_response_id"] = previous
    out["input"] = items
    out.update(tool_choice="auto", parallel_tool_calls=False, reasoning=REASONING, store=False, stream=True,
               stream_options=STREAM_OPTIONS, include=INCLUDE, prompt_cache_key=ctx["thread_id"], text=TEXT)
    if generate is not None:
        out["generate"] = generate
    if stream_id:
        out["stream_id"] = stream_id
    out["client_metadata"] = {"session_id": ctx["session_id"], "thread_id": ctx["thread_id"],
                              LITE_METADATA_KEY: "true"}
    return out


def headers_for(ctx: dict, auth: dict) -> dict:
    headers = {"OpenAI-Beta": BETA_HEADER, "originator": "codex_cli_rs", "session-id": ctx["session_id"],
               "thread-id": ctx["thread_id"], "x-client-request-id": ctx["thread_id"]}
    headers.update(auth)
    return headers


def new_context() -> dict:
    return {"session_id": str(uuid.uuid4()), "thread_id": str(uuid.uuid4()), "nonce": uuid.uuid4().hex[:12]}


# ---- arm sequence -------------------------------------------------------------
class StepFailed(Exception):
    pass


async def run_arm(arm: str, ctx: dict, send) -> None:
    """Send one arm repeat. send(step, kind, frame) returns the completed response record."""
    history = base_items(ctx["nonce"])
    response_ids: dict[int, str] = {}
    call_ids: dict[int, str] = {}
    after_pinned_call = 0
    for k in range(1, CHUNKS + 1):
        lane = None if arm != "D" else ("main" if k <= 2 else "work")
        if k == 1:
            items, previous, kind = history, None, "full"
        else:
            appended = [chunk_output(call_ids[k - 1], ctx["nonce"], k - 1)]
            history = history + appended
            if arm == "C":
                items, previous, kind = history, None, "full"
            else:
                items, previous, kind = appended, response_ids[k - 1], "chained"
        result = await send(f"R{k}", kind, frame(ctx, items, previous, stream_id=lane))
        outputs = [resend_form(item) for item in result["output"]]
        call = next((item for item in outputs if item.get("type") == "function_call"), None)
        if call is None:
            raise StepFailed(f"R{k} returned no function_call")
        response_ids[k], call_ids[k] = result["response_id"], call["call_id"]
        history = history + outputs
        if k == PINNED:
            after_pinned_call = len(history)
    final = history + [chunk_output(call_ids[CHUNKS], ctx["nonce"], CHUNKS)]
    assert final[after_pinned_call].get("call_id") == call_ids[PINNED]
    rewritten = list(final)
    rewritten[after_pinned_call] = pin_output(call_ids[PINNED], PINNED)
    rewritten.append(checkpoint(call_ids[PINNED]))
    if arm == "B":
        await send("R6", "anchor", frame(ctx, final, generate=False))
        await send("R7", "rewrite", frame(ctx, rewritten))
    elif arm == "D":
        await send("R6", "rewrite", frame(ctx, rewritten[after_pinned_call:], response_ids[PINNED], stream_id="main"))
    else:
        await send("R6", "rewrite", frame(ctx, rewritten))


# ---- transport ----------------------------------------------------------------
def usage_fields(usage: dict | None) -> dict:
    usage = usage or {}
    return {"input": usage.get("input_tokens"),
            "cached": (usage.get("input_tokens_details") or {}).get("cached_tokens"),
            "output": usage.get("output_tokens"),
            "reasoning": (usage.get("output_tokens_details") or {}).get("reasoning_tokens")}


async def exchange(ws, payload: dict, timeout: float) -> dict:
    record = {"response_id": None, "status": None, "error": None, "usage": None, "output": [],
              "t_first_s": None, "t_total_s": None}
    done_items: list[dict] = []
    start = time.monotonic()
    await ws.send(json.dumps(payload, separators=(",", ":")))
    while True:
        try:
            raw = await asyncio.wait_for(ws.recv(), timeout)
        except TimeoutError:
            record["status"] = "timeout"
            break
        except Exception as err:  # connection closed or transport failure
            record["status"] = "transport_error"
            record["error"] = {"code": type(err).__name__, "message": str(err)[:300]}
            break
        if record["t_first_s"] is None:
            record["t_first_s"] = round(time.monotonic() - start, 3)
        event = json.loads(raw)
        kind = event.get("type", "")
        response = event.get("response") if isinstance(event.get("response"), dict) else {}
        record["response_id"] = response.get("id") or record["response_id"]
        if kind == "response.output_item.done" and isinstance(event.get("item"), dict):
            done_items.append(event["item"])
        if kind == "error":
            record["status"], record["error"] = "error", event.get("error") or event
            break
        if kind in ("response.completed", "response.incomplete", "response.failed") or (
                kind.startswith("response.") and response.get("status") in ("completed", "failed", "incomplete", "cancelled")):
            record["status"] = response.get("status") or kind.split(".", 1)[1]
            record["usage"], record["error"] = response.get("usage"), response.get("error")
            record["output"] = response.get("output") or done_items
            break
    record["t_total_s"] = round(time.monotonic() - start, 3)
    return record


async def connect(url: str, headers: dict):
    from websockets.asyncio.client import connect as ws_connect
    return await ws_connect(url, additional_headers=headers, compression="deflate", max_size=None,
                            open_timeout=30, ping_interval=20, ping_timeout=60)


async def execute(arms: list[str], repeats: int, url: str, auth: dict, out_path: Path, budget: float,
                  step_timeout: float) -> None:
    spent = 0.0
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with out_path.open("a", encoding="utf-8") as sink:
        for repeat in range(1, repeats + 1):
            for arm in arms:
                ctx = new_context()
                try:
                    ws = await connect(url, headers_for(ctx, auth))
                except Exception as err:  # never echo headers
                    status = getattr(getattr(err, "response", None), "status_code", None)
                    raise SystemExit(f"connect failed: {type(err).__name__}" + (f" HTTP {status}" if status else ""))
                turn_state = bool(ws.response is not None and ws.response.headers.get("x-codex-turn-state"))

                async def send(step: str, kind: str, payload: dict) -> dict:
                    nonlocal spent
                    result = await exchange(ws, payload, step_timeout)
                    usage = usage_fields(result["usage"])
                    spent += usage["input"] or 0
                    error = result["error"] if isinstance(result["error"], dict) else (
                        {"message": str(result["error"])} if result["error"] else None)
                    row = {"arm": arm, "repeat": repeat, "step": step, "kind": kind, "thread_id": ctx["thread_id"],
                           "chained": "previous_response_id" in payload, "stream_id": payload.get("stream_id"),
                           "generate": payload.get("generate"), "input_items": len(payload["input"]),
                           "frame_bytes": len(json.dumps(payload, separators=(",", ":"))),
                           "response_id": result["response_id"], "status": result["status"],
                           "error": {"code": error.get("code"), "message": str(error.get("message", ""))[:300]} if error else None,
                           "usage": usage, "t_first_s": result["t_first_s"], "t_total_s": result["t_total_s"],
                           "turn_state_header": turn_state, "ts": time.time()}
                    sink.write(json.dumps(row) + "\n")
                    sink.flush()
                    print(f"{arm}{repeat} {step:3} {kind:8} status={result['status']} input={usage['input']} "
                          f"cached={usage['cached']} output={usage['output']}"
                          + (f" error={row['error']['code']}" if error else ""))
                    if result["status"] != "completed":
                        raise StepFailed(f"{step} {result['status']}")
                    if spent > budget:
                        raise SystemExit(f"input-token budget {budget:.0f} exceeded ({spent:.0f}); stopped")
                    return result

                try:
                    await run_arm(arm, ctx, send)
                except StepFailed as err:
                    print(f"{arm}{repeat} stopped: {err}")
                finally:
                    await ws.close()


def load_auth(path: str | None) -> tuple[dict, str]:
    if path:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
        tokens = data.get("tokens") or {}
        if tokens.get("access_token"):
            auth = {"Authorization": "Bearer " + tokens["access_token"]}
            if tokens.get("account_id"):
                auth["chatgpt-account-id"] = tokens["account_id"]
            return auth, CHATGPT_WS_URL
        if data.get("OPENAI_API_KEY"):
            return {"Authorization": "Bearer " + data["OPENAI_API_KEY"]}, API_WS_URL
        raise SystemExit("auth file has neither ChatGPT tokens nor OPENAI_API_KEY")
    if os.environ.get("OPENAI_API_KEY"):
        return {"Authorization": "Bearer " + os.environ["OPENAI_API_KEY"]}, API_WS_URL
    raise SystemExit("run needs --auth-json PATH or OPENAI_API_KEY")


# ---- plan -------------------------------------------------------------------
def est_tokens(item: dict) -> float:
    if item.get("type") == "additional_tools":
        return len(json.dumps(item)) / 4
    text = item.get("output") if "output" in item else " ".join(part.get("text", "") for part in item.get("content", []))
    return len(text.split()) * TOKENS_PER_WORD + 8


def floor128(tokens: float) -> int:
    return int(tokens // 128 * 128)


def plan_steps(arm: str, output: float = ASSUMED_OUTPUT) -> list[dict]:
    """Estimated per-step tokens: cached as production behaves (first prompt only after a
    rewrite) and as it would if the provider kept the unchanged prefix."""
    nonce = "plan"
    p1 = sum(est_tokens(item) for item in base_items(nonce))
    chunk = est_tokens(chunk_output("call", nonce, 1))
    pin, ckpt = est_tokens(pin_output("call", PINNED)), est_tokens(checkpoint("call"))
    inputs = [p1 + (k - 1) * (output + chunk) for k in range(1, CHUNKS + 1)]
    steps = []
    for k, value in enumerate(inputs, start=1):
        cached = 0 if k == 1 else floor128(inputs[k - 2] + output)
        steps.append({"step": f"R{k}", "kind": "full" if k == 1 or arm == "C" else "chained",
                      "input": value, "cached_observed": cached, "cached_kept": cached, "output": output})
    final = p1 + CHUNKS * (output + chunk)
    rewrite = final - chunk + pin + ckpt
    prefix_before_pin = inputs[PINNED - 1] + output
    rewrite_step = {"step": "R6", "kind": "rewrite", "input": rewrite, "output": output,
                    "cached_observed": max(0, floor128(p1) - 128), "cached_kept": floor128(prefix_before_pin)}
    if arm == "B":
        steps.append({"step": "R6", "kind": "anchor", "input": final, "output": 0,
                      "cached_observed": floor128(inputs[-1] + output), "cached_kept": floor128(inputs[-1] + output)})
        rewrite_step["step"] = "R7"
    steps.append(rewrite_step)
    return steps


def weighted(uncached: float, cached: float, output: float) -> float:
    return WEIGHTS[0] * uncached + WEIGHTS[1] * cached + WEIGHTS[2] * output


def plan_totals(arms: list[str], repeats: int, column: str = "cached_observed") -> dict:
    total = defaultdict(float)
    for arm in arms:
        for step in plan_steps(arm):
            total["input"] += step["input"] * repeats
            total["cached"] += step[column] * repeats
            total["output"] += step["output"] * repeats
            total["requests"] += repeats
    total["uncached"] = total["input"] - total["cached"]
    total["weighted"] = weighted(total["uncached"], total["cached"], total["output"])
    return dict(total)


def cmd_plan(args) -> None:
    ctx = {"session_id": "<session uuid>", "thread_id": "<thread uuid>"}
    fixed = {k: v for k, v in frame(ctx, []).items() if k != "input"}
    print("Fixed response.create fields (every step):")
    print("  " + json.dumps(fixed))
    print("Handshake headers: OpenAI-Beta: " + BETA_HEADER + ", originator, session-id, thread-id, "
          "x-client-request-id, Authorization (+ chatgpt-account-id for ChatGPT auth)")
    print(f"Content: developer instructions + {DEV_WORDS} filler words, additional_tools[read_chunk], user;"
          f" {CHUNKS} tool rounds, each output {CHUNK_WORDS} filler words; rewrite pins chunk {PINNED} and"
          " appends a context_pressure checkpoint. Estimates assume "
          f"{TOKENS_PER_WORD} tokens/word and {ASSUMED_OUTPUT} output tokens per response.")
    for arm in ARMS:
        print(f"\nArm {arm}  step  kind      input   cached(production-like)  cached(prefix kept)  output")
        for step in plan_steps(arm):
            print(f"        {step['step']:4}  {step['kind']:8} {step['input']:7.0f}   {step['cached_observed']:7d}"
                  f"                  {step['cached_kept']:7d}           {step['output']:5.0f}")
    for label, arms in (("Stage 1 (A)", ["A"]), ("Stage 2 (B, C, D)", ["B", "C", "D"])):
        t = plan_totals(arms, args.repeats)
        print(f"\n{label} x{args.repeats} repeats: {t['requests']:.0f} requests, input {t['input'] / 1e3:.0f}k "
              f"(uncached {t['uncached'] / 1e3:.0f}k, cached {t['cached'] / 1e3:.0f}k), output {t['output'] / 1e3:.1f}k, "
              f"weighted w(0.1,8) {t['weighted'] / 1e3:.0f}k")
    print("\nDecision rules: see `analyze`; a rewrite keeps the prefix when cached >= floor128(input(R2) + "
          "output(R2)) - 256 and falls back to the first prompt when cached <= input(R1) + 128.")


# ---- analysis -----------------------------------------------------------------
def classify_repeat(arm: str, steps: dict) -> dict:
    def usage(step: str, field: str) -> int:
        return ((steps.get(step) or {}).get("usage") or {}).get(field) or 0

    for k in range(1, CHUNKS + 1):
        row = steps.get(f"R{k}")
        if row is None or row.get("status") != "completed":
            code = ((row or {}).get("error") or {}).get("code")
            return {"class": "build-up failed", "detail": f"R{k} {row.get('status') if row else 'missing'} {code or ''}".strip()}
    bad = [k for k in range(2, CHUNKS + 1) if usage(f"R{k}", "cached") < 0.95 * usage(f"R{k - 1}", "input")]
    rewrite = steps.get("R7" if arm == "B" else "R6")
    if arm == "B" and (steps.get("R6") or {}).get("status") != "completed":
        return {"class": "anchor failed", "detail": str((steps.get("R6") or {}).get("error"))}
    if rewrite is None or rewrite.get("status") != "completed":
        code = ((rewrite or {}).get("error") or {}).get("code")
        return {"class": "rewrite rejected" if code else "rewrite missing", "detail": code or ""}
    p1, t_d = usage("R1", "input"), usage("R2", "input") + usage("R2", "output")
    cached = rewrite["usage"]["cached"] or 0
    if cached >= floor128(t_d) - 256:
        verdict = "prefix kept"
    elif cached <= p1 + 128:
        verdict = "first prompt only"
    else:
        verdict = "partial"
    out = {"class": verdict, "valid": not bad, "p1": p1, "prefix_before_pin": t_d, "rewrite_input": rewrite["usage"]["input"],
           "rewrite_cached": cached, "build_up_misses": [f"R{k}" for k in bad]}
    if arm == "B":
        out["anchor_cached_ratio"] = round(usage("R6", "cached") / max(1, usage("R5", "input")), 3)
    return out


def cmd_analyze(args) -> None:
    rows = [json.loads(line) for line in Path(args.results).read_text(encoding="utf-8").splitlines() if line.strip()]
    by_repeat: dict[tuple[str, int], dict] = defaultdict(dict)
    for row in rows:
        by_repeat[(row["arm"], row["repeat"])][row["step"]] = row
    classes: dict[str, list[dict]] = defaultdict(list)
    for (arm, repeat), steps in sorted(by_repeat.items()):
        result = classify_repeat(arm, steps)
        classes[arm].append(result)
        print(f"{arm}{repeat}: " + ", ".join(f"{k}={v}" for k, v in result.items()))
    print()
    for arm in sorted(classes):
        valid = [r for r in classes[arm] if r.get("valid")]
        kinds = {r["class"] for r in valid}
        rejected = [r for r in classes[arm] if r["class"] in ("rewrite rejected", "build-up failed")]
        if arm == "A":
            if len(valid) >= 2 and kinds <= {"first prompt only", "partial"} and all(
                    r["rewrite_cached"] < r["prefix_before_pin"] - 256 for r in valid):
                text = "provider behaviour: the unchanged prefix is not reused (harness cause rejected)"
            elif len(valid) >= 2 and kinds == {"prefix kept"}:
                text = "provider keeps the unchanged prefix: the production loss is harness-specific"
            else:
                text = "inconclusive: run one more A repeat"
        elif len(valid) >= 2 and kinds == {"prefix kept"}:
            text = "mitigation keeps the prefix"
        elif (len(valid) >= 2 and "prefix kept" not in kinds) or (rejected and not valid):
            text = "mitigation fails" + (f" ({rejected[0]['class']}: {rejected[0]['detail']})" if rejected and not valid else "")
        else:
            text = "inconclusive"
        print(f"Arm {arm}: {text}  [{len(valid)} valid of {len(classes[arm])}]")


# ---- selftest -------------------------------------------------------------------
async def mock_responses(ws) -> None:
    """Loopback stand-in with the production-like cache behaviour: the latest state per lane
    serves chained and exact-extension requests; other requests reuse only the first prompt."""
    lanes: dict[str, dict] = {}
    first_prompt: list[str] | None = None
    counter = 0

    def tokens(seq: list[str]) -> int:
        return sum(len(part) // 4 for part in seq)

    async for raw in ws:
        request = json.loads(raw)
        previous = request.get("previous_response_id")
        parent = next((r for r in lanes.values() if r["id"] == previous), None) if previous else None
        if previous and parent is None:
            await ws.send(json.dumps({"type": "error", "error": {"code": "previous_response_not_found",
                                                                 "message": "Previous response not found."}}))
            continue
        seq = (parent["seq"] if parent else []) + [json.dumps(item, sort_keys=True) for item in request["input"]]
        if parent:
            cached = tokens(parent["seq"])
        else:
            extended = [r["seq"] for r in lanes.values() if seq[:len(r["seq"])] == r["seq"]]
            if extended:
                cached = tokens(max(extended, key=len))
            elif first_prompt and seq[:len(first_prompt)] == first_prompt:
                cached = max(0, floor128(tokens(first_prompt)) - 128)
            else:
                cached = 0
        if first_prompt is None:
            first_prompt = seq
        counter += 1
        response_id = f"resp_mock{counter}"
        output: list[dict] = []
        if request.get("generate") is not False:
            done = sum(1 for part in seq if json.loads(part).get("type") == "function_call_output")
            if done < CHUNKS:
                output = [{"type": "reasoning", "id": f"rs_mock{counter}", "summary": [], "encrypted_content": "enc"},
                          {"type": "function_call", "id": f"fc_mock{counter}", "status": "completed", "name": "read_chunk",
                           "arguments": json.dumps({"n": done + 1}), "call_id": f"call_mock{counter}"}]
            else:
                output = [{"type": "message", "id": f"msg_mock{counter}", "role": "assistant",
                           "content": [{"type": "output_text", "text": "OK"}]}]
        usage = {"input_tokens": tokens(seq), "input_tokens_details": {"cached_tokens": floor128(cached)},
                 "output_tokens": 40 if output else 0, "output_tokens_details": {"reasoning_tokens": 30 if output else 0}}
        lanes[request.get("stream_id") or ""] = {"id": response_id, "seq": seq + [json.dumps(resend_form(o), sort_keys=True) for o in output]}
        await ws.send(json.dumps({"type": "response.created", "response": {"id": response_id, "status": "in_progress"}}))
        for item in output:
            await ws.send(json.dumps({"type": "response.output_item.done", "item": item}))
        await ws.send(json.dumps({"type": "response.completed", "response": {
            "id": response_id, "status": "completed", "output": output, "usage": usage}}))


async def selftest(out_path: Path) -> None:
    from websockets.asyncio.server import serve
    async with serve(mock_responses, "127.0.0.1", 0, max_size=None) as server:
        port = server.sockets[0].getsockname()[1]
        await execute(list(ARMS), 2, f"ws://127.0.0.1:{port}/responses", {"Authorization": "Bearer selftest"},
                      out_path, float("inf"), 30.0)


def cmd_selftest(args) -> None:
    with tempfile.TemporaryDirectory() as tmp:
        out_path = Path(tmp) / "selftest.jsonl"
        asyncio.run(selftest(out_path))
        print()
        cmd_analyze(argparse.Namespace(results=str(out_path)))


def cmd_run(args) -> None:
    if not args.execute:
        raise SystemExit("run makes paid model calls; add --execute after approval")
    arms = [arm.strip().upper() for arm in args.arms.split(",") if arm.strip()]
    if not arms or any(arm not in ARMS for arm in arms):
        raise SystemExit(f"--arms must be a comma list of {','.join(ARMS)}")
    auth, default_url = load_auth(args.auth_json)
    budget = args.max_input_tokens or math.ceil(1.5 * plan_totals(arms, args.repeats)["input"])
    print(f"arms={','.join(arms)} repeats={args.repeats} input-token budget={budget}")
    asyncio.run(execute(arms, args.repeats, args.ws_url or default_url, auth, Path(args.out), budget, args.step_timeout))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    plan = sub.add_parser("plan", help="print the request plan and expected consumption (no network)")
    plan.add_argument("--repeats", type=int, default=2)
    plan.set_defaults(func=cmd_plan)
    sub.add_parser("selftest", help="run every arm against a loopback mock").set_defaults(func=cmd_selftest)
    run = sub.add_parser("run", help="execute arms against the provider (paid)")
    run.add_argument("--execute", action="store_true", help="required: confirms paid calls were approved")
    run.add_argument("--arms", default="A")
    run.add_argument("--repeats", type=int, default=2)
    run.add_argument("--auth-json", help="auth.json to read credentials from (ChatGPT tokens or OPENAI_API_KEY)")
    run.add_argument("--ws-url", help="override the websocket URL")
    run.add_argument("--out", default="prompt_cache_probe_results.jsonl")
    run.add_argument("--max-input-tokens", type=int, help="stop once measured input tokens exceed this")
    run.add_argument("--step-timeout", type=float, default=300.0)
    run.set_defaults(func=cmd_run)
    analyze = sub.add_parser("analyze", help="apply the decision rules to a results file")
    analyze.add_argument("results")
    analyze.set_defaults(func=cmd_analyze)
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
