from pathlib import Path

from llm_browser_worker import worker


def test_worker_run_executes_in_persistent_session_namespace(tmp_path: Path) -> None:
    first = worker._run(
        {
            "id": "one",
            "session_id": "task-1",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "counter = globals().get('counter', 0) + 1\nresult = counter\nemit_output(f'counter={counter}')",
        }
    )
    second = worker._run(
        {
            "id": "two",
            "session_id": "task-1",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "counter = globals().get('counter', 0) + 1\nresult = counter",
        }
    )

    assert first["ok"] is True
    assert first["data"] == 1
    assert first["outputs"] == [{"text": "counter=1"}]
    assert second["ok"] is True
    assert second["data"] == 2


def test_worker_records_artifacts_and_images(tmp_path: Path) -> None:
    source = tmp_path / "source.png"
    source.write_bytes(b"png")

    response = worker._run(
        {
            "id": "image",
            "session_id": "task-2",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": f"emit_image({str(source)!r}, label='shot', mime_type='image/png')",
        }
    )

    assert response["ok"] is True
    assert response["images"][0]["label"] == "shot"
    assert response["images"][0]["mime_type"] == "image/png"
    assert Path(response["images"][0]["path"]).exists()


def test_worker_records_browser_state_details(tmp_path: Path) -> None:
    response = worker._run(
        {
            "id": "browser-state",
            "session_id": "task-3",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "emit_browser_state(url='https://example.com', title='Example', status='connected', tabs=2, viewport={'w': 1440, 'h': 900})",
        }
    )

    assert response["ok"] is True
    assert response["browser_events"] == [
        {
            "type": "browser.state",
            "payload": {
                "url": "https://example.com",
                "title": "Example",
                "status": "connected",
                "tabs": 2,
                "viewport": {"w": 1440, "h": 900},
            },
        }
    ]


def test_worker_captures_browser_harness_startup_stdout(
    tmp_path: Path, monkeypatch
) -> None:
    def fake_load_browser_harness(ns):
        print("cloud startup chatter")
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None

    monkeypatch.setattr(worker, "_load_browser_harness", fake_load_browser_harness)

    response = worker._run(
        {
            "id": "startup-chatter",
            "session_id": "task-startup-chatter",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "result = {'ok': True}",
        }
    )

    assert response["ok"] is True
    assert "cloud startup chatter" in response["text"]
    assert response["data"] == {"ok": True}


def test_worker_capture_screenshot_attaches_image_by_default(
    tmp_path: Path, monkeypatch
) -> None:
    def fake_load_browser_harness(ns):
        def fake_capture_screenshot(path=None, full=False, max_dim=None):
            target = Path(path or "shot.png").expanduser()
            if not target.is_absolute():
                target = tmp_path / target
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(b"png")
            return str(target)

        ns["capture_screenshot"] = fake_capture_screenshot
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None

    monkeypatch.setattr(worker, "_load_browser_harness", fake_load_browser_harness)

    response = worker._run(
        {
            "id": "attached-screenshot",
            "session_id": "task-attached-screenshot",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "path = capture_screenshot('after-click.png')\nresult = {'path': path}",
        }
    )

    assert response["ok"] is True
    assert response["data"]["path"].endswith("after-click.png")
    assert response["images"][0]["label"] == "after-click"
    assert Path(response["images"][0]["path"]).exists()


def test_worker_screenshot_shorthand_emits_labeled_image(
    tmp_path: Path, monkeypatch
) -> None:
    def fake_load_browser_harness(ns):
        def fake_capture_screenshot(path=None, full=False, max_dim=None):
            target = tmp_path / "shot.png"
            target.write_bytes(b"png")
            return str(target)

        ns["capture_screenshot"] = fake_capture_screenshot
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None

    monkeypatch.setattr(worker, "_load_browser_harness", fake_load_browser_harness)

    response = worker._run(
        {
            "id": "screenshot-shorthand",
            "session_id": "task-screenshot-shorthand",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "image = screenshot('verified-state')\nresult = image",
        }
    )

    assert response["ok"] is True
    assert response["data"]["label"] == "verified-state"
    assert response["images"][0]["label"] == "verified-state"
    assert Path(response["images"][0]["path"]).exists()


def test_worker_screenshot_clip_uses_cdp_clip_and_attaches_image(
    tmp_path: Path, monkeypatch
) -> None:
    calls = []

    def fake_load_browser_harness(ns):
        def fake_cdp(method, **kwargs):
            calls.append((method, kwargs))
            return {"data": "cG5n"}

        ns["cdp"] = fake_cdp
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None

    monkeypatch.setattr(worker, "_load_browser_harness", fake_load_browser_harness)

    response = worker._run(
        {
            "id": "screenshot-clip",
            "session_id": "task-screenshot-clip",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "image = screenshot_clip('table', 10, 20, 300, 120)\nresult = image",
        }
    )

    assert response["ok"] is True
    assert calls[0][0] == "Page.captureScreenshot"
    assert calls[0][1]["clip"] == {
        "x": 10.0,
        "y": 20.0,
        "width": 300.0,
        "height": 120.0,
        "scale": 1.0,
    }
    assert response["images"][0]["label"] == "table"
    assert len(response["images"]) == 1
    assert Path(response["images"][0]["path"]).exists()


def test_worker_raw_cdp_capture_screenshot_attaches_image(
    tmp_path: Path, monkeypatch
) -> None:
    def fake_load_browser_harness(ns):
        def fake_cdp(method, session_id=None, **kwargs):
            assert session_id is None
            if method == "Page.captureScreenshot":
                return {"data": "cG5n"}
            return {}

        ns["cdp"] = fake_cdp
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None

    monkeypatch.setattr(worker, "_load_browser_harness", fake_load_browser_harness)

    response = worker._run(
        {
            "id": "raw-cdp-screenshot",
            "session_id": "task-raw-cdp-screenshot",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "result = cdp('Page.captureScreenshot', format='png')",
        }
    )

    assert response["ok"] is True
    assert response["data"] == {"data": "cG5n"}
    assert len(response["images"]) == 1
    assert response["images"][0]["label"] == "cdp_screenshot_1"
    assert Path(response["images"][0]["path"]).exists()


def test_worker_page_info_fallback_reads_target_url_and_title(
    tmp_path: Path, monkeypatch
) -> None:
    class FakeHelpers:
        def current_tab(self):
            return {"targetId": "target-2"}

        def cdp(self, method, **kwargs):
            if method == "Target.getTargets":
                return {
                    "targetInfos": [
                        {
                            "targetId": "target-1",
                            "type": "page",
                            "attached": True,
                            "url": "https://old.example/",
                            "title": "Old",
                        },
                        {
                            "targetId": "target-2",
                            "type": "page",
                            "attached": True,
                            "url": "https://example.com/",
                            "title": "Example",
                        },
                    ]
                }
            if method == "Page.getLayoutMetrics":
                return {"cssVisualViewport": {"clientWidth": 800, "clientHeight": 600}}
            raise AssertionError(method)

    def fake_load_browser_harness(ns):
        ns["page_info"] = lambda: (_ for _ in ()).throw(RuntimeError("page JS wedged"))
        ns["__browser_harness_helpers__"] = FakeHelpers()
        ns["browser_harness_available"] = True
        ns["browser_harness_error"] = None

    monkeypatch.setattr(worker, "_load_browser_harness", fake_load_browser_harness)

    response = worker._run(
        {
            "id": "page-info-fallback",
            "session_id": "task-page-info-fallback",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "result = page_info()",
        }
    )

    assert response["ok"] is True
    assert response["data"]["url"] == "https://example.com/"
    assert response["data"]["title"] == "Example"
    assert response["data"]["w"] == 800
    assert response["data"]["h"] == 600
    assert response["data"]["fallback"] == "cdp"


def test_worker_autoloads_agent_workspace_helpers(tmp_path: Path) -> None:
    workspace = tmp_path / ".browser-use" / "agent-workspace"
    workspace.mkdir(parents=True)
    (workspace / "agent_helpers.py").write_text(
        "def helper_value():\n    return 42\n",
        encoding="utf-8",
    )

    response = worker._run(
        {
            "id": "agent-helpers",
            "session_id": "task-agent-helpers",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "result = {'workspace': agent_workspace(create=False), 'value': helper_value()}",
        }
    )

    assert response["ok"] is True
    assert response["data"]["workspace"] == str(workspace)
    assert response["data"]["value"] == 42


def test_worker_error_hints_are_appended(tmp_path: Path) -> None:
    cases = [
        (
            "raise RuntimeError(\"':contains' is not a valid CSS selector\")",
            "':contains' is jQuery, not CSS.",
        ),
        (
            "raise RuntimeError(\"Identifier 'buttons' has already been declared\")",
            "execution contexts persist",
        ),
        (
            "raise RuntimeError('Blocked a frame with origin https://a from accessing a cross-origin frame')",
            "Cross-origin iframe DOM access",
        ),
        (
            "raise RuntimeError('-32602 No target with given id found')",
            "target closed or was replaced",
        ),
        (
            "raise RuntimeError(\"Runtime.getExecutionContexts wasn't found\")",
            "Runtime.getExecutionContexts is not a CDP method",
        ),
    ]

    for idx, (code, expected_hint) in enumerate(cases):
        response = worker._run(
            {
                "id": f"hint-{idx}",
                "session_id": f"task-hint-{idx}",
                "cwd": str(tmp_path),
                "artifact_dir": str(tmp_path / "artifacts"),
                "code": code,
            }
        )
        assert response["ok"] is False
        assert "Hint:" in response["error"]
        assert expected_hint in response["error"]


def test_worker_set_final_answer_persists_metadata_and_compact_result(tmp_path: Path) -> None:
    response = worker._run(
        {
            "id": "final-answer",
            "session_id": "task-final-answer",
            "cwd": str(tmp_path),
            "artifact_dir": str(tmp_path / "artifacts"),
            "code": "summary = set_final_answer({'stores': [{'name': 'A', 'address': 'B'}]}, artifact_name='stores.json')\nresult = summary",
        }
    )

    assert response["ok"] is True
    assert response["data"]["count"] == 1
    assert response["outputs"][0]["text"].startswith("final answer ready:")
    assert Path(response["data"]["artifact"]["path"]).exists()
    metadata = tmp_path / "artifacts" / ".final_answer.json"
    assert metadata.exists()
    assert '"stores"' in metadata.read_text()


def test_managed_browser_does_not_use_system_chromium_without_opt_in(
    tmp_path: Path, monkeypatch
) -> None:
    system_chromium = tmp_path / "chromium"
    system_chromium.write_text("#!/bin/sh\n")
    monkeypatch.delenv("CHROME_PATH", raising=False)
    monkeypatch.delenv("LLM_BROWSER_ALLOW_SYSTEM_CHROMIUM", raising=False)
    monkeypatch.delenv("LLM_BROWSER_ALLOW_GOOGLE_CHROME", raising=False)
    monkeypatch.setattr(worker, "_playwright_chromium_candidates", lambda: [])
    monkeypatch.setattr(worker.shutil, "which", lambda name: str(system_chromium))

    try:
        worker._pick_chromium_path()
    except RuntimeError as exc:
        assert "Playwright Chromium not found" in str(exc)
    else:
        raise AssertionError("system Chromium should require explicit opt-in")

    monkeypatch.setenv("LLM_BROWSER_ALLOW_SYSTEM_CHROMIUM", "1")
    assert worker._pick_chromium_path()


def test_visible_managed_browser_prefers_google_chrome(monkeypatch) -> None:
    monkeypatch.delenv("CHROME_PATH", raising=False)

    class FakePath:
        def __init__(self, value: str) -> None:
            self.value = value

        def exists(self) -> bool:
            return self.value == "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

        def __str__(self) -> str:
            return self.value

    monkeypatch.setattr(worker, "Path", FakePath)

    assert (
        worker._pick_managed_chrome_path(visible=True)
        == "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    )


def test_managed_chrome_args_visible_vs_headless(tmp_path: Path) -> None:
    visible = worker._managed_chrome_args("/chrome", 9333, tmp_path / "profile", True)
    headless = worker._managed_chrome_args("/chrome", 9334, tmp_path / "profile", False)

    assert "--new-window" in visible
    assert "--window-size=1512,900" in visible
    assert "--headless=new" not in visible
    assert "--headless=new" in headless
    assert "--new-window" not in headless
