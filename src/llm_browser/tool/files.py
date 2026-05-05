from __future__ import annotations

import fnmatch
from pathlib import Path
from typing import Any, Dict, Iterable, List

from llm_browser.tool.context import ToolContext
from llm_browser.tool.result import ToolResult


MAX_READ_CHARS = 20000


def read_file(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    path = _resolve(ctx, str(arguments["path"]))
    offset = int(arguments.get("offset", 0))
    limit = int(arguments.get("limit", MAX_READ_CHARS))
    text = path.read_text(encoding="utf-8", errors="replace")
    chunk = text[offset : offset + limit]
    return ToolResult(text=chunk, data={"path": str(path), "offset": offset, "total_chars": len(text)})


def write_file(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    path = _resolve(ctx, str(arguments["path"]))
    content = str(arguments.get("content", ""))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    return ToolResult(text=f"wrote {path}", data={"path": str(path), "chars": len(content)})


def edit_file(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    path = _resolve(ctx, str(arguments["path"]))
    old = str(arguments["old"])
    new = str(arguments["new"])
    replace_all = bool(arguments.get("replace_all", False))
    text = path.read_text(encoding="utf-8", errors="replace")
    count = text.count(old)
    if count == 0:
        raise ValueError(f"old text not found in {path}")
    if count > 1 and not replace_all:
        raise ValueError(f"old text appears {count} times in {path}; set replace_all=true or make old text unique")
    updated = text.replace(old, new) if replace_all else text.replace(old, new, 1)
    path.write_text(updated, encoding="utf-8")
    changed = count if replace_all else 1
    return ToolResult(text=f"edited {path}", data={"path": str(path), "replacements": changed})


def glob_files(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    pattern = str(arguments["pattern"])
    root = _resolve(ctx, str(arguments.get("root", ".")))
    limit = int(arguments.get("limit", 200))
    matches = [str(path) for path in sorted(root.glob(pattern))][:limit]
    return ToolResult(text="\n".join(matches), data={"root": str(root), "pattern": pattern, "count": len(matches)})


def grep_files(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    pattern = str(arguments["pattern"])
    root = _resolve(ctx, str(arguments.get("root", ".")))
    include = str(arguments.get("include", "*"))
    limit = int(arguments.get("limit", 200))
    lines: List[str] = []
    for path in _iter_files(root, include):
        if len(lines) >= limit:
            break
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            if pattern in line:
                lines.append(f"{path}:{lineno}:{line}")
                if len(lines) >= limit:
                    break
    return ToolResult(text="\n".join(lines), data={"root": str(root), "pattern": pattern, "count": len(lines)})


def _resolve(ctx: ToolContext, path: str) -> Path:
    candidate = Path(path).expanduser()
    if not candidate.is_absolute():
        candidate = ctx.session.cwd / candidate
    return candidate.resolve()


def _iter_files(root: Path, include: str) -> Iterable[Path]:
    if root.is_file():
        yield root
        return
    for path in sorted(root.rglob("*")):
        if path.is_file() and fnmatch.fnmatch(path.name, include):
            yield path
