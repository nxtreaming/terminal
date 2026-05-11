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
    assert Path(response["images"][0]["path"]).exists()


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
