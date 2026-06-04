//! Facade for OpenAI-compatible third-party providers.
//!
//! Many providers expose an OpenAI Chat Completions-compatible API at a
//! provider-specific base URL. This facade lets callers either supply an
//! explicit base URL via [`OpenAiCompatible::configure`], or select a known
//! provider by id via [`OpenAiCompatible::provider`], which resolves the base
//! URL from a built-in profile table.

use crate::protocols::OpenAiChatProtocol;
use crate::route::{Auth, Endpoint, Route};

/// A built-in OpenAI-compatible provider profile.
struct Profile {
    /// The provider identifier, e.g. `"openrouter"`.
    id: &'static str,
    /// The default base URL for the provider's OpenAI-compatible API.
    base_url: &'static str,
    /// Whether the provider needs no auth (e.g. a local Ollama server).
    no_auth: bool,
    /// Whether this provider's chat endpoint accepts image content parts.
    supports_image_content: bool,
}

/// The static table of known OpenAI-compatible providers.
const PROFILES: &[Profile] = &[
    Profile {
        id: "ollama",
        base_url: "http://localhost:11434/v1",
        no_auth: true,
        supports_image_content: true,
    },
    Profile {
        id: "openrouter",
        base_url: "https://openrouter.ai/api/v1",
        no_auth: false,
        supports_image_content: true,
    },
    Profile {
        id: "deepseek",
        base_url: "https://api.deepseek.com/v1",
        no_auth: false,
        supports_image_content: false,
    },
    Profile {
        id: "fireworks",
        base_url: "https://api.fireworks.ai/inference/v1",
        no_auth: false,
        supports_image_content: true,
    },
];

/// Look up a built-in provider profile by id.
fn profile(provider_id: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.id == provider_id)
}

/// A configured OpenAI-compatible provider, ready to bind a model.
///
/// Construct either with [`OpenAiCompatible::configure`] (explicit base URL) or
/// [`OpenAiCompatible::provider`] (known provider id), then call
/// [`OpenAiCompatible::chat`] to obtain a ready [`Route`].
#[derive(Debug, Clone)]
pub struct OpenAiCompatible {
    provider_id: String,
    base_url: String,
    api_key: Option<String>,
    supports_image_content: bool,
}

impl OpenAiCompatible {
    /// Configure a provider with an explicit base URL and API key.
    ///
    /// Use this for self-hosted or otherwise unlisted providers. For known
    /// providers, prefer [`OpenAiCompatible::provider`].
    pub fn configure(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            base_url: base_url.into(),
            api_key: Some(api_key.into()),
            supports_image_content: true,
        }
    }

    /// Configure a known provider by id, resolving its default base URL from the
    /// built-in profile table.
    ///
    /// Returns `None` if `provider_id` is not a recognised provider. Providers
    /// flagged as auth-free in the table (e.g. `ollama`) ignore the supplied
    /// `api_key` and produce a route with no auth headers.
    pub fn provider(provider_id: &str, api_key: impl Into<String>) -> Option<Self> {
        let profile = profile(provider_id)?;
        let api_key = if profile.no_auth {
            None
        } else {
            Some(api_key.into())
        };
        Some(Self {
            provider_id: profile.id.to_string(),
            base_url: profile.base_url.to_string(),
            api_key,
            supports_image_content: profile.supports_image_content,
        })
    }

    /// The provider id this facade was configured with.
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Bind a model to the OpenAI Chat Completions API (`/chat/completions`).
    ///
    /// The `model` names the deployment to target; it travels with each
    /// [`LlmRequest`](crate::schema::LlmRequest) issued against the route.
    pub fn chat(&self, model: impl Into<String>) -> Route {
        let model = model.into();
        let auth = match &self.api_key {
            Some(key) => Auth::bearer(key.clone()),
            None => Auth::None,
        };
        let protocol = if self.supports_image_content && !is_deepseek_v4_model(&model) {
            OpenAiChatProtocol::new()
        } else {
            OpenAiChatProtocol::without_image_content()
        };
        Route::new(
            Box::new(protocol),
            Endpoint::new(self.base_url.clone(), "/chat/completions"),
            auth,
        )
    }
}

fn is_deepseek_v4_model(model: &str) -> bool {
    let normalized = model.trim().to_ascii_lowercase().replace('_', "-");
    normalized.contains("deepseek")
        && (normalized.contains("v4-pro") || normalized.contains("v4-flash"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ContentPart, LlmRequest, Message, MessageRole};

    fn header(route: &Route, name: &str) -> Option<String> {
        route
            .auth
            .headers()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    #[test]
    fn configure_uses_explicit_base_url_and_bearer_auth() {
        let provider =
            OpenAiCompatible::configure("custom", "https://llm.internal.example/v1", "secret-key");
        let route = provider.chat("my-model");

        assert_eq!(provider.provider_id(), "custom");
        assert_eq!(
            route.endpoint.url(),
            "https://llm.internal.example/v1/chat/completions"
        );
        assert_eq!(
            header(&route, "authorization").as_deref(),
            Some("Bearer secret-key")
        );
    }

    #[test]
    fn provider_resolves_openrouter_base_url_and_auth() {
        let provider = OpenAiCompatible::provider("openrouter", "or-key")
            .expect("openrouter is a known provider");
        let route = provider.chat("anthropic/claude-3.5-sonnet");

        assert_eq!(
            route.endpoint.url(),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            header(&route, "authorization").as_deref(),
            Some("Bearer or-key")
        );
    }

    #[test]
    fn provider_profile_table_resolves_expected_base_urls() {
        let cases = [
            ("ollama", "http://localhost:11434/v1/chat/completions"),
            (
                "openrouter",
                "https://openrouter.ai/api/v1/chat/completions",
            ),
            ("deepseek", "https://api.deepseek.com/v1/chat/completions"),
            (
                "fireworks",
                "https://api.fireworks.ai/inference/v1/chat/completions",
            ),
        ];

        for (id, expected_url) in cases {
            let provider = OpenAiCompatible::provider(id, "key").expect("provider should be known");
            let route = provider.chat("model");
            assert_eq!(route.endpoint.url(), expected_url, "base url for {id}");
        }
    }

    #[test]
    fn ollama_uses_no_auth() {
        let provider =
            OpenAiCompatible::provider("ollama", "ignored").expect("ollama is a known provider");
        let route = provider.chat("llama3");

        assert!(
            route.auth.headers().is_empty(),
            "ollama route should carry no auth headers"
        );
    }

    #[test]
    fn unknown_provider_returns_none() {
        assert!(OpenAiCompatible::provider("nope", "key").is_none());
    }

    #[test]
    fn chat_protocol_builds_sane_body() {
        let provider =
            OpenAiCompatible::provider("deepseek", "ds-key").expect("deepseek is a known provider");
        let route = provider.chat("deepseek-chat");
        let mut request = LlmRequest::new("deepseek-chat", "deepseek");
        request.messages.push(Message::user_text("hi"));
        let body = route.protocol.build_body(&request).expect("body builds");

        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn deepseek_provider_omits_image_content_parts() {
        let provider =
            OpenAiCompatible::provider("deepseek", "ds-key").expect("deepseek is a known provider");
        let route = provider.chat("deepseek-v4-flash");
        let mut request = LlmRequest::new("deepseek-v4-flash", "deepseek");
        request.messages.push(Message::new(
            MessageRole::User,
            vec![
                ContentPart::text("look"),
                ContentPart::Media {
                    mime_type: "image/png".into(),
                    data: Some("AAAA".into()),
                    url: None,
                    detail: Some("high".into()),
                },
            ],
        ));
        request.messages.push(Message::new(
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: "call_browser".into(),
                content: vec![ContentPart::Media {
                    mime_type: "image/png".into(),
                    data: Some("BBBB".into()),
                    url: None,
                    detail: Some("high".into()),
                }],
                is_error: false,
            }],
        ));

        let body = route.protocol.build_body(&request).expect("body builds");
        let serialized = serde_json::to_string(&body).expect("body serializes");

        assert!(!serialized.contains("image_url"));
        assert!(serialized.contains("image omitted"));
        assert!(serialized
            .contains("omitted because this model endpoint does not accept image content"));
    }

    #[test]
    fn deepseek_v4_model_ids_omit_images_even_for_generic_provider() {
        let provider =
            OpenAiCompatible::configure("custom", "https://llm.internal.example/v1", "secret-key");
        let route = provider.chat("deepseek/deepseek-v4-pro");
        let mut request = LlmRequest::new("deepseek/deepseek-v4-pro", "custom");
        request.messages.push(Message::new(
            MessageRole::User,
            vec![ContentPart::Media {
                mime_type: "image/png".into(),
                data: Some("AAAA".into()),
                url: None,
                detail: None,
            }],
        ));

        let body = route.protocol.build_body(&request).expect("body builds");
        let serialized = serde_json::to_string(&body).expect("body serializes");

        assert!(!serialized.contains("image_url"));
        assert!(serialized.contains("image omitted"));
    }
}
