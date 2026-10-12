"""Repair only the four normal trials invalidated by benchmark discovery instructions."""
import asyncio
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("original_cargo_benchmark", HERE / "benchmark.py")
original = importlib.util.module_from_spec(spec)
spec.loader.exec_module(original)
OUTPUT = original.ROOT / ".codex" / "audits" / "rust23-cargo-context" / "results-corrected-normal"
OLD = "Optional read-only fixture tool cargo_context_facts({}) supplies files and lookup contracts; discover its callable contract if needed even when deferred. "
NEW = ("Optional read-only dynamic fixture tool cargo_context_facts({}) supplies files and lookup contracts. "
       "It is registered directly, not a deferred tool. If needed, call it directly in code mode as "
       "text(await tools.cargo_context_facts({})); its input is an empty object and its output is read-only context facts. "
       "Do not use tool_search or resolve_tool for this dynamic fixture tool. ")

class Server(original.Server):
    async def request(self, method, params):
        if method == "thread/start":
            assert params["developerInstructions"].count(OLD) == 1
            params = dict(params)
            params["developerInstructions"] = params["developerInstructions"].replace(OLD, NEW)
            instruction = params["developerInstructions"]
            if getattr(self, "instruction", instruction) != instruction:
                raise RuntimeError("developer instruction changed between trials")
            self.instruction = instruction
        return await super().request(method, params)

def fingerprints():
    return {**original.fingerprint(), str(Path(__file__)): original.existing.digest(Path(__file__))}

async def run():
    assert original.assess("normal", json.dumps(original.EXPECTED["normal"]))["correct"]
    assert not original.assess("normal", json.dumps(original.EXPECTED["explicit"]))["correct"]
    OUTPUT.mkdir(exist_ok=False)
    before = fingerprints()
    protocol = {"scope": __doc__, "model": original.MODEL, "effort": original.EFFORT,
                "packets": original.PACKETS, "facts": original.FACTS,
                "expected": original.EXPECTED["normal"], "task": original.TASKS["normal"],
                "instruction_replacement": {"old": OLD, "new": NEW},
                "ordering": ["baseline", "candidate", "candidate", "baseline"], "hashes_before": before,
                "callable_evidence": "original events.jsonl lines736-738: dynamicToolCall cargo_context_facts, namespace null, arguments {}, completed success"}
    protocol_text = json.dumps(protocol, indent=2)
    (OUTPUT / "protocol.json").write_text(protocol_text, encoding="utf-8")
    rows = []
    instruction = None
    try:
        with tempfile.TemporaryDirectory(prefix="kd4-rust23-auth-") as directory:
            home = Path(directory)
            shutil.copyfile(original.HOME / "auth.json", home / "auth.json")
            work = home / "work"
            work.mkdir()
            env = {k:v for k,v in os.environ.items() if not k.startswith("CODEX_") or k == "CODEX_API_KEY"}
            env.update(CODEX_HOME=str(home), CODEX_EXEC_SERVER_URL="none")
            command = [str(original.BINARY), "app-server", "--listen", "stdio://"]
            for key,value in original.existing.SAFE_CONFIG.items():
                command += ["-c", f"{key}={json.dumps(value)}"]
            with (OUTPUT/"events.jsonl").open("x",encoding="utf-8") as log, (OUTPUT/"stderr.log").open("xb") as stderr:
                process = await asyncio.create_subprocess_exec(*command,cwd=work,env=env,
                    stdin=asyncio.subprocess.PIPE,stdout=asyncio.subprocess.PIPE,stderr=stderr,
                    limit=16*1024*1024,creationflags=subprocess.CREATE_NO_WINDOW)
                try:
                    server = Server(process,log,work,"")
                    await asyncio.wait_for(server.request("initialize",{"clientInfo":{"name":"kd4_rust23_cargo_corrected","version":"1"},"capabilities":{"experimentalApi":True}}),30)
                    await server.send({"method":"initialized","params":{}})
                    for repeat in range(2):
                        for variant in (("baseline","candidate") if repeat == 0 else ("candidate","baseline")):
                            row = await asyncio.wait_for(server.trial(work,"normal",variant),240)
                            instruction = server.instruction
                            row["repeat"] = repeat
                            rows.append(row)
                            with (OUTPUT/"trials.jsonl").open("a",encoding="utf-8") as stream:
                                stream.write(json.dumps(row)+"\n")
                            print(json.dumps({k:row[k] for k in ("case","variant","repeat","assessment","wall_seconds","model_requests")}),flush=True)
                finally:
                    if process.returncode is None:
                        process.stdin.close()
                        try:
                            await asyncio.wait_for(process.wait(),5)
                        except TimeoutError:
                            process.kill()
                            await process.wait()
    finally:
        after = fingerprints()
        summary = []
        for variant in ("baseline","candidate"):
            chosen = [r for r in rows if r["variant"] == variant]
            if chosen:
                summary.append({"variant":variant,"runs":len(chosen),"correct":sum(r["assessment"]["correct"] for r in chosen),
                    "median_wall_seconds":statistics.median(r["wall_seconds"] for r in chosen),
                    "model_requests":sum(r["model_requests"] for r in chosen),
                    "fixture_tool_calls":sum(len(r["calls"]) for r in chosen),
                    "actual_tool_calls":sum(r["timing"]["counters"]["toolCallCount"] for r in chosen),
                    "tool_failures":sum(t.get("outcome")=="failure" for r in chosen for t in r["timing"].get("toolCalls",[]))})
        report = {"scope":__doc__,"model":original.MODEL,"effort":original.EFFORT,"completed_trials":len(rows),
            "hashes_before":before,"hashes_after":after,"hashes_unchanged":before==after,"summary":summary,
            "protocol_sha256":hashlib.sha256(protocol_text.encode()).hexdigest(),
            "developer_instruction":instruction,
            "developer_instruction_sha256":hashlib.sha256(instruction.encode()).hexdigest() if instruction else None}
        (OUTPUT/"report.json").write_text(json.dumps(report,indent=2),encoding="utf-8")
        print(json.dumps(report),flush=True)

if __name__ == "__main__":
    asyncio.run(run())

