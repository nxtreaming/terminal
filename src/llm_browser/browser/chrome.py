from __future__ import annotations

import os
import shutil
import socket
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

import requests


def find_chrome_path() -> Path:
    env_path = os.environ.get("LLM_BROWSER_CHROME")
    if env_path:
        path = Path(env_path).expanduser()
        if path.exists():
            return path

    candidates = [
        Path("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        Path("/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary"),
        Path("/Applications/Chromium.app/Contents/MacOS/Chromium"),
        Path("/usr/bin/google-chrome"),
        Path("/usr/bin/google-chrome-stable"),
        Path("/usr/bin/chromium"),
        Path("/usr/bin/chromium-browser"),
    ]
    for candidate in candidates:
        if candidate.exists():
            return candidate
    raise FileNotFoundError("Chrome not found. Set LLM_BROWSER_CHROME to the Chrome executable.")


def find_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


@dataclass(frozen=True)
class ChromeConfig:
    chrome_path: Path
    user_data_dir: Path
    port: int
    downloads_dir: Path
    profile_template: Optional[Path] = None
    headless: bool = False
    width: int = 1280
    height: int = 900


@dataclass
class ChromeProcess:
    config: ChromeConfig
    process: subprocess.Popen

    @property
    def http_url(self) -> str:
        return f"http://127.0.0.1:{self.config.port}"

    def stop(self) -> None:
        if self.process.poll() is not None:
            return
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)


def prepare_profile(user_data_dir: Path, profile_template: Optional[Path]) -> None:
    if user_data_dir.exists():
        return
    if profile_template:
        shutil.copytree(profile_template.expanduser(), user_data_dir)
    else:
        user_data_dir.mkdir(parents=True, exist_ok=True)


def start_chrome(
    root_dir: Path,
    profile_template: Optional[Path] = None,
    chrome_path: Optional[Path] = None,
    headless: bool = False,
    width: int = 1280,
    height: int = 900,
) -> ChromeProcess:
    root_dir.mkdir(parents=True, exist_ok=True)
    user_data_dir = root_dir / "chrome-profile"
    downloads_dir = root_dir / "downloads"
    downloads_dir.mkdir(parents=True, exist_ok=True)
    prepare_profile(user_data_dir, profile_template)

    config = ChromeConfig(
        chrome_path=chrome_path or find_chrome_path(),
        user_data_dir=user_data_dir,
        port=find_free_port(),
        downloads_dir=downloads_dir,
        profile_template=profile_template,
        headless=headless,
        width=width,
        height=height,
    )
    args = [
        str(config.chrome_path),
        f"--remote-debugging-port={config.port}",
        "--remote-allow-origins=*",
        f"--user-data-dir={config.user_data_dir}",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-background-networking",
        "--disable-background-timer-throttling",
        "--disable-client-side-phishing-detection",
        "--disable-component-update",
        "--disable-default-apps",
        "--disable-extensions",
        "--disable-renderer-backgrounding",
        "--disable-search-engine-choice-screen",
        "--disable-sync",
        "--metrics-recording-only",
        "--safebrowsing-disable-auto-update",
        "--disable-features=AutofillServerCommunication,MediaRouter,OptimizationGuideModelDownloading,OptimizationHints,OptimizationHintsFetching,OptimizationTargetPrediction,PrivacySandboxSettings4,Translate",
        f"--window-size={config.width},{config.height}",
        "about:blank",
    ]
    if headless:
        args.insert(-1, "--headless=new")

    process = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    chrome = ChromeProcess(config=config, process=process)
    wait_for_devtools(chrome.http_url, process)
    return chrome


def wait_for_devtools(http_url: str, process: subprocess.Popen, timeout_s: float = 20.0) -> None:
    deadline = time.time() + timeout_s
    last_error = None
    while time.time() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"Chrome exited early with status {process.returncode}")
        try:
            response = requests.get(f"{http_url}/json/version", timeout=1)
            if response.status_code == 200:
                return
            last_error = RuntimeError(response.text[:500])
        except Exception as exc:
            last_error = exc
        time.sleep(0.1)
    raise TimeoutError(f"Chrome DevTools endpoint did not start at {http_url}: {last_error}")
