"""Offline verification of the saved eight-trial diagnostic benchmark; no provider calls."""
import importlib.util
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("diagnostic_benchmark", HERE / "benchmark.py")
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


def main():
    output = benchmark.OUTPUT
    protocol = json.loads((output / "protocol.json").read_text(encoding="utf-8"))
    report = json.loads((output / "results/report.json").read_text(encoding="utf-8"))
    rows = [json.loads(line) for line in (output / "results/trials.jsonl").read_text(encoding="utf-8").splitlines()]
    events = [json.loads(line) for line in (output / "results/events.jsonl").read_text(encoding="utf-8").splitlines()]
    assert protocol == benchmark.protocol(benchmark.load_fixture()), "input/source identities changed"
    assert len(rows) == 8 and report["completed_trials"] == 8 and report["fatal"] is None
    assert report["hashes_unchanged"] and report["hashes_before"] == report["hashes_after"] == protocol["hashes"]
    assert report["protocol_sha256"] == benchmark.existing.digest(output / "protocol.json")
    assert [{key: row[key] for key in ("repeat", "case", "variant")} for row in rows] == protocol["ordering"]
    assert all(row["status"] == "completed" and not row["completion_error"] and not row["harness_error"] for row in rows)
    assert all(row["assessment"] == benchmark.assess(row["case"], row["answer"], protocol["expected"]) for row in rows)
    assert all(row["assessment"]["correct"] for row in rows)
    assert all(row["packet_sha256"] == protocol["cases"][row["case"]]["packets"][row["variant"]]["packet_sha256"] for row in rows)
    settings = [event["params"]["threadSettings"] for event in events if event.get("method") == "thread/settings/updated"]
    assert len(settings) == 8
    assert all((s["model"], s["modelProvider"], s["effort"]) == ("gpt-6-astra", "openai", "xhigh") for s in settings)
    messages = [event["params"]["item"] for event in events if event.get("method") == "item/completed"]
    users = [item for item in messages if item["type"] == "userMessage"]
    assert len(users) == 8
    for row, user in zip(rows, users):
        text = "\n".join(item.get("text", "") for item in user["content"])
        packet = protocol["cases"][row["case"]]["packets"][row["variant"]]["packet"]
        assert packet in text, "exact packet absent from observed user input"
    tools = [item for item in messages if item["type"] == "dynamicToolCall"]
    assert len(tools) == sum(row["actual_tool_calls"] for row in rows) == 8
    assert all(item["status"] == "completed" and item["success"] is True and item["error"] is None for item in tools)
    assert sum(item["tool"] == "diagnostic_raw_log" for item in tools) == sum(row["raw_log_recoveries"] for row in rows) == 4
    assert not any(event.get("error") or event.get("method") == "error" for event in events)
    assert all(call["outcome"] == "success" for row in rows for call in row["timing"].get("toolCalls", []))
    assert sum(row["model_requests"] for row in rows) == 12
    for key in ("artifact_recoveries", "model_retries", "model_fallbacks", "compactions"):
        assert all(row[key] == 0 for row in rows), key
    receipt = {
        "scope": __doc__, "trials_verified": len(rows), "correct": sum(row["assessment"]["correct"] for row in rows),
        "scorer_checks": len(protocol["scorer_checks"]), "events_scanned": len(events),
        "model_provider_effort": ["gpt-6-astra", "openai", "xhigh"],
        "model_requests": 12, "all_tool_calls": 8, "direct_raw_log_recoveries": 4,
        "tool_failures": 0, "rpc_or_turn_errors": 0,
        "manual_review": "See REPORT.md: all eight diagnoses reviewed; trial 5 has a non-passing-proof wording caveat.",
        "artifact_hashes": {str(path.relative_to(output)): benchmark.existing.digest(path) for path in [output / "protocol.json", output / "results/report.json", output / "results/trials.jsonl", output / "results/events.jsonl", output / "results/stderr.log"]},
    }
    (output / "verification.json").write_text(json.dumps(receipt, indent=2), encoding="utf-8")
    print(json.dumps(receipt))


if __name__ == "__main__":
    main()
