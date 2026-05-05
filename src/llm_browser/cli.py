from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Optional, Sequence

from llm_browser.agent import Agent
from llm_browser.brand import CLI_NAME, DEFAULT_STATE_DIR
from llm_browser.session.store import SessionStore


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog=CLI_NAME)
    parser.add_argument(
        "--state-dir",
        default=DEFAULT_STATE_DIR,
        help=f"Runtime state directory. Defaults to {DEFAULT_STATE_DIR} in the current directory.",
    )

    sub = parser.add_subparsers(dest="command", required=True)

    doctor = sub.add_parser("doctor", help="Print local harness diagnostics.")
    doctor.set_defaults(func=cmd_doctor)

    run = sub.add_parser("run", help="Create a session with a user task.")
    run.add_argument("task", help="Task to give the browser agent.")
    run.add_argument("--parent-id", default=None, help="Optional parent session id.")
    run.add_argument(
        "--provider",
        choices=["fake", "openai", "codex"],
        default="fake",
        help="Provider to use.",
    )
    run.add_argument("--model", default=None, help="Model name for provider=openai.")
    run.add_argument("--max-turns", type=int, default=80, help="Maximum model/tool turns before failing.")
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

    browser = sub.add_parser("browser", help="Browser runtime commands.")
    browser_sub = browser.add_subparsers(dest="browser_command", required=True)

    browser_smoke = browser_sub.add_parser("smoke", help="Launch Chrome, navigate, and capture a screenshot.")
    browser_smoke.add_argument("--url", default="https://example.com", help="URL to open.")
    browser_smoke.add_argument("--headless", action="store_true", help="Run Chrome headless.")
    browser_smoke.set_defaults(func=cmd_browser_smoke)

    tui = sub.add_parser("tui", help="Start the simple terminal UI.")
    tui.add_argument("--provider", choices=["fake", "openai", "codex"], default="fake")
    tui.add_argument("--model", default=None)
    tui.add_argument("--max-turns", type=int, default=80)
    tui.set_defaults(func=cmd_tui)

    auth = sub.add_parser("auth", help="Authentication commands.")
    auth_sub = auth.add_subparsers(dest="auth_command", required=True)

    auth_status = auth_sub.add_parser("status", help="Print redacted auth status.")
    auth_status.set_defaults(func=cmd_auth_status)

    return parser


def store_from_args(args: argparse.Namespace) -> SessionStore:
    return SessionStore(Path(args.state_dir))


def cmd_doctor(args: argparse.Namespace) -> int:
    state_dir = Path(args.state_dir)
    print(json.dumps({"ok": True, "state_dir": str(state_dir.resolve())}, indent=2))
    return 0


def cmd_run(args: argparse.Namespace) -> int:
    store = store_from_args(args)
    provider = None
    if args.provider == "openai":
        from llm_browser.provider.openai_responses import OpenAIResponsesProvider

        provider = OpenAIResponsesProvider(model=args.model)
    elif args.provider == "codex":
        from llm_browser.provider.codex_responses import CodexResponsesProvider

        provider = CodexResponsesProvider(model=args.model)
    agent = Agent(store, provider=provider, max_turns=args.max_turns)
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


def cmd_browser_smoke(args: argparse.Namespace) -> int:
    from llm_browser.browser import BrowserRuntime

    root_dir = Path(args.state_dir) / "browser-smoke"
    runtime = BrowserRuntime.start(root_dir=root_dir, headless=args.headless)
    try:
        runtime.new_tab(args.url)
        runtime.wait_for_load()
        image = runtime.screenshot("loaded", attach=True)
        payload = {"page": runtime.page_info(), "screenshot": image.to_dict()}
        print(json.dumps(payload, indent=2))
    finally:
        runtime.close()
    return 0


def cmd_tui(args: argparse.Namespace) -> int:
    from llm_browser.tui import TextualTui

    def provider_factory():
        if args.provider == "openai":
            from llm_browser.provider.openai_responses import OpenAIResponsesProvider

            return OpenAIResponsesProvider(model=args.model)
        if args.provider == "codex":
            from llm_browser.provider.codex_responses import CodexResponsesProvider

            return CodexResponsesProvider(model=args.model)
        return None

    store = store_from_args(args)
    return TextualTui(store, provider_factory=provider_factory, max_turns=args.max_turns).run()


def cmd_auth_status(args: argparse.Namespace) -> int:
    import os

    from llm_browser.auth import load_codex_auth

    codex_auth = load_codex_auth()
    payload = {
        "codex": codex_auth.redacted_summary() if codex_auth else {"available": False},
        "openai_api_key": {
            "available": bool(os.environ.get("LLM_BROWSER_OPENAI_API_KEY") or os.environ.get("OPENAI_API_KEY"))
        },
    }
    print(json.dumps(payload, indent=2))
    return 0


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return int(args.func(args))


if __name__ == "__main__":
    raise SystemExit(main())
