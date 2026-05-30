//! Facade for Anthropic's first-party Messages API.

use crate::protocols::AnthropicMessagesProtocol;
use crate::route::{Auth, Endpoint, Route};

/// The default base URL for Anthropic's first-party API.
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";

/// The Anthropic API version pinned by this facade.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Deployment configuration for the Anthropic provider.
///
/// Captures what is shared across models: the API key (sent via the
/// `x-api-key` header) and an optional base URL override. When no base URL is
/// supplied, [`DEFAULT_BASE_URL`] is used.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// The Anthropic API key, sent via the `x-api-key` header.
    pub api_key: String,
    /// An optional base URL override; defaults to the public Anthropic API.
    pub base_url: Option<String>,
}

/// A configured Anthropic provider, ready to bind a model.
///
/// Construct with [`Anthropic::configure`], then call [`Anthropic::model`] to
/// obtain a ready [`Route`].
#[derive(Debug, Clone)]
pub struct Anthropic {
    base_url: String,
    api_key: String,
}

impl Anthropic {
    /// Resolve deployment configuration into a ready-to-use provider.
    pub fn configure(config: AnthropicConfig) -> Self {
        let base_url = config
            .base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Self {
            base_url,
            api_key: config.api_key,
        }
    }

    /// Bind a model to the Anthropic Messages API (`/messages`).
    ///
    /// Auth uses the `x-api-key` header alongside the pinned `anthropic-version`
    /// header. The `model` names the deployment to target; it travels with each
    /// [`LlmRequest`](crate::schema::LlmRequest) issued against the route.
    pub fn model(&self, model: impl Into<String>) -> Route {
        let _model = model.into();
        Route::new(
            Box::new(AnthropicMessagesProtocol::new()),
            Endpoint::new(self.base_url.clone(), "/messages"),
            Auth::header("x-api-key", self.api_key.clone())
                .and_then(Auth::header("anthropic-version", ANTHROPIC_VERSION)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{LlmRequest, Message, SystemPart};

    fn provider() -> Anthropic {
        Anthropic::configure(AnthropicConfig {
            api_key: "anthropic-key".to_string(),
            base_url: None,
        })
    }

    fn header(route: &Route, name: &str) -> Option<String> {
        route
            .auth
            .headers()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    #[test]
    fn model_route_has_correct_url() {
        let route = provider().model("claude-sonnet-4");
        assert_eq!(
            route.endpoint.url(),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn model_route_has_correct_auth_headers() {
        let route = provider().model("claude-sonnet-4");

        assert_eq!(
            header(&route, "x-api-key").as_deref(),
            Some("anthropic-key")
        );
        assert_eq!(
            header(&route, "anthropic-version").as_deref(),
            Some("2023-06-01")
        );
        // The Anthropic facade must not use bearer auth.
        assert!(header(&route, "authorization").is_none());
    }

    #[test]
    fn base_url_override_is_respected() {
        let provider = Anthropic::configure(AnthropicConfig {
            api_key: "anthropic-key".to_string(),
            base_url: Some("https://gateway.example.com/anthropic".to_string()),
        });
        let route = provider.model("claude-sonnet-4");

        assert_eq!(
            route.endpoint.url(),
            "https://gateway.example.com/anthropic/messages"
        );
    }

    #[test]
    fn protocol_builds_sane_body_with_system_split() {
        let route = provider().model("claude-sonnet-4");
        let mut request = LlmRequest::new("claude-sonnet-4", "anthropic");
        request.system.push(SystemPart::new("be terse"));
        request.messages.push(Message::user_text("hi"));
        let body = route.protocol.build_body(&request).expect("body builds");

        assert_eq!(body["model"], "claude-sonnet-4");
        // System parts are lifted out of `messages` into a top-level `system`
        // array of text blocks for the Messages API.
        assert_eq!(body["system"][0]["text"], "be terse");
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["messages"][0]["role"], "user");
    }
}
