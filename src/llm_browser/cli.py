from __future__ import annotations

import argparse
import json
import os
import sys
import threading
import time
import uuid
from pathlib import Path
from typing import Any, Dict, Optional, Sequence

from llm_browser.agent import Agent
from llm_browser.brand import CLI_NAME, DEFAULT_STATE_DIR
from llm_browser.config import (
    apply_config_environment,
    config_get,
    example_config,
    load_config,
    redacted_config,
    writable_config_path,
    write_config_value,
    write_default_config,
)
from llm_browser.datasets import (
    build_dataset_prompt,
    dataset_summary,
    load_dataset,
    load_manifest,
    manifest_path,
    select_tasks,
    summarize_manifest,
)
from llm_browser.provider.base import Provider
from llm_browser.llm.registry import provider_names
from llm_browser.session.trace import build_self_eval_prompt, write_trace_bundle
from llm_browser.session.store import SessionStore

DATASET_TIMEOUT_DRAIN_S = 10.0
MAX_INLINE_DATASET_RESULT = 20_000
BROWSER_MODE_CHOICES = ["auto", "chromium", "headless-chromium", "real", "cdp", "cloud", "remote", "daemon"]
AGENT_MODE_CHOICES = ["auto", "browser", "codex"]
PROVIDER_CHOICES = provider_names()
API_KEY_PROVIDER_CHOICES = ["openai", "anthropic", "zai", "qwen", "openrouter"]
TOP_LEVEL_COMMANDS = {"doctor", "provider", "run", "sessions", "browser", "datasets", "tui", "config", "auth"}


def add_browser_runtime_args(
    parser: argparse.ArgumentParser,
    headless_default: Optional[bool],
    config: Optional[Dict[str, Any]] = None,
) -> None:
    browser_config = config_get(config or {}, "browser", {})
    if not isinstance(browser_config, dict):
        browser_config = {}

    def browser_default(name: str, default: Any = None) -> Any:
        value = browser_config.get(name)
        return default if value is None else value

    parser.add_argument(
        "--browser",
        choices=BROWSER_MODE_CHOICES,
        default=browser_default("mode", None),
        help="Browser backend: owned Chromium, real Chrome, explicit CDP, or Browser Use cloud.",
    )
    parser.add_argument(
        "--headless",
        action=argparse.BooleanOptionalAction,
        default=browser_default("headless", headless_default),
        help="Run owned Chromium headless. Ignored for real/cdp/cloud backends.",
    )
    parser.add_argument("--cdp-url", default=browser_default("cdp_url"), help="DevTools HTTP endpoint, e.g. http://127.0.0.1:9222.")
    parser.add_argument("--cdp-ws", default=browser_default("cdp_ws"), help="Raw CDP websocket URL.")
    parser.add_argument("--chrome-path", default=browser_default("chrome_path"), help="Chrome/Chromium executable for owned Chromium mode.")
    parser.add_argument("--profile-template", default=browser_default("profile_template"), help="Copy this profile directory into the owned Chromium profile.")
    parser.add_argument(
        "--keep-profile",
        action=argparse.BooleanOptionalAction,
        default=browser_default("keep_profile", False),
        help="Keep the owned Chromium profile after runtime close.",
    )
    parser.add_argument("--browser-width", type=int, default=browser_default("width"), help="Browser viewport/window width.")
    parser.add_argument("--browser-height", type=int, default=browser_default("height"), help="Browser viewport/window height.")
    parser.add_argument("--cloud-profile-id", default=browser_default("cloud_profile_id"), help="Browser Use cloud profile UUID.")
    parser.add_argument("--cloud-profile-name", default=browser_default("cloud_profile_name"), help="Browser Use cloud profile name, resolved at startup.")
    parser.add_argument("--cloud-proxy-country", default=browser_default("cloud_proxy_country"), help="Browser Use proxy country code; pass 'none' to disable.")
    parser.add_argument("--cloud-timeout", type=int, default=browser_default("cloud_timeout"), help="Browser Use cloud timeout in minutes.")
    parser.add_argument("--cloud-allow-resizing", action=argparse.BooleanOptionalAction, default=browser_default("cloud_allow_resizing"))
    parser.add_argument("--cloud-recording", action=argparse.BooleanOptionalAction, default=browser_default("cloud_recording"))
    parser.add_argument("--cloud-custom-proxy-json", default=browser_default("cloud_custom_proxy_json"), help="JSON object forwarded as Browser Use customProxy.")
    parser.add_argument("--daemon-name", default=browser_default("daemon_name"), help="Name for daemon browser mode.")
    parser.add_argument("--daemon-backend", default=browser_default("daemon_backend"), help="Backend used inside daemon mode: chromium, cdp, real, or cloud.")


def apply_browser_runtime_args(args: argparse.Namespace) -> None:
    mappings = [
        ("browser", "LLM_BROWSER_MODE"),
        ("cdp_url", "LLM_BROWSER_CDP_HTTP_URL"),
        ("cdp_ws", "LLM_BROWSER_CDP_WS_URL"),
        ("chrome_path", "LLM_BROWSER_CHROME_PATH"),
        ("profile_template", "LLM_BROWSER_PROFILE_TEMPLATE"),
        ("browser_width", "LLM_BROWSER_WIDTH"),
        ("browser_height", "LLM_BROWSER_HEIGHT"),
        ("cloud_profile_id", "LLM_BROWSER_CLOUD_PROFILE_ID"),
        ("cloud_profile_name", "LLM_BROWSER_CLOUD_PROFILE_NAME"),
        ("cloud_proxy_country", "LLM_BROWSER_CLOUD_PROXY_COUNTRY"),
        ("cloud_timeout", "LLM_BROWSER_CLOUD_TIMEOUT"),
        ("cloud_custom_proxy_json", "LLM_BROWSER_CLOUD_CUSTOM_PROXY_JSON"),
        ("daemon_name", "LLM_BROWSER_DAEMON_NAME"),
        ("daemon_backend", "LLM_BROWSER_DAEMON_BACKEND"),
    ]
    for attr, env_name in mappings:
        if not hasattr(args, attr):
            continue
        value = getattr(args, attr)
        if value is not None:
            os.environ[env_name] = str(value)
    if hasattr(args, "headless") and args.headless is not None:
        os.environ["LLM_BROWSER_HEADLESS"] = "1" if args.headless else "0"
    if getattr(args, "keep_profile", False):
        os.environ["LLM_BROWSER_KEEP_CHROME_PROFILE"] = "1"
    if hasattr(args, "cloud_allow_resizing") and args.cloud_allow_resizing is not None:
        os.environ["LLM_BROWSER_CLOUD_ALLOW_RESIZING"] = "1" if args.cloud_allow_resizing else "0"
    if hasattr(args, "cloud_recording") and args.cloud_recording is not None:
        os.environ["LLM_BROWSER_CLOUD_ENABLE_RECORDING"] = "1" if args.cloud_recording else "0"


def _provider_default(config: Optional[Dict[str, Any]], default: str) -> str:
    provider = config_get(config or {}, "provider", default)
    if provider in PROVIDER_CHOICES:
        return str(provider)
    return default


def _model_default(config: Optional[Dict[str, Any]], default: Optional[str]) -> Optional[str]:
    value = config_get(config or {}, "model", default)
    return str(value) if value else default


def _max_turns_default(config: Optional[Dict[str, Any]], default: int) -> int:
    value = config_get(config or {}, "agent.max_turns", config_get(config or {}, "max_turns", default))
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def _agent_mode_default(config: Optional[Dict[str, Any]], default: str = "auto") -> str:
    value = config_get(config or {}, "agent.mode", default)
    return str(value) if value in AGENT_MODE_CHOICES else default


def build_parser(config: Optional[Dict[str, Any]] = None) -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog=CLI_NAME)
    parser.add_argument("--config", default=None, help="JSON config file. Defaults to ~/.browser-use-terminal/config.json and ./.browser-use-terminal/config.json.")
    parser.add_argument(
        "--state-dir",
        default=config_get(config or {}, "state_dir", DEFAULT_STATE_DIR),
        help=f"Runtime state directory. Defaults to {DEFAULT_STATE_DIR} in the current directory.",
    )

    sub = parser.add_subparsers(dest="command", required=True)

    doctor = sub.add_parser("doctor", help="Print local harness diagnostics.")
    doctor.set_defaults(func=cmd_doctor)

    provider = sub.add_parser("provider", help="Provider diagnostics and smoke tests.")
    provider_sub = provider.add_subparsers(dest="provider_command", required=True)

    provider_image_smoke = provider_sub.add_parser(
        "image-smoke",
        help="Send a two-frame synthetic screenshot timeline through a provider.",
    )
    provider_image_smoke.add_argument("--provider", choices=PROVIDER_CHOICES, default=_provider_default(config, "codex"))
    provider_image_smoke.add_argument("--model", default=_model_default(config, "gpt-5.5"))
    provider_image_smoke.set_defaults(func=cmd_provider_image_smoke)

    run = sub.add_parser("run", help="Create a session with a user task.")
    run.add_argument("task", help="Task to give the browser agent.")
    run.add_argument("--parent-id", default=None, help="Optional parent session id.")
    run.add_argument(
        "--provider",
        choices=PROVIDER_CHOICES,
        default=_provider_default(config, "fake"),
        help="Provider to use.",
    )
    run.add_argument("--model", default=_model_default(config, None), help="Model name for the selected provider.")
    run.add_argument("--max-turns", type=int, default=_max_turns_default(config, 80), help="Maximum model/tool turns before failing.")
    run.add_argument("--agent-mode", choices=AGENT_MODE_CHOICES, default=_agent_mode_default(config), help="Instruction mode: auto, browser, or codex.")
    add_browser_runtime_args(run, headless_default=None, config=config)
    run.set_defaults(func=cmd_run)

    sessions = sub.add_parser("sessions", help="Inspect sessions.")
    sessions_sub = sessions.add_subparsers(dest="sessions_command", required=True)

    sessions_list = sessions_sub.add_parser("list", help="List known sessions.")
    sessions_list.set_defaults(func=cmd_sessions_list)

    sessions_show = sessions_sub.add_parser("show", help="Show a session and recent events.")
    sessions_show.add_argument("session_id")
    sessions_show.add_argument("--events", type=int, default=20, help="Number of events to print.")
    sessions_show.set_defaults(func=cmd_sessions_show)

    sessions_cancel = sessions_sub.add_parser("cancel", help="Request cancellation for a running session.")
    sessions_cancel.add_argument("session_id")
    sessions_cancel.add_argument("--reason", default="cli requested cancellation")
    sessions_cancel.set_defaults(func=cmd_sessions_cancel)

    sessions_resume = sessions_sub.add_parser("resume", help="Resume a session from its event trace.")
    sessions_resume.add_argument("session_id")
    sessions_resume.add_argument("instruction", nargs="?", default="Continue from the previous session state.")
    sessions_resume.add_argument("--provider", choices=PROVIDER_CHOICES, default=_provider_default(config, "codex"))
    sessions_resume.add_argument("--model", default=_model_default(config, "gpt-5.5"))
    sessions_resume.add_argument("--max-turns", type=int, default=_max_turns_default(config, 80))
    sessions_resume.add_argument("--agent-mode", choices=AGENT_MODE_CHOICES, default=_agent_mode_default(config))
    sessions_resume.set_defaults(func=cmd_sessions_resume)

    sessions_compact = sessions_sub.add_parser("compact", help="Compact a session history checkpoint.")
    sessions_compact.add_argument("session_id")
    sessions_compact.add_argument("--provider", choices=PROVIDER_CHOICES, default=_provider_default(config, "codex"))
    sessions_compact.add_argument("--model", default=_model_default(config, "gpt-5.5"))
    sessions_compact.add_argument("--agent-mode", choices=AGENT_MODE_CHOICES, default=_agent_mode_default(config))
    sessions_compact.set_defaults(func=cmd_sessions_compact)

    sessions_trace = sessions_sub.add_parser("trace", help="Write a JSON trace bundle for a session.")
    sessions_trace.add_argument("session_id")
    sessions_trace.set_defaults(func=cmd_sessions_trace)

    sessions_eval = sessions_sub.add_parser("self-eval", help="Run an LLM evaluator as a child session over a trace.")
    sessions_eval.add_argument("session_id")
    sessions_eval.add_argument("--provider", choices=PROVIDER_CHOICES, default=_provider_default(config, "codex"))
    sessions_eval.add_argument("--model", default=_model_default(config, "gpt-5.5"))
    sessions_eval.add_argument("--max-turns", type=int, default=_max_turns_default(config, 20))
    sessions_eval.add_argument("--agent-mode", choices=AGENT_MODE_CHOICES, default=_agent_mode_default(config))
    sessions_eval.set_defaults(func=cmd_sessions_self_eval)

    browser = sub.add_parser("browser", help="Browser runtime commands.")
    browser_sub = browser.add_subparsers(dest="browser_command", required=True)

    browser_smoke = browser_sub.add_parser("smoke", help="Launch Chrome, navigate, and capture a screenshot.")
    browser_smoke.add_argument("--url", default="https://example.com", help="URL to open.")
    add_browser_runtime_args(browser_smoke, headless_default=False, config=config)
    browser_smoke.set_defaults(func=cmd_browser_smoke)

    browser_daemon = browser_sub.add_parser("daemon", help="Manage local browser daemon.")
    browser_daemon_sub = browser_daemon.add_subparsers(dest="browser_daemon_command", required=True)

    browser_daemon_status = browser_daemon_sub.add_parser("status", help="Show daemon status.")
    browser_daemon_status.add_argument("--name", default=None)
    browser_daemon_status.set_defaults(func=cmd_browser_daemon_status)

    browser_daemon_stop = browser_daemon_sub.add_parser("stop", help="Stop daemon.")
    browser_daemon_stop.add_argument("--name", default=None)
    browser_daemon_stop.set_defaults(func=cmd_browser_daemon_stop)

    datasets = sub.add_parser("datasets", help="Run or sample browser benchmark datasets.")
    datasets_sub = datasets.add_subparsers(dest="datasets_command", required=True)

    datasets_list = datasets_sub.add_parser("list", help="List bundled dataset aliases.")
    datasets_list.set_defaults(func=cmd_datasets_list)

    datasets_sample = datasets_sub.add_parser("sample", help="Print random tasks from a dataset.")
    datasets_sample.add_argument("dataset")
    datasets_sample.add_argument("--count", type=int, default=1)
    datasets_sample.add_argument("--seed", type=int, default=None)
    datasets_sample.add_argument("--task-id", action="append", default=None)
    datasets_sample.set_defaults(func=cmd_datasets_sample)

    datasets_run = datasets_sub.add_parser("run", help="Run selected dataset tasks through the harness.")
    datasets_run.add_argument("dataset")
    datasets_run.add_argument("--all", action="store_true", help="Run every task in the dataset.")
    datasets_run.add_argument("--count", type=int, default=1)
    datasets_run.add_argument("--seed", type=int, default=None)
    datasets_run.add_argument("--task-id", action="append", default=None)
    datasets_run.add_argument("--run-id", default=None, help="Use a stable run id for manifest/workspaces.")
    datasets_run.add_argument("--resume", action="store_true", help="Skip latest successful task attempts in the run manifest.")
    datasets_run.add_argument("--provider", choices=PROVIDER_CHOICES, default=_provider_default(config, "codex"))
    datasets_run.add_argument("--model", default=_model_default(config, "gpt-5.5"))
    datasets_run.add_argument("--max-turns", type=int, default=_max_turns_default(config, 80))
    datasets_run.add_argument("--task-timeout-s", type=float, default=0.0, help="Optional per-task timeout in seconds.")
    add_browser_runtime_args(datasets_run, headless_default=True, config=config)
    datasets_run.add_argument(
        "--skip-failed",
        action="store_true",
        help="With --resume, skip latest failed attempts so pending tasks can continue; rerun failed tasks separately.",
    )
    datasets_run.add_argument("--stop-on-failure", action="store_true")
    datasets_run.set_defaults(func=cmd_datasets_run)

    datasets_report = datasets_sub.add_parser("report", help="Summarize a dataset run manifest.")
    datasets_report.add_argument("run_id_or_path")
    datasets_report.set_defaults(func=cmd_datasets_report)

    tui = sub.add_parser("tui", help="Start the terminal UI.")
    tui.add_argument("--provider", choices=PROVIDER_CHOICES, default=_provider_default(config, "codex"))
    tui.add_argument("--model", default=_model_default(config, "gpt-5.5"))
    tui.add_argument("--max-turns", type=int, default=_max_turns_default(config, 80))
    add_browser_runtime_args(tui, headless_default=None, config=config)
    tui.set_defaults(func=cmd_tui)

    config_cmd = sub.add_parser("config", help="Inspect or create browser use terminal config.")
    config_sub = config_cmd.add_subparsers(dest="config_command", required=True)

    config_show = config_sub.add_parser("show", help="Print merged redacted config.")
    config_show.set_defaults(func=cmd_config_show)

    config_set = config_sub.add_parser("set", help="Persist one config value.")
    config_set.add_argument("key", help="Dotted config key, e.g. browser.cloud_api_key.")
    config_set.add_argument("value", help="Value to save.")
    config_set.set_defaults(func=cmd_config_set)

    config_init = config_sub.add_parser("init", help="Write an example JSON config.")
    config_init.add_argument("--path", default=str(Path.home() / ".browser-use-terminal" / "config.json"))
    config_init.add_argument("--force", action="store_true")
    config_init.set_defaults(func=cmd_config_init)

    auth = sub.add_parser("auth", help="Authentication commands.")
    auth_sub = auth.add_subparsers(dest="auth_command", required=True)

    auth_status = auth_sub.add_parser("status", help="Print redacted auth status.")
    auth_status.set_defaults(func=cmd_auth_status)

    auth_codex = auth_sub.add_parser("codex", help="Codex subscription auth commands.")
    auth_codex_sub = auth_codex.add_subparsers(dest="codex_auth_command", required=True)

    auth_codex_status = auth_codex_sub.add_parser("status", help="Print redacted Codex auth status.")
    auth_codex_status.set_defaults(func=cmd_auth_status)

    auth_codex_import = auth_codex_sub.add_parser("import", help="Copy current Codex CLI auth into this harness auth store.")
    auth_codex_import.add_argument("--codex-home", default=None, help="Override Codex CLI home directory.")
    auth_codex_import.set_defaults(func=cmd_auth_codex_import)

    auth_codex_refresh = auth_codex_sub.add_parser("refresh", help="Refresh harness-owned Codex auth.")
    auth_codex_refresh.set_defaults(func=cmd_auth_codex_refresh)

    auth_codex_logout = auth_codex_sub.add_parser("logout", help="Remove harness-owned Codex auth.")
    auth_codex_logout.set_defaults(func=cmd_auth_codex_logout)

    auth_codex_login = auth_codex_sub.add_parser("login", help="Login with OpenAI Codex device code auth.")
    auth_codex_login.set_defaults(func=cmd_auth_codex_login)

    auth_api_key = auth_sub.add_parser("api-key", help="Store API keys for API-key providers.")
    auth_api_key_sub = auth_api_key.add_subparsers(dest="api_key_command", required=True)

    auth_api_key_set = auth_api_key_sub.add_parser("set", help="Store an API key for a provider.")
    auth_api_key_set.add_argument("provider", choices=API_KEY_PROVIDER_CHOICES)
    auth_api_key_set.add_argument("--key", default=None, help="API key. If omitted, prompts securely.")
    auth_api_key_set.set_defaults(func=cmd_auth_api_key_set)

    auth_api_key_remove = auth_api_key_sub.add_parser("remove", help="Remove a stored API key/OAuth credential.")
    auth_api_key_remove.add_argument("provider", choices=API_KEY_PROVIDER_CHOICES)
    auth_api_key_remove.set_defaults(func=cmd_auth_api_key_remove)

    auth_anthropic = auth_sub.add_parser("anthropic", help="Anthropic API key and Claude Code login commands.")
    auth_anthropic_sub = auth_anthropic.add_subparsers(dest="anthropic_auth_command", required=True)

    auth_anthropic_status = auth_anthropic_sub.add_parser("status", help="Print Anthropic auth status.")
    auth_anthropic_status.set_defaults(func=cmd_auth_anthropic_status)

    auth_anthropic_login = auth_anthropic_sub.add_parser("login", help="Login with Anthropic Claude Code OAuth.")
    auth_anthropic_login.add_argument("--code", default=None, help="Authorization code or final redirect URL.")
    auth_anthropic_login.add_argument("--no-browser", action="store_true", help="Print the login URL without opening a browser.")
    auth_anthropic_login.set_defaults(func=cmd_auth_anthropic_login)

    auth_anthropic_refresh = auth_anthropic_sub.add_parser("refresh", help="Refresh stored Anthropic Claude Code OAuth.")
    auth_anthropic_refresh.set_defaults(func=cmd_auth_anthropic_refresh)

    auth_anthropic_logout = auth_anthropic_sub.add_parser("logout", help="Remove stored Anthropic auth.")
    auth_anthropic_logout.set_defaults(func=cmd_auth_anthropic_logout)

    return parser


def store_from_args(args: argparse.Namespace) -> SessionStore:
    return SessionStore(Path(args.state_dir))


def cmd_doctor(args: argparse.Namespace) -> int:
    from llm_browser.browser import browser_runtime_diagnostics

    state_dir = Path(args.state_dir)
    print(
        json.dumps(
            {
                "ok": True,
                "state_dir": str(state_dir.resolve()),
                "config": redacted_config(getattr(args, "loaded_config", {})),
                "browser": browser_runtime_diagnostics(),
            },
            indent=2,
        )
    )
    return 0


def cmd_provider_image_smoke(args: argparse.Namespace) -> int:
    from llm_browser.provider.image_smoke import run_image_smoke

    provider = make_provider(args.provider, args.model, getattr(args, "loaded_config", {}))
    if provider is None:
        print("image smoke requires a configured non-fake provider", file=sys.stderr)
        return 2
    artifact_dir = Path(args.state_dir) / "provider-smoke" / f"{args.provider}-{int(time.time())}"
    result = run_image_smoke(provider, artifact_dir)
    print(json.dumps(result, indent=2))
    return 0 if result.get("ok") else 1


def cmd_run(args: argparse.Namespace) -> int:
    apply_browser_runtime_args(args)
    store = store_from_args(args)
    agent = Agent(
        store,
        provider_factory=lambda: make_provider(args.provider, args.model, getattr(args, "loaded_config", {})),
        max_turns=args.max_turns,
        mode=args.agent_mode,
    )
    session = agent.run(args.task, parent_id=args.parent_id)
    print(json.dumps(session.to_dict(), indent=2))
    return 0


def cmd_sessions_list(args: argparse.Namespace) -> int:
    store = store_from_args(args)
    rows = [session.to_dict() for session in store.list()]
    print(json.dumps(rows, indent=2))
    return 0


def cmd_sessions_show(args: argparse.Namespace) -> int:
    store = store_from_args(args)
    session = store.load(args.session_id)
    if session is None:
        print(f"session not found: {args.session_id}", file=sys.stderr)
        return 1

    events = store.events.read(args.session_id)
    payload = {
        "session": session.to_dict(),
        "events": [event.to_dict() for event in events[-args.events :]],
    }
    print(json.dumps(payload, indent=2))
    return 0


def cmd_sessions_cancel(args: argparse.Namespace) -> int:
    store = store_from_args(args)
    store.request_cancel(args.session_id, reason=args.reason)
    print(json.dumps({"ok": True, "session_id": args.session_id, "reason": args.reason}, indent=2))
    return 0


def cmd_sessions_resume(args: argparse.Namespace) -> int:
    store = store_from_args(args)
    agent = Agent(
        store,
        provider_factory=lambda: make_provider(args.provider, args.model, getattr(args, "loaded_config", {})),
        max_turns=args.max_turns,
        mode=args.agent_mode,
    )
    session = agent.resume_session(args.session_id, args.instruction)
    print(json.dumps(session.to_dict(), indent=2))
    return 0


def cmd_sessions_compact(args: argparse.Namespace) -> int:
    store = store_from_args(args)
    agent = Agent(
        store,
        provider_factory=lambda: make_provider(args.provider, args.model, getattr(args, "loaded_config", {})),
        mode=args.agent_mode,
    )
    session = agent.compact_session(args.session_id)
    print(json.dumps(session.to_dict(), indent=2))
    return 0


def cmd_sessions_trace(args: argparse.Namespace) -> int:
    store = store_from_args(args)
    path = write_trace_bundle(store, args.session_id)
    print(json.dumps({"ok": True, "path": str(path)}, indent=2))
    return 0


def cmd_sessions_self_eval(args: argparse.Namespace) -> int:
    store = store_from_args(args)
    prompt = build_self_eval_prompt(store, args.session_id)
    agent = Agent(
        store,
        provider_factory=lambda: make_provider(args.provider, args.model, getattr(args, "loaded_config", {})),
        max_turns=args.max_turns,
        mode=args.agent_mode,
    )
    session = agent.run(prompt, parent_id=args.session_id)
    print(json.dumps(session.to_dict(), indent=2))
    return 0


def cmd_browser_smoke(args: argparse.Namespace) -> int:
    from llm_browser.browser import BrowserRuntime

    apply_browser_runtime_args(args)
    root_dir = Path(args.state_dir) / "browser-smoke"
    runtime = BrowserRuntime.start(root_dir=root_dir, headless=args.headless)
    try:
        runtime.new_tab(args.url)
        runtime.wait_for_load()
        image = runtime.screenshot("loaded", attach=True)
        payload = {"connection": runtime.connection_info(), "page": runtime.page_info(), "screenshot": image.to_dict()}
        print(json.dumps(payload, indent=2))
    finally:
        runtime.close()
    return 0


def _daemon_name_from_args(args: argparse.Namespace) -> str:
    return (
        args.name
        or os.environ.get("LLM_BROWSER_DAEMON_NAME")
        or os.environ.get("BROWSER_USE_TERMINAL_DAEMON_NAME")
        or "default"
    )


def cmd_browser_daemon_status(args: argparse.Namespace) -> int:
    from llm_browser.browser.daemon_client import daemon_status

    print(json.dumps(daemon_status(_daemon_name_from_args(args)), indent=2))
    return 0


def cmd_browser_daemon_stop(args: argparse.Namespace) -> int:
    from llm_browser.browser.daemon_client import stop_daemon

    name = _daemon_name_from_args(args)
    stopped = stop_daemon(name)
    print(json.dumps({"ok": True, "name": name, "stopped": stopped}, indent=2))
    return 0


def cmd_datasets_list(args: argparse.Namespace) -> int:
    from llm_browser.datasets import DATASET_ALIASES

    payload = {}
    for alias, path in DATASET_ALIASES.items():
        resolved = (Path.cwd() / path).resolve()
        payload[alias] = {"path": str(resolved), "exists": resolved.exists()}
    print(json.dumps(payload, indent=2))
    return 0


def cmd_datasets_sample(args: argparse.Namespace) -> int:
    tasks = load_dataset(args.dataset)
    selected = select_tasks(tasks, count=args.count, seed=args.seed, task_ids=args.task_id)
    print(json.dumps([task.to_dict() for task in selected], indent=2))
    return 0


def cmd_datasets_run(args: argparse.Namespace) -> int:
    apply_browser_runtime_args(args)
    tasks = load_dataset(args.dataset)
    count = len(tasks) if args.all else args.count
    selected = select_tasks(tasks, count=count, seed=args.seed, task_ids=args.task_id)
    store = store_from_args(args)
    run_id = args.run_id or uuid.uuid4().hex[:12]
    path = manifest_path(store.state_dir, run_id)
    if args.resume and path.exists():
        manifest = json.loads(path.read_text(encoding="utf-8"))
        selected_by_id = {task.task_id: task for task in selected}
        selected = [selected_by_id[str(item["task_id"])] for item in manifest.get("selection", []) if str(item.get("task_id")) in selected_by_id]
    else:
        manifest = {
            "run_id": run_id,
            "dataset": args.dataset,
            "selection": [task.to_dict() for task in selected],
            "summary": dataset_summary(selected),
            "provider": args.provider,
            "model": args.model,
            "headless": args.headless,
            "browser": args.browser or os.environ.get("LLM_BROWSER_MODE") or "auto",
            "sessions": [],
        }

    _write_dataset_manifest(store, run_id, manifest)
    completed_task_ids = _resume_skip_task_ids(manifest, skip_failed=args.skip_failed) if args.resume else set()

    for task in selected:
        if task.task_id in completed_task_ids:
            continue
        prompt = build_dataset_prompt(task, headless=args.headless)
        if args.task_timeout_s and args.task_timeout_s > 0:
            prompt += (
                f"\n\nRuntime budget: this task has a hard timeout of {args.task_timeout_s:g} seconds. "
                "Do not spend the full budget on one unreliable path. If the primary site stalls, switch to raw HTTP, "
                "CDP, alternate endpoints, mirrors, search, or local scripts, then call done with the best supported answer."
            )
        workspace = store.state_dir / "dataset-runs" / run_id / f"task-{task.task_id}-workspace"
        workspace.mkdir(parents=True, exist_ok=True)
        result = _run_dataset_task_with_retries(
            store=store,
            task_id=task.task_id,
            prompt=prompt,
            workspace=workspace,
            provider_name=args.provider,
            model=args.model,
            max_turns=args.max_turns,
            timeout_s=args.task_timeout_s,
        )
        manifest["sessions"].append(result)
        _write_dataset_manifest(store, run_id, manifest)
        if result.get("fatal_runner_restart_required"):
            print(json.dumps(manifest, indent=2))
            sys.stdout.flush()
            sys.stderr.flush()
            os._exit(124)
        if args.stop_on_failure and not result.get("ok"):
            print(json.dumps(manifest, indent=2))
            return 1

    print(json.dumps(manifest, indent=2))
    return _dataset_manifest_exit_code(manifest)


def cmd_datasets_report(args: argparse.Namespace) -> int:
    store = store_from_args(args)
    manifest = load_manifest(store.state_dir, args.run_id_or_path)
    print(json.dumps(summarize_manifest(manifest), indent=2))
    return 0


def cmd_config_show(args: argparse.Namespace) -> int:
    config = getattr(args, "loaded_config", {})
    payload = {
        "path": str(getattr(args, "loaded_config_path", writable_config_path())),
        "config": redacted_config(config),
        "example": example_config() if not config else None,
    }
    print(json.dumps(payload, indent=2))
    return 0


def cmd_config_set(args: argparse.Namespace) -> int:
    path, config = write_config_value(args.key, _parse_config_value(args.value), path=getattr(args, "loaded_config_path", None))
    apply_config_environment(config, override=True)
    print(json.dumps({"ok": True, "path": str(path), "config": redacted_config(config)}, indent=2))
    return 0


def _parse_config_value(value: str) -> Any:
    try:
        return json.loads(value)
    except ValueError:
        return value


def cmd_config_init(args: argparse.Namespace) -> int:
    path = write_default_config(args.path, force=args.force)
    print(json.dumps({"ok": True, "path": str(path)}, indent=2))
    return 0


def cmd_tui(args: argparse.Namespace) -> int:
    from llm_browser.tui import TextualTui

    apply_browser_runtime_args(args)

    def provider_factory():
        return make_provider(args.provider, args.model, getattr(args, "loaded_config", {}))

    store = store_from_args(args)
    return TextualTui(
        store,
        provider_factory=provider_factory,
        max_turns=args.max_turns,
        provider_label=args.provider,
        model_label=args.model,
        config=getattr(args, "loaded_config", {}),
        config_path=getattr(args, "loaded_config_path", None),
    ).run()


def cmd_auth_status(args: argparse.Namespace) -> int:
    from llm_browser.auth import auth_status
    from llm_browser.auth.store import provider_auth_status

    payload = {
        "codex": auth_status(),
        "providers": provider_auth_status(),
    }
    print(json.dumps(payload, indent=2))
    return 0


def cmd_auth_api_key_set(args: argparse.Namespace) -> int:
    import getpass

    from llm_browser.auth.store import ProviderAuthStore

    key = args.key or getpass.getpass(f"{args.provider} API key: ")
    if not key:
        print("API key cannot be empty", file=sys.stderr)
        return 2
    ProviderAuthStore().set_api_key(args.provider, key)
    print(json.dumps({"ok": True, "provider": args.provider, "type": "api_key"}, indent=2))
    return 0


def cmd_auth_api_key_remove(args: argparse.Namespace) -> int:
    from llm_browser.auth.store import ProviderAuthStore

    removed = ProviderAuthStore().remove(args.provider)
    print(json.dumps({"ok": True, "provider": args.provider, "removed": removed}, indent=2))
    return 0


def cmd_auth_anthropic_status(args: argparse.Namespace) -> int:
    from llm_browser.auth.store import ProviderAuthStore

    print(json.dumps({"anthropic": ProviderAuthStore().status("anthropic")}, indent=2))
    return 0


def cmd_auth_anthropic_login(args: argparse.Namespace) -> int:
    from llm_browser.auth.anthropic import login_anthropic_oauth
    from llm_browser.auth.store import ProviderAuthStore

    credentials = login_anthropic_oauth(open_browser=not args.no_browser, code=args.code)
    ProviderAuthStore().set_oauth(
        "anthropic",
        access=str(credentials["access"]),
        refresh=str(credentials["refresh"]),
        expires=int(credentials["expires"]),
    )
    print(
        json.dumps(
            {
                "ok": True,
                "provider": "anthropic",
                "type": "oauth",
                "expires": credentials["expires"],
                "warning": (
                    "Anthropic Claude Code subscription auth is active. Usage may draw from Claude extra usage "
                    "and be billed per token."
                ),
            },
            indent=2,
        )
    )
    return 0


def cmd_auth_anthropic_refresh(args: argparse.Namespace) -> int:
    from llm_browser.auth.store import ProviderAuthStore

    credential = ProviderAuthStore().resolve("anthropic", refresh_oauth=True)
    if credential is None or credential.credential_type != "oauth":
        print("No stored Anthropic OAuth credential to refresh.", file=sys.stderr)
        return 1
    print(json.dumps({"ok": True, "provider": "anthropic", "source": credential.source, "type": "oauth"}, indent=2))
    return 0


def cmd_auth_anthropic_logout(args: argparse.Namespace) -> int:
    from llm_browser.auth.store import ProviderAuthStore

    removed = ProviderAuthStore().remove("anthropic")
    print(json.dumps({"ok": True, "provider": "anthropic", "removed": removed}, indent=2))
    return 0


def cmd_auth_codex_import(args: argparse.Namespace) -> int:
    from llm_browser.auth import import_codex_cli_auth

    codex_home = Path(args.codex_home).expanduser() if args.codex_home else None
    auth = import_codex_cli_auth(codex_home=codex_home)
    print(json.dumps({"ok": True, "codex": auth.redacted_summary()}, indent=2))
    return 0


def cmd_auth_codex_refresh(args: argparse.Namespace) -> int:
    from llm_browser.auth import refresh_codex_auth

    auth = refresh_codex_auth()
    print(json.dumps({"ok": True, "codex": auth.redacted_summary()}, indent=2))
    return 0


def cmd_auth_codex_logout(args: argparse.Namespace) -> int:
    from llm_browser.auth import logout_codex_auth

    removed = logout_codex_auth()
    print(json.dumps({"ok": True, "removed": removed}, indent=2))
    return 0


def cmd_auth_codex_login(args: argparse.Namespace) -> int:
    from llm_browser.auth import complete_device_code_login, request_device_code

    device = request_device_code()
    print(
        "Open this URL and enter the code:\n"
        f"{device.verification_url}\n\n"
        f"Code: {device.user_code}\n\n"
        "Waiting for authorization..."
    )
    auth = complete_device_code_login(device)
    print(json.dumps({"ok": True, "codex": auth.redacted_summary()}, indent=2))
    return 0


def make_provider(provider_name: str, model: Optional[str], config: Optional[Dict[str, Any]] = None) -> Optional[Provider]:
    from llm_browser.provider.factory import make_provider as make_provider_impl

    return make_provider_impl(provider_name, model, config=config)


def _write_dataset_manifest(store: SessionStore, run_id: str, manifest: dict) -> Path:
    path = manifest_path(store.state_dir, run_id)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return path


def _successful_task_ids(manifest: Dict[str, Any]) -> set[str]:
    summary = summarize_manifest(manifest)
    return set(summary["passed_task_ids"])


def _resume_skip_task_ids(manifest: Dict[str, Any], *, skip_failed: bool = False) -> set[str]:
    summary = summarize_manifest(manifest)
    skipped = set(summary["passed_task_ids"])
    if skip_failed:
        skipped.update(summary["failed_task_ids"])
    return skipped


def _dataset_manifest_exit_code(manifest: Dict[str, Any]) -> int:
    summary = summarize_manifest(manifest)
    return 0 if summary["failed"] == 0 and summary["pending"] == 0 else 1


def _run_dataset_task_with_retries(
    store: SessionStore,
    task_id: str,
    prompt: str,
    workspace: Path,
    provider_name: str,
    model: Optional[str],
    max_turns: int,
    timeout_s: float,
    max_attempts: int = 3,
) -> Dict[str, Any]:
    retry_history = []
    attempts = max(1, max_attempts)
    for attempt in range(1, attempts + 1):
        result = _run_dataset_task(
            store=store,
            task_id=task_id,
            prompt=prompt,
            workspace=workspace,
            provider_name=provider_name,
            model=model,
            max_turns=max_turns,
            timeout_s=timeout_s,
        )
        result["attempt_number"] = attempt
        if result.get("ok") or result.get("fatal_runner_restart_required") or not _is_transient_provider_failure(result):
            if retry_history:
                result["retry_history"] = retry_history
            return result
        retry_history.append(
            {
                "attempt": attempt,
                "session_id": (result.get("session") or {}).get("id"),
                "error_type": result.get("error_type"),
                "error": result.get("error"),
            }
        )
        if attempt < attempts:
            time.sleep(min(2 ** attempt, 10))
    if retry_history:
        result["retry_history"] = retry_history
    return result


def _is_transient_provider_failure(result: Dict[str, Any]) -> bool:
    text = f"{result.get('error_type', '')} {result.get('error', '')}".lower()
    transient_markers = (
        "service_unavailable_error",
        "server_is_overloaded",
        "servers are currently overloaded",
        "rate_limit_exceeded",
        "temporarily unavailable",
    )
    return "codex stream error" in text and any(marker in text for marker in transient_markers)


def _run_dataset_task(
    store: SessionStore,
    task_id: str,
    prompt: str,
    workspace: Path,
    provider_name: str,
    model: Optional[str],
    max_turns: int,
    timeout_s: float,
) -> Dict[str, Any]:
    session = store.create(cwd=workspace)
    result: Dict[str, Any] = {"task_id": task_id, "workspace": str(workspace), "session": session.to_dict()}
    error: Dict[str, str] = {}
    agent_ref: Dict[str, Agent] = {}

    def target() -> None:
        try:
            agent = Agent(
                store,
                provider_factory=lambda: make_provider(provider_name, model),
                max_turns=max_turns,
                time_budget_s=timeout_s if timeout_s and timeout_s > 0 else None,
            )
            agent_ref["agent"] = agent
            finished = agent.run_session(session.id, prompt)
            result["session"] = finished.to_dict()
            result["ok"] = finished.status == "done"
        except Exception as exc:
            loaded = store.load(session.id)
            if loaded is not None:
                result["session"] = loaded.to_dict()
            error["error"] = str(exc)
            error["error_type"] = type(exc).__name__

    if timeout_s and timeout_s > 0:
        thread = threading.Thread(target=target, name=f"dataset-task-{task_id}", daemon=True)
        thread.start()
        try:
            thread.join(timeout_s)
        except KeyboardInterrupt:
            store.request_cancel(session.id, reason="dataset runner interrupted")
            agent = agent_ref.get("agent")
            if agent is not None:
                try:
                    agent.tools.close_session(session.id)
                except Exception as exc:
                    result["cleanup_error"] = str(exc)
            loaded = store.load(session.id)
            if loaded is not None:
                result["session"] = loaded.to_dict()
            raise
        if thread.is_alive():
            store.request_cancel(session.id, reason=f"dataset task timeout after {timeout_s:g}s")
            agent = agent_ref.get("agent")
            if agent is not None:
                try:
                    agent.tools.close_session(session.id)
                except Exception as exc:
                    result["cleanup_error"] = str(exc)
            thread.join(DATASET_TIMEOUT_DRAIN_S)
            still_alive = thread.is_alive()
            loaded = store.load(session.id)
            if loaded is not None:
                if loaded.status == "running":
                    loaded = store.update_status(session.id, "cancelled")
                    store.emit(session.id, "session.cancelled", {"reason": f"dataset task timeout after {timeout_s:g}s"})
                result["session"] = loaded.to_dict()
            result.update({"ok": False, "error": f"dataset task timeout after {timeout_s:g}s", "error_type": "TimeoutError"})
            if still_alive:
                result["fatal_runner_restart_required"] = True
                result["error"] = (
                    f"dataset task timeout after {timeout_s:g}s; worker thread did not stop cleanly, "
                    "so the runner must restart before running another task"
                )
            return result
    else:
        target()

    if error:
        result.update({"ok": False, **error})
    _attach_dataset_final_result(store, result, session.id, workspace)
    _reject_placeholder_dataset_result(store, result, session.id)
    result.setdefault("ok", False)
    return result


def _attach_dataset_final_result(
    store: SessionStore,
    result: Dict[str, Any],
    session_id: str,
    workspace: Path,
) -> None:
    final_result = _session_done_result(store, session_id)
    if final_result is None:
        return
    result["final_result_chars"] = len(final_result)
    if len(final_result) <= MAX_INLINE_DATASET_RESULT:
        result["final_result"] = final_result
        return
    workspace.mkdir(parents=True, exist_ok=True)
    path = workspace / "final_result.txt"
    path.write_text(final_result, encoding="utf-8")
    result["final_result_preview"] = final_result[:MAX_INLINE_DATASET_RESULT]
    result["final_result_path"] = str(path)


def _reject_placeholder_dataset_result(store: SessionStore, result: Dict[str, Any], session_id: str) -> None:
    final_result = str(result.get("final_result") or result.get("final_result_preview") or "").strip()
    if result.get("ok") and not _session_called_done_tool(store, session_id):
        result["ok"] = False
        result["error"] = "dataset task ended without calling the done tool"
        result["error_type"] = "MissingDoneToolError"
    if final_result in {"[Response interrupted]", "[response interrupted]"}:
        result["ok"] = False
        result["error"] = "model returned placeholder interrupted response"
        result["error_type"] = "InterruptedResponseError"
    if final_result.startswith("[tool error:"):
        result["ok"] = False
        result["error"] = "model finished with a tool error instead of a task result"
        result["error_type"] = "ToolErrorFinalResult"


def _session_called_done_tool(store: SessionStore, session_id: str) -> bool:
    return any(event.type == "tool.started" and event.payload.get("name") == "done" for event in store.events.read(session_id))


def _session_done_result(store: SessionStore, session_id: str) -> Optional[str]:
    for event in reversed(store.events.read(session_id)):
        if event.type == "session.done":
            return str(event.payload.get("result") or "")
    return None


def main(argv: Optional[Sequence[str]] = None) -> int:
    pre = argparse.ArgumentParser(add_help=False)
    pre.add_argument("--config", default=None)
    pre_args, _ = pre.parse_known_args(argv)
    config = load_config(pre_args.config)
    apply_config_environment(config)
    parser = build_parser(config=config)
    args = parser.parse_args(argv)
    args.loaded_config = config
    args.loaded_config_path = writable_config_path(pre_args.config)
    return int(args.func(args))


def tui_main(argv: Optional[Sequence[str]] = None) -> int:
    if argv is None:
        argv = sys.argv[1:]
    global_args: list[str] = []
    tui_args: list[str] = []
    index = 0
    argv = list(argv)
    while index < len(argv):
        arg = argv[index]
        if arg in {"--config", "--state-dir"} and index + 1 < len(argv):
            global_args.extend([arg, argv[index + 1]])
            index += 2
            continue
        if arg.startswith("--config=") or arg.startswith("--state-dir="):
            global_args.append(arg)
            index += 1
            continue
        tui_args.append(arg)
        index += 1
    if tui_args and tui_args[0] in TOP_LEVEL_COMMANDS:
        return main([*global_args, *tui_args])
    return main([*global_args, "tui", *tui_args])


if __name__ == "__main__":
    raise SystemExit(main())
