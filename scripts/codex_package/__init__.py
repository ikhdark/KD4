"""Helpers for building canonical Codex package archives."""

from pathlib import Path
import sys

# Script entrypoints import this package from scripts/, but shared helpers must
# keep their scripts.* identity so all workers share the same operation context.
if __name__ == "codex_package":
    repo_root = str(Path(__file__).resolve().parents[2])
    if repo_root not in sys.path:
        sys.path.insert(0, repo_root)
