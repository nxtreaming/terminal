from __future__ import annotations

import argparse
import json
import os
import sys
import threading
import uuid
from pathlib import Path
from typing import Any, Dict, Optional, Sequence

from llm_browser.agent import Agent
from llm_browser.brand import CLI_NAME, DEFAULT_STATE_DIR
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
from llm_browser.session.trace import build_self_eval_prompt, write_trace_bundle
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

    sessions_resume = sessions_sub.add_parser("resume", help="Resume a session from its event trace.")
    sessions_resume.add_argument("session_id")
    sessions_resume.add_argument("instruction", nargs="?", default="Continue from the previous session state.")
    sessions_resume.add_argument("--provider", choices=["fake", "openai", "codex"], default="codex")
    sessions_resume.add_argument("--model", default="gpt-5.5")
    sessions_resume.add_argument("--max-turns", type=int, default=80)
    sessions_resume.set_defaults(func=cmd_sessions_resume)

    sessions_trace = sessions_sub.add_parser("trace", help="Write a JSON trace bundle for a session.")
    sessions_trace.add_argument("session_id")
    sessions_trace.set_defaults(func=cmd_sessions_trace)

    sessions_eval = sessions_sub.add_parser("self-eval", help="Run an LLM evaluator as a child session over a trace.")
    sessions_eval.add_argument("session_id")
    sessions_eval.add_argument("--provider", choices=["fake", "openai", "codex"], default="codex")
    sessions_eval.add_argument("--model", default="gpt-5.5")
    sessions_eval.add_argument("--max-turns", type=int, default=20)
    sessions_eval.set_defaults(func=cmd_sessions_self_eval)

    browser = sub.add_parser("browser", help="Browser runtime commands.")
    browser_sub = browser.add_subparsers(dest="browser_command", required=True)

    browser_smoke = browser_sub.add_parser("smoke", help="Launch Chrome, navigate, and capture a screenshot.")
    browser_smoke.add_argument("--url", default="https://example.com", help="URL to open.")
    browser_smoke.add_argument("--headless", action="store_true", help="Run Chrome headless.")
    browser_smoke.set_defaults(func=cmd_browser_smoke)

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
    datasets_run.add_argument("--provider", choices=["fake", "openai", "codex"], default="codex")
    datasets_run.add_argument("--model", default="gpt-5.5")
    datasets_run.add_argument("--max-turns", type=int, default=80)
    datasets_run.add_argument("--task-timeout-s", type=float, default=0.0, help="Optional per-task timeout in seconds.")
    datasets_run.add_argument("--headless", action=argparse.BooleanOptionalAction, default=True)
    datasets_run.add_argument("--stop-on-failure", action="store_true")
    datasets_run.set_defaults(func=cmd_datasets_run)

    datasets_report = datasets_sub.add_parser("report", help="Summarize a dataset run manifest.")
    datasets_report.add_argument("run_id_or_path")
    datasets_report.set_defaults(func=cmd_datasets_report)

    tui = sub.add_parser("tui", help="Start the terminal UI.")
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
    provider = make_provider(args.provider, args.model)
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


def cmd_sessions_resume(args: argparse.Namespace) -> int:
    store = store_from_args(args)
    agent = Agent(store, provider=make_provider(args.provider, args.model), max_turns=args.max_turns)
    session = agent.resume_session(args.session_id, args.instruction)
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
    agent = Agent(store, provider=make_provider(args.provider, args.model), max_turns=args.max_turns)
    session = agent.run(prompt, parent_id=args.session_id)
    print(json.dumps(session.to_dict(), indent=2))
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
    if args.headless:
        os.environ["LLM_BROWSER_HEADLESS"] = "1"
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
            "sessions": [],
        }

    completed_task_ids = _successful_task_ids(manifest) if args.resume else set()

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
        result = _run_dataset_task(
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


def cmd_tui(args: argparse.Namespace) -> int:
    from llm_browser.tui import TextualTui

    def provider_factory():
        return make_provider(args.provider, args.model)

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


def make_provider(provider_name: str, model: Optional[str]) -> Optional[Provider]:
    if provider_name == "openai":
        from llm_browser.provider.openai_responses import OpenAIResponsesProvider

        return OpenAIResponsesProvider(model=model)
    if provider_name == "codex":
        from llm_browser.provider.codex_responses import CodexResponsesProvider

        return CodexResponsesProvider(model=model)
    return None


def _write_dataset_manifest(store: SessionStore, run_id: str, manifest: dict) -> Path:
    path = manifest_path(store.state_dir, run_id)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return path


def _successful_task_ids(manifest: Dict[str, Any]) -> set[str]:
    summary = summarize_manifest(manifest)
    return set(summary["passed_task_ids"])


def _dataset_manifest_exit_code(manifest: Dict[str, Any]) -> int:
    summary = summarize_manifest(manifest)
    return 0 if summary["failed"] == 0 and summary["pending"] == 0 else 1


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

    def target() -> None:
        try:
            agent = Agent(
                store,
                provider=make_provider(provider_name, model),
                max_turns=max_turns,
                time_budget_s=timeout_s if timeout_s and timeout_s > 0 else None,
            )
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
        thread.join(timeout_s)
        if thread.is_alive():
            store.request_cancel(session.id, reason=f"dataset task timeout after {timeout_s:g}s")
            thread.join(10)
            loaded = store.load(session.id)
            if loaded is not None:
                result["session"] = loaded.to_dict()
            result.update({"ok": False, "error": f"dataset task timeout after {timeout_s:g}s", "error_type": "TimeoutError"})
            return result
    else:
        target()

    if error:
        result.update({"ok": False, **error})
    result.setdefault("ok", False)
    return result


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return int(args.func(args))


if __name__ == "__main__":
    raise SystemExit(main())
