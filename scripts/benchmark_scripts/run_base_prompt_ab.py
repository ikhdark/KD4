"""Same-batch A/B of two base-prompt texts on the rd-exp experiment binary.

usage: python -I -B run_base_prompt_ab.py <batch> <control.md> <treatment.md> [reps]

Both arms run rd-exp/build/bin/codex.exe with the ELT toggles (lean exec contract
and typed command result, matching the checkout's exec-description change) and
differ only in their `model_instructions_file` override. Runs start in parallel
through rd-compare/scripts/run_arms.py, which copies the workspace snapshot per
run and writes runs/<batch>-<arm>-<rep>/{rollout.jsonl,meta.json,...}.
"""
import os
import sys

CTX = r"C:\Users\kuh\AppData\Local\Temp\rd-compare"
EXP = r"C:\Users\kuh\AppData\Local\Temp\rd-exp"
# Appended, not prepended: that folder has a `types.py` that shadows the stdlib.
sys.path.append(os.path.join(CTX, "scripts"))
import run_arms  # noqa: E402


def main():
    batch, control, treatment = sys.argv[1:4]
    reps = int(sys.argv[4]) if len(sys.argv) > 4 else 3
    elt_env = dict(run_arms.ARMS["ELT"][3])
    for arm, path in (("BC", control), ("BT", treatment)):
        if not os.path.isfile(path):
            sys.exit(f"missing instructions file for {arm}: {path}")
        override = 'model_instructions_file="' + os.path.abspath(path).replace("\\", "/") + '"'
        run_arms.ARMS[arm] = (run_arms.EXP_BIN, run_arms.FORK_HOME, ["-c", override], elt_env)
    specs = [f"{arm}.{i}" for i in range(1, reps + 1) for arm in ("BC", "BT")]
    sys.argv = ["run_arms.py", EXP, batch, ",".join(specs)]
    run_arms.main()


if __name__ == "__main__":
    main()
