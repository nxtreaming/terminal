from __future__ import annotations

import subprocess
from typing import Any, Dict

from llm_browser.tool.context import ToolContext
from llm_browser.tool.result import ToolResult


MAX_INLINE_OUTPUT = 20000


def shell(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    command = str(arguments["command"])
    timeout_s = float(arguments.get("timeout_s", 60))
    result = subprocess.run(
        command,
        shell=True,
        cwd=str(ctx.session.cwd),
        text=True,
        capture_output=True,
        timeout=timeout_s,
    )
    combined = ""
    if result.stdout:
        combined += result.stdout
    if result.stderr:
        if combined:
            combined += "\n"
        combined += result.stderr

    data: Dict[str, Any] = {"returncode": result.returncode}
    if len(combined) > MAX_INLINE_OUTPUT:
        output_dir = ctx.session.artifact_dir / "tool-output"
        output_dir.mkdir(parents=True, exist_ok=True)
        path = output_dir / f"{ctx.tool_call_id}_{ctx.tool_name}.txt"
        path.write_text(combined, encoding="utf-8")
        text = combined[:MAX_INLINE_OUTPUT] + f"\n\n[full output saved to {path}]"
        data["output_path"] = str(path)
        data["truncated"] = True
    else:
        text = combined
        data["truncated"] = False
    return ToolResult(text=text, data=data)
