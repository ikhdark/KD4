"""Private first-write-wins dispatch acknowledgement shared by Rust owners.

Never infer nonexecution from command output or an exit code. A started record
cannot be replaced by a later busy report (including nested managed wrappers).
"""

import json
import os
from pathlib import Path


def record_dispatch(status, env=None):
    env = os.environ if env is None else env
    path = env.get("CODEX_FULL_SUITE_ACK_PATH")
    attempt = env.get("CODEX_FULL_SUITE_ATTEMPT")
    if not path or not attempt:
        return
    try:
        with Path(path).open("x", encoding="utf-8") as stream:
            json.dump({"version": 1, "attempt": attempt, "status": status}, stream)
            stream.flush()
            os.fsync(stream.fileno())
    except FileExistsError:
        # A started parent/earlier batch irrevocably prevents a busy refund.
        return
    except OSError:
        # Failed optional nonexecution evidence must not hide admission busy.
        # Failed start evidence must prevent execution, never permit a refund.
        if status == "started":
            raise


def record_test_dispatch(argv, env=None):
    program = Path(argv[0]).stem.lower() if argv else ""
    if (
        program == "cargo"
        and ("test" in argv[1:3] or list(argv[1:3]) == ["nextest", "run"])
        and "--no-run" not in argv
        and "--list" not in argv
    ):
        record_dispatch("started", env)
