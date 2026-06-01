//! Define-once tools: name + description + JSON-Schema + a handler closure.
//!
//! A [`Tool`] couples the *schema* the model sees (rendered into a
//! [`ToolDefinition`] on the request) with the *handler* that runs when the
//! model calls it — so a tool is defined exactly once and never drifts between
//! "what the model was told" and "what actually executes".
//!
//! Handlers return `Result<ToolResult, ToolFailure>`:
//!
//! - [`ToolResult`] is the normal payload (the content parts fed back to the
//!   model as a successful `tool-result`).
//! - [`ToolFailure`] is a *soft* failure: a tool ran but could not satisfy the
//!   request (bad arguments, not-found, validation error, ...). The runtime
//!   turns it into an **error** `tool-result` (`is_error: true`) so the model
//!   can read the message and self-correct on the next turn. This is distinct
//!   from a hard [`LlmError`](crate::schema::LlmError), which aborts the loop.
//!
//! The handler is a boxed `Fn(serde_json::Value) -> Result<…>`; the input
//! `Value` is the decoded tool-call arguments. Decoding into a typed struct is
//! left to the handler (it can `serde_json::from_value` and map a parse error to
//! [`ToolFailure`]).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::schema::{ContentPart, ToolDefinition};

/// The successful output of a tool handler: the content fed back to the model.
///
/// Most tools return a single text block ([`ToolResult::text`]); richer tools
/// can return arbitrary [`ContentPart`]s (e.g. media). The runtime wraps these
/// in a [`ContentPart::ToolResult`] with `is_error: false`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    /// The content parts to surface to the model as the tool's result.
    pub content: Vec<ContentPart>,
}

impl ToolResult {
    /// A result carrying explicit content parts.
    pub fn new(content: Vec<ContentPart>) -> Self {
        Self { content }
    }

    /// A result carrying a single text block (the common case).
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: vec![ContentPart::text(s)],
        }
    }
}

/// A *soft* tool failure: the tool ran but could not satisfy the request.
///
/// Becomes an error `tool-result` (`is_error: true`) so the model sees the
/// message and can retry. Use this for bad arguments, validation errors,
/// not-found, etc. — anything the model could plausibly recover from. Reserve a
/// hard [`LlmError`](crate::schema::LlmError) (which aborts the loop) for
/// transport/infra failures the model cannot fix.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolFailure {
    /// Human-readable explanation, shown to the model in the error result.
    pub message: String,
}

impl ToolFailure {
    /// Construct a soft failure with the given message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Render this failure as the content parts of an error `tool-result`.
    pub fn into_content(self) -> Vec<ContentPart> {
        vec![ContentPart::text(self.message)]
    }
}

impl std::fmt::Display for ToolFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolFailure {}

/// The dispatchable behaviour of a tool: decode-and-run on a JSON input.
///
/// Implemented blanket-style for any `Fn(Value) -> Result<ToolResult,
/// ToolFailure>`, so callers usually pass a closure rather than naming a type.
/// `Send + Sync` so a tool set can be shared across tasks.
pub trait ToolHandler: Send + Sync {
    /// Run the tool with the decoded tool-call arguments.
    fn call(&self, input: Value) -> Result<ToolResult, ToolFailure>;
}

impl<F> ToolHandler for F
where
    F: Fn(Value) -> Result<ToolResult, ToolFailure> + Send + Sync,
{
    fn call(&self, input: Value) -> Result<ToolResult, ToolFailure> {
        (self)(input)
    }
}

/// A tool defined exactly once: the schema the model sees plus the handler that
/// runs when it is called.
///
/// Build with [`Tool::new`] (passing any closure / [`ToolHandler`]). Render the
/// schema half with [`Tool::definition`]; dispatch the behaviour half with
/// [`Tool::invoke`].
pub struct Tool {
    name: String,
    description: String,
    parameters: Value,
    handler: Box<dyn ToolHandler>,
}

impl Tool {
    /// Define a tool from its name, description, JSON-Schema parameters, and a
    /// handler closure.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: impl ToolHandler + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            handler: Box::new(handler),
        }
    }

    /// The tool's name (must match the name the model emits in a tool call).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The tool's human/model-facing description.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The JSON-Schema for the tool's input parameters.
    pub fn parameters(&self) -> &Value {
        &self.parameters
    }

    /// Render the schema half of this tool into a wire [`ToolDefinition`].
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.parameters.clone(),
            output_schema: None,
            namespace: None,
            namespace_description: None,
        }
    }

    /// Dispatch the handler with decoded tool-call arguments.
    pub fn invoke(&self, input: Value) -> Result<ToolResult, ToolFailure> {
        self.handler.call(input)
    }
}

impl std::fmt::Debug for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The handler is an opaque closure; omit it from the debug dump.
        f.debug_struct("Tool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("parameters", &self.parameters)
            .finish_non_exhaustive()
    }
}

/// A named set of [`Tool`]s the model may call, with O(log n) lookup by name.
///
/// Render the whole set into the request's tool list with
/// [`ToolSet::definitions`]; look a tool up by the name from a tool call with
/// [`ToolSet::get`].
#[derive(Default)]
pub struct ToolSet {
    by_name: BTreeMap<String, Tool>,
}

impl ToolSet {
    /// An empty tool set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a tool, keyed by its name. A later tool with the same name
    /// replaces the earlier one (last-wins).
    pub fn insert(&mut self, tool: Tool) -> &mut Self {
        self.by_name.insert(tool.name().to_string(), tool);
        self
    }

    /// Builder-style insert for fluent construction.
    pub fn with(mut self, tool: Tool) -> Self {
        self.insert(tool);
        self
    }

    /// Look up a tool by the name the model emitted.
    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.by_name.get(name)
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// The number of tools in the set.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Render every tool's schema into the [`ToolDefinition`] list for a
    /// request. Order is by name (deterministic).
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.by_name.values().map(Tool::definition).collect()
    }
}

impl FromIterator<Tool> for ToolSet {
    fn from_iter<I: IntoIterator<Item = Tool>>(iter: I) -> Self {
        let mut set = ToolSet::new();
        for tool in iter {
            set.insert(tool);
        }
        set
    }
}

impl std::fmt::Debug for ToolSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolSet")
            .field("tools", &self.by_name.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn add_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "a": { "type": "number" },
                "b": { "type": "number" }
            },
            "required": ["a", "b"]
        })
    }

    fn add_tool() -> Tool {
        Tool::new("add", "Add two numbers", add_schema(), |input: Value| {
            let a = input.get("a").and_then(Value::as_i64);
            let b = input.get("b").and_then(Value::as_i64);
            match (a, b) {
                (Some(a), Some(b)) => Ok(ToolResult::text((a + b).to_string())),
                _ => Err(ToolFailure::new("both `a` and `b` must be integers")),
            }
        })
    }

    #[test]
    fn definition_mirrors_define_once_fields() {
        let def = add_tool().definition();
        assert_eq!(def.name, "add");
        assert_eq!(def.description, "Add two numbers");
        assert_eq!(def.input_schema, add_schema());
    }

    #[test]
    fn invoke_dispatches_handler_with_decoded_input() {
        let result = add_tool().invoke(json!({ "a": 2, "b": 3 })).unwrap();
        assert_eq!(result, ToolResult::text("5"));
    }

    #[test]
    fn invoke_returns_soft_failure_on_bad_input() {
        let err = add_tool().invoke(json!({ "a": "nope" })).unwrap_err();
        assert_eq!(err.message, "both `a` and `b` must be integers");
        // The failure renders to a single text content part for the error result.
        assert_eq!(
            err.into_content(),
            vec![ContentPart::text("both `a` and `b` must be integers")]
        );
    }

    #[test]
    fn tool_set_renders_definitions_sorted_by_name() {
        let set = ToolSet::new()
            .with(Tool::new("zed", "z", json!({}), |_| {
                Ok(ToolResult::text("z"))
            }))
            .with(add_tool());
        let defs = set.definitions();
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["add", "zed"]);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn tool_set_get_and_invoke_round_trips() {
        let set = ToolSet::from_iter([add_tool()]);
        let tool = set.get("add").expect("tool present");
        assert_eq!(
            tool.invoke(json!({ "a": 10, "b": 5 })).unwrap(),
            ToolResult::text("15")
        );
        assert!(set.get("missing").is_none());
    }

    #[test]
    fn last_insert_wins_on_duplicate_name() {
        let set = ToolSet::new()
            .with(Tool::new("dup", "first", json!({}), |_| {
                Ok(ToolResult::text("1"))
            }))
            .with(Tool::new("dup", "second", json!({}), |_| {
                Ok(ToolResult::text("2"))
            }));
        assert_eq!(set.len(), 1);
        assert_eq!(set.get("dup").unwrap().description(), "second");
    }
}
