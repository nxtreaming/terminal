from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from llm_browser.agent import Agent
from llm_browser.agent.service import MaxTurnsExceeded
from llm_browser.provider.types import ModelEvent, ToolCall
from llm_browser.session.store import SessionStore


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


class AgentLoopTest(unittest.TestCase):
    def test_fake_provider_executes_tools_and_finishes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            agent = Agent(store)

            session = agent.run("Open example.com", cwd=Path(tmp))

            loaded = store.load(session.id)
            self.assertIsNotNone(loaded)
            self.assertEqual(loaded.status, "done")

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
            events = store.events.read(sessions[0].id)
            self.assertEqual(events[-1].type, "session.failed")
            self.assertIn("did not call done", events[-1].payload["error"])

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

    def test_resume_session_replays_existing_trace(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            agent = Agent(store)
            session = agent.run("first", cwd=Path(tmp))

            resumed = Agent(store).resume_session(session.id, "continue")

            self.assertEqual(resumed.status, "done")
            inputs = [event for event in store.events.read(session.id) if event.type == "session.input"]
            self.assertTrue(inputs[-1].payload["resumed"])


if __name__ == "__main__":
    raise SystemExit(unittest.main())
