#!/usr/bin/env python3
"""Shared GitHub helpers for skill install scripts."""

from __future__ import annotations

import os
import urllib.parse
import urllib.request


def github_request(url: str, user_agent: str) -> bytes:
    headers = {"User-Agent": user_agent}
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if token:
        headers["Authorization"] = f"token {token}"
    req = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(req) as resp:
        return resp.read()


def github_api_contents_url(repo: str, path: str, ref: str) -> str:
    repo = urllib.parse.quote(repo, safe="/")
    path = urllib.parse.quote(path, safe="/")
    query = urllib.parse.urlencode({"ref": ref})
    return f"https://api.github.com/repos/{repo}/contents/{path}?{query}"
