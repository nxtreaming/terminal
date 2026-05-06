from __future__ import annotations

import queue
import os
import signal
import subprocess
import threading
import time
from typing import Any, Dict

from llm_browser.tool.context import ToolContext
from llm_browser.tool.result import ToolResult


MAX_INLINE_OUTPUT = 20000


def shell(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    command = str(arguments["command"])
    timeout_s = float(arguments.get("timeout_s", 60))
    deadline = time.time() + timeout_s
    output: list[tuple[str, str]] = []
    pending: list[tuple[str, str]] = []
    chunks: "queue.Queue[tuple[str, str]]" = queue.Queue()
    process = subprocess.Popen(
        command,
        shell=True,
        cwd=str(ctx.session.cwd),
        text=True,
        bufsize=1,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )

    readers = [
        threading.Thread(target=_read_pipe, args=(process.stdout, "stdout", chunks), daemon=True),
        threading.Thread(target=_read_pipe, args=(process.stderr, "stderr", chunks), daemon=True),
    ]
    for reader in readers:
        reader.start()

    last_emit = time.time()
    while process.poll() is None:
        _drain_chunks(chunks, output, pending)
        if pending and (time.time() - last_emit >= 0.5 or _pending_size(pending) >= 2000):
            _emit_pending(ctx, pending)
            last_emit = time.time()
        if ctx.is_cancel_requested():
            _terminate_process_group(process, timeout=2)
            _finish_readers(readers, chunks, output, pending)
            _emit_pending(ctx, pending)
            combined_cancel = _combine_ordered(output)
            return ToolResult(
                text=combined_cancel,
                data={"returncode": process.returncode, "cancelled": True, "truncated": False},
            )
        if time.time() >= deadline:
            _kill_process_group(process, timeout=2)
            _finish_readers(readers, chunks, output, pending)
            _emit_pending(ctx, pending)
            combined_timeout = _combine_ordered(output)
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

    process.communicate()
    _finish_readers(readers, chunks, output, pending)
    _emit_pending(ctx, pending)
    combined = _combine_ordered(output)
    return _tool_result_from_output(ctx, combined, process.returncode)


def _read_pipe(pipe, stream: str, chunks: "queue.Queue[tuple[str, str]]") -> None:
    if pipe is None:
        return
    try:
        for line in iter(pipe.readline, ""):
            if line:
                chunks.put((stream, line))
    finally:
        try:
            pipe.close()
        except Exception:
            pass


def _terminate_process_group(process: subprocess.Popen, timeout: float) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        _kill_process_group(process, timeout=timeout)


def _kill_process_group(process: subprocess.Popen, timeout: float) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=timeout)


def _drain_chunks(
    chunks: "queue.Queue[tuple[str, str]]",
    output: list[tuple[str, str]],
    pending: list[tuple[str, str]],
) -> None:
    while True:
        try:
            item = chunks.get_nowait()
        except queue.Empty:
            return
        output.append(item)
        pending.append(item)


def _finish_readers(
    readers: list[threading.Thread],
    chunks: "queue.Queue[tuple[str, str]]",
    output: list[tuple[str, str]],
    pending: list[tuple[str, str]],
) -> None:
    for reader in readers:
        reader.join(timeout=1)
    _drain_chunks(chunks, output, pending)


def _pending_size(pending: list[tuple[str, str]]) -> int:
    return sum(len(text) for _, text in pending)


def _emit_pending(ctx: ToolContext, pending: list[tuple[str, str]]) -> None:
    if not pending:
        return
    text = _combine_ordered(pending)
    if len(text) > 4000:
        text = text[:4000] + f"\n[... streamed chunk truncated by {len(text) - 4000} chars ...]"
    streams = {stream for stream, _ in pending}
    stream = next(iter(streams)) if len(streams) == 1 else "mixed"
    ctx.emit_output(text, stream=stream)
    pending.clear()


def _combine_ordered(chunks: list[tuple[str, str]]) -> str:
    return "".join(text for _, text in chunks)


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
