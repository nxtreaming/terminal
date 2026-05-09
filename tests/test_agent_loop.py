from __future__ import annotations

import json
from importlib import resources
import tempfile
import threading
import time
import unittest
from pathlib import Path

from llm_browser.agent import Agent
from llm_browser.agent.compaction import (
    compact_messages,
    message_context_units,
    message_image_count,
    trim_message_images,
)
from llm_browser.agent.service import MaxTurnsExceeded, _is_parallel_safe_shell_call
from llm_browser.browser.instructions import BROWSER_AGENT_INSTRUCTIONS, BROWSER_HELP_PLAYBOOK, CODEX_AGENT_INSTRUCTIONS
from llm_browser.provider.fake import FakeProvider
from llm_browser.provider.types import ModelEvent, ToolCall
from llm_browser.session.store import SessionStore
from llm_browser.session.usage import ModelTokenUsage
from llm_browser.tool.context import ToolContext
from llm_browser.tool.registry import ToolRegistry
from llm_browser.tool.result import ToolImage, ToolResult
from llm_browser.tool.session import SessionTool
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


class EmptyThenDoneProvider:
    def __init__(self):
        self.turn = 0
        self.messages = []

    def start_turn(self, messages, tools):
        self.turn += 1
        self.messages.append(list(messages))
        if self.turn == 1:
            return
        yield ModelEvent.text("recovered final")


class UsageProvider:
    model = "gpt-5.5"

    def start_turn(self, messages, tools):
        yield ModelEvent.text("direct final")
        yield ModelEvent.usage(
            ModelTokenUsage(input_tokens=1000, output_tokens=10, cache_read_tokens=100),
            model=self.model,
            provider="openai",
        )


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


class ForkContextParentProvider:
    def __init__(self):
        self.turn = 0

    def start_turn(self, messages, tools):
        self.turn += 1
        if self.turn == 1:
            yield ModelEvent.call(ToolCall(id="call_echo", name="echo", arguments={"text": "parent secret"}))
        elif self.turn == 2:
            yield ModelEvent.call(
                ToolCall(
                    id="call_spawn",
                    name="spawn_agent",
                    arguments={
                        "agent_type": "explorer",
                        "fork_context": True,
                        "message": "What value did the parent echo? Do not use tools.",
                    },
                )
            )
        elif self.turn == 3:
            tool_message = [message for message in messages if message.get("role") == "tool"][-1]
            text = str(tool_message["content"])
            marker = "agent_id': '"
            if marker in text:
                agent_id = text.split(marker, 1)[1].split("'", 1)[0]
            else:
                agent_id = text.split('"agent_id": "', 1)[1].split('"', 1)[0]
            yield ModelEvent.call(
                ToolCall(
                    id="call_wait",
                    name="wait_agent",
                    arguments={"targets": [agent_id], "timeout_ms": 5000},
                )
            )
        else:
            tool_message = [message for message in messages if message.get("role") == "tool"][-1]
            yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"result": str(tool_message["content"])}))


class ForkContextChildProvider:
    def start_turn(self, messages, tools):
        text = str(messages)
        result = "saw parent secret" if "parent secret" in text else "NO INHERITED CONTEXT"
        yield ModelEvent.call(ToolCall(id="call_done_child", name="done", arguments={"result": result}))


class InstructionCaptureProvider:
    def __init__(self):
        self.instructions = ""
        self.provider_label = "test-provider"
        self.model = "test-model"

    def set_instructions(self, instructions: str) -> None:
        self.instructions = instructions

    def start_turn(self, messages, tools):
        yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"result": "ok"}))


class CloseRecordingTool:
    def __init__(self) -> None:
        self.closed: list[tuple[str, bool]] = []

    def __call__(self, ctx: ToolContext, arguments):
        return ToolResult(text="ok")

    def close_session(self, session_id: str, stop_browser: bool = True) -> None:
        self.closed.append((session_id, stop_browser))


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
            self.assertIn("model.config", event_types)
            self.assertIn("model.delta", event_types)
            self.assertEqual(event_types.count("tool.started"), 2)
            self.assertEqual(event_types.count("tool.finished"), 2)
            self.assertIn("session.done", event_types)
            config_event = [event for event in events if event.type == "model.config"][0]
            self.assertEqual(config_event.payload["provider"], "fake")
            self.assertEqual(config_event.payload["model"], "unknown")

            tool_names = [
                event.payload["name"]
                for event in events
                if event.type == "tool.started"
            ]
            self.assertEqual(tool_names, ["echo", "done"])

    def test_agent_can_leave_tools_open_after_done(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            registry = ToolRegistry()
            recorder = CloseRecordingTool()
            registry.register(ToolSpec(name="record", description="record", input_schema={"type": "object"}), recorder)
            registry.register(ToolSpec(name="done", description="done", input_schema={"type": "object"}), lambda ctx, args: ToolResult(text="ok"))

            session = Agent(
                store,
                provider=InstructionCaptureProvider(),
                tools=registry,
                close_tools_on_finish=False,
            ).run("Open example.com", cwd=Path(tmp))

            self.assertEqual(session.status, "done")
            self.assertEqual(recorder.closed, [])

            registry.close_session(session.id)
            self.assertEqual(recorder.closed, [(session.id, True)])

    def test_auto_instruction_mode_switches_to_repo_instructions_for_repo_tasks(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            provider = InstructionCaptureProvider()

            Agent(store, provider=provider).run("what is in this repo", cwd=Path(tmp))

            self.assertIn("Runtime provider: test-provider", provider.instructions)
            self.assertIn("Runtime model: test-model", provider.instructions)
            self.assertIn("You are Browser Use", provider.instructions)
            self.assertIn("rg --files", provider.instructions)

    def test_auto_instruction_mode_keeps_browser_instructions_for_browser_tasks(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            provider = InstructionCaptureProvider()

            Agent(store, provider=provider).run("Open example.com", cwd=Path(tmp))

            self.assertIn("Runtime provider: test-provider", provider.instructions)
            self.assertIn("Runtime model: test-model", provider.instructions)
            self.assertIn("You control the harness-owned Chrome browser through Python and CDP", provider.instructions)
            self.assertIn("CDP is the source of truth", provider.instructions)
            self.assertIn("Helpers are convenience wrappers", provider.instructions)
            self.assertIn("they are not top-level tools", provider.instructions)
            self.assertIn("Do not discover the browser through raw DevTools URLs", provider.instructions)
            self.assertNotIn("reconnect_browser", provider.instructions)

    def test_runtime_identity_uses_provider_attributes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            provider = InstructionCaptureProvider()
            provider.provider_label = "openrouter"
            provider.model = "qwen/qwen3.6-plus"

            Agent(store, provider=provider).run("which model are you", cwd=Path(tmp))

            self.assertIn("Runtime provider: openrouter", provider.instructions)
            self.assertIn("Runtime model: qwen/qwen3.6-plus", provider.instructions)

    def test_browser_prompts_load_from_markdown_resources(self) -> None:
        prompt_root = resources.files("llm_browser.browser").joinpath("prompts")

        self.assertEqual(
            CODEX_AGENT_INSTRUCTIONS,
            prompt_root.joinpath("codex-agent-instructions.md").read_text(encoding="utf-8").rstrip("\n"),
        )
        self.assertEqual(
            BROWSER_AGENT_INSTRUCTIONS,
            prompt_root.joinpath("browser-agent-instructions.md").read_text(encoding="utf-8").rstrip("\n"),
        )
        self.assertEqual(
            BROWSER_HELP_PLAYBOOK,
            prompt_root.joinpath("browser-help-playbook.md").read_text(encoding="utf-8").rstrip("\n"),
        )

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

    def test_empty_model_turn_is_reprompted(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            provider = EmptyThenDoneProvider()
            session = Agent(store, provider=provider, max_turns=3).run("answer directly", cwd=Path(tmp))

            self.assertEqual(session.status, "done")
            events = store.events.read(session.id)
            self.assertEqual([event.type for event in events].count("model.empty_turn"), 1)
            self.assertEqual(events[-1].payload["result"], "recovered final")
            self.assertIn("no visible text", provider.messages[-1][-1]["content"])

    def test_model_usage_event_is_persisted_with_cost(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = Agent(store, provider=UsageProvider()).run("answer directly", cwd=Path(tmp))

            events = store.events.read(session.id)
            usage_events = [event for event in events if event.type == "model.usage"]
            self.assertEqual(len(usage_events), 1)
            self.assertEqual(usage_events[0].payload["model"], "gpt-5.5")
            self.assertEqual(usage_events[0].payload["usage"]["input_tokens"], 1000)
            self.assertEqual(usage_events[0].payload["usage"]["cache_read_tokens"], 100)
            self.assertGreater(usage_events[0].payload["cost_usd"], 0)

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

    def test_done_recovers_session_file_from_stale_absolute_path(self) -> None:
        final_text = "**1 - recovered record**\nsummary"

        class DonePathProvider:
            def start_turn(self, messages, tools):
                stale_path = "/Users/greg/Documents/browser-use/llm-browser/.browser-use-terminal/dataset-runs/run/task/final.md"
                yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"path": stale_path}))

        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp) / "dataset-runs" / "run" / "task"
            workspace.mkdir(parents=True)
            workspace.joinpath("final.md").write_text(final_text, encoding="utf-8")
            store = SessionStore(Path(tmp))
            Agent(store, provider=DonePathProvider()).run("large final file", cwd=workspace)
            events = store.events.read(store.list()[0].id)
            done_event = [event for event in events if event.type == "session.done"][-1]

            self.assertEqual(done_event.payload["result"], final_text)

    def test_done_can_use_binary_workspace_file_as_final_artifact(self) -> None:
        class DonePathProvider:
            def start_turn(self, messages, tools):
                yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"path": "table.jpg"}))

        with tempfile.TemporaryDirectory() as tmp:
            image_path = Path(tmp, "table.jpg")
            image_path.write_bytes(b"\xff\xd8\xff\xe0binary-jpeg")
            store = SessionStore(Path(tmp))
            Agent(store, provider=DonePathProvider()).run("return image", cwd=Path(tmp))
            events = store.events.read(store.list()[0].id)
            done_event = [event for event in events if event.type == "session.done"][-1]
            done_output = [
                event.payload["output"]
                for event in events
                if event.type == "tool.finished" and event.payload["name"] == "done"
            ][0]

            self.assertEqual(Path(done_event.payload["result"]).resolve(), image_path.resolve())
            self.assertTrue(done_output["data"]["binary"])
            self.assertEqual(done_output["data"]["mime_type"], "image/jpeg")
            self.assertEqual(done_output["data"]["bytes"], image_path.stat().st_size)

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

    def test_compaction_keeps_tool_output_with_matching_function_call(self) -> None:
        messages = [{"role": "user", "content": "start"}]
        for index in range(5):
            messages.append(
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": f"call_{index}",
                            "name": "echo",
                            "arguments": {"text": str(index)},
                        }
                    ],
                }
            )
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": f"call_{index}",
                    "name": "echo",
                    "content": f"result {index}",
                }
            )

        with tempfile.TemporaryDirectory() as tmp:
            compacted, _ = compact_messages(messages, Path(tmp), keep_last=3)

        self.assertEqual(compacted[1]["role"], "assistant")
        self.assertEqual(compacted[1]["tool_calls"][0]["id"], "call_3")
        self.assertEqual(compacted[2]["role"], "tool")
        self.assertEqual(compacted[2]["tool_call_id"], "call_3")
        self.assertEqual(compacted[3]["role"], "assistant")
        self.assertEqual(compacted[4]["role"], "tool")

    def test_compaction_counts_and_prunes_screenshot_payloads(self) -> None:
        image = {
            "type": "input_image",
            "detail": "auto",
            "image_url": "data:image/png;base64," + ("x" * 500000),
        }
        messages = [{"role": "user", "content": "start"}]
        for index in range(8):
            messages.append(
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": f"call_{index}",
                            "name": "python",
                            "arguments": {"code": "capture_screenshot(attach=True)"},
                        }
                    ],
                }
            )
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": f"call_{index}",
                    "name": "python",
                    "content": [
                        {"type": "input_text", "text": f"screenshot {index}"},
                        dict(image),
                    ],
                }
            )

        before_units = message_context_units(messages)
        with tempfile.TemporaryDirectory() as tmp:
            compacted, path = compact_messages(messages, Path(tmp), keep_last=12, max_kept_images=2)
            self.assertTrue(path.exists())
            payload = json.loads(path.read_text(encoding="utf-8"))

        self.assertGreater(before_units, message_context_units(compacted))
        self.assertIn("replay_messages", payload)
        self.assertNotIn("image_url", json.dumps(payload["replay_messages"]))
        kept_images = 0
        omitted_markers = 0
        for message in compacted:
            content = message.get("content")
            if not isinstance(content, list):
                continue
            for item in content:
                if item.get("type") == "input_image":
                    kept_images += 1
                if "omitted by compaction" in str(item.get("text") or ""):
                    omitted_markers += 1

        self.assertEqual(kept_images, 2)
        self.assertGreater(omitted_markers, 0)

    def test_context_image_pruning_keeps_latest_images_without_threshold_compaction(self) -> None:
        messages = [{"role": "user", "content": "start"}]
        for index in range(4):
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": f"call_{index}",
                    "name": "python",
                    "content": [
                        {"type": "input_text", "text": f"screenshot {index}"},
                        {
                            "type": "input_image",
                            "detail": "auto",
                            "image_url": f"data:image/png;base64,img{index}",
                        },
                    ],
                }
            )

        trimmed = trim_message_images(messages, max_images=2)
        payload = json.dumps(trimmed)

        self.assertEqual(message_image_count(messages), 4)
        self.assertEqual(message_image_count(trimmed), 2)
        self.assertNotIn("img0", payload)
        self.assertNotIn("img1", payload)
        self.assertIn("img2", payload)
        self.assertIn("img3", payload)
        self.assertIn("omitted by context pruning", payload)

    def test_old_tool_output_pruning_preserves_recent_turns(self) -> None:
        from llm_browser.agent.compaction import prune_old_tool_outputs

        big = "x" * 30000
        messages = [{"role": "user", "content": "start"}]
        for index in range(5):
            messages.extend(
                [
                    {
                        "role": "assistant",
                        "tool_calls": [{"id": f"call_{index}", "name": "python", "arguments": {}}],
                    },
                    {"role": "tool", "tool_call_id": f"call_{index}", "name": "python", "content": big + str(index)},
                    {"role": "user", "content": f"next {index}"},
                ]
            )

        pruned = prune_old_tool_outputs(messages, protect_context_units=40000, minimum_pruned_units=20000)
        payload = json.dumps(pruned)

        self.assertIn("Old tool result content cleared", payload)
        self.assertIn(big + "4", payload)
        self.assertIn(big + "3", payload)

    def test_provider_size_error_compacts_and_retries_without_images(self) -> None:
        class OversizeThenDoneProvider:
            def __init__(self) -> None:
                self.calls = 0
                self.image_counts = []

            def start_turn(self, messages, tools):
                self.calls += 1
                self.image_counts.append(message_image_count(messages))
                if self.calls == 1:
                    raise RuntimeError(
                        "Codex Responses request failed: HTTP 507: exceeded request buffer limit while retrying upstream"
                    )
                yield ModelEvent.text("recovered")

        image = {
            "type": "input_image",
            "detail": "auto",
            "image_url": "data:image/png;base64," + ("x" * 500000),
        }
        messages = [{"role": "user", "content": "continue"}]
        for index in range(4):
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": f"call_{index}",
                    "name": "python",
                    "content": [{"type": "input_text", "text": f"shot {index}"}, dict(image)],
                }
            )

        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            provider = OversizeThenDoneProvider()
            result = Agent(store, provider=provider, compact_after_chars=10**9)._run_with_messages(session, messages)
            events = store.events.read(session.id)

        self.assertEqual(result.status, "done")
        self.assertEqual(provider.calls, 3)
        self.assertGreater(provider.image_counts[0], 0)
        self.assertEqual(provider.image_counts[1], 0)
        self.assertEqual(provider.image_counts[2], 0)
        compacted = [event for event in events if event.type == "session.compacted"]
        self.assertTrue(compacted)
        self.assertIn("replacement_history", compacted[-1].payload)

    def test_resume_replay_starts_from_latest_compaction_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            store = SessionStore(root)
            session = store.create(cwd=root)
            store.emit(session.id, "session.input", {"text": "old task"})
            compaction_path = session.artifact_dir / "compactions" / "001.json"
            compaction_path.parent.mkdir(parents=True, exist_ok=True)
            compaction_path.write_text(
                json.dumps(
                    {
                        "summary": "old task summarized",
                        "replay_messages": [
                            {
                                "role": "user",
                                "content": "Conversation was compacted.\n\nold task summarized",
                            }
                        ],
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            store.emit(session.id, "session.compacted", {"path": str(compaction_path)})
            store.emit(session.id, "session.input", {"text": "new instruction", "resumed": True})

            messages = Agent(store, provider=TextOnlyProvider())._messages_from_events(session.id)

        self.assertEqual(len(messages), 2)
        self.assertIn("old task summarized", messages[0]["content"])
        self.assertEqual(messages[1]["content"], "new instruction")
        self.assertNotIn("old task", messages[1]["content"])

    def test_resume_replay_starts_from_latest_compaction_checkpoint(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            store = SessionStore(root)
            session = store.create(cwd=root)
            store.emit(session.id, "session.input", {"text": "old task"})
            store.emit(
                session.id,
                "session.compacted",
                {
                    "schema_version": 1,
                    "compaction_id": "compact_1",
                    "phase": "standalone_turn",
                    "reason": "user_requested",
                    "message": "checkpoint summary",
                    "replacement_history": [{"role": "user", "content": "checkpoint summary"}],
                    "before_messages": 1,
                    "after_messages": 1,
                },
            )
            store.emit(session.id, "session.input", {"text": "new instruction", "resumed": True})

            messages = Agent(store, provider=TextOnlyProvider())._messages_from_events(session.id)

        self.assertEqual(messages, [{"role": "user", "content": "checkpoint summary"}, {"role": "user", "content": "new instruction"}])

    def test_manual_compact_allows_empty_session_history(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            store = SessionStore(root)
            session = store.create(cwd=root)

            Agent(store, provider=TextOnlyProvider()).compact_session(session.id)

            compacted = [event for event in store.events.read(session.id) if event.type == "session.compacted"]

        self.assertTrue(compacted)
        self.assertEqual(compacted[-1].payload["reason"], "user_requested")
        self.assertIn("replacement_history", compacted[-1].payload)

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

    def test_codex_subagent_wrappers_use_child_sessions(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            parent = store.create(cwd=Path(tmp))
            ctx = ToolContext(session=parent, store=store, tool_call_id="call_spawn", tool_name="spawn_agent")
            tool = SessionTool(store, provider_factory=lambda: None, max_turns=4, mode="codex")

            spawned = tool.spawn_agent(ctx, {"agent_type": "explorer", "message": "Inspect the repo shape."})
            agent_id = spawned.data["agent_id"]
            waited = tool.wait_agent(ctx, {"targets": [agent_id], "timeout_ms": 5000})
            closed = tool.close_agent(ctx, {"target": agent_id})

            child = store.load(agent_id)
            self.assertIsNotNone(child)
            assert child is not None
            self.assertEqual(child.parent_id, parent.id)
            self.assertEqual(waited.data["statuses"][agent_id]["status"], "done")
            self.assertEqual(closed.data["previous_status"]["status"], "done")
            parent_events = [event.type for event in store.events.read(parent.id)]
            self.assertIn("session.child_started", parent_events)

    def test_spawn_agent_fork_context_passes_parent_transcript(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            parent = Agent(
                store,
                provider=ForkContextParentProvider(),
                provider_factory=ForkContextChildProvider,
                max_turns=6,
                mode="codex",
            ).run("spawn with forked context", cwd=Path(tmp))

            children = [session for session in store.list() if session.parent_id == parent.id]
            self.assertEqual(len(children), 1)
            child_done = [
                event
                for event in store.events.read(children[0].id)
                if event.type == "session.done"
            ][-1]
            self.assertEqual(child_done.payload["result"], "saw parent secret")
            parent_done = [
                event
                for event in store.events.read(parent.id)
                if event.type == "session.done"
            ][-1]
            self.assertIn("saw parent secret", parent_done.payload["result"])

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

    def test_codex_exec_command_read_only_calls_run_in_parallel(self) -> None:
        class ParallelExecProvider:
            def __init__(self):
                self.turn = 0

            def start_turn(self, messages, tools):
                self.turn += 1
                if self.turn == 1:
                    yield ModelEvent.call(ToolCall(id="call_pwd", name="exec_command", arguments={"cmd": "pwd"}))
                    yield ModelEvent.call(ToolCall(id="call_ls", name="exec_command", arguments={"cmd": "ls"}))
                else:
                    outputs = [message["content"] for message in messages if message.get("role") == "tool"]
                    yield ModelEvent.call(ToolCall(id="call_done", name="done", arguments={"result": "|".join(outputs)}))

        starts = {}
        ends = {}

        def slow_exec(ctx: ToolContext, arguments):
            cmd = str(arguments["cmd"])
            starts[cmd] = time.monotonic()
            time.sleep(0.2)
            ends[cmd] = time.monotonic()
            return ToolResult(text=cmd)

        registry = ToolRegistry()
        registry.register(
            ToolSpec(
                name="exec_command",
                description="slow exec",
                input_schema={"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]},
            ),
            slow_exec,
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
            session = Agent(store, provider=ParallelExecProvider(), tools=registry).run("parallel", cwd=Path(tmp))

            self.assertEqual(session.status, "done")
            self.assertLess(starts["ls"], ends["pwd"])
            self.assertLess(starts["pwd"], ends["ls"])
            done = [event for event in store.events.read(session.id) if event.type == "session.done"][-1]
            self.assertEqual(done.payload["result"], "pwd|ls")

    def test_shell_parallel_heuristic_is_conservative(self) -> None:
        self.assertTrue(
            _is_parallel_safe_shell_call(ToolCall(id="call_rg", name="exec_command", arguments={"cmd": "rg --files"}))
        )
        self.assertTrue(
            _is_parallel_safe_shell_call(ToolCall(id="call_git", name="exec_command", arguments={"cmd": "git status --short"}))
        )
        self.assertTrue(
            _is_parallel_safe_shell_call(ToolCall(id="call_pipe", name="exec_command", arguments={"cmd": "rg needle | head"}))
        )
        self.assertTrue(
            _is_parallel_safe_shell_call(
                ToolCall(id="call_find", name="exec_command", arguments={"cmd": "find docs -maxdepth 2 -type f | sort | head"})
            )
        )
        self.assertFalse(
            _is_parallel_safe_shell_call(ToolCall(id="call_redirect", name="exec_command", arguments={"cmd": "printf x > out.txt"}))
        )
        self.assertFalse(
            _is_parallel_safe_shell_call(ToolCall(id="call_find_delete", name="exec_command", arguments={"cmd": "find . -delete"}))
        )
        self.assertFalse(
            _is_parallel_safe_shell_call(ToolCall(id="call_mutating_git", name="exec_command", arguments={"cmd": "git checkout main"}))
        )
        self.assertFalse(
            _is_parallel_safe_shell_call(ToolCall(id="call_chain", name="exec_command", arguments={"cmd": "pwd && ls"}))
        )

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

    def test_resume_reconstructs_parallel_tool_calls_by_call_id(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            store.emit(session.id, "session.input", {"text": "parallel"})
            store.emit(session.id, "tool.started", {"tool_call_id": "call_a", "name": "read", "arguments": {"path": "a.txt"}})
            store.emit(session.id, "tool.started", {"tool_call_id": "call_b", "name": "read", "arguments": {"path": "b.txt"}})
            store.emit(
                session.id,
                "tool.finished",
                {"tool_call_id": "call_b", "name": "read", "output": ToolResult(text="b").to_event_payload()},
            )
            store.emit(
                session.id,
                "tool.finished",
                {"tool_call_id": "call_a", "name": "read", "output": ToolResult(text="a").to_event_payload()},
            )

            messages = Agent(store)._messages_from_events(session.id)

            assistant = [message for message in messages if message.get("role") == "assistant"][-1]
            tool_messages = [message for message in messages if message.get("role") == "tool"]
            self.assertEqual([call["id"] for call in assistant["tool_calls"]], ["call_a", "call_b"])
            self.assertEqual({message["tool_call_id"] for message in tool_messages}, {"call_a", "call_b"})
            self.assertEqual({message["content"] for message in tool_messages}, {"a", "b"})

    def test_resume_synthesizes_output_for_dangling_tool_call_before_followup(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = SessionStore(Path(tmp))
            session = store.create(cwd=Path(tmp))
            store.emit(session.id, "session.input", {"text": "first"})
            store.emit(session.id, "tool.started", {"tool_call_id": "call_missing", "name": "python", "arguments": {"code": "x()"}})
            store.emit(session.id, "session.input", {"text": "follow up"})

            messages = Agent(store)._messages_from_events(session.id)

            self.assertEqual(messages[1]["role"], "assistant")
            self.assertEqual(messages[1]["tool_calls"][0]["id"], "call_missing")
            self.assertEqual(messages[2]["role"], "tool")
            self.assertEqual(messages[2]["tool_call_id"], "call_missing")
            self.assertIn("missing tool output", messages[2]["content"])
            self.assertEqual(messages[3], {"role": "user", "content": "follow up"})


if __name__ == "__main__":
    raise SystemExit(unittest.main())
