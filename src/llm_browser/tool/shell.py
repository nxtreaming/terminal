from __future__ import annotations

import errno
import os
import pty
import queue
import secrets
import signal
import subprocess
import threading
import time
from typing import Any, Dict, Optional

from llm_browser.tool.context import ToolContext
from llm_browser.tool.result import ToolResult


MAX_INLINE_OUTPUT = 20000
MAX_POLL_OUTPUT = 20000
_PROCESS_LOCK = threading.Lock()
_PROCESSES: Dict[str, "ManagedProcess"] = {}


class ManagedProcess:
    def __init__(
        self,
        process_id: str,
        session_id: str,
        process: subprocess.Popen,
        command: str,
        timeout_s: float,
        pty_fd: Optional[int] = None,
    ) -> None:
        self.process_id = process_id
        self.session_id = session_id
        self.process = process
        self.command = command
        self.pty_fd = pty_fd
        self.started_at = time.time()
        self.timeout_at = self.started_at + timeout_s if timeout_s > 0 else None
        self.output: list[tuple[str, str]] = []
        self.read_index = 0
        self.lock = threading.Lock()
        self.readers: list[threading.Thread] = []

    def append(self, stream: str, text: str) -> None:
        with self.lock:
            self.output.append((stream, text))

    def read(self, all_output: bool = False) -> str:
        with self.lock:
            start = 0 if all_output else self.read_index
            chunks = self.output[start:]
            self.read_index = len(self.output)
        return _combine_ordered(chunks)

    def status(self) -> Dict[str, Any]:
        return {
            "process_id": self.process_id,
            "command": self.command,
            "returncode": self.process.poll(),
            "running": self.process.poll() is None,
            "age_s": max(0, time.time() - self.started_at),
            "pty": self.pty_fd is not None,
        }


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


def shell_start(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    command = str(arguments["command"])
    timeout_s = float(arguments.get("timeout_s", 0))
    use_pty = bool(arguments.get("pty", False))
    master_fd: int | None = None
    if use_pty:
        master_fd, slave_fd = pty.openpty()
        try:
            process = subprocess.Popen(
                command,
                shell=True,
                cwd=str(ctx.session.cwd),
                stdin=slave_fd,
                stdout=slave_fd,
                stderr=slave_fd,
                close_fds=True,
                start_new_session=True,
            )
        finally:
            os.close(slave_fd)
    else:
        process = subprocess.Popen(
            command,
            shell=True,
            cwd=str(ctx.session.cwd),
            text=True,
            bufsize=1,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
    process_id = f"proc_{secrets.token_hex(4)}"
    managed = ManagedProcess(process_id, ctx.session.id, process, command, timeout_s, pty_fd=master_fd)
    if use_pty:
        managed.readers = [threading.Thread(target=_read_managed_pty, args=(managed, master_fd), daemon=True)]
    else:
        managed.readers = [
            threading.Thread(target=_read_managed_pipe, args=(managed, process.stdout, "stdout"), daemon=True),
            threading.Thread(target=_read_managed_pipe, args=(managed, process.stderr, "stderr"), daemon=True),
        ]
    for reader in managed.readers:
        reader.start()
    with _PROCESS_LOCK:
        _PROCESSES[process_id] = managed
    return ToolResult(
        text=f"started {process_id}",
        data={"process_id": process_id, "pid": process.pid, "running": True, "command": command, "pty": use_pty},
    )


def shell_poll(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    process_id = str(arguments["process_id"])
    all_output = bool(arguments.get("all", False))
    managed = _get_process(ctx, process_id)
    _enforce_managed_timeout(managed)
    text = managed.read(all_output=all_output)
    if text:
        ctx.emit_output(_cap(text, 4000), stream="process")
    status = managed.status()
    if not status["running"]:
        _finish_managed(managed)
    return ToolResult(text=_cap(text, MAX_POLL_OUTPUT), data=status)


def shell_stdin(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    process_id = str(arguments["process_id"])
    text = str(arguments.get("text", ""))
    managed = _get_process(ctx, process_id)
    if managed.process.poll() is not None:
        raise RuntimeError(f"process is not running: {process_id}")
    if managed.pty_fd is not None:
        os.write(managed.pty_fd, text.encode("utf-8", errors="replace"))
    elif managed.process.stdin is None:
        raise RuntimeError(f"process has no stdin: {process_id}")
    else:
        managed.process.stdin.write(text)
        managed.process.stdin.flush()
    return ToolResult(text=f"wrote {len(text)} chars to {process_id}", data=managed.status())


def shell_stop(ctx: ToolContext, arguments: Dict[str, Any]) -> ToolResult:
    process_id = str(arguments["process_id"])
    managed = _get_process(ctx, process_id)
    _terminate_process_group(managed.process, timeout=2)
    _finish_managed(managed)
    text = managed.read(all_output=True)
    with _PROCESS_LOCK:
        _PROCESSES.pop(process_id, None)
    return ToolResult(text=_cap(text, MAX_POLL_OUTPUT), data={**managed.status(), "stopped": True})


def close_shell_session(session_id: str) -> None:
    with _PROCESS_LOCK:
        owned = [process for process in _PROCESSES.values() if process.session_id == session_id]
    for managed in owned:
        _terminate_process_group(managed.process, timeout=1)
        _finish_managed(managed)
        with _PROCESS_LOCK:
            _PROCESSES.pop(managed.process_id, None)


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


def _read_managed_pipe(managed: ManagedProcess, pipe, stream: str) -> None:
    if pipe is None:
        return
    try:
        for line in iter(pipe.readline, ""):
            if line:
                managed.append(stream, line)
    finally:
        try:
            pipe.close()
        except Exception:
            pass


def _read_managed_pty(managed: ManagedProcess, fd: Optional[int]) -> None:
    if fd is None:
        return
    while True:
        try:
            data = os.read(fd, 4096)
        except OSError as exc:
            if exc.errno in {errno.EIO, errno.EBADF}:
                return
            managed.append("pty", f"\n[pty read failed: {exc}]\n")
            return
        if not data:
            return
        managed.append("pty", data.decode("utf-8", errors="replace"))


def _get_process(ctx: ToolContext, process_id: str) -> ManagedProcess:
    with _PROCESS_LOCK:
        managed = _PROCESSES.get(process_id)
    if managed is None:
        raise KeyError(f"unknown process id: {process_id}")
    if managed.session_id != ctx.session.id:
        raise PermissionError(f"process {process_id} belongs to another session")
    return managed


def _enforce_managed_timeout(managed: ManagedProcess) -> None:
    if managed.timeout_at is None or managed.process.poll() is not None:
        return
    if time.time() < managed.timeout_at:
        return
    _kill_process_group(managed.process, timeout=1)
    managed.append("stderr", f"\n[process timed out after {managed.timeout_at - managed.started_at:.1f}s]\n")


def _finish_managed(managed: ManagedProcess) -> None:
    if managed.process.stdin is not None:
        try:
            managed.process.stdin.close()
        except Exception:
            pass
    for reader in managed.readers:
        reader.join(timeout=0.5)
    if managed.pty_fd is not None:
        try:
            os.close(managed.pty_fd)
        except OSError:
            pass
        managed.pty_fd = None


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


def _cap(text: str, max_chars: int) -> str:
    if len(text) <= max_chars:
        return text
    head = max_chars // 2
    tail = max_chars - head
    return f"{text[:head]}\n[... omitted {len(text) - max_chars} chars ...]\n{text[-tail:]}"


shell_start.close_session = close_shell_session  # type: ignore[attr-defined]
shell_poll.close_session = close_shell_session  # type: ignore[attr-defined]
shell_stdin.close_session = close_shell_session  # type: ignore[attr-defined]
shell_stop.close_session = close_shell_session  # type: ignore[attr-defined]
