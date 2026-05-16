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
}

pub(crate) const ACCOUNT_CODEX: &str = "Codex login";
pub(crate) const ACCOUNT_CLAUDE_CODE: &str = "Claude Code subscription";
pub(crate) const ACCOUNT_CLAUDE_CODE_LEGACY: &str = "Claude Code login";
pub(crate) const ACCOUNT_OPENAI: &str = "OpenAI API key";
pub(crate) const ACCOUNT_ANTHROPIC: &str = "Anthropic API key";
pub(crate) const ACCOUNT_OPENROUTER: &str = "OpenRouter API key";

pub(crate) const ACCOUNT_CHOICES: [&str; 5] = [
    ACCOUNT_CODEX,
    ACCOUNT_CLAUDE_CODE,
    ACCOUNT_OPENAI,
    ACCOUNT_ANTHROPIC,
    ACCOUNT_OPENROUTER,
];

pub(crate) const BROWSER_USE_CLOUD: &str = "Browser Use cloud";
pub(crate) const BROWSER_USE_CLOUD_API_KEY_SETTING: &str = "auth.browser_use_cloud.api_key";
pub(crate) const BROWSER_USE_CLOUD_API_KEY_ENV: &str = "BROWSER_USE_API_KEY";
pub(crate) const BROWSER_CHOICES: [&str; 3] =
    [BROWSER_USE_CLOUD, "Local Chrome", "Headless Chromium"];

pub(crate) fn browser_use_cloud_env_key_present() -> bool {
    std::env::var(BROWSER_USE_CLOUD_API_KEY_ENV).is_ok_and(|value| !value.trim().is_empty())
}

pub(crate) fn is_claude_code_account(account: &str) -> bool {
    account == ACCOUNT_CLAUDE_CODE || account == ACCOUNT_CLAUDE_CODE_LEGACY
}

pub(crate) const MODEL_CHOICES: [ModelChoice; 9] = [
    ModelChoice {
        display: "GPT-5.5",
        account: ACCOUNT_CODEX,
        backend: AgentBackend::Codex,
        provider_model: "gpt-5.5",
    },
    ModelChoice {
        display: "Claude Sonnet 4.6",
        account: ACCOUNT_CLAUDE_CODE,
        backend: AgentBackend::Anthropic,
        provider_model: "claude-sonnet-4-6",
    },
    ModelChoice {
        display: "Claude Opus 4.7",
        account: ACCOUNT_CLAUDE_CODE,
        backend: AgentBackend::Anthropic,
        provider_model: "claude-opus-4-7",
    },
    ModelChoice {
        display: "GPT-5.5",
        account: ACCOUNT_OPENAI,
        backend: AgentBackend::Openai,
        provider_model: "gpt-5.5",
    },
    ModelChoice {
        display: "Claude Sonnet 4.6",
        account: ACCOUNT_ANTHROPIC,
        backend: AgentBackend::Anthropic,
        provider_model: "claude-sonnet-4-6",
    },
    ModelChoice {
        display: "Claude Opus 4.7",
        account: ACCOUNT_ANTHROPIC,
        backend: AgentBackend::Anthropic,
        provider_model: "claude-opus-4-7",
    },
    ModelChoice {
        display: "Qwen3.6 Plus",
        account: ACCOUNT_OPENROUTER,
        backend: AgentBackend::Openrouter,
        provider_model: "qwen/qwen3.6-plus",
    },
    ModelChoice {
        display: "Kimi K2.5",
        account: ACCOUNT_OPENROUTER,
        backend: AgentBackend::Openrouter,
        provider_model: "moonshotai/kimi-k2.5",
    },
    ModelChoice {
        display: "DeepSeek V4 Pro",
        account: ACCOUNT_OPENROUTER,
        backend: AgentBackend::Openrouter,
        provider_model: "deepseek/deepseek-v4-pro",
    },
];

pub(crate) fn provider_model_for_display(display: &str) -> &str {
    MODEL_CHOICES
        .iter()
        .find(|choice| choice.display == display)
        .map(|choice| choice.provider_model)
        .unwrap_or(display)
}
