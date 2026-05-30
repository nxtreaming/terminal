//! Facade for OpenAI's first-party APIs (Responses and Chat Completions).

use crate::protocols::{OpenAiChatProtocol, OpenAiResponsesProtocol};
use crate::route::{Auth, Endpoint, Route};

/// The default base URL for OpenAI's first-party API.
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Deployment configuration for the OpenAI provider.
///
/// Captures what is shared across models: the API key (sent as a bearer token)
/// and an optional base URL override for proxies or compatible gateways. When
/// no base URL is supplied, [`DEFAULT_BASE_URL`] is used.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// The OpenAI API key, sent as `Authorization: Bearer <key>`.
    pub api_key: String,
    /// An optional base URL override; defaults to the public OpenAI API.
    pub base_url: Option<String>,
}

/// A configured OpenAI provider, ready to bind a model and protocol.
///
/// Construct with [`OpenAi::configure`], then call [`OpenAi::responses`] or
/// [`OpenAi::chat`] to obtain a ready [`Route`].
#[derive(Debug, Clone)]
pub struct OpenAi {
    base_url: String,
    api_key: String,
}

impl OpenAi {
    /// Resolve deployment configuration into a ready-to-use provider.
    pub fn configure(config: OpenAiConfig) -> Self {
        let base_url = config
            .base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Self {
            base_url,
            api_key: config.api_key,
        }
    }

    /// Bind a model to the OpenAI Responses API (`/responses`).
    ///
    /// The `model` names the deployment to target; it travels with each
    /// [`LlmRequest`](crate::schema::LlmRequest) issued against the route, so it
    /// is recorded only where the protocol needs it.
    pub fn responses(&self, model: impl Into<String>) -> Route {
        let _model = model.into();
        Route::new(
            Box::new(OpenAiResponsesProtocol::new()),
            Endpoint::new(self.base_url.clone(), "/responses"),
            Auth::bearer(self.api_key.clone()),
        )
    }

    /// Bind a model to the OpenAI Chat Completions API (`/chat/completions`).
    ///
    /// The `model` names the deployment to target; it travels with each
    /// [`LlmRequest`](crate::schema::LlmRequest) issued against the route.
    pub fn chat(&self, model: impl Into<String>) -> Route {
        let _model = model.into();
        Route::new(
            Box::new(OpenAiChatProtocol::new()),
            Endpoint::new(self.base_url.clone(), "/chat/completions"),
            Auth::bearer(self.api_key.clone()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{LlmRequest, Message};

    fn provider() -> OpenAi {
        OpenAi::configure(OpenAiConfig {
            api_key: "sk-test".to_string(),
            base_url: None,
        })
    }

    /// A tiny request carrying a single user-text message.
    fn sample_request(model: &str) -> LlmRequest {
        let mut req = LlmRequest::new(model, "openai");
        req.messages.push(Message::user_text("hi"));
        req
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
    fn responses_route_has_correct_url_and_auth() {
        let route = provider().responses("gpt-5");

        assert_eq!(route.endpoint.url(), "https://api.openai.com/v1/responses");
        assert_eq!(
            header(&route, "authorization").as_deref(),
            Some("Bearer sk-test")
        );
    }

    #[test]
    fn chat_route_has_correct_url_and_auth() {
        let route = provider().chat("gpt-4o");

        assert_eq!(
            route.endpoint.url(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            header(&route, "authorization").as_deref(),
            Some("Bearer sk-test")
        );
    }

    #[test]
    fn base_url_override_is_respected() {
        let provider = OpenAi::configure(OpenAiConfig {
            api_key: "sk-test".to_string(),
            base_url: Some("https://proxy.example.com/v1/".to_string()),
        });
        let route = provider.chat("gpt-4o");

        assert_eq!(
            route.endpoint.url(),
            "https://proxy.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn responses_protocol_builds_sane_body() {
        let route = provider().responses("gpt-5");
        let body = route
            .protocol
            .build_body(&sample_request("gpt-5"))
            .expect("body builds");

        assert_eq!(body["model"], "gpt-5");
        // The Responses API lowers messages into a top-level `input` array.
        assert_eq!(body["input"][0]["role"], "user");
    }

    #[test]
    fn chat_protocol_builds_sane_body() {
        let route = provider().chat("gpt-4o");
        let body = route
            .protocol
            .build_body(&sample_request("gpt-4o"))
            .expect("body builds");

        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["messages"][0]["role"], "user");
    }
}
