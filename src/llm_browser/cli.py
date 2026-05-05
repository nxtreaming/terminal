from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Optional, Sequence

from llm_browser.agent import Agent
from llm_browser.session.store import SessionStore


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="llm-browser")
    parser.add_argument(
        "--state-dir",
        default=".llm-browser",
        help="Runtime state directory. Defaults to .llm-browser in the current directory.",
    )

    sub = parser.add_subparsers(dest="command", required=True)

    doctor = sub.add_parser("doctor", help="Print local harness diagnostics.")
    doctor.set_defaults(func=cmd_doctor)

    run = sub.add_parser("run", help="Create a session with a user task.")
    run.add_argument("task", help="Task to give the browser agent.")
    run.add_argument("--parent-id", default=None, help="Optional parent session id.")
    run.add_argument(
        "--provider",
        choices=["fake"],
        default="fake",
        help="Provider to use. Only fake is implemented so far.",
    )
    run.set_defaults(func=cmd_run)

    sessions = sub.add_parser("sessions", help="Inspect sessions.")
    sessions_sub = sessions.add_subparsers(dest="sessions_command", required=True)

    sessions_list = sessions_sub.add_parser("list", help="List known sessions.")
    sessions_list.set_defaults(func=cmd_sessions_list)

    sessions_show = sessions_sub.add_parser("show", help="Show a session and recent events.")
    sessions_show.add_argument("session_id")
    sessions_show.add_argument("--events", type=int, default=20, help="Number of events to print.")
    sessions_show.set_defaults(func=cmd_sessions_show)

    return parser


def store_from_args(args: argparse.Namespace) -> SessionStore:
    return SessionStore(Path(args.state_dir))


def cmd_doctor(args: argparse.Namespace) -> int:
    state_dir = Path(args.state_dir)
    print(json.dumps({"ok": True, "state_dir": str(state_dir.resolve())}, indent=2))
    return 0


def cmd_run(args: argparse.Namespace) -> int:
    store = store_from_args(args)
    agent = Agent(store)
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


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return int(args.func(args))


if __name__ == "__main__":
    raise SystemExit(main())
