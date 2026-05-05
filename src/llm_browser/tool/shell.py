from __future__ import annotations

import subprocess
import time
from typing import Any, Dict

from llm_browser.tool.context import ToolContext
from llm_browser.tool.result import ToolResult


MAX_INLINE_OUTPUT = 20000


def shell(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    command = str(arguments["command"])
    timeout_s = float(arguments.get("timeout_s", 60))
    deadline = time.time() + timeout_s
    process = subprocess.Popen(
        command,
        shell=True,
        cwd=str(ctx.session.cwd),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    while process.poll() is None:
        if ctx.is_cancel_requested():
            process.terminate()
            try:
                stdout, stderr = process.communicate(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                stdout, stderr = process.communicate(timeout=2)
            combined_cancel = _combine(stdout, stderr)
            return ToolResult(
                text=combined_cancel,
                data={"returncode": process.returncode, "cancelled": True, "truncated": False},
            )
        if time.time() >= deadline:
            process.kill()
            stdout, stderr = process.communicate(timeout=2)
            combined_timeout = _combine(stdout, stderr)
            return ToolResult(
                text=combined_timeout,
                data={
                    "returncode": process.returncode,
                    "timeout_s": timeout_s,
                    "timed_out": True,
                    "truncated": False,
                },
            )
        time.sleep(0.1)

    stdout, stderr = process.communicate()
    combined = _combine(stdout, stderr)
    return _tool_result_from_output(ctx, combined, process.returncode)


def _combine(stdout: str, stderr: str) -> str:
    combined = ""
    if stdout:
        combined += stdout
    if stderr:
        if combined:
            combined += "\n"
        combined += stderr
    return combined


def _tool_result_from_output(ctx: ToolContext, combined: str, returncode: int) -> ToolResult:
    data: Dict[str, Any] = {"returncode": returncode}
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
