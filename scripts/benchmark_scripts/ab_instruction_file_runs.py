"""Run matched `codex exec` sessions that differ only in the base-instructions file.

usage: python -I -B ab_instruction_file_runs.py EXP_ROOT BATCH ARM=INSTRUCTIONS_FILE[,...] [--reps N]
       [--bin PATH] [--master DIR] [--env KEY=VALUE ...] [--prompt TEXT]

Each run copies MASTER (default: EXP_ROOT/master/kd4) to EXP_ROOT/runs/<batch>-<arm>-<rep>/kd4 and
writes events.jsonl, stderr.txt, meta.json and rollout.jsonl there (the rd-exp layout read by
audit_output_token_composition.py). All runs in one invocation start together so arms share
provider conditions.
"""
import argparse
import glob
import json
import os
import shutil
import subprocess
import time

FORK_HOME = r"C:\Users\kuh\Desktop\LOCAL-KD"
DEFAULT_BIN = r"C:\Users\kuh\AppData\Local\Temp\rd-exp\build\bin\codex.exe"
DEFAULT_PROMPT = "list all prompt/guidance/templates in codex-rs, put it into a .txt file."
COMMON = ["-s", "danger-full-access", "-c", 'approval_policy="never"', "-c", 'model="gpt-6-astra"',
          "-c", 'model_reasoning_effort="high"', "-c", 'model_reasoning_summary="concise"', "--json"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root")
    ap.add_argument("batch")
    ap.add_argument("arms", help="ARM=INSTRUCTIONS_FILE[,ARM=INSTRUCTIONS_FILE...]")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--bin", default=DEFAULT_BIN)
    ap.add_argument("--master")
    ap.add_argument("--env", action="append", default=[], help="KEY=VALUE added to every run")
    ap.add_argument("--prompt", default=DEFAULT_PROMPT)
    a = ap.parse_args()
    arms = dict(spec.split("=", 1) for spec in a.arms.split(","))
    for arm, path in arms.items():
        if not os.path.isfile(path):
            raise SystemExit(f"{arm}: missing instructions file {path}; nothing started")
    extra_env = dict(kv.split("=", 1) for kv in a.env)
    master = a.master or os.path.join(a.root, "master", "kd4")
    runs = [(arm, rep) for rep in range(1, a.reps + 1) for arm in arms]
    dirs = {r: os.path.join(a.root, "runs", f"{a.batch}-{r[0]}-{r[1]}") for r in runs}
    taken = [d for d in dirs.values() if os.path.exists(d)]
    if taken:
        raise SystemExit(f"run dirs exist: {taken}; nothing started")
    for d in dirs.values():
        shutil.copytree(master, os.path.join(d, "kd4"))
    procs = []
    for arm, rep in runs:
        rd = dirs[(arm, rep)]
        instructions = 'model_instructions_file="' + os.path.abspath(arms[arm]).replace("\\", "/") + '"'
        cmd = [a.bin, "exec", "-C", os.path.join(rd, "kd4")] + COMMON + ["-c", instructions, a.prompt]
        env = dict(os.environ, CODEX_HOME=FORK_HOME, **extra_env)
        out = open(os.path.join(rd, "events.jsonl"), "wb")
        err = open(os.path.join(rd, "stderr.txt"), "wb")
        p = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=out, stderr=err, env=env)
        procs.append((arm, rd, cmd, p, time.time(), out, err))
        print(f"started {os.path.basename(rd)} pid={p.pid}", flush=True)
    for arm, rd, cmd, p, t0, out, err in procs:
        try:
            rc = p.wait(timeout=1800)
        except subprocess.TimeoutExpired:
            p.kill()
            rc = "timeout"
        wall = round(time.time() - t0, 1)
        out.close()
        err.close()
        thread = None
        for line in open(os.path.join(rd, "events.jsonl"), "rb"):
            try:
                ev = json.loads(line)
            except ValueError:
                continue
            if ev.get("type") == "thread.started":
                thread = ev.get("thread_id")
                break
        rollout = None
        if thread:
            hits = glob.glob(os.path.join(FORK_HOME, "sessions", "**", f"*{thread}*.jsonl*"), recursive=True)
            if hits:
                rollout = hits[0]
                shutil.copy2(rollout, os.path.join(rd, "rollout.jsonl"))
        meta = dict(arm=arm, batch=a.batch, rc=rc, wall_s=wall, thread_id=thread, rollout_source=rollout,
                    cmd=cmd, env=extra_env, instructions_file=os.path.abspath(arms[arm]))
        json.dump(meta, open(os.path.join(rd, "meta.json"), "w"), indent=1)
        print(json.dumps({k: meta[k] for k in ("arm", "rc", "wall_s", "thread_id")}), flush=True)


if __name__ == "__main__":
    main()
