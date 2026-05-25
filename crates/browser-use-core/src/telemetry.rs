use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context as AnyhowContext, Result};
use browser_use_protocol::{ModelEvent, ModelUsage, ToolCall, ToolSpec};
use browser_use_store::Store;
use opentelemetry::trace::{SpanKind, Status, TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{Array, Context, KeyValue, StringValue, Value as OtelValue};
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, SdkTracer, SdkTracerProvider,
};
use opentelemetry_sdk::Resource;
use serde_json::Value;

const DEFAULT_LAMINAR_HTTP_ENDPOINT: &str = "https://api.lmnr.ai/v1/traces";
const DEFAULT_MAX_ATTR_CHARS: usize = 16_000;
const DEFAULT_MAX_PROMPT_ATTRS: usize = 24;
const DEFAULT_BATCH_DELAY_MS: u64 = 1_000;
const DEFAULT_BATCH_QUEUE_SIZE: usize = 2_048;
const DEFAULT_BATCH_EXPORT_SIZE: usize = 128;
const SPAN_INPUT: &str = "lmnr.span.input";
const SPAN_OUTPUT: &str = "lmnr.span.output";
const SPAN_TYPE: &str = "lmnr.span.type";
const SPAN_PATH: &str = "lmnr.span.path";
const SPAN_IDS_PATH: &str = "lmnr.span.ids_path";
const SPAN_INSTRUMENTATION_SOURCE: &str = "lmnr.span.instrumentation_source";
const SPAN_SDK_VERSION: &str = "lmnr.span.sdk_version";
const SESSION_ID: &str = "lmnr.association.properties.session_id";
const LAMINAR_API_KEY_SETTING: &str = "telemetry.laminar.api_key";
const LAMINAR_ENDPOINT_SETTING: &str = "telemetry.laminar.endpoint";

#[derive(Clone)]
pub(crate) struct AgentTelemetry {
    inner: Option<Arc<TelemetryInner>>,
}

struct TelemetryInner {
    endpoint: String,
    tracer_provider: SdkTracerProvider,
    tracer: SdkTracer,
    capture_payloads: bool,
    flush_on_finish: bool,
    max_attr_chars: usize,
    max_prompt_attrs: usize,
}

pub(crate) struct ActiveSpan {
    cx: Option<Context>,
    path: Vec<String>,
    ids_path: Vec<String>,
}

pub(crate) struct ModelTurnSpanInput<'a> {
    pub(crate) parent: &'a ActiveSpan,
    pub(crate) session_id: &'a str,
    pub(crate) turn_idx: usize,
    pub(crate) provider_name: &'a str,
    pub(crate) model_name: &'a str,
    pub(crate) messages: &'a [Value],
    pub(crate) tools: &'a [ToolSpec],
}

impl AgentTelemetry {
    pub(crate) fn from_store(store: &Store) -> Result<Self> {
        Self::from_config(
            stored_setting(store, LAMINAR_API_KEY_SETTING)?
                .or_else(|| env_value("LMNR_PROJECT_API_KEY")),
            stored_setting(store, LAMINAR_ENDPOINT_SETTING)?,
        )
    }

    pub(crate) fn disabled() -> Self {
        Self { inner: None }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub(crate) fn endpoint(&self) -> Option<&str> {
        self.inner.as_ref().map(|inner| inner.endpoint.as_str())
    }

    pub(crate) fn start_agent_span(
        &self,
        session_id: &str,
        parent_session_id: Option<&str>,
        cwd: &str,
        task_text: Option<&str>,
    ) -> ActiveSpan {
        let Some(inner) = &self.inner else {
            return ActiveSpan::disabled();
        };
        let mut attrs = vec![
            KeyValue::new(SPAN_TYPE, "DEFAULT"),
            KeyValue::new(SESSION_ID, session_id.to_string()),
            KeyValue::new("browser_use.session_id", session_id.to_string()),
            KeyValue::new("browser_use.cwd", cwd.to_string()),
        ];
        if let Some(parent_session_id) = parent_session_id {
            attrs.push(KeyValue::new(
                "browser_use.parent_session_id",
                parent_session_id.to_string(),
            ));
        }
        if let Some(task_text) = task_text {
            attrs.push(KeyValue::new(
                "browser_use.task",
                truncate_chars(task_text, inner.max_attr_chars),
            ));
            if inner.capture_payloads {
                attrs.push(KeyValue::new(
                    SPAN_INPUT,
                    truncate_chars(task_text, inner.max_attr_chars),
                ));
            }
        }
        self.start_span(None, "browser_use.agent".to_string(), attrs)
    }

    pub(crate) fn start_model_turn_span(&self, input: ModelTurnSpanInput<'_>) -> ActiveSpan {
        let Some(inner) = &self.inner else {
            return ActiveSpan::disabled();
        };
        let span_name = llm_span_name(input.provider_name);
        let mut attrs = vec![
            KeyValue::new(SPAN_TYPE, "LLM"),
            KeyValue::new(SESSION_ID, input.session_id.to_string()),
            KeyValue::new("gen_ai.system", input.provider_name.to_string()),
            KeyValue::new("gen_ai.request.model", input.model_name.to_string()),
            KeyValue::new("gen_ai.response.model", input.model_name.to_string()),
            KeyValue::new("browser_use.turn_index", input.turn_idx as i64),
        ];
        if inner.capture_payloads {
            let input_messages =
                compact_json_value(&Value::Array(input.messages.to_vec()), inner.max_attr_chars);
            attrs.push(KeyValue::new(SPAN_INPUT, input_messages.clone()));
            attrs.push(KeyValue::new("gen_ai.input.messages", input_messages));
            attrs.push(KeyValue::new(
                "gen_ai.request.tools",
                compact_json_value(
                    &Value::Array(
                        input
                            .tools
                            .iter()
                            .map(|tool| {
                                serde_json::json!({
                                    "name": tool.name,
                                    "description": tool.description,
                                    "input_schema": tool.input_schema,
                                })
                            })
                            .collect(),
                    ),
                    inner.max_attr_chars,
                ),
            ));
            for (idx, message) in input
                .messages
                .iter()
                .take(inner.max_prompt_attrs)
                .enumerate()
            {
                attrs.push(KeyValue::new(
                    format!("gen_ai.prompt.{idx}.role"),
                    message_role(message).to_string(),
                ));
                attrs.push(KeyValue::new(
                    format!("gen_ai.prompt.{idx}.content"),
                    message_content_attribute(message, inner.max_attr_chars),
                ));
            }
            if input.messages.len() > inner.max_prompt_attrs {
                attrs.push(KeyValue::new(
                    "browser_use.llm.prompt_attrs_truncated",
                    (input.messages.len() - inner.max_prompt_attrs) as i64,
                ));
            }
            for (idx, tool) in input.tools.iter().enumerate() {
                attrs.push(KeyValue::new(
                    format!("llm.request.functions.{idx}.name"),
                    tool.name.clone(),
                ));
                attrs.push(KeyValue::new(
                    format!("llm.request.functions.{idx}.description"),
                    truncate_chars(&tool.description, inner.max_attr_chars),
                ));
                attrs.push(KeyValue::new(
                    format!("llm.request.functions.{idx}.parameters"),
                    compact_json_value(&tool.input_schema, inner.max_attr_chars),
                ));
            }
        }
        self.start_span(Some(input.parent), span_name.to_string(), attrs)
    }

    pub(crate) fn start_step_span(
        &self,
        parent: &ActiveSpan,
        session_id: &str,
        turn_idx: usize,
        max_turns: usize,
    ) -> ActiveSpan {
        let Some(inner) = &self.inner else {
            return ActiveSpan::disabled();
        };
        let step = turn_idx + 1;
        let mut attrs = vec![
            KeyValue::new(SPAN_TYPE, "DEFAULT"),
            KeyValue::new(SESSION_ID, session_id.to_string()),
            KeyValue::new("browser_use.turn_index", turn_idx as i64),
        ];
        if inner.capture_payloads {
            attrs.push(KeyValue::new(
                SPAN_INPUT,
                compact_json_value(
                    &serde_json::json!({
                        "step": step,
                        "max_steps": max_turns,
                    }),
                    inner.max_attr_chars,
                ),
            ));
        }
        self.start_span(Some(parent), format!("step.{step}"), attrs)
    }

    pub(crate) fn start_tool_span(
        &self,
        parent: &ActiveSpan,
        session_id: &str,
        turn_idx: usize,
        call: &ToolCall,
    ) -> ActiveSpan {
        let Some(inner) = &self.inner else {
            return ActiveSpan::disabled();
        };
        let mut attrs = vec![
            KeyValue::new(SPAN_TYPE, "TOOL"),
            KeyValue::new(SESSION_ID, session_id.to_string()),
            KeyValue::new("browser_use.turn_index", turn_idx as i64),
            KeyValue::new("browser_use.tool_call_id", call.id.clone()),
            KeyValue::new("ai.toolCall.id", call.id.clone()),
            KeyValue::new("ai.toolCall.name", call.name.clone()),
        ];
        if inner.capture_payloads {
            attrs.push(KeyValue::new(
                SPAN_INPUT,
                compact_json_value(
                    &serde_json::json!({
                        "tool": call.name,
                        "arguments": compact_json_value(&call.arguments, inner.max_attr_chars),
                    }),
                    inner.max_attr_chars,
                ),
            ));
        }
        self.start_span(Some(parent), call.name.clone(), attrs)
    }

    fn start_span(
        &self,
        parent: Option<&ActiveSpan>,
        name: String,
        mut attrs: Vec<KeyValue>,
    ) -> ActiveSpan {
        let Some(inner) = &self.inner else {
            return ActiveSpan::disabled();
        };
        let path = parent
            .map(|span| {
                let mut path = span.path.clone();
                path.push(name.clone());
                path
            })
            .unwrap_or_else(|| vec![name.clone()]);
        attrs.push(string_array_attr(SPAN_PATH, &path));
        attrs.push(KeyValue::new(SPAN_INSTRUMENTATION_SOURCE, "rust"));
        attrs.push(KeyValue::new(SPAN_SDK_VERSION, env!("CARGO_PKG_VERSION")));
        let builder = inner
            .tracer
            .span_builder(name)
            .with_kind(SpanKind::Internal)
            .with_attributes(attrs);
        let span = match parent.and_then(|span| span.cx.as_ref()) {
            Some(parent_cx) => builder.start_with_context(&inner.tracer, parent_cx),
            None => builder.start(&inner.tracer),
        };
        let active = ActiveSpan {
            cx: Some(Context::current_with_span(span)),
            path,
            ids_path: Vec::new(),
        };
        active.with_ids_path(parent)
    }

    pub(crate) fn record_model_events(
        &self,
        span: &ActiveSpan,
        provider_name: &str,
        turn_idx: usize,
        events: &[ModelEvent],
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut output_text = String::new();
        let mut tool_calls = Vec::new();
        let mut usage = None;
        for event in events {
            match event {
                ModelEvent::TextDelta { text } => output_text.push_str(text),
                ModelEvent::ThinkingDelta { .. } => {}
                ModelEvent::ToolCall { call } => tool_calls.push(call),
                ModelEvent::CustomToolCallInputDelta { .. } => {}
                ModelEvent::Usage { usage: event_usage } => usage = Some(event_usage),
                ModelEvent::ResponseOutputItem { .. } => {}
                ModelEvent::ResponseCompleted { .. } => {}
                ModelEvent::ServerModel { .. }
                | ModelEvent::ModelRateLimits { .. }
                | ModelEvent::ModelVerifications { .. }
                | ModelEvent::ModelsEtag { .. }
                | ModelEvent::ServerReasoningIncluded { .. } => {}
                ModelEvent::Done => {}
            }
        }
        if let Some(usage) = usage {
            set_usage_attrs(span, usage);
        }
        if inner.capture_payloads {
            let output_payload =
                llm_output_payload(provider_name, turn_idx, &output_text, &tool_calls, inner);
            span.set_attribute(KeyValue::new(
                "gen_ai.output.messages",
                compact_json_value(&output_payload, inner.max_attr_chars),
            ));
            if matches!(provider_name, "openai" | "codex") {
                span.set_attribute(KeyValue::new(
                    "gen_ai.response.id",
                    synthetic_response_id(turn_idx),
                ));
            }
            span.set_attribute(KeyValue::new("gen_ai.completion.0.role", "assistant"));
            span.set_attribute(KeyValue::new(
                "gen_ai.completion.0.finish_reason",
                if tool_calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                },
            ));
            if !output_text.is_empty() {
                span.set_attribute(KeyValue::new(
                    "gen_ai.completion.0.content",
                    truncate_chars(&output_text, inner.max_attr_chars),
                ));
            }
            for (idx, call) in tool_calls.iter().enumerate() {
                span.set_attribute(KeyValue::new(
                    format!("gen_ai.completion.0.message.tool_calls.{idx}.id"),
                    call.id.clone(),
                ));
                span.set_attribute(KeyValue::new(
                    format!("gen_ai.completion.0.message.tool_calls.{idx}.name"),
                    call.name.clone(),
                ));
                span.set_attribute(KeyValue::new(
                    format!("gen_ai.completion.0.message.tool_calls.{idx}.arguments"),
                    compact_json_value(&call.arguments, inner.max_attr_chars),
                ));
            }
        }
        span.set_ok();
    }

    pub(crate) fn record_agent_output(&self, span: &ActiveSpan, output: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner.capture_payloads {
            span.set_attribute(KeyValue::new(
                SPAN_OUTPUT,
                truncate_chars(output, inner.max_attr_chars),
            ));
        }
    }

    pub(crate) fn record_tool_outcome(
        &self,
        span: &ActiveSpan,
        messages: &[Value],
        finished: bool,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner.capture_payloads {
            let output = api_v3_tool_output(messages, finished, inner);
            span.set_attribute(KeyValue::new(
                SPAN_OUTPUT,
                compact_json_value(&output, inner.max_attr_chars),
            ));
        }
    }

    pub(crate) fn force_flush(&self) {
        if let Some(inner) = &self.inner {
            if inner.flush_on_finish {
                let _ = inner.tracer_provider.force_flush();
            }
        }
    }

    fn from_config(
        project_api_key: Option<String>,
        endpoint_override: Option<String>,
    ) -> Result<Self> {
        let Some(project_api_key) = project_api_key else {
            return Ok(Self { inner: None });
        };
        Ok(Self {
            inner: Some(Arc::new(build_telemetry_inner(
                project_api_key,
                endpoint_override,
            )?)),
        })
    }
}

fn build_telemetry_inner(
    project_api_key: String,
    endpoint_override: Option<String>,
) -> Result<TelemetryInner> {
    if project_api_key.trim().is_empty() {
        bail!("Laminar API key is empty");
    }
    let endpoint = endpoint_override
        .or_else(|| env_value("LLM_BROWSER_LAMINAR_OTLP_ENDPOINT"))
        .or_else(|| env_value("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"))
        .or_else(|| env_value("OTEL_EXPORTER_OTLP_ENDPOINT").map(trace_endpoint_from_base))
        .unwrap_or_else(|| DEFAULT_LAMINAR_HTTP_ENDPOINT.to_string());
    let timeout_seconds = env_value("LLM_BROWSER_LAMINAR_TIMEOUT_SECONDS")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(5);
    let max_attr_chars = env_value("LLM_BROWSER_LAMINAR_MAX_ATTR_CHARS")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_ATTR_CHARS)
        .max(256);
    let max_prompt_attrs = env_value("LLM_BROWSER_LAMINAR_MAX_PROMPT_ATTRS")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_PROMPT_ATTRS);
    let capture_payloads = env_flag("LLM_BROWSER_LAMINAR_CAPTURE_PAYLOADS", true);
    let flush_on_finish = env_flag("LLM_BROWSER_LAMINAR_FLUSH_ON_FINISH", false);
    let scheduled_delay_ms = env_value("LLM_BROWSER_LAMINAR_BATCH_DELAY_MS")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_BATCH_DELAY_MS);
    let max_queue_size = env_value("LLM_BROWSER_LAMINAR_BATCH_QUEUE_SIZE")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_BATCH_QUEUE_SIZE)
        .max(1);
    let max_export_batch_size = env_value("LLM_BROWSER_LAMINAR_BATCH_EXPORT_SIZE")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_BATCH_EXPORT_SIZE)
        .max(1)
        .min(max_queue_size);

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint.clone())
        .with_timeout(Duration::from_secs(timeout_seconds))
        .with_headers(HashMap::from([(
            "authorization".to_string(),
            format!("Bearer {project_api_key}"),
        )]))
        .build()
        .context("build Laminar OTLP span exporter")?;

    let batch_config = BatchConfigBuilder::default()
        .with_scheduled_delay(Duration::from_millis(scheduled_delay_ms))
        .with_max_queue_size(max_queue_size)
        .with_max_export_batch_size(max_export_batch_size)
        .build();
    let batch_processor = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch_config)
        .build();

    let tracer_provider = SdkTracerProvider::builder()
        .with_span_processor(batch_processor)
        .with_resource(
            Resource::builder()
                .with_service_name("browser-use-terminal")
                .with_attributes([KeyValue::new("service.version", env!("CARGO_PKG_VERSION"))])
                .build(),
        )
        .build();
    let tracer = tracer_provider.tracer("browser-use-agent");

    Ok(TelemetryInner {
        endpoint,
        tracer_provider,
        tracer,
        capture_payloads,
        flush_on_finish,
        max_attr_chars,
        max_prompt_attrs,
    })
}

fn stored_setting(store: &Store, key: &str) -> Result<Option<String>> {
    Ok(store
        .get_setting(key)?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

impl ActiveSpan {
    fn disabled() -> Self {
        Self {
            cx: None,
            path: Vec::new(),
            ids_path: Vec::new(),
        }
    }

    fn with_ids_path(mut self, parent: Option<&ActiveSpan>) -> Self {
        let Some(cx) = self.cx.as_ref() else {
            return self;
        };
        let span_context = cx.span().span_context().clone();
        if !span_context.is_valid() {
            return self;
        }
        let mut ids_path = parent.map(|span| span.ids_path.clone()).unwrap_or_default();
        ids_path.push(otel_span_id_to_uuid(&span_context.span_id().to_string()));
        self.set_attribute(string_array_attr(SPAN_IDS_PATH, &ids_path));
        self.ids_path = ids_path;
        self
    }

    pub(crate) fn trace_id(&self) -> Option<String> {
        let cx = self.cx.as_ref()?;
        let span_context = cx.span().span_context().clone();
        span_context
            .is_valid()
            .then(|| span_context.trace_id().to_string())
    }

    pub(crate) fn set_attribute(&self, attribute: KeyValue) {
        if let Some(cx) = &self.cx {
            cx.span().set_attribute(attribute);
        }
    }

    pub(crate) fn record_error(&self, error: &dyn std::error::Error) {
        if let Some(cx) = &self.cx {
            cx.span().record_error(error);
            cx.span().set_status(Status::error(error.to_string()));
        }
    }

    pub(crate) fn set_ok(&self) {
        if let Some(cx) = &self.cx {
            cx.span().set_status(Status::Ok);
        }
    }
}

impl Drop for ActiveSpan {
    fn drop(&mut self) {
        if let Some(cx) = &self.cx {
            cx.span().end();
        }
    }
}

fn set_usage_attrs(span: &ActiveSpan, usage: &ModelUsage) {
    if let Some(input_tokens) = usage.input_tokens {
        span.set_attribute(KeyValue::new("gen_ai.usage.input_tokens", input_tokens));
    }
    if let Some(input_cached_tokens) = usage.input_cached_tokens {
        span.set_attribute(KeyValue::new(
            "gen_ai.usage.input_cached_tokens",
            input_cached_tokens,
        ));
    }
    if let Some(input_cache_creation_tokens) = usage.input_cache_creation_tokens {
        span.set_attribute(KeyValue::new(
            "gen_ai.usage.input_cache_creation_tokens",
            input_cache_creation_tokens,
        ));
    }
    if let Some(output_tokens) = usage.output_tokens {
        span.set_attribute(KeyValue::new("gen_ai.usage.output_tokens", output_tokens));
    }
    if let Some(reasoning_output_tokens) = usage.reasoning_output_tokens {
        span.set_attribute(KeyValue::new(
            "gen_ai.usage.reasoning_output_tokens",
            reasoning_output_tokens,
        ));
    }
    if let Some(total_tokens) = usage.total_tokens {
        span.set_attribute(KeyValue::new("llm.usage.total_tokens", total_tokens));
    }
    if let Some(cost_usd) = usage.cost_usd {
        span.set_attribute(KeyValue::new("gen_ai.usage.cost", cost_usd));
    }
    if let Some(input_cost_usd) = usage.input_cost_usd {
        span.set_attribute(KeyValue::new("gen_ai.usage.input_cost", input_cost_usd));
    }
    if let Some(output_cost_usd) = usage.output_cost_usd {
        span.set_attribute(KeyValue::new("gen_ai.usage.output_cost", output_cost_usd));
    }
}

fn llm_span_name(provider_name: &str) -> &'static str {
    match provider_name {
        "anthropic" => "anthropic.messages",
        "openai-compatible" => "openai.chat",
        "openai" | "codex" => "openai.responses",
        "fake" | "scripted" => "llm.generate",
        _ => "llm.generate",
    }
}

fn api_v3_tool_output(messages: &[Value], finished: bool, inner: &TelemetryInner) -> Value {
    let output = tool_output_text(messages);
    let output_char_count = output.chars().count();
    let truncated_output = truncate_chars(&output, 4_000);
    let mut payload = serde_json::Map::new();
    if finished {
        payload.insert("task_complete".to_string(), Value::Bool(true));
        payload.insert("message".to_string(), Value::String(truncated_output));
    } else if looks_like_tool_error(&output) {
        payload.insert("error".to_string(), Value::String(truncated_output));
    } else {
        payload.insert("result".to_string(), Value::String(truncated_output));
    }
    if output_char_count > 4_000 {
        payload.insert("log_truncated".to_string(), Value::Bool(true));
        payload.insert(
            "full_length".to_string(),
            Value::Number(serde_json::Number::from(output_char_count)),
        );
    }
    scrub_value(&Value::Object(payload), inner.max_attr_chars)
}

fn tool_output_text(messages: &[Value]) -> String {
    messages
        .iter()
        .map(message_content_text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn looks_like_tool_error(output: &str) -> bool {
    output.starts_with("Error:")
        || output.starts_with("error:")
        || output.starts_with("unknown tool:")
        || output.starts_with("timed out")
}

fn string_array_attr(key: &'static str, values: &[String]) -> KeyValue {
    KeyValue::new(
        key,
        OtelValue::Array(Array::String(
            values
                .iter()
                .cloned()
                .map(StringValue::from)
                .collect::<Vec<_>>(),
        )),
    )
}

fn otel_span_id_to_uuid(span_id: &str) -> String {
    let id = span_id.trim_start_matches("0x").to_ascii_lowercase();
    let padded = if id.len() >= 32 {
        id[id.len() - 32..].to_string()
    } else {
        format!("{id:0>32}")
    };
    format!(
        "{}-{}-{}-{}-{}",
        &padded[0..8],
        &padded[8..12],
        &padded[12..16],
        &padded[16..20],
        &padded[20..32]
    )
}

fn synthetic_response_id(turn_idx: usize) -> String {
    format!("browser_use_turn_{turn_idx}")
}

fn llm_output_payload(
    provider_name: &str,
    turn_idx: usize,
    output_text: &str,
    tool_calls: &[&ToolCall],
    inner: &TelemetryInner,
) -> Value {
    match provider_name {
        "openai" | "codex" => {
            openai_responses_output_payload(turn_idx, output_text, tool_calls, inner)
        }
        "anthropic" => anthropic_output_payload(output_text, tool_calls, inner),
        _ => Value::Array(vec![chat_output_message(output_text, tool_calls, inner)]),
    }
}

fn openai_responses_output_payload(
    turn_idx: usize,
    output_text: &str,
    tool_calls: &[&ToolCall],
    inner: &TelemetryInner,
) -> Value {
    let mut output = Vec::new();
    if !output_text.is_empty() {
        output.push(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [
                {
                    "type": "output_text",
                    "text": output_text,
                },
            ],
        }));
    }
    output.extend(tool_calls.iter().map(|call| {
        serde_json::json!({
            "type": "function_call",
            "call_id": call.id.clone(),
            "name": call.name.clone(),
            "arguments": compact_json_value(&call.arguments, inner.max_attr_chars),
        })
    }));
    serde_json::json!({
        "id": synthetic_response_id(turn_idx),
        "object": "response",
        "output": output,
    })
}

fn anthropic_output_payload(
    output_text: &str,
    tool_calls: &[&ToolCall],
    inner: &TelemetryInner,
) -> Value {
    let mut content = Vec::new();
    if !output_text.is_empty() {
        content.push(serde_json::json!({
            "type": "text",
            "text": output_text,
        }));
    }
    content.extend(tool_calls.iter().map(|call| {
        serde_json::json!({
            "type": "tool_use",
            "id": call.id.clone(),
            "name": call.name.clone(),
            "input": scrub_value(&call.arguments, inner.max_attr_chars),
        })
    }));
    Value::Array(vec![serde_json::json!({
        "role": "assistant",
        "content": content,
    })])
}

fn chat_output_message(
    output_text: &str,
    tool_calls: &[&ToolCall],
    inner: &TelemetryInner,
) -> Value {
    let tool_calls = tool_calls
        .iter()
        .map(|call| {
            serde_json::json!({
                "id": call.id.clone(),
                "type": "function",
                "function": {
                    "name": call.name.clone(),
                    "arguments": compact_json_value(&call.arguments, inner.max_attr_chars),
                },
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "role": "assistant",
        "content": output_text,
        "tool_calls": tool_calls,
    })
}

fn env_value(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_flag(name: &str, default: bool) -> bool {
    match env_value(name).as_deref() {
        Some("0") | Some("false") | Some("False") | Some("FALSE") | Some("no") | Some("NO") => {
            false
        }
        Some("1") | Some("true") | Some("True") | Some("TRUE") | Some("yes") | Some("YES") => true,
        Some(_) => default,
        None => default,
    }
}

fn trace_endpoint_from_base(endpoint: String) -> String {
    if endpoint.ends_with("/v1/traces") {
        endpoint
    } else {
        format!("{}/v1/traces", endpoint.trim_end_matches('/'))
    }
}

fn message_role(message: &Value) -> &str {
    message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user")
}

fn message_content_attribute(message: &Value, max_chars: usize) -> String {
    match message.get("content") {
        Some(content) => compact_json_value(content, max_chars),
        None => compact_json_value(message, max_chars),
    }
}

fn message_content_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn compact_json_value(value: &Value, max_chars: usize) -> String {
    let scrubbed = scrub_value(value, max_chars);
    let serialized = serde_json::to_string(&scrubbed).unwrap_or_else(|_| "<unserializable>".into());
    truncate_chars(&serialized, max_chars)
}

fn scrub_value(value: &Value, max_chars: usize) -> Value {
    match value {
        Value::String(text) => {
            if looks_like_data_url(text) {
                Value::String(format!(
                    "[omitted data URL, chars={}]",
                    text.chars().count()
                ))
            } else {
                Value::String(truncate_chars(text, max_chars))
            }
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| scrub_value(item, max_chars))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), scrub_value(value, max_chars)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn looks_like_data_url(text: &str) -> bool {
    text.starts_with("data:image/")
        || text.starts_with("data:application/")
        || text.starts_with("data:video/")
        || text.starts_with("data:audio/")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(32).max(16);
    let mut out = text.chars().take(keep).collect::<String>();
    out.push_str("\n[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{SpanData, SpanExporter};
    use std::sync::Mutex;

    #[derive(Clone, Debug)]
    struct RecordingExporter {
        spans: Arc<Mutex<Vec<SpanData>>>,
    }

    impl RecordingExporter {
        fn new() -> Self {
            Self {
                spans: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn spans(&self) -> Vec<SpanData> {
            self.spans.lock().unwrap().clone()
        }
    }

    impl SpanExporter for RecordingExporter {
        fn export(
            &self,
            batch: Vec<SpanData>,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            let spans = self.spans.clone();
            async move {
                spans.lock().unwrap().extend(batch);
                Ok(())
            }
        }
    }

    #[test]
    fn emits_laminar_typescript_style_llm_and_tool_spans() {
        let exporter = RecordingExporter::new();
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let telemetry = AgentTelemetry {
            inner: Some(Arc::new(TelemetryInner {
                endpoint: "memory".to_string(),
                tracer: tracer_provider.tracer("test"),
                tracer_provider,
                capture_payloads: true,
                flush_on_finish: true,
                max_attr_chars: DEFAULT_MAX_ATTR_CHARS,
                max_prompt_attrs: DEFAULT_MAX_PROMPT_ATTRS,
            })),
        };

        let agent = telemetry.start_agent_span("session-1", None, "/tmp", Some("do it"));
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": "call the tool",
        })];
        let tools = vec![ToolSpec {
            name: "python".to_string(),
            namespace: None,
            namespace_description: None,
            description: "Run Python".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            freeform: None,
        }];
        let step = telemetry.start_step_span(&agent, "session-1", 0, 80);
        let llm = telemetry.start_model_turn_span(ModelTurnSpanInput {
            parent: &step,
            session_id: "session-1",
            turn_idx: 0,
            provider_name: "openai-compatible",
            model_name: "gpt-test",
            messages: &messages,
            tools: &tools,
        });
        telemetry.record_model_events(
            &llm,
            "openai-compatible",
            0,
            &[
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "call_1".to_string(),
                        name: "python".to_string(),
                        namespace: None,
                        arguments: serde_json::json!({"code": "print(1)"}),
                    },
                },
                ModelEvent::Usage {
                    usage: ModelUsage {
                        input_tokens: Some(10),
                        output_tokens: Some(2),
                        total_tokens: Some(12),
                        cost_usd: None,
                        ..Default::default()
                    },
                },
                ModelEvent::Done,
            ],
        );
        drop(llm);

        let responses = telemetry.start_model_turn_span(ModelTurnSpanInput {
            parent: &step,
            session_id: "session-1",
            turn_idx: 1,
            provider_name: "openai",
            model_name: "gpt-5.5",
            messages: &messages,
            tools: &tools,
        });
        telemetry.record_model_events(
            &responses,
            "openai",
            1,
            &[
                ModelEvent::ToolCall {
                    call: ToolCall {
                        id: "call_response_1".to_string(),
                        name: "python".to_string(),
                        namespace: None,
                        arguments: serde_json::json!({"code": "print(2)"}),
                    },
                },
                ModelEvent::Done,
            ],
        );
        drop(responses);

        let tool = telemetry.start_tool_span(
            &step,
            "session-1",
            0,
            &ToolCall {
                id: "call_1".to_string(),
                name: "python".to_string(),
                namespace: None,
                arguments: serde_json::json!({"code": "print(1)"}),
            },
        );
        telemetry.record_tool_outcome(
            &tool,
            &[serde_json::json!({
                "role": "tool",
                "tool_call_id": "call_1",
                "content": "1",
            })],
            false,
        );
        drop(tool);
        drop(step);
        drop(agent);
        telemetry.force_flush();

        let spans = exporter.spans();
        let step = spans
            .iter()
            .find(|span| span.name.as_ref() == "step.1")
            .expect("step span");
        assert_eq!(attr_string(step, SPAN_TYPE).as_deref(), Some("DEFAULT"));
        assert_eq!(
            attr_string_array(step, SPAN_PATH),
            vec!["browser_use.agent", "step.1"]
        );
        assert!(attr_string(step, SPAN_INPUT)
            .as_deref()
            .is_some_and(|value| value.contains("\"max_steps\":80")));

        let llm = spans
            .iter()
            .find(|span| span.name.as_ref() == "openai.chat")
            .expect("openai chat span");
        assert_eq!(attr_string(llm, SPAN_TYPE).as_deref(), Some("LLM"));
        assert_eq!(
            attr_string_array(llm, SPAN_PATH),
            vec!["browser_use.agent", "step.1", "openai.chat"]
        );
        assert_eq!(
            attr_string(llm, "gen_ai.request.model").as_deref(),
            Some("gpt-test")
        );
        assert_eq!(attr_i64(llm, "gen_ai.usage.input_tokens"), Some(10));
        assert!(attr_string(llm, SPAN_INPUT)
            .as_deref()
            .is_some_and(|value| value.contains("call the tool")));
        assert!(attr_string(llm, "gen_ai.input.messages").is_some());
        assert_eq!(
            attr_string(llm, "llm.request.functions.0.name").as_deref(),
            Some("python")
        );
        assert_eq!(
            attr_string(llm, "gen_ai.completion.0.message.tool_calls.0.name").as_deref(),
            Some("python")
        );
        assert!(attr_string(llm, SPAN_OUTPUT).is_none());

        let responses = spans
            .iter()
            .find(|span| span.name.as_ref() == "openai.responses")
            .expect("openai responses span");
        assert_eq!(attr_string(responses, SPAN_TYPE).as_deref(), Some("LLM"));
        assert_eq!(
            attr_string_array(responses, SPAN_PATH),
            vec!["browser_use.agent", "step.1", "openai.responses"]
        );
        assert_eq!(
            attr_string(responses, "gen_ai.response.id").as_deref(),
            Some("browser_use_turn_1")
        );
        let output: Value = serde_json::from_str(
            &attr_string(responses, "gen_ai.output.messages").expect("responses output"),
        )
        .expect("valid responses output json");
        assert_eq!(output["object"], "response");
        assert_eq!(output["output"][0]["type"], "function_call");
        assert_eq!(output["output"][0]["call_id"], "call_response_1");
        assert_eq!(output["output"][0]["name"], "python");
        assert!(output["output"][0]["arguments"]
            .as_str()
            .is_some_and(|value| value.contains("print(2)")));
        assert!(attr_string(responses, SPAN_OUTPUT).is_none());

        let tool = spans
            .iter()
            .find(|span| span.name.as_ref() == "python")
            .expect("python tool span");
        assert_eq!(attr_string(tool, SPAN_TYPE).as_deref(), Some("TOOL"));
        assert_eq!(
            attr_string_array(tool, SPAN_PATH),
            vec!["browser_use.agent", "step.1", "python"]
        );
        assert!(attr_string(tool, SPAN_INPUT)
            .as_deref()
            .is_some_and(
                |value| value.contains("\"tool\":\"python\"") && value.contains("print(1)")
            ));
        assert!(attr_string(tool, SPAN_OUTPUT)
            .as_deref()
            .is_some_and(|value| value.contains("\"result\":\"1\"")));
    }

    #[test]
    fn from_store_uses_laminar_settings() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(temp.path())?;
        store.set_setting(LAMINAR_API_KEY_SETTING, "lmnr_test_key")?;
        store.set_setting(LAMINAR_ENDPOINT_SETTING, "http://127.0.0.1:9/v1/traces")?;

        let telemetry = AgentTelemetry::from_store(&store)?;

        assert!(telemetry.is_enabled());
        assert_eq!(telemetry.endpoint(), Some("http://127.0.0.1:9/v1/traces"));
        Ok(())
    }

    fn attr_string(span: &SpanData, key: &str) -> Option<String> {
        span.attributes
            .iter()
            .find(|attr| attr.key.as_str() == key)
            .map(|attr| attr.value.as_str().into_owned())
    }

    fn attr_i64(span: &SpanData, key: &str) -> Option<i64> {
        span.attributes
            .iter()
            .find(|attr| attr.key.as_str() == key)
            .and_then(|attr| match &attr.value {
                OtelValue::I64(value) => Some(*value),
                _ => None,
            })
    }

    fn attr_string_array(span: &SpanData, key: &str) -> Vec<String> {
        span.attributes
            .iter()
            .find(|attr| attr.key.as_str() == key)
            .and_then(|attr| match &attr.value {
                OtelValue::Array(Array::String(values)) => Some(
                    values
                        .iter()
                        .map(|value| value.as_str().to_string())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .unwrap_or_default()
    }
}
