#!/usr/bin/env python3
"""Build a canonical Codex package directory and optional archive."""

# Support direct execution (including python -I) from any working directory.
if __package__ in (None, ""):
    import sys as _sys
    from pathlib import Path as _Path

    _sys.path.insert(0, str(_Path(__file__).resolve().parents[2]))


from pathlib import Path
import sys


# Some developer environments set PYTHONSAFEPATH=1, which prevents Python from
# adding the script directory to sys.path. Add it explicitly so the local helper
# package remains importable when this executable is launched from any cwd.
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from scripts.codex_package.cli import main

if __name__ == "__main__":
    raise SystemExit(main())
