from __future__ import annotations

import ast
import contextlib
import json
import os
import sys
import time
import threading
from pathlib import Path
from typing import Any, Callable, Dict, Iterator, Optional


def execute_python(code: str, namespace: Dict[str, Any]) -> Any:
    try:
        module = ast.parse(code, mode="exec")
    except SyntaxError:
        module = None
    if module is not None:
        if module.body and isinstance(module.body[-1], ast.Expr):
            prefix = ast.Module(body=module.body[:-1], type_ignores=module.type_ignores)
            ast.fix_missing_locations(prefix)
            if prefix.body:
                exec(compile(prefix, "<llm-browser-python>", "exec"), namespace, namespace)
            expression = ast.Expression(module.body[-1].value)
            ast.fix_missing_locations(expression)
            return eval(compile(expression, "<llm-browser-python>", "eval"), namespace, namespace)
        if looks_like_statements(code):
            exec(compile(module, "<llm-browser-python>", "exec"), namespace, namespace)
            return None
    try:
        compiled = compile(code, "<llm-browser-python>", "eval")
    except SyntaxError:
        exec(compile(code, "<llm-browser-python>", "exec"), namespace, namespace)
        return None
    return eval(compiled, namespace, namespace)


@contextlib.contextmanager
def cancellation_trace(cancel_check: Optional[Callable[[], None]], interval_s: float = 0.05) -> Iterator[None]:
    if cancel_check is None:
        yield
        return

    previous = sys.gettrace()
    next_check_at = 0.0

    def trace(frame: Any, event: str, arg: Any) -> Any:
        nonlocal next_check_at
        if event in {"line", "call", "return"}:
            now = time.monotonic()
            if now >= next_check_at:
                cancel_check()
                next_check_at = now + interval_s
        return trace

    sys.settrace(trace)
    try:
        yield
    finally:
        sys.settrace(previous)


def cancellable_sleep(seconds: float, cancel_check: Optional[Callable[[], None]], interval_s: float = 0.05) -> None:
    deadline = time.monotonic() + max(0.0, float(seconds))
    while True:
        if cancel_check is not None:
            cancel_check()
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return
        time.sleep(min(interval_s, remaining))


class CancellableTimeModule:
    def __init__(self, cancel_check: Callable[[], None]) -> None:
        self._cancel_check = cancel_check

    def sleep(self, seconds: float) -> None:
        cancellable_sleep(seconds, self._cancel_check)

    def __getattr__(self, name: str) -> Any:
        return getattr(time, name)


@contextlib.contextmanager
def execution_cwd(cwd: Path, lock: threading.RLock) -> Iterator[None]:
    with lock:
        previous = Path.cwd()
        cwd.mkdir(parents=True, exist_ok=True)
        os.chdir(cwd)
        try:
            yield
        finally:
            os.chdir(previous)


def is_jsonable(value: Any) -> bool:
    try:
        json.dumps(value)
        return True
    except TypeError:
        return False


def looks_like_statements(code: str) -> bool:
    stripped = code.strip()
    if "\n" in stripped:
        return True
    statement_prefixes = (
        "import ",
        "from ",
        "for ",
        "while ",
        "if ",
        "with ",
        "try:",
        "def ",
        "class ",
        "return ",
        "raise ",
        "assert ",
        "print(",
    )
    return stripped.startswith(statement_prefixes) or "=" in stripped and "==" not in stripped
