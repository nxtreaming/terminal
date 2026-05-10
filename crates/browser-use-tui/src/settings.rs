use browser_use_core::ProviderBackend;
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum AgentBackend {
    Codex,
    Openai,
    Anthropic,
    Openrouter,
    Fake,
    None,
}

impl AgentBackend {
    pub(crate) fn as_setting(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Openrouter => "openrouter",
            Self::Fake => "fake",
            Self::None => "none",
        }
    }

    pub(crate) fn from_setting(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "openai" => Some(Self::Openai),
            "anthropic" => Some(Self::Anthropic),
            "openrouter" => Some(Self::Openrouter),
            "fake" => Some(Self::Fake),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

impl From<AgentBackend> for ProviderBackend {
    fn from(value: AgentBackend) -> Self {
        match value {
            AgentBackend::Codex => Self::Codex,
            AgentBackend::Openai => Self::Openai,
            AgentBackend::Anthropic => Self::Anthropic,
            AgentBackend::Openrouter => Self::Openrouter,
            AgentBackend::Fake => Self::Fake,
            AgentBackend::None => Self::None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ModelChoice {
    pub(crate) display: &'static str,
    pub(crate) account: &'static str,
    pub(crate) backend: AgentBackend,
    pub(crate) provider_model: &'static str,
    pub(crate) row: &'static str,
}

pub(crate) const ACCOUNT_CHOICES: [&str; 5] = [
    "Codex login",
    "Claude Code login",
    "OpenAI API key",
    "Anthropic API key",
    "OpenRouter API key",
];

pub(crate) const BROWSER_CHOICES: [&str; 3] =
    ["Local Chrome", "Browser Use cloud", "Headless Chromium"];

pub(crate) const MODEL_CHOICES: [ModelChoice; 9] = [
    ModelChoice {
        display: "GPT-5.5",
        account: "Codex login",
        backend: AgentBackend::Codex,
        provider_model: "gpt-5.5",
        row: "GPT-5.5                         Codex login             best default",
    },
    ModelChoice {
        display: "GPT-5.5",
        account: "OpenAI API key",
        backend: AgentBackend::Openai,
        provider_model: "gpt-5.5",
        row: "GPT-5.5                         OpenAI API key          sign in required",
    },
    ModelChoice {
        display: "Claude Opus 4.7",
        account: "Claude Code login",
        backend: AgentBackend::Anthropic,
        provider_model: "claude-opus-4-7",
        row: "Claude Opus 4.7                 Claude Code login       sign in required",
    },
    ModelChoice {
        display: "Claude Opus 4.7",
        account: "Anthropic API key",
        backend: AgentBackend::Anthropic,
        provider_model: "claude-opus-4-7",
        row: "Claude Opus 4.7                 Anthropic API key       sign in required",
    },
    ModelChoice {
        display: "Claude Sonnet 4.6",
        account: "Claude Code login",
        backend: AgentBackend::Anthropic,
        provider_model: "claude-sonnet-4-6",
        row: "Claude Sonnet 4.6               Claude Code login       sign in required",
    },
    ModelChoice {
        display: "Claude Sonnet 4.6",
        account: "Anthropic API key",
        backend: AgentBackend::Anthropic,
        provider_model: "claude-sonnet-4-6",
        row: "Claude Sonnet 4.6               Anthropic API key       sign in required",
    },
    ModelChoice {
        display: "Qwen3.6 Plus",
        account: "OpenRouter API key",
        backend: AgentBackend::Openrouter,
        provider_model: "qwen/qwen3.6-plus",
        row: "Qwen3.6 Plus                    OpenRouter API key      sign in required",
    },
    ModelChoice {
        display: "GLM-5.1",
        account: "OpenRouter API key",
        backend: AgentBackend::Openrouter,
        provider_model: "z-ai/glm-5.1",
        row: "GLM-5.1                         OpenRouter API key      sign in required",
    },
    ModelChoice {
        display: "DeepSeek V4 Pro",
        account: "OpenRouter API key",
        backend: AgentBackend::Openrouter,
        provider_model: "deepseek/deepseek-v4-pro",
        row: "DeepSeek V4 Pro                 OpenRouter API key      sign in required",
    },
];

pub(crate) fn provider_model_for_display(display: &str) -> &str {
    MODEL_CHOICES
        .iter()
        .find(|choice| choice.display == display)
        .map(|choice| choice.provider_model)
        .unwrap_or(display)
}
