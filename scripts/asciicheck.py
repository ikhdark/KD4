#!/usr/bin/env python3

import argparse
import codecs
import re
import sys
from pathlib import Path

"""
Utility script that takes a list of files and returns non-zero if any of them
contain non-ASCII characters other than those in the allowed list.

If --fix is used, it will attempt to replace non-ASCII characters with ASCII
equivalents.

The motivation behind this script is that characters like U+00A0 (non-breaking
space) can cause regexes not to match and can result in surprising anchor
values for headings when GitHub renders Markdown as HTML.
"""


"""
When --fix is used, perform the following substitutions.
"""
substitutions: dict[int, str] = {
    0x00A0: " ",  # non-breaking space
    0x2011: "-",  # non-breaking hyphen
    0x2013: "-",  # en dash
    0x2014: "-",  # em dash
    0x2018: "'",  # left single quote
    0x2019: "'",  # right single quote
    0x201C: '"',  # left double quote
    0x201D: '"',  # right double quote
    0x2026: "...",  # ellipsis
    0x202F: " ",  # narrow non-breaking space
}

"""
Unicode codepoints that are allowed in addition to ASCII.
Be conservative with this list.

Note that it is always an option to use the hex HTML representation
instead of the character itself so the source code is ASCII-only.
For example, U+2728 (sparkles) can be written as `&#x2728;`.
"""
allowed_unicode_codepoints = {
    0x2728,  # sparkles
}

_TRANSLATION_TABLE = str.maketrans(substitutions)
# Tab and carriage return are ordinary ASCII whitespace; this repo checks out
# text files with CRLF endings (core.autocrlf), so flagging \r would fail
# every clean file.
_INVALID_ASCII_RE = re.compile(rb"[^\x09\x0A\x0D\x20-\x7E]")
_READ_CHUNK_SIZE = 1024 * 1024
_OUTPUT_BATCH_CHARS = 64 * 1024
_safe_char_cache: dict[int, str] = {}


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check for non-ASCII characters in files."
    )
    parser.add_argument(
        "--fix",
        action="store_true",
        help="Rewrite files, replacing non-ASCII characters with ASCII equivalents, where possible.",
    )
    parser.add_argument(
        "files",
        nargs="+",
        help="Files to check for non-ASCII characters.",
    )
    args = parser.parse_args()

    has_errors = False
    for filename in args.files:
        path = Path(filename)
        has_errors |= lint_utf8_ascii(path, fix=args.fix)
    return 1 if has_errors else 0


def lint_utf8_ascii(filename: Path, fix: bool) -> bool:
    """Returns True if an error was printed."""
    if fix:
        return lint_utf8_ascii_fix(filename)
    return lint_utf8_ascii_check(filename)


def lint_utf8_ascii_check(filename: Path) -> bool:
    """Check a file without loading non-ASCII files fully into memory."""
    reporter = ErrorReporter()
    decoder = codecs.getincrementaldecoder("utf-8")()
    line = 1
    col = 1
    byte_line = 1
    byte_col = 1
    byte_offset = 0

    with open(filename, "rb") as f:
        while chunk := f.read(_READ_CHUNK_SIZE):
            decoder_has_pending_bytes = bool(decoder.getstate()[0])
            if chunk.isascii() and not decoder_has_pending_bytes:
                line, col = scan_ascii_chunk(chunk, line, col, reporter)
            else:
                try:
                    text = decoder.decode(chunk, final=False)
                except UnicodeDecodeError as e:
                    print_decode_error(e, chunk, byte_offset, byte_line, byte_col)
                    reporter.flush()
                    return True
                line, col = scan_text(text, line, col, reporter)

            byte_line, byte_col = advance_position_bytes(chunk, byte_line, byte_col)
            byte_offset += len(chunk)

    try:
        text = decoder.decode(b"", final=True)
    except UnicodeDecodeError as e:
        print_decode_error(e, b"", byte_offset, byte_line, byte_col)
        reporter.flush()
        return True
    scan_text(text, line, col, reporter)
    reporter.flush()
    return reporter.has_errors


def lint_utf8_ascii_fix(filename: Path) -> bool:
    """Check and rewrite a file using a C-level translation table."""
    try:
        with open(filename, "rb") as f:
            raw = f.read()
        if raw.isascii() and _INVALID_ASCII_RE.search(raw) is None:
            return False
        text = raw.decode("utf-8")
    except UnicodeDecodeError as e:
        print_decode_error(e, raw, 0, 1, 1)
        return True

    reporter = ErrorReporter()
    scan_text(text, 1, 1, reporter)
    reporter.flush()

    if reporter.has_errors:
        print(f"Attempting to fix {filename}...")
        new_contents = text.translate(_TRANSLATION_TABLE)
        # newline="" prevents \r\n in the decoded text from being re-expanded
        # to \r\r\n by platform newline translation.
        with open(filename, "w", encoding="utf-8", newline="") as f:
            f.write(new_contents)
        print(
            f"Fixed {reporter.fixable_count} of {reporter.error_count} errors in {filename}."
        )

    return reporter.has_errors


class ErrorReporter:
    def __init__(self) -> None:
        self.error_count = 0
        self.fixable_count = 0
        self._parts: list[str] = []
        self._chars = 0

    @property
    def has_errors(self) -> bool:
        return self.error_count > 0

    def invalid_character(
        self, lineno: int, colno: int, char: str, codepoint: int
    ) -> None:
        self.error_count += 1
        if codepoint in substitutions:
            self.fixable_count += 1
        self._write(
            f"Invalid character at line {lineno}, column {colno}: "
            f"U+{codepoint:04X} ({safe_char_display(char, codepoint)})\n"
        )

    def flush(self) -> None:
        if self._parts:
            sys.stdout.write("".join(self._parts))
            self._parts.clear()
            self._chars = 0

    def _write(self, message: str) -> None:
        self._parts.append(message)
        self._chars += len(message)
        if self._chars >= _OUTPUT_BATCH_CHARS:
            self.flush()


def scan_ascii_chunk(
    chunk: bytes, line: int, col: int, reporter: ErrorReporter
) -> tuple[int, int]:
    match = _INVALID_ASCII_RE.search(chunk)
    if match is None:
        return advance_position_bytes(chunk, line, col)

    pos = 0
    while match is not None:
        line, col = advance_position_bytes(chunk[pos : match.start()], line, col)
        codepoint = chunk[match.start()]
        reporter.invalid_character(line, col, chr(codepoint), codepoint)
        col += 1
        pos = match.start() + 1
        match = _INVALID_ASCII_RE.search(chunk, pos)
    return advance_position_bytes(chunk[pos:], line, col)


def scan_text(
    text: str, line: int, col: int, reporter: ErrorReporter
) -> tuple[int, int]:
    for char in text:
        codepoint = ord(char)
        if char == "\n":
            line += 1
            col = 1
            continue
        if (
            not (0x20 <= codepoint <= 0x7E)
            and codepoint not in (0x09, 0x0D)
            and codepoint not in allowed_unicode_codepoints
        ):
            reporter.invalid_character(line, col, char, codepoint)
        col += 1
    return line, col


def advance_position_bytes(data: bytes, line: int, col: int) -> tuple[int, int]:
    last_newline = data.rfind(b"\n")
    if last_newline == -1:
        return line, col + len(data)
    return line + data.count(b"\n"), len(data) - last_newline


def print_decode_error(
    error: UnicodeDecodeError,
    data: bytes,
    byte_offset: int,
    line: int,
    col: int,
) -> None:
    partial = data[: error.start]
    line, col = advance_position_bytes(partial, line, col)
    sys.stdout.write(
        "UTF-8 decoding error:\n"
        f"  byte offset: {byte_offset + error.start}\n"
        f"  reason: {error.reason}\n"
        f"  location: line {line}, column {col}\n"
    )


def safe_char_display(char: str, codepoint: int) -> str:
    safe_char = _safe_char_cache.get(codepoint)
    if safe_char is None:
        safe_char = repr(char)[1:-1]  # nicely escape things like \u202f
        _safe_char_cache[codepoint] = safe_char
    return safe_char


if __name__ == "__main__":
    sys.exit(main())
