from __future__ import annotations

import tempfile
import threading
import time
import unittest
from pathlib import Path

from llm_browser.agent import Agent
from llm_browser.agent.service import MaxTurnsExceeded
from llm_browser.provider.fake import FakeProvider
from llm_browser.provider.types import ModelEvent, ToolCall
from llm_browser.session.store import SessionStore
from llm_browser.tool.context import ToolContext
from llm_browser.tool.registry import ToolRegistry
from llm_browser.tool.result import ToolImage, ToolResult
from llm_browser.tool.spec import ToolSpec


class NeverDoneProvider:
    def start_turn(self, messages, tools):
        yield ModelEvent.call(ToolCall(id=f"call_{len(messages)}", name="echo", arguments={"text": "again"}))


class BadToolThenDoneProvider:
    def __init__(self):
        self.turn = 0

    def start_turn(self, messages, tools):
        self.turn += 1
        if self.turn == 1:
            yield ModelEvent.call(ToolCall(id="call_bad", name="missing_tool", arguments={}))
        else:
            tool_message = [message for message in messages if message.get("role") == "tool"][-1]
            yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"result": tool_message["content"]}))


class ManyToolCallsProvider:
    def __init__(self):
        self.turn = 0

    def start_turn(self, messages, tools):
        self.turn += 1
        if self.turn < 16:
            yield ModelEvent.call(ToolCall(id=f"call_{self.turn}", name="echo", arguments={"text": "x" * 80}))
        else:
            yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"result": "ok"}))


class TextOnlyProvider:
    def start_turn(self, messages, tools):
        yield ModelEvent.text("direct final")


class SpawnChildProvider:
    def __init__(self):
        self.turn = 0

    def start_turn(self, messages, tools):
        self.turn += 1
        if self.turn == 1:
            yield ModelEvent.call(
                ToolCall(
                    id="call_session",
                    name="session",
                    arguments={"action": "create", "prompt": "child task"},
                )
            )
        else:
            tool_message = [message for message in messages if message.get("role") == "tool"][-1]
            yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"result": str(tool_message["content"])}))


class AgentLoopTest(unittest.TestCase):
    def test_fake_provider_executes_tools_and_finishes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            agent = Agent(store)

            session = agent.run("Open example.com", cwd=Path(tmp))

            loaded = store.load(session.id)
            self.assertIsNotNone(loaded)
            self.assertEqual(loaded.status, "done")
            self.assertIsNone(store.runner_info(session.id))

            events = store.events.read(session.id)
            event_types = [event.type for event in events]
            self.assertIn("session.created", event_types)
            self.assertIn("session.input", event_types)
            self.assertIn("model.delta", event_types)
            self.assertEqual(event_types.count("tool.started"), 2)
            self.assertEqual(event_types.count("tool.finished"), 2)
            self.assertIn("session.done", event_types)

            tool_names = [
                event.payload["name"]
                for event in events
                if event.type == "tool.started"
            ]
            self.assertEqual(tool_names, ["echo", "done"])

    def test_max_turns_exhaustion_fails_session(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            agent = Agent(store, provider=NeverDoneProvider(), max_turns=2)

            with self.assertRaises(MaxTurnsExceeded):
                agent.run("keep going", cwd=Path(tmp))

            sessions = store.list()
            self.assertEqual(len(sessions), 1)
            self.assertEqual(sessions[0].status, "failed")
            self.assertIsNone(store.runner_info(sessions[0].id))
            events = store.events.read(sessions[0].id)
            self.assertEqual(events[-1].type, "session.failed")
            self.assertIn("did not call done", events[-1].payload["error"])

    def test_live_runner_marker_blocks_duplicate_session_runner(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            store.begin_run(session.id)
            agent = Agent(store, provider=TextOnlyProvider())

            with self.assertRaises(RuntimeError):
                agent.run_session(session.id, "duplicate")

            loaded = store.load(session.id)
            self.assertIsNotNone(loaded)
            self.assertEqual(loaded.status, "running")
            self.assertIsNotNone(store.runner_info(session.id))

    def test_text_only_model_response_is_final_result(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = Agent(store, provider=TextOnlyProvider()).run("answer directly", cwd=Path(tmp))

            self.assertEqual(session.status, "done")
            events = store.events.read(session.id)
            self.assertEqual(events[-1].type, "session.done")
            self.assertEqual(events[-1].payload["result"], "direct final")

    def test_large_tool_output_spills_to_artifact(self) -> None:
        class LargeOutputProvider:
            def __init__(self):
                self.turn = 0

            def start_turn(self, messages, tools):
                self.turn += 1
                if self.turn == 1:
                    yield ModelEvent.call(ToolCall(id="call_large", name="echo", arguments={"text": "x" * 21000}))
                else:
                    tool_message = [m for m in messages if m.get("role") == "tool"][-1]
                    yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"result": tool_message["content"]}))

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = Agent(store, provider=LargeOutputProvider()).run("large", cwd=Path(tmp))
            events = store.events.read(session.id)
            large_output = [
                event.payload["output"]
                for event in events
                if event.type == "tool.finished" and event.payload["name"] == "echo"
            ][0]

            self.assertTrue(large_output["data"]["truncated"])
            self.assertTrue(Path(large_output["data"]["output_path"]).exists())
            self.assertIn("full output saved", large_output["text"])

    def test_large_done_result_is_preserved_while_event_output_spills(self) -> None:
        final_text = "x" * 21000

        class LargeDoneProvider:
            def start_turn(self, messages, tools):
                yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"result": final_text}))

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            Agent(store, provider=LargeDoneProvider()).run("large final", cwd=Path(tmp))
            events = store.events.read(store.list()[0].id)
            done_event = [event for event in events if event.type == "session.done"][-1]
            done_output = [
                event.payload["output"]
                for event in events
                if event.type == "tool.finished" and event.payload["name"] == "done"
            ][0]

            self.assertEqual(done_event.payload["result"], final_text)
            self.assertTrue(done_output["data"]["truncated"])
            self.assertTrue(Path(done_output["data"]["output_path"]).exists())

    def test_done_can_use_workspace_file_as_final_result(self) -> None:
        final_text = "{\"stores\":[{\"name\":\"Example\",\"address\":\"1 Main St\"}]}" * 500

        class DonePathProvider:
            def start_turn(self, messages, tools):
                yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"path": "final.json"}))

        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "final.json").write_text(final_text, encoding="utf-8")
            store = SessionStore(Path(tmp))
            Agent(store, provider=DonePathProvider()).run("large final file", cwd=Path(tmp))
            events = store.events.read(store.list()[0].id)
            done_event = [event for event in events if event.type == "session.done"][-1]

            self.assertEqual(done_event.payload["result"], final_text)

    def test_large_tool_data_spills_without_reinlining_data(self) -> None:
        class LargeDataProvider:
            def __init__(self):
                self.turn = 0

            def start_turn(self, messages, tools):
                self.turn += 1
                if self.turn == 1:
                    yield ModelEvent.call(ToolCall(id="call_large_data", name="large_data", arguments={}))
                else:
                    tool_message = [m for m in messages if m.get("role") == "tool"][-1]
                    yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"result": tool_message["content"]}))

        def large_data(ctx: ToolContext, arguments):
            return ToolResult(data={"ok": True, "result": {"payload": "x" * 21000}})

        registry = ToolRegistry()
        registry.register(
            ToolSpec(
                name="large_data",
                description="return large data",
                input_schema={"type": "object", "properties": {}, "additionalProperties": False},
            ),
            large_data,
        )
        registry.register(
            ToolSpec(
                name="done",
                description="finish",
                input_schema={"type": "object", "properties": {"result": {"type": "string"}}, "required": ["result"]},
            ),
            lambda ctx, arguments: ToolResult(text=str(arguments.get("result", ""))),
        )

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            Agent(store, provider=LargeDataProvider(), tools=registry).run("large data", cwd=Path(tmp))
            events = store.events.read(store.list()[0].id)
            large_output = [
                event.payload["output"]
                for event in events
                if event.type == "tool.finished" and event.payload["name"] == "large_data"
            ][0]

            self.assertTrue(large_output["data"]["truncated"])
            self.assertNotIn("result", large_output["data"])
            self.assertTrue(Path(large_output["data"]["output_path"]).exists())

    def test_tool_errors_are_returned_to_model_for_recovery(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = Agent(store, provider=BadToolThenDoneProvider()).run("recover", cwd=Path(tmp))

            self.assertEqual(session.status, "done")
            events = store.events.read(session.id)
            event_types = [event.type for event in events]
            self.assertIn("tool.failed", event_types)
            self.assertIn("session.done", event_types)
            done = [event for event in events if event.type == "session.done"][-1]
            self.assertIn("unknown tool", done.payload["result"])

    def test_compaction_emits_event_and_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = Agent(store, provider=ManyToolCallsProvider(), compact_after_chars=500).run("compact", cwd=Path(tmp))
            events = store.events.read(session.id)
            compacted = [event for event in events if event.type == "session.compacted"]

            self.assertTrue(compacted)
            self.assertTrue(Path(compacted[0].payload["path"]).exists())
            self.assertLess(compacted[-1].payload["after_messages"], compacted[-1].payload["before_messages"])

    def test_deadline_warning_is_injected_before_timeout(self) -> None:
        class WarnAwareProvider:
            def start_turn(self, messages, tools):
                if any("Runtime note:" in str(message.get("content")) for message in messages):
                    yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"result": "finished"}))
                else:
                    yield ModelEvent.call(ToolCall(id="call_echo", name="echo", arguments={"text": "wait"}))

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = Agent(store, provider=WarnAwareProvider(), time_budget_s=1).run("deadline", cwd=Path(tmp))

            self.assertEqual(session.status, "done")
            self.assertIn("session.deadline_warning", [event.type for event in store.events.read(session.id)])

    def test_resume_session_replays_existing_trace(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            agent = Agent(store)
            session = agent.run("first", cwd=Path(tmp))

            resumed = Agent(store).resume_session(session.id, "continue")

            self.assertEqual(resumed.status, "done")
            inputs = [event for event in store.events.read(session.id) if event.type == "session.input"]
            self.assertTrue(inputs[-1].payload["resumed"])

    def test_session_tool_starts_child_as_normal_background_session(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            parent = Agent(
                store,
                provider=SpawnChildProvider(),
                provider_factory=lambda: None,
            ).run("spawn child", cwd=Path(tmp))

            children = [session for session in store.list() if session.parent_id == parent.id]
            self.assertEqual(len(children), 1)
            child = children[0]
            for _ in range(40):
                loaded = store.load(child.id)
                if loaded and loaded.status == "done":
                    break
                __import__("time").sleep(0.05)
            loaded = store.load(child.id)
            self.assertIsNotNone(loaded)
            assert loaded is not None
            self.assertEqual(loaded.status, "done")
            parent_events = [event.type for event in store.events.read(parent.id)]
            self.assertIn("session.child_started", parent_events)
            child_events = [event.type for event in store.events.read(child.id)]
            self.assertIn("session.parent", child_events)

    def test_provider_object_is_not_reused_for_child_sessions(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            provider = SpawnChildProvider()
            agent = Agent(store, provider=provider)

            child_provider = agent._child_provider_factory()

            self.assertIs(agent.provider, provider)
            self.assertIsInstance(child_provider, FakeProvider)
            self.assertIsNot(child_provider, provider)

    def test_read_only_tool_calls_run_in_parallel_and_preserve_message_order(self) -> None:
        class ParallelReadProvider:
            def __init__(self):
                self.turn = 0

            def start_turn(self, messages, tools):
                self.turn += 1
                if self.turn == 1:
                    yield ModelEvent.call(ToolCall(id="call_a", name="read", arguments={"path": "a.txt"}))
                    yield ModelEvent.call(ToolCall(id="call_b", name="read", arguments={"path": "b.txt"}))
                else:
                    outputs = [message["content"] for message in messages if message.get("role") == "tool"]
                    yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"result": "|".join(outputs)}))

        starts = {}
        ends = {}

        def slow_read(ctx: ToolContext, arguments):
            path = str(arguments["path"])
            starts[path] = time.monotonic()
            time.sleep(0.2)
            ends[path] = time.monotonic()
            return ToolResult(text=path)

        registry = ToolRegistry()
        registry.register(
            ToolSpec(
                name="read",
                description="slow read",
                input_schema={"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]},
            ),
            slow_read,
        )
        registry.register(
            ToolSpec(
                name="done",
                description="finish",
                input_schema={"type": "object", "properties": {"result": {"type": "string"}}, "required": ["result"]},
            ),
            lambda ctx, arguments: ToolResult(text=str(arguments.get("result", ""))),
        )

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = Agent(store, provider=ParallelReadProvider(), tools=registry).run("parallel", cwd=Path(tmp))

            self.assertEqual(session.status, "done")
            self.assertLess(starts["b.txt"], ends["a.txt"])
            self.assertLess(starts["a.txt"], ends["b.txt"])
            done = [event for event in store.events.read(session.id) if event.type == "session.done"][-1]
            self.assertEqual(done.payload["result"], "a.txt|b.txt")

    def test_parallel_read_batch_observes_cancellation_without_waiting_for_all_reads(self) -> None:
        class ParallelReadProvider:
            def start_turn(self, messages, tools):
                yield ModelEvent.call(ToolCall(id="call_a", name="read", arguments={"path": "a.txt"}))
                yield ModelEvent.call(ToolCall(id="call_b", name="read", arguments={"path": "b.txt"}))

        started = threading.Event()

        def slow_read(ctx: ToolContext, arguments):
            started.set()
            time.sleep(0.8)
            return ToolResult(text=str(arguments["path"]))

        registry = ToolRegistry()
        registry.register(
            ToolSpec(
                name="read",
                description="slow read",
                input_schema={"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]},
            ),
            slow_read,
        )

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            agent = Agent(store, provider=ParallelReadProvider(), tools=registry)
            finished: list[str] = []

            def run_agent() -> None:
                session = agent.run("parallel cancel", cwd=Path(tmp))
                finished.append(session.status)

            thread = threading.Thread(target=run_agent)
            started_at = time.monotonic()
            thread.start()
            self.assertTrue(started.wait(timeout=1.0))
            session_id = store.list()[0].id
            store.request_cancel(session_id, reason="stop reads")
            thread.join(timeout=0.5)

            self.assertFalse(thread.is_alive())
            self.assertLess(time.monotonic() - started_at, 0.7)
            self.assertEqual(finished, ["cancelled"])

    def test_resume_rehydrates_tool_images_from_event_payloads(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            image_path = Path(tmp) / "frame.png"
            image_path.write_bytes(b"png-bytes")
            image = ToolImage(label="frame", path=str(image_path), order=1)
            store.emit(session.id, "session.input", {"text": "look"})
            store.emit(session.id, "tool.started", {"tool_call_id": "call_1", "name": "python", "arguments": {"code": "screenshot()"}})
            store.emit(
                session.id,
                "tool.finished",
                {
                    "tool_call_id": "call_1",
                    "name": "python",
                    "output": ToolResult(text="captured", images=[image], data={"ok": True}).to_event_payload(),
                },
            )

            messages = Agent(store)._messages_from_events(session.id)

            tool_message = [message for message in messages if message.get("role") == "tool"][-1]
            self.assertIsInstance(tool_message["content"], list)
            self.assertEqual(tool_message["content"][0]["type"], "input_text")
            self.assertEqual(tool_message["content"][1]["type"], "input_image")


if __name__ == "__main__":
    raise SystemExit(unittest.main())
