from __future__ import annotations

import asyncio
import json
from pathlib import Path
from typing import Any, Dict, List, Optional

import pytest

from browser_use import Agent, AgentHistoryList, Browser, BrowserAlreadyInUseError, ChatBrowserUse
from browser_use.runtime import JsonRpcError, RuntimeClient


class FakeRuntime:
    def __init__(self, *, run_delay: float = 0.0, run_result: Optional[Dict[str, Any]] = None):
        self.calls: List[tuple[str, Dict[str, Any]]] = []
        self.run_delay = run_delay
        self.run_result = run_result or {"history": {"output": "done", "success": True}}
        self.next_browser = 1
        self.next_agent = 1
        self.queues: Dict[str, asyncio.Queue[Dict[str, Any]]] = {}

    async def call(self, method: str, params: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
        payload = dict(params or {})
        self.calls.append((method, payload))
        if method == "browser.create":
            browser_id = f"browser-{self.next_browser}"
            self.next_browser += 1
            return {"browser_id": browser_id}
        if method == "agent.create":
            agent_id = f"agent-{self.next_agent}"
            self.next_agent += 1
            return {"agent_id": agent_id, "session_id": f"session-{self.next_agent}"}
        if method == "agent.run":
            if self.run_delay:
                queue = self.event_queue("session-2")
                queue.put_nowait({"kind": "agent_started"})
                await asyncio.sleep(self.run_delay)
            return self.run_result
        return {}

    def event_queue(self, run_id: str) -> asyncio.Queue[Dict[str, Any]]:
        return self.queues.setdefault(run_id, asyncio.Queue())


def test_llm_protocol_preserves_model_options() -> None:
    llm = ChatBrowserUse(model="bu-2-0", base_url="https://example.invalid", temperature=0)

    assert llm.to_protocol() == {
        "provider": "browser-use",
        "model": "bu-2-0",
        "base_url": "https://example.invalid",
        "timeout": 120.0,
        "options": {"temperature": 0},
    }


def test_browser_start_creates_rust_browser() -> None:
    asyncio.run(_test_browser_start_creates_rust_browser())


async def _test_browser_start_creates_rust_browser() -> None:
    runtime = FakeRuntime()
    browser = Browser(headless=True, keep_alive=True, _runtime=runtime)  # type: ignore[arg-type]

    await browser.start()

    assert browser.browser_id == "browser-1"
    assert runtime.calls == [
        ("browser.create", {"headless": True, "keep_alive": True}),
    ]


def test_browser_stop_clears_runtime_browser_id() -> None:
    asyncio.run(_test_browser_stop_clears_runtime_browser_id())


async def _test_browser_stop_clears_runtime_browser_id() -> None:
    runtime = FakeRuntime()
    browser = Browser(_runtime=runtime)  # type: ignore[arg-type]

    await browser.start()
    await browser.stop()

    assert browser.browser_id is None
    assert [call[0] for call in runtime.calls] == ["browser.create", "browser.stop"]


def test_agent_run_uses_browser_and_returns_history() -> None:
    asyncio.run(_test_agent_run_uses_browser_and_returns_history())


async def _test_agent_run_uses_browser_and_returns_history() -> None:
    runtime = FakeRuntime(run_result={"history": {"output": "Paris", "success": True}})
    browser = Browser(_runtime=runtime)  # type: ignore[arg-type]
    agent = Agent("capital?", llm=ChatBrowserUse(model="bu-2-0"), browser=browser)

    history = await agent.run(max_steps=3)

    assert history.final_result() == "Paris"
    assert history.output == "Paris"
    assert history.is_done()
    assert history.is_successful()
    assert [call[0] for call in runtime.calls] == [
        "browser.create",
        "agent.create",
        "agent.run",
    ]
    assert runtime.calls[-1][1]["browser_id"] == "browser-1"
    assert runtime.calls[-1][1]["max_steps"] == 3


def test_agent_stream_yields_runtime_events_and_final_history() -> None:
    asyncio.run(_test_agent_stream_yields_runtime_events_and_final_history())


async def _test_agent_stream_yields_runtime_events_and_final_history() -> None:
    runtime = FakeRuntime(
        run_delay=0.01,
        run_result={"history": {"output": "streamed", "success": True}},
    )
    browser = Browser(_runtime=runtime)  # type: ignore[arg-type]
    agent = Agent("stream?", llm=ChatBrowserUse(model="bu-2-0"), browser=browser)

    events = [event async for event in agent.stream(max_steps=4)]

    assert events[0]["kind"] == "agent_started"
    assert events[-1]["type"] == "agent.completed"
    assert events[-1]["output"] == "streamed"
    assert events[-1]["history"].final_result() == "streamed"
    assert runtime.calls[-1][0] == "agent.run"
    assert runtime.calls[-1][1]["max_steps"] == 4


def test_two_running_agents_cannot_share_one_browser() -> None:
    asyncio.run(_test_two_running_agents_cannot_share_one_browser())


async def _test_two_running_agents_cannot_share_one_browser() -> None:
    runtime = FakeRuntime(run_delay=0.05)
    browser = Browser(_runtime=runtime)  # type: ignore[arg-type]
    first = Agent("first", browser=browser)
    second = Agent("second", browser=browser)

    task = asyncio.create_task(first.run())
    await asyncio.sleep(0)

    with pytest.raises(BrowserAlreadyInUseError):
        await second.run()

    await task


def test_agent_run_fails_loudly_when_server_does_not_support_run() -> None:
    asyncio.run(_test_agent_run_fails_loudly_when_server_does_not_support_run())


async def _test_agent_run_fails_loudly_when_server_does_not_support_run() -> None:
    class CreateOnlyRuntime(FakeRuntime):
        async def call(self, method: str, params: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
            if method == "agent.run":
                raise JsonRpcError(-32601, "Method not found")
            return await super().call(method, params)

    agent = Agent("task", browser=Browser(_runtime=CreateOnlyRuntime()))  # type: ignore[arg-type]

    with pytest.raises(NotImplementedError, match="agent.run is not supported"):
        await agent.run()


def test_history_validates_structured_output_with_pydantic_v2_style() -> None:
    class Product:
        @classmethod
        def model_validate_json(cls, text: str) -> Dict[str, Any]:
            return json.loads(text)

    history = AgentHistoryList(output='{"name":"book"}', output_model_schema=Product)

    assert history.structured_output == {"name": "book"}


def test_runtime_client_round_trips_against_sdk_server_binary(tmp_path: Path) -> None:
    asyncio.run(_test_runtime_client_round_trips_against_sdk_server_binary(tmp_path))


async def _test_runtime_client_round_trips_against_sdk_server_binary(tmp_path: Path) -> None:
    binary = Path("target/debug/browser-use-terminal")
    if not binary.exists():
        pytest.skip("Rust SDK server binary is not built")

    runtime = RuntimeClient(command=[str(binary), "--state-dir", str(tmp_path), "sdk-server", "--transport", "stdio"])
    try:
        assert await runtime.call("runtime.ping") == {"ok": True}
        browser = await runtime.call("browser.create", {"headless": True})
        assert browser["browser_id"]
        agent = await runtime.call("agent.create", {"task": "inspect", "cwd": str(tmp_path)})
        assert agent["agent_id"]
        assert agent["session_id"]

        sdk_browser = Browser(_runtime=runtime)  # type: ignore[arg-type]
        sdk_agent = Agent(
            "inspect",
            llm=ChatBrowserUse(model="fake"),
            browser=sdk_browser,
        )
        history = await sdk_agent.run(max_steps=2)
        assert history.final_result() == "Fake result for: inspect"
        assert sdk_agent.session_id is not None
        queue = runtime.event_queue(sdk_agent.session_id)
        for _ in range(20):
            if not queue.empty():
                break
            await asyncio.sleep(0.01)
        observed = []
        while not queue.empty():
            observed.append(queue.get_nowait())
        assert observed
        assert any(event.get("kind") in {"agent_created", "store_event_appended"} for event in observed)
    finally:
        await runtime.close()
