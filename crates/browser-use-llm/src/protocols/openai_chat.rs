//! OpenAI-compatible Chat Completions protocol (`/chat/completions`).
//!
//! This is the de-facto standard chat wire format spoken by OpenAI as well as
//! the many compatible backends (Ollama, OpenRouter, DeepSeek, Fireworks, ...).
//! Requests carry a flat `messages` array with `system`/`user`/`assistant`/`tool`
//! roles; responses stream as SSE frames whose `choices[].delta` objects carry
//! text and incremental `tool_calls`, followed by a terminal frame bearing a
//! `finish_reason` and (when requested) a `usage` object.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::protocols::utils::{Lifecycle, ToolStream};
use crate::route::framing::SseFrame;
use crate::route::protocol::{Protocol, ProtocolStream};
use crate::schema::{
    ContentPart, FinishReason, GenerationOptions, LlmError, LlmErrorReason, LlmEvent, LlmRequest,
    Message, MessageRole, ReasoningEffort, ToolChoice, Usage,
};

/// Stable block ids used to drive the [`Lifecycle`]. Chat Completions streams a
/// single text/reasoning channel per choice, so one id each is sufficient.
const TEXT_ID: &str = "0";
const REASONING_ID: &str = "reasoning";

/// Adapter for the OpenAI-compatible Chat Completions wire format.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAiChatProtocol;

impl OpenAiChatProtocol {
    /// Create a new protocol adapter.
    pub fn new() -> Self {
        Self
    }
}

impl Protocol for OpenAiChatProtocol {
    fn build_body(&self, req: &LlmRequest) -> Result<Value, LlmError> {
        let mut body = Map::new();

        body.insert("model".to_string(), json!(req.model.as_str()));

        // The system prompt is prepended as a `role:"system"` message.
        let mut messages: Vec<Value> = Vec::new();
        if !req.system.is_empty() {
            let system_text: String = req
                .system
                .iter()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            messages.push(json!({ "role": "system", "content": system_text }));
        }
        for message in &req.messages {
            messages.push(build_message(message)?);
        }
        body.insert("messages".to_string(), Value::Array(messages));

        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.input_schema,
                        }
                    })
                })
                .collect();
            body.insert("tools".to_string(), Value::Array(tools));
        }

        if let Some(choice) = &req.tool_choice {
            body.insert("tool_choice".to_string(), build_tool_choice(choice));
        }

        apply_generation(&mut body, &req.generation);

        body.insert("stream".to_string(), Value::Bool(true));
        body.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );

        Ok(Value::Object(body))
    }

    fn decoder(&self) -> Box<dyn ProtocolStream> {
        Box::new(OpenAiChatStream::new())
    }
}

/// Pass through the generation knobs Chat Completions understands.
fn apply_generation(body: &mut Map<String, Value>, gen: &GenerationOptions) {
    if let Some(temperature) = gen.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = gen.top_p {
        body.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(max_tokens) = gen.max_tokens {
        body.insert("max_tokens".to_string(), json!(max_tokens));
    }
    if let Some(effort) = gen.reasoning_effort {
        body.insert(
            "reasoning_effort".to_string(),
            json!(reasoning_effort_str(effort)),
        );
    }
    if !gen.stop.is_empty() {
        body.insert("stop".to_string(), json!(gen.stop));
    }
}

/// Lower the neutral reasoning effort to the Chat Completions wire string.
fn reasoning_effort_str(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None => "none",
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
    }
}

/// Translate a neutral [`Message`] into one Chat Completions message object.
fn build_message(message: &Message) -> Result<Value, LlmError> {
    match message.role {
        MessageRole::Tool => Ok(build_tool_message(message)),
        MessageRole::Assistant => build_assistant_message(message),
        MessageRole::System => Ok(build_simple_message("system", message)),
        MessageRole::Developer => Ok(build_simple_message("developer", message)),
        MessageRole::User => Ok(build_simple_message("user", message)),
    }
}

/// Render a `system`/`developer`/`user` message: text parts joined into `content`.
fn build_simple_message(role: &str, message: &Message) -> Value {
    json!({ "role": role, "content": collect_text(message) })
}

/// Render an `assistant` message, including any `tool_calls`.
fn build_assistant_message(message: &Message) -> Result<Value, LlmError> {
    let mut obj = Map::new();
    obj.insert("role".to_string(), json!("assistant"));
    obj.insert("content".to_string(), json!(collect_text(message)));

    let mut tool_calls: Vec<Value> = Vec::new();
    for part in &message.content {
        if let ContentPart::ToolCall {
            id, name, input, ..
        } = part
        {
            let arguments = serde_json::to_string(input).map_err(|e| {
                LlmError::new(
                    LlmErrorReason::InvalidRequest,
                    format!("tool call arguments not serializable: {e}"),
                )
            })?;
            tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments },
            }));
        }
    }
    if !tool_calls.is_empty() {
        obj.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }

    Ok(Value::Object(obj))
}

/// Render a `tool` message from the first [`ContentPart::ToolResult`] it carries.
fn build_tool_message(message: &Message) -> Value {
    let mut obj = Map::new();
    obj.insert("role".to_string(), json!("tool"));
    for part in &message.content {
        if let ContentPart::ToolResult {
            tool_call_id,
            content,
            ..
        } = part
        {
            obj.insert("tool_call_id".to_string(), json!(tool_call_id));
            obj.insert("content".to_string(), json!(collect_text_parts(content)));
            break;
        }
    }
    Value::Object(obj)
}

/// Concatenate all [`ContentPart::Text`] fragments in a message into one string.
fn collect_text(message: &Message) -> String {
    collect_text_parts(&message.content)
}

/// Concatenate the text of a content-part slice into one string.
fn collect_text_parts(parts: &[ContentPart]) -> String {
    let mut text = String::new();
    for part in parts {
        if let ContentPart::Text { text: fragment } = part {
            text.push_str(fragment);
        }
    }
    text
}

/// Translate a neutral [`ToolChoice`] into the Chat Completions wire form.
fn build_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({
            "type": "function",
            "function": { "name": name }
        }),
    }
}

/// Streaming decoder for Chat Completions SSE responses.
struct OpenAiChatStream {
    lifecycle: Lifecycle,
    tools: ToolStream,
    /// Maps a tool call's stream-local `index` to the stable `id` reported on its
    /// first chunk. Later chunks carry only `index` + argument fragments, so the
    /// index is the correlator and the id is the value we emit.
    tool_ids: BTreeMap<u64, String>,
    finish_reason: Option<FinishReason>,
    usage: Usage,
}

impl OpenAiChatStream {
    fn new() -> Self {
        Self {
            lifecycle: Lifecycle::new(),
            tools: ToolStream::new(),
            tool_ids: BTreeMap::new(),
            finish_reason: None,
            usage: Usage::default(),
        }
    }
}

impl ProtocolStream for OpenAiChatStream {
    fn on_frame(&mut self, frame: &SseFrame) -> Result<Vec<LlmEvent>, LlmError> {
        let data = frame.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(Vec::new());
        }

        let chunk: Value = serde_json::from_str(data).map_err(|e| {
            LlmError::new(LlmErrorReason::Decode, format!("invalid chat chunk: {e}"))
        })?;

        let mut events = Vec::new();

        if let Some(usage) = chunk.get("usage") {
            if !usage.is_null() {
                self.usage = parse_usage(usage);
            }
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return Ok(events);
        };

        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                if !content.is_empty() {
                    events.extend(self.lifecycle.text_delta(TEXT_ID, content));
                }
            }
            if let Some(reasoning) = delta
                .get("reasoning")
                .or_else(|| delta.get("reasoning_content"))
                .and_then(Value::as_str)
            {
                if !reasoning.is_empty() {
                    events.extend(self.lifecycle.reasoning_delta(REASONING_ID, reasoning));
                }
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in tool_calls {
                    events.extend(self.handle_tool_call(call));
                }
            }
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(map_finish_reason(reason));
        }

        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        if self.lifecycle.is_finished() {
            return Ok(Vec::new());
        }
        // Close text/reasoning, then emit completed tool calls, then the
        // terminal step_finish / finish carrying usage + reason.
        let mut events = Vec::new();
        events.extend(self.lifecycle.text_end(TEXT_ID));
        events.extend(self.lifecycle.reasoning_end(REASONING_ID));
        events.extend(self.tools.flush()?);
        if self.usage.total_tokens == 0 {
            self.usage.total_tokens = self.usage.computed_total();
        }
        events.extend(self.lifecycle.finish(self.usage, self.finish_reason));
        Ok(events)
    }
}

impl OpenAiChatStream {
    /// Feed one streamed `tool_calls[]` entry into the [`ToolStream`].
    ///
    /// The `id` and `function.name` arrive on a tool call's first chunk; later
    /// chunks for the same call carry only the array `index` plus
    /// `function.arguments` fragments. We therefore correlate on `index`,
    /// resolving the stable `id` (and `name`) recorded on the first chunk.
    fn handle_tool_call(&mut self, call: &Value) -> Vec<LlmEvent> {
        let explicit_id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|i| !i.is_empty());

        // Resolve the stable id this fragment belongs to. Prefer the array
        // `index`; fall back to a present explicit `id` if the index is absent.
        let id = match call.get("index").and_then(Value::as_u64) {
            Some(index) => match self.tool_ids.get(&index) {
                Some(id) => id.clone(),
                None => {
                    let id = explicit_id.map(str::to_string).unwrap_or(index.to_string());
                    self.tool_ids.insert(index, id.clone());
                    id
                }
            },
            None => match explicit_id {
                Some(id) => id.to_string(),
                None => return Vec::new(),
            },
        };

        let function = call.get("function");
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty());
        let fragment = function
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or("");

        // Skip empty argument fragments: the first chunk's `"arguments":""` would
        // otherwise emit a no-op `ToolInputDelta`. When that first chunk carries
        // the tool name, open the block explicitly so `ToolInputStart` is still
        // emitted; otherwise there is nothing to do for this fragment.
        if fragment.is_empty() {
            return match name {
                Some(name) => self.tools.start(&id, name),
                None => Vec::new(),
            };
        }

        self.tools.delta(&id, name, fragment)
    }
}

/// Map a Chat Completions `finish_reason` string onto a [`FinishReason`].
fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolUse,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    }
}

/// Parse a Chat Completions `usage` object into [`Usage`].
fn parse_usage(usage: &Value) -> Usage {
    let u = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens: u("prompt_tokens"),
        cached_input_tokens: cached,
        output_tokens: u("completion_tokens"),
        reasoning_output_tokens: reasoning,
        total_tokens: u("total_tokens"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{LlmRequest, SystemPart, ToolDefinition};

    fn frame(data: &str) -> SseFrame {
        SseFrame {
            event: None,
            data: data.to_string(),
        }
    }

    #[test]
    fn build_body_system_user_tool() {
        let mut req = LlmRequest::new("gpt-4o", "openai");
        req.system.push(SystemPart::new("be helpful"));
        req.messages
            .push(Message::user_text("what is the weather?"));
        req.messages.push(Message::new(
            MessageRole::Assistant,
            vec![ContentPart::ToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: json!({ "city": "Paris" }),
                provider_metadata: None,
            }],
        ));
        req.messages.push(Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: "call_1".into(),
                content: vec![ContentPart::text("sunny")],
                is_error: false,
            }],
        ));
        req.tools.push(ToolDefinition {
            name: "get_weather".into(),
            description: "Look up the weather".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "city": { "type": "string" } }
            }),
        });
        req.tool_choice = Some(ToolChoice::Auto);
        // 0.5 is exactly representable in both f32 and f64, so the widening that
        // happens when serde lowers the f32 into a JSON number is lossless.
        req.generation.temperature = Some(0.5);
        req.generation.max_tokens = Some(256);

        let body = OpenAiChatProtocol::new().build_body(&req).unwrap();

        let expected = json!({
            "model": "gpt-4o",
            "messages": [
                { "role": "system", "content": "be helpful" },
                { "role": "user", "content": "what is the weather?" },
                {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{\"city\":\"Paris\"}"
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "content": "sunny"
                }
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "description": "Look up the weather",
                        "parameters": {
                            "type": "object",
                            "properties": { "city": { "type": "string" } }
                        }
                    }
                }
            ],
            "tool_choice": "auto",
            "temperature": 0.5,
            "max_tokens": 256,
            "stream": true,
            "stream_options": { "include_usage": true }
        });

        assert_eq!(body, expected);
    }

    #[test]
    fn build_body_specific_tool_choice_and_no_options() {
        let mut req = LlmRequest::new("llama3", "ollama");
        req.messages.push(Message::user_text("hi"));
        req.tool_choice = Some(ToolChoice::Tool { name: "go".into() });

        let body = OpenAiChatProtocol::new().build_body(&req).unwrap();
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "function", "function": { "name": "go" } })
        );
        // No system message when `system` is empty.
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        // No tools, no sampling knobs: those keys must be absent.
        assert!(body.get("tools").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn decoder_text_then_tool_call_with_usage() {
        let mut stream = OpenAiChatProtocol::new().decoder();
        let mut events = Vec::new();

        // Streamed text answer across two deltas.
        events.extend(
            stream
                .on_frame(&frame(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#))
                .unwrap(),
        );
        events.extend(
            stream
                .on_frame(&frame(r#"{"choices":[{"delta":{"content":"lo"}}]}"#))
                .unwrap(),
        );

        // Tool call: id + name on the first chunk, arguments in fragments.
        events.extend(
            stream
                .on_frame(&frame(
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_42","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}"#,
                ))
                .unwrap(),
        );
        events.extend(
            stream
                .on_frame(&frame(
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}]}"#,
                ))
                .unwrap(),
        );
        events.extend(
            stream
                .on_frame(&frame(
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]}}]}"#,
                ))
                .unwrap(),
        );

        // Terminal chunk: finish_reason.
        events.extend(
            stream
                .on_frame(&frame(
                    r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                ))
                .unwrap(),
        );
        // Usage chunk (from include_usage), choices empty.
        events.extend(
            stream
                .on_frame(&frame(
                    r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
                ))
                .unwrap(),
        );
        events.extend(stream.on_frame(&frame("[DONE]")).unwrap());
        events.extend(stream.finish().unwrap());

        let usage = Usage {
            input_tokens: 10,
            cached_input_tokens: 0,
            output_tokens: 5,
            reasoning_output_tokens: 0,
            total_tokens: 15,
        };
        let expected = vec![
            LlmEvent::StepStart,
            LlmEvent::TextStart { id: TEXT_ID.into() },
            LlmEvent::TextDelta {
                id: TEXT_ID.into(),
                delta: "Hel".into(),
            },
            LlmEvent::TextDelta {
                id: TEXT_ID.into(),
                delta: "lo".into(),
            },
            LlmEvent::ToolInputStart {
                id: "call_42".into(),
                name: "get_weather".into(),
            },
            LlmEvent::ToolInputDelta {
                id: "call_42".into(),
                delta: "{\"city\":".into(),
            },
            LlmEvent::ToolInputDelta {
                id: "call_42".into(),
                delta: "\"Paris\"}".into(),
            },
            LlmEvent::TextEnd { id: TEXT_ID.into() },
            LlmEvent::ToolInputEnd {
                id: "call_42".into(),
            },
            LlmEvent::ToolCall {
                id: "call_42".into(),
                name: "get_weather".into(),
                input: json!({ "city": "Paris" }),
            },
            LlmEvent::StepFinish {
                usage,
                finish_reason: Some(FinishReason::ToolUse),
            },
            LlmEvent::Finish {
                usage,
                finish_reason: Some(FinishReason::ToolUse),
            },
        ];

        assert_eq!(events, expected);
    }

    #[test]
    fn decoder_plain_text_finish_computes_total() {
        let mut stream = OpenAiChatProtocol::new().decoder();
        let mut events = Vec::new();
        events.extend(
            stream
                .on_frame(&frame(r#"{"choices":[{"delta":{"content":"hi"}}]}"#))
                .unwrap(),
        );
        events.extend(
            stream
                .on_frame(&frame(
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
                ))
                .unwrap(),
        );
        events.extend(stream.finish().unwrap());

        let usage = Usage {
            input_tokens: 3,
            cached_input_tokens: 0,
            output_tokens: 1,
            reasoning_output_tokens: 0,
            total_tokens: 4, // computed: 3 + 1
        };
        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart,
                LlmEvent::TextStart { id: TEXT_ID.into() },
                LlmEvent::TextDelta {
                    id: TEXT_ID.into(),
                    delta: "hi".into(),
                },
                LlmEvent::TextEnd { id: TEXT_ID.into() },
                LlmEvent::StepFinish {
                    usage,
                    finish_reason: Some(FinishReason::Stop),
                },
                LlmEvent::Finish {
                    usage,
                    finish_reason: Some(FinishReason::Stop),
                },
            ]
        );
    }

    #[test]
    fn finish_is_idempotent() {
        let mut stream = OpenAiChatProtocol::new().decoder();
        stream
            .on_frame(&frame(r#"{"choices":[{"delta":{"content":"hi"}}]}"#))
            .unwrap();
        assert!(!stream.finish().unwrap().is_empty());
        assert!(stream.finish().unwrap().is_empty());
    }
}
