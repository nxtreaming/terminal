from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path


def main() -> None:
    _exec_rust_binary("browser-use-cli", "browser-use-terminal", sys.argv[1:])


def tui_main() -> None:
    _exec_rust_binary("browser-use-tui", "but", sys.argv[1:])


def _exec_rust_binary(package: str, binary: str, args: list[str]) -> None:
    repo_root = Path(__file__).resolve().parents[2]
    if (repo_root / "Cargo.toml").exists():
        os.chdir(repo_root)
        _ensure_agent_ripgrep(repo_root)
        raise SystemExit(subprocess.call(["cargo", "run", "-q", "-p", package, "--", *args]))
    binary_path = repo_root / "target" / "debug" / binary
    if binary_path.exists():
        os.execv(str(binary_path), [str(binary_path), *args])
    raise SystemExit(f"could not find Rust binary for {package}")


def _ensure_agent_ripgrep(repo_root: Path) -> None:
    script = repo_root / "scripts" / "install-agent-ripgrep.sh"
    if not script.exists():
        return
    dest = repo_root / "target" / "debug" / "agent-tools"
    rg = dest / "rg"
    if rg.exists():
        return
    subprocess.run(
        [str(script), str(dest)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
