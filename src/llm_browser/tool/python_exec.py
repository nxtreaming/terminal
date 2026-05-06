from __future__ import annotations

import contextlib
import json
import os
import threading
from pathlib import Path
from typing import Any, Dict, Iterator


def execute_python(code: str, namespace: Dict[str, Any]) -> Any:
    if looks_like_statements(code):
        exec(compile(code, "<llm-browser-python>", "exec"), namespace, namespace)
        return None
    try:
        compiled = compile(code, "<llm-browser-python>", "eval")
    except SyntaxError:
        exec(compile(code, "<llm-browser-python>", "exec"), namespace, namespace)
        return None
    return eval(compiled, namespace, namespace)


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
