from __future__ import annotations

import difflib
import fnmatch
import shutil
import subprocess
import threading
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Tuple

from llm_browser.tool.context import ToolContext
from llm_browser.tool.result import ToolResult


MAX_READ_CHARS = 20000
MAX_LINE_LENGTH = 2000
MAX_DIFF_CHARS = 20000
MAX_SEARCH_OUTPUT = 30000
MAX_GLOB_RESULTS = 5000
MAX_GREP_RESULTS = 1000
_FILE_LOCKS: Dict[str, threading.RLock] = {}
_FILE_LOCKS_LOCK = threading.Lock()
_IGNORED_DIR_NAMES = {
    ".browser-use-terminal",
    ".git",
    ".hg",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".svn",
    ".tox",
    ".venv",
    "__pycache__",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "target",
    "venv",
}
_IGNORED_FILE_NAMES = {".DS_Store"}
_IGNORED_SUFFIXES = {".pyc", ".pyo"}


def read_file(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    path = _resolve(ctx, str(arguments["path"]))
    if not path.exists():
        raise FileNotFoundError(_missing_path_message(path))
    if path.is_dir():
        limit = _bounded_int(arguments.get("limit"), 200, 1, MAX_GLOB_RESULTS)
        all_entries = [
            entry
            for entry in sorted(path.iterdir(), key=lambda item: (not item.is_dir(), item.name.lower()))
            if not _is_ignored_path(entry, path)
        ]
        entries = all_entries[:limit]
        text = "\n".join(("/" if entry.is_dir() else "") + entry.name for entry in entries)
        return ToolResult(
            text=text,
            data={
                "path": str(path),
                "kind": "directory",
                "count": len(entries),
                "total_count": len(all_entries),
                "truncated": len(all_entries) > len(entries),
            },
        )

    raw = path.read_bytes()
    if _looks_binary(raw):
        return ToolResult(
            text=f"{path} appears to be binary; use shell/file-specific tooling to inspect it.",
            data={"path": str(path), "kind": "binary", "bytes": len(raw), "binary": True},
        )

    text, _ = _decode_text(raw)
    line_offset = arguments.get("line_offset")
    line_limit = arguments.get("line_limit")
    char_window_requested = "offset" in arguments or "limit" in arguments
    line_window_requested = line_offset is not None or line_limit is not None
    line_limit_value = _optional_int(line_limit, 200)
    line_offset_value = _optional_int(line_offset, 0)
    ignore_zero_line_window = (
        char_window_requested
        and line_window_requested
        and line_offset_value <= 0
        and line_limit is not None
        and line_limit_value <= 0
    )
    if line_window_requested and not ignore_zero_line_window:
        start = max(0, line_offset_value)
        limit = max(0, line_limit_value)
        lines = text.splitlines()
        if lines and start >= len(lines):
            raise ValueError("line_offset exceeds file length")
        window = lines[start : start + limit]
        formatted, truncated_lines = _numbered_lines(window, start)
        next_line_offset = start + len(window)
        return ToolResult(
            text=formatted,
            data={
                "path": str(path),
                "kind": "text",
                "line_offset": start,
                "line_count": len(lines),
                "returned_lines": len(window),
                "next_line_offset": next_line_offset if next_line_offset < len(lines) else None,
                "truncated": next_line_offset < len(lines),
                "line_truncated_count": truncated_lines,
            },
        )

    offset = max(0, _optional_int(arguments.get("offset"), 0))
    limit = _bounded_int(arguments.get("limit"), MAX_READ_CHARS, 0, MAX_READ_CHARS)
    chunk = text[offset : offset + limit]
    next_offset = offset + len(chunk)
    truncated = next_offset < len(text)
    if truncated:
        chunk += f"\n[... omitted {len(text) - next_offset} chars. Continue with offset={next_offset}.]"
    return ToolResult(
        text=chunk,
        data={
            "path": str(path),
            "kind": "text",
            "offset": offset,
            "returned_chars": len(text[offset : offset + limit]),
            "total_chars": len(text),
            "next_offset": next_offset if truncated else None,
            "truncated": truncated,
        },
    )


def write_file(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    path = _resolve(ctx, str(arguments["path"]))
    content = str(arguments.get("content", ""))
    with _file_lock(path):
        old_text = ""
        old_meta = _TextMeta()
        existed = path.exists()
        if existed:
            old_text, old_meta = _read_text_file(path)
        path.parent.mkdir(parents=True, exist_ok=True)
        newline = old_meta.newline if existed else _detect_newline(content)
        next_text = _normalize_newlines(content, newline)
        _write_text_file(path, next_text, old_meta if existed else _TextMeta(newline=newline))
    diff = _unified_diff(old_text, next_text, fromfile=str(path) if existed else "/dev/null", tofile=str(path))
    return ToolResult(
        text=f"wrote {path}\n{_cap(diff, MAX_DIFF_CHARS)}",
        data={"path": str(path), "chars": len(next_text), "created": not existed, "diff": _cap(diff, MAX_DIFF_CHARS)},
    )


def edit_file(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    path = _resolve(ctx, str(arguments["path"]))
    old = str(arguments["old"])
    new = str(arguments["new"])
    replace_all = bool(arguments.get("replace_all", False))
    with _file_lock(path):
        text, meta = _read_text_file(path)
        count = text.count(old)
        if count == 0:
            raise ValueError(f"old text not found in {path}\n{_near_miss_message(text, old)}")
        if count > 1 and not replace_all:
            raise ValueError(f"old text appears {count} times in {path}; set replace_all=true or make old text unique")
        updated = text.replace(old, new) if replace_all else text.replace(old, new, 1)
        _write_text_file(path, updated, meta)
    changed = count if replace_all else 1
    diff = _unified_diff(text, updated, fromfile=str(path), tofile=str(path))
    return ToolResult(
        text=f"edited {path}\n{_cap(diff, MAX_DIFF_CHARS)}",
        data={"path": str(path), "replacements": changed, "diff": _cap(diff, MAX_DIFF_CHARS)},
    )


def apply_patch_file(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    patch = str(arguments["patch"])
    check = bool(arguments.get("check", False))
    if _is_codex_patch(patch):
        return _apply_codex_patch(ctx, patch, check=check)

    command = ["git", "apply", "--recount", "--whitespace=nowarn"]
    if check:
        command.append("--check")
    result = subprocess.run(
        command,
        cwd=str(ctx.session.cwd),
        input=patch,
        text=True,
        capture_output=True,
        timeout=30,
    )
    output = (result.stdout or "") + (("\n" if result.stdout and result.stderr else "") + result.stderr if result.stderr else "")
    if result.returncode != 0:
        raise ValueError(f"patch failed with status {result.returncode}: {output.strip()}")
    text = "patch check passed" if check else "patch applied"
    changed_files = _patch_paths(patch)
    return ToolResult(
        text=(text + (f"\nfiles: {', '.join(changed_files)}" if changed_files else "") + (f"\n{output}" if output else "")),
        data={"returncode": result.returncode, "check": check, "files": changed_files},
    )


def _is_codex_patch(patch: str) -> bool:
    return patch.lstrip().startswith("*** Begin Patch")


def _apply_codex_patch(ctx: ToolContext, patch: str, check: bool) -> ToolResult:
    operations = _parse_codex_patch(patch)
    if not operations:
        raise ValueError("patch contains no operations")

    changed_files: List[str] = []
    diffs: List[str] = []
    for operation in operations:
        op = str(operation["op"])
        path = _resolve(ctx, str(operation["path"]))
        changed_files.append(str(path.relative_to(ctx.session.cwd)) if path.is_relative_to(ctx.session.cwd) else str(path))

        if op == "add":
            if path.exists():
                raise ValueError(f"cannot add existing file: {path}")
            new_text = "\n".join(operation["lines"]) + ("\n" if operation["lines"] else "")
            diff = _unified_diff("", new_text, fromfile="/dev/null", tofile=str(path))
            if not check:
                path.parent.mkdir(parents=True, exist_ok=True)
                _write_text_file(path, new_text, _TextMeta(newline=_detect_newline(new_text)))
            diffs.append(diff)
            continue

        if op == "delete":
            old_text, _ = _read_text_file(path)
            diff = _unified_diff(old_text, "", fromfile=str(path), tofile="/dev/null")
            if not check:
                path.unlink()
            diffs.append(diff)
            continue

        if op != "update":
            raise ValueError(f"unknown Codex patch operation: {op}")

        with _file_lock(path):
            old_text, meta = _read_text_file(path)
            updated = _apply_codex_hunks(old_text, operation["hunks"], path)
            updated = _normalize_newlines(updated, meta.newline)
            diff = _unified_diff(old_text, updated, fromfile=str(path), tofile=str(path))
            move_to = operation.get("move_to")
            if move_to:
                target = _resolve(ctx, str(move_to))
                changed_files.append(str(target.relative_to(ctx.session.cwd)) if target.is_relative_to(ctx.session.cwd) else str(target))
                diff = _unified_diff(old_text, updated, fromfile=str(path), tofile=str(target))
            if not check:
                if move_to:
                    target.parent.mkdir(parents=True, exist_ok=True)
                    _write_text_file(target, updated, meta)
                    path.unlink()
                else:
                    _write_text_file(path, updated, meta)
            diffs.append(diff)

    text = "patch check passed" if check else "patch applied"
    diff_text = _cap("\n".join(diff for diff in diffs if diff), MAX_DIFF_CHARS)
    if diff_text:
        text += f"\n{diff_text}"
    return ToolResult(
        text=text,
        data={"returncode": 0, "check": check, "files": changed_files, "diff": diff_text, "format": "codex"},
    )


def _parse_codex_patch(patch: str) -> List[Dict[str, Any]]:
    lines = patch.strip().splitlines()
    if not lines or lines[0] != "*** Begin Patch" or lines[-1] != "*** End Patch":
        raise ValueError("invalid Codex patch envelope")

    operations: List[Dict[str, Any]] = []
    index = 1
    while index < len(lines) - 1:
        line = lines[index]
        if line.startswith("*** Add File: "):
            path = line.removeprefix("*** Add File: ").strip()
            index += 1
            add_lines: List[str] = []
            while index < len(lines) - 1 and not lines[index].startswith("*** "):
                if not lines[index].startswith("+"):
                    raise ValueError(f"invalid add-file line: {lines[index]}")
                add_lines.append(lines[index][1:])
                index += 1
            operations.append({"op": "add", "path": path, "lines": add_lines})
            continue
        if line.startswith("*** Delete File: "):
            path = line.removeprefix("*** Delete File: ").strip()
            operations.append({"op": "delete", "path": path})
            index += 1
            continue
        if line.startswith("*** Update File: "):
            path = line.removeprefix("*** Update File: ").strip()
            index += 1
            move_to: Optional[str] = None
            hunks: List[List[str]] = []
            while index < len(lines) - 1:
                if lines[index].startswith("*** Move to: "):
                    move_to = lines[index].removeprefix("*** Move to: ").strip()
                    index += 1
                    continue
                if lines[index].startswith("*** "):
                    break
                if not lines[index].startswith("@@"):
                    raise ValueError(f"expected hunk header, got: {lines[index]}")
                index += 1
                hunk: List[str] = []
                while index < len(lines) - 1 and not lines[index].startswith("@@") and not lines[index].startswith("*** "):
                    if lines[index].startswith("\\ No newline"):
                        index += 1
                        continue
                    if not lines[index] or lines[index][0] not in {" ", "+", "-"}:
                        raise ValueError(f"invalid hunk line: {lines[index]}")
                    hunk.append(lines[index])
                    index += 1
                hunks.append(hunk)
            operations.append({"op": "update", "path": path, "move_to": move_to, "hunks": hunks})
            continue
        raise ValueError(f"unknown Codex patch line: {line}")
    return operations


def _apply_codex_hunks(old_text: str, hunks: List[List[str]], path: Path) -> str:
    old_normalized = old_text.replace("\r\n", "\n").replace("\r", "\n")
    had_final_newline = old_normalized.endswith("\n")
    lines = old_normalized.split("\n")
    if had_final_newline:
        lines = lines[:-1]
    cursor = 0

    for hunk in hunks:
        old_seq = [line[1:] for line in hunk if line.startswith((" ", "-"))]
        new_seq = [line[1:] for line in hunk if line.startswith((" ", "+"))]
        start = _find_subsequence(lines, old_seq, cursor)
        if start is None:
            start = _find_subsequence(lines, old_seq, 0)
        if start is None:
            context = "\n".join(old_seq[:8])
            raise ValueError(f"patch context not found in {path}:\n{context}")
        lines[start : start + len(old_seq)] = new_seq
        cursor = start + len(new_seq)

    updated = "\n".join(lines)
    if had_final_newline:
        updated += "\n"
    return updated


def _find_subsequence(lines: List[str], needle: List[str], start: int) -> Optional[int]:
    if not needle:
        return min(max(start, 0), len(lines))
    max_start = len(lines) - len(needle)
    for index in range(max(0, start), max_start + 1):
        if lines[index : index + len(needle)] == needle:
            return index
    return None


def glob_files(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    pattern = str(arguments["pattern"])
    root = _resolve(ctx, str(arguments.get("root", ".")))
    if not root.exists():
        raise FileNotFoundError(_missing_path_message(root))
    limit = _bounded_int(arguments.get("limit"), 200, 1, MAX_GLOB_RESULTS)
    recursive = bool(arguments.get("recursive", True))
    rg_result = _glob_with_rg(root=root, pattern=pattern, recursive=recursive, limit=limit)
    if rg_result is not None:
        return rg_result

    all_matches: List[Path]
    if root.is_file():
        matches = [root] if fnmatch.fnmatch(root.name, pattern) else []
        all_matches = matches
    else:
        iterator = root.rglob(pattern) if recursive else root.glob(pattern)
        all_matches = sorted(
            [path for path in iterator if path.is_file() and not _is_ignored_path(path, root)],
            key=lambda path: (path.stat().st_mtime if path.exists() else 0),
            reverse=True,
        )
        matches = all_matches[:limit]
    truncated = root.is_dir() and len(all_matches) > len(matches)
    text = "\n".join(str(path) for path in matches)
    if truncated:
        text += f"\n[... omitted {len(all_matches) - len(matches)} matches. Narrow pattern or increase limit.]"
    return ToolResult(
        text=text,
        data={
            "root": str(root),
            "pattern": pattern,
            "count": len(matches),
            "recursive": recursive,
            "engine": "python",
            "truncated": truncated,
        },
    )


def grep_files(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    pattern = str(arguments["pattern"])
    root = _resolve(ctx, str(arguments.get("root", ".")))
    include = str(arguments.get("include", "*"))
    limit = _bounded_int(arguments.get("limit"), 200, 1, MAX_GREP_RESULTS)
    if not root.exists():
        raise FileNotFoundError(_missing_path_message(root))
    rg_result = _grep_with_rg(pattern=pattern, root=root, include=include, limit=limit)
    if rg_result is not None:
        return rg_result
    lines: List[str] = []
    for path in _iter_files(root, include):
        if len(lines) >= limit:
            break
        try:
            raw = path.read_bytes()
            if _looks_binary(raw):
                continue
            text, _ = _decode_text(raw)
        except OSError:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            if pattern in line:
                lines.append(f"{path}:{lineno}:{_truncate_line(line)}")
                if len(lines) >= limit:
                    break
    truncated = len(lines) >= limit
    text = "\n".join(lines)
    if truncated:
        text += "\n[... omitted matches. Narrow pattern or increase limit.]"
    return ToolResult(
        text=text,
        data={"root": str(root), "pattern": pattern, "count": len(lines), "engine": "python", "truncated": truncated},
    )


class _TextMeta:
    def __init__(self, newline: str = "\n", bom: bool = False) -> None:
        self.newline = newline
        self.bom = bom


def _resolve(ctx: ToolContext, path: str) -> Path:
    candidate = Path(path).expanduser()
    if not candidate.is_absolute():
        candidate = ctx.session.cwd / candidate
    return candidate.resolve()


def _file_lock(path: Path):
    key = str(path)
    with _FILE_LOCKS_LOCK:
        lock = _FILE_LOCKS.get(key)
        if lock is None:
            lock = threading.RLock()
            _FILE_LOCKS[key] = lock
    return lock


def _read_text_file(path: Path) -> Tuple[str, _TextMeta]:
    if not path.exists():
        raise FileNotFoundError(_missing_path_message(path))
    raw = path.read_bytes()
    if _looks_binary(raw):
        raise ValueError(f"{path} appears to be binary; refusing text edit")
    text, bom = _decode_text(raw)
    return text, _TextMeta(newline=_detect_newline(text), bom=bom)


def _write_text_file(path: Path, text: str, meta: _TextMeta) -> None:
    data = text.encode("utf-8")
    if meta.bom:
        data = b"\xef\xbb\xbf" + data
    path.write_bytes(data)


def _decode_text(raw: bytes) -> Tuple[str, bool]:
    bom = raw.startswith(b"\xef\xbb\xbf")
    if bom:
        raw = raw[3:]
    return raw.decode("utf-8", errors="replace"), bom


def _looks_binary(raw: bytes) -> bool:
    if not raw:
        return False
    if b"\x00" in raw[:4096]:
        return True
    sample = raw[:4096]
    textish = sum(1 for byte in sample if byte in b"\n\r\t\b\f" or 32 <= byte <= 126 or byte >= 128)
    return (textish / max(1, len(sample))) < 0.85


def _detect_newline(text: str) -> str:
    crlf = text.count("\r\n")
    lf = text.count("\n") - crlf
    cr = text.count("\r") - crlf
    if crlf >= lf and crlf >= cr and crlf > 0:
        return "\r\n"
    if cr > lf and cr > 0:
        return "\r"
    return "\n"


def _normalize_newlines(text: str, newline: str) -> str:
    normalized = text.replace("\r\n", "\n").replace("\r", "\n")
    if newline == "\n":
        return normalized
    return normalized.replace("\n", newline)


def _unified_diff(old: str, new: str, fromfile: str, tofile: str) -> str:
    return "".join(
        difflib.unified_diff(
            old.splitlines(keepends=True),
            new.splitlines(keepends=True),
            fromfile=fromfile,
            tofile=tofile,
        )
    )


def _grep_with_rg(pattern: str, root: Path, include: str, limit: int) -> Optional[ToolResult]:
    if shutil.which("rg") is None:
        return None
    command = [
        "rg",
        "--line-number",
        "--fixed-strings",
        "--color=never",
        "--max-columns",
        str(MAX_LINE_LENGTH),
        "--max-columns-preview",
        "--glob",
        include,
    ]
    for ignored in _ignored_rg_globs():
        command.extend(["--glob", ignored])
    command.extend(
        [
            "--",
            pattern,
            str(root),
        ]
    )
    result = subprocess.run(command, text=True, capture_output=True, timeout=30)
    if result.returncode not in {0, 1}:
        return None
    all_lines = result.stdout.splitlines()
    lines = all_lines[:limit]
    truncated = len(all_lines) > len(lines)
    text = "\n".join(lines)
    if len(text) > MAX_SEARCH_OUTPUT:
        text = _cap(text, MAX_SEARCH_OUTPUT)
        truncated = True
    if truncated:
        text += "\n[... omitted matches. Narrow pattern or increase limit.]"
    return ToolResult(
        text=text,
        data={"root": str(root), "pattern": pattern, "count": len(lines), "engine": "rg", "truncated": truncated},
    )


def _glob_with_rg(root: Path, pattern: str, recursive: bool, limit: int) -> Optional[ToolResult]:
    if shutil.which("rg") is None or root.is_file():
        return None
    command = ["rg", "--files", "--color=never", "--glob", pattern]
    for ignored in _ignored_rg_globs():
        command.extend(["--glob", ignored])
    try:
        result = subprocess.run(command, cwd=str(root), text=True, capture_output=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if result.returncode not in {0, 1}:
        return None

    all_matches: List[Path] = []
    for line in result.stdout.splitlines():
        if not line:
            continue
        relative = Path(line)
        if not recursive and len(relative.parts) > 1:
            continue
        path = (root / relative).resolve()
        if path.is_file() and not _is_ignored_path(path, root):
            all_matches.append(path)
    all_matches.sort(key=lambda path: (path.stat().st_mtime if path.exists() else 0), reverse=True)
    matches = all_matches[:limit]
    truncated = len(all_matches) > len(matches)
    text = "\n".join(str(path) for path in matches)
    if truncated:
        text += f"\n[... omitted {len(all_matches) - len(matches)} matches. Narrow pattern or increase limit.]"
    return ToolResult(
        text=text,
        data={
            "root": str(root),
            "pattern": pattern,
            "count": len(matches),
            "recursive": recursive,
            "engine": "rg",
            "truncated": truncated,
        },
    )


def _iter_files(root: Path, include: str) -> Iterable[Path]:
    if root.is_file():
        if not _is_ignored_path(root, root.parent):
            yield root
        return
    for path in sorted(root.rglob("*")):
        if path.is_file() and fnmatch.fnmatch(path.name, include) and not _is_ignored_path(path, root):
            yield path


def _optional_int(value: Any, default: int) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def _bounded_int(value: Any, default: int, minimum: int, maximum: int) -> int:
    return min(max(_optional_int(value, default), minimum), maximum)


def _numbered_lines(lines: List[str], start: int) -> Tuple[str, int]:
    formatted: List[str] = []
    truncated = 0
    for index, line in enumerate(lines, start=start + 1):
        display = _truncate_line(line)
        if len(display) < len(line):
            truncated += 1
        formatted.append(f"L{index}: {display}")
    return "\n".join(formatted), truncated


def _truncate_line(line: str) -> str:
    if len(line) <= MAX_LINE_LENGTH:
        return line
    return line[:MAX_LINE_LENGTH]


def _is_ignored_path(path: Path, root: Path) -> bool:
    try:
        relative = path.relative_to(root)
    except ValueError:
        relative = path
    parts = relative.parts
    container_parts = parts[:-1] if path.is_file() else parts
    for part in container_parts:
        if part in _IGNORED_DIR_NAMES or part.endswith(".egg-info") or part.endswith(".dist-info"):
            return True
    return path.name in _IGNORED_FILE_NAMES or path.suffix in _IGNORED_SUFFIXES


def _ignored_rg_globs() -> List[str]:
    globs = [f"!**/{name}/**" for name in sorted(_IGNORED_DIR_NAMES)]
    globs.extend(f"!**/*{suffix}" for suffix in sorted(_IGNORED_SUFFIXES))
    globs.extend(f"!**/{name}" for name in sorted(_IGNORED_FILE_NAMES))
    globs.extend(["!**/*.egg-info/**", "!**/*.dist-info/**"])
    return globs


def _missing_path_message(path: Path) -> str:
    parent = path.parent if path.parent.exists() else _nearest_existing_parent(path)
    suggestions: List[str] = []
    if parent and parent.exists():
        needle = path.name.lower()
        for child in sorted(parent.iterdir(), key=lambda item: item.name.lower()):
            if needle in child.name.lower() or child.name.lower() in needle:
                suggestions.append(str(child))
            if len(suggestions) >= 8:
                break
    message = f"path not found: {path}"
    if suggestions:
        message += "\ndid you mean:\n" + "\n".join(suggestions)
    return message


def _nearest_existing_parent(path: Path) -> Optional[Path]:
    for parent in path.parents:
        if parent.exists():
            return parent
    return None


def _near_miss_message(text: str, needle: str) -> str:
    if not needle:
        return "old text is empty"
    lines = text.splitlines()
    needle_head = needle.strip().splitlines()[0][:80] if needle.strip() else needle[:80]
    candidates = difflib.get_close_matches(needle_head, [line[:120] for line in lines], n=5, cutoff=0.45)
    if not candidates:
        return "no close line matches found"
    return "closest lines:\n" + "\n".join(candidates)


def _patch_paths(patch: str) -> List[str]:
    paths: List[str] = []
    for line in patch.splitlines():
        if line.startswith("+++ ") or line.startswith("--- "):
            value = line[4:].strip()
            if value == "/dev/null":
                continue
            if value.startswith("a/") or value.startswith("b/"):
                value = value[2:]
            if value not in paths:
                paths.append(value)
    return paths


def _cap(text: str, max_chars: int) -> str:
    if len(text) <= max_chars:
        return text
    head = max_chars // 2
    tail = max_chars - head
    return f"{text[:head]}\n\n[... omitted {len(text) - max_chars} chars ...]\n\n{text[-tail:]}"
