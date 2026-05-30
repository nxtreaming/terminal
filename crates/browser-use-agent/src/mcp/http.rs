//! Streamable-HTTP MCP transport over `reqwest`.
//!
//! POSTs JSON-RPC to the server URL with `Content-Type: application/json` and an
//! optional `Authorization: Bearer <token>` plus extra headers. Handles BOTH a
//! plain `application/json` response body and a `text/event-stream` (SSE) body
//! whose `data:` lines carry JSON-RPC messages. Mirrors codex's
//! `new_streamable_http_client(url, bearer_token)`
//! (`rmcp-client/src/rmcp_client.rs:340-345`), with the same `initialize` →
//! `tools/list` / `tools/call` sequence. The JSON-RPC framing/result extraction
//! matches the legacy stdio client (`browser-use-core/src/mcp.rs:1022-1040`):
//! match the response id, error on an `error` field, return `result`.
//!
//! The MCP streamable-HTTP binding allows the server to answer a POST with
//! either a single JSON object or an SSE stream; we accept both and pick the
//! JSON-RPC message whose id matches our request.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::Client;
use serde_json::Value;

use crate::mcp::protocol::{
    call_tool_params, initialize_params, initialized_notification, CallToolResult, HeaderMap,
    JsonRpcMessage, JsonRpcRequest, ListToolsResult, McpToolInfo, RequestId,
};

/// A connected streamable-HTTP MCP server.
pub struct StreamableHttpTransport {
    client: Client,
    url: String,
    bearer_token: Option<String>,
    headers: HeaderMap,
    next_id: AtomicI64,
    tool_timeout: Duration,
    /// MCP session id returned by the server in the `initialize` response header
    /// (`Mcp-Session-Id`), echoed on subsequent requests if present.
    session_id: tokio::sync::Mutex<Option<String>>,
}

impl StreamableHttpTransport {
    /// Build the transport and perform the `initialize` handshake.
    pub async fn connect(
        url: &str,
        bearer_token: Option<String>,
        headers: HeaderMap,
        startup_timeout: Duration,
        tool_timeout: Duration,
    ) -> Result<Self> {
        let client = Client::builder()
            .build()
            .context("building reqwest client for MCP http transport")?;

        let transport = Self {
            client,
            url: url.to_string(),
            bearer_token,
            headers,
            next_id: AtomicI64::new(1),
            tool_timeout,
            session_id: tokio::sync::Mutex::new(None),
        };

        transport
            .request("initialize", Some(initialize_params()), startup_timeout)
            .await
            .context("initialize handshake failed")?;
        transport
            .notify(initialized_notification(), startup_timeout)
            .await
            .context("sending notifications/initialized failed")?;

        Ok(transport)
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let result = self.request("tools/list", None, self.tool_timeout).await?;
        let parsed: ListToolsResult = serde_json::from_value(result)?;
        Ok(parsed.tools)
    }

    pub async fn call_tool(&self, tool: &str, arguments: Option<Value>) -> Result<CallToolResult> {
        let params = call_tool_params(tool, arguments);
        let result = self
            .request("tools/call", Some(params), self.tool_timeout)
            .await?;
        let parsed: CallToolResult = serde_json::from_value(result)?;
        Ok(parsed)
    }

    /// Send a notification (no response correlation). A 2xx is success.
    async fn notify(
        &self,
        notification: crate::mcp::protocol::JsonRpcNotification,
        timeout_dur: Duration,
    ) -> Result<()> {
        let body = serde_json::to_value(&notification)?;
        let builder = self.request_builder(&body, timeout_dur).await;
        let resp = builder.send().await.context("sending notification")?;
        if !resp.status().is_success() {
            bail!(
                "notification {} returned HTTP {}",
                notification.method,
                resp.status()
            );
        }
        Ok(())
    }

    /// POST a JSON-RPC request and return its `result` (or surface its error).
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout_dur: Duration,
    ) -> Result<Value> {
        let id = RequestId::from_i64(self.next_id.fetch_add(1, Ordering::SeqCst));
        let req = JsonRpcRequest::new(id.clone(), method, params);
        let body = serde_json::to_value(&req)?;

        let builder = self.request_builder(&body, timeout_dur).await;
        let resp = builder
            .send()
            .await
            .with_context(|| format!("POST {method} to {}", self.url))?;

        let status = resp.status();
        // Capture an MCP session id from the response headers if present.
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().await = Some(sid.to_string());
        }

        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("{method}: HTTP {status}: {text}");
        }

        let messages = if content_type.contains("text/event-stream") {
            read_sse_messages(resp).await?
        } else {
            // Plain application/json: a single JSON-RPC message (or, per spec, a
            // batch array). Accept either.
            let text = resp.text().await.context("reading json body")?;
            parse_json_body(&text)?
        };

        // Find the response matching our request id (mirrors legacy id matching,
        // browser-use-core/src/mcp.rs:1032).
        let matched = messages
            .into_iter()
            .find(|m| m.is_response() && m.id.as_ref() == Some(&id))
            .ok_or_else(|| anyhow!("{method}: no JSON-RPC response with matching id in body"))?;

        if let Some(err) = matched.error {
            bail!(
                "MCP server returned error for {method}: {} (code {})",
                err.message,
                err.code
            );
        }
        Ok(matched.result.unwrap_or(Value::Null))
    }

    /// Build a POST request with the standard headers + auth + extra headers +
    /// session id.
    async fn request_builder(
        &self,
        body: &Value,
        timeout_dur: Duration,
    ) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .post(&self.url)
            .timeout(timeout_dur)
            .header(CONTENT_TYPE, "application/json")
            // Accept both response encodings the server may choose.
            .header(ACCEPT, "application/json, text/event-stream")
            .json(body);

        if let Some(token) = &self.bearer_token {
            builder = builder.bearer_auth(token);
        }
        for (k, v) in &self.headers {
            builder = builder.header(k, v);
        }
        if let Some(sid) = self.session_id.lock().await.clone() {
            builder = builder.header("Mcp-Session-Id", sid);
        }
        builder
    }
}

/// Parse a plain JSON body that is either a single JSON-RPC object or a batch
/// array.
fn parse_json_body(text: &str) -> Result<Vec<JsonRpcMessage>> {
    let value: Value = serde_json::from_str(text).context("parsing json-rpc body")?;
    match value {
        Value::Array(items) => items
            .into_iter()
            .map(|v| serde_json::from_value(v).context("parsing json-rpc array element"))
            .collect(),
        other => Ok(vec![serde_json::from_value(other)?]),
    }
}

/// Read an SSE (`text/event-stream`) body and extract JSON-RPC messages from its
/// `data:` lines.
async fn read_sse_messages(resp: reqwest::Response) -> Result<Vec<JsonRpcMessage>> {
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading sse chunk")?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
    }
    Ok(parse_sse_data(&buffer))
}

/// Parse the `data:` lines of an SSE body into JSON-RPC messages. Multiple
/// `data:` lines within one event are joined by `\n`; a blank line ends an
/// event. Each event's joined payload is parsed as one JSON-RPC message. Other
/// SSE fields (`event:`, `id:`, `retry:`, comments) are ignored.
pub(crate) fn parse_sse_data(body: &str) -> Vec<JsonRpcMessage> {
    let mut messages = Vec::new();
    let mut current: Vec<String> = Vec::new();

    fn flush(current: &mut Vec<String>, messages: &mut Vec<JsonRpcMessage>) {
        if current.is_empty() {
            return;
        }
        let joined = current.join("\n");
        current.clear();
        if let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(joined.trim()) {
            messages.push(msg);
        }
    }

    for line in body.lines() {
        if line.is_empty() {
            flush(&mut current, &mut messages);
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            // A single optional leading space after the colon is stripped (SSE
            // spec); additional whitespace is preserved.
            current.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }
    flush(&mut current, &mut messages);
    messages
}
