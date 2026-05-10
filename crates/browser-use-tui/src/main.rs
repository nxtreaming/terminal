use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use browser_use_core::{run_existing_session_with_provider, AgentRunOptions};
use browser_use_protocol::{
    project_workbench, HistoryRow, SessionMeta, SessionStatus, WorkbenchState,
};
use browser_use_providers::{
    load_codex_auth, AnthropicMessagesProvider, CodexAuth, CodexResponsesProvider, FakeProvider,
    OpenAICompatibleChatProvider, OpenAIResponsesProvider,
};
use browser_use_store::Store;
use clap::{Parser, ValueEnum};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

#[derive(Debug, Parser)]
#[command(name = "but", bin_name = "but")]
struct Args {
    #[arg(long, default_value = ".browser-use-terminal")]
    state_dir: PathBuf,
    #[arg(long, default_value = "GPT-5.5")]
    model: String,
    #[arg(long, default_value = "Codex login")]
    account: String,
    #[arg(long, default_value = "Local Chrome")]
    browser: String,
    #[arg(long)]
    dump_screen: bool,
    #[arg(long, default_value_t = 120)]
    width: u16,
    #[arg(long, default_value_t = 34)]
    height: u16,
    #[arg(long)]
    select_latest: bool,
    #[arg(long)]
    seed_demo: Option<String>,
    #[arg(long, value_enum)]
    overlay: Option<OverlayArg>,
    #[arg(long, value_enum, default_value = "codex", hide = true)]
    agent: AgentBackend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum AgentBackend {
    Codex,
    Openai,
    Anthropic,
    Openrouter,
    Fake,
    None,
}

impl AgentBackend {
    fn as_setting(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Openrouter => "openrouter",
            Self::Fake => "fake",
            Self::None => "none",
        }
    }

    fn from_setting(value: &str) -> Option<Self> {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Overlay {
    None,
    Setup,
    Account,
    Model,
    Browser,
    BrowserChoice,
    SetupComplete,
    History,
    Actions,
    Help,
    Developer,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OverlayArg {
    Setup,
    Account,
    Model,
    Browser,
    History,
    Actions,
    Help,
    Developer,
}

impl From<OverlayArg> for Overlay {
    fn from(value: OverlayArg) -> Self {
        match value {
            OverlayArg::Setup => Self::Setup,
            OverlayArg::Account => Self::Account,
            OverlayArg::Model => Self::Model,
            OverlayArg::Browser => Self::Browser,
            OverlayArg::History => Self::History,
            OverlayArg::Actions => Self::Actions,
            OverlayArg::Help => Self::Help,
            OverlayArg::Developer => Self::Developer,
        }
    }
}

struct App {
    store: Store,
    args: Args,
    selected_session_id: Option<String>,
    input: String,
    overlay: Overlay,
    selected_row: usize,
    setup_complete: bool,
    account: String,
    model: String,
    model_configured: bool,
    provider_model: String,
    browser: String,
    browser_notice: Option<String>,
    agent_backend: AgentBackend,
    quit_hint_until: Option<Instant>,
}

#[derive(Clone, Copy)]
struct ModelChoice {
    display: &'static str,
    account: &'static str,
    backend: AgentBackend,
    provider_model: &'static str,
    row: &'static str,
}

const ACCOUNT_CHOICES: [&str; 5] = [
    "Codex login",
    "Claude Code login",
    "OpenAI API key",
    "Anthropic API key",
    "OpenRouter API key",
];

const BROWSER_CHOICES: [&str; 3] = ["Local Chrome", "Browser Use cloud", "Headless Chromium"];

const MODEL_CHOICES: [ModelChoice; 9] = [
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

fn provider_model_for_display(display: &str) -> &str {
    MODEL_CHOICES
        .iter()
        .find(|choice| choice.display == display)
        .map(|choice| choice.provider_model)
        .unwrap_or(display)
}

impl App {
    fn new(args: Args) -> Result<Self> {
        let store = Store::open(&args.state_dir)?;
        seed_demo_if_requested(&store, args.seed_demo.as_deref())?;
        let selected_session_id = if args.select_latest {
            store
                .list_sessions()?
                .first()
                .map(|session| session.id.clone())
        } else {
            None
        };
        let overlay = args.overlay.map(Into::into).unwrap_or(Overlay::None);
        let setup_complete = store.get_setting("setup.complete")?.as_deref() == Some("1");
        let account = store
            .get_setting("account")?
            .unwrap_or_else(|| args.account.clone());
        let stored_model = store.get_setting("model")?;
        let model_configured = stored_model.is_some() || setup_complete;
        let model = stored_model.unwrap_or_else(|| args.model.clone());
        let provider_model = store
            .get_setting("provider.model")?
            .unwrap_or_else(|| provider_model_for_display(&model).to_string());
        let browser = store
            .get_setting("browser")?
            .unwrap_or_else(|| args.browser.clone());
        let agent_backend = store
            .get_setting("agent.backend")?
            .and_then(|value| AgentBackend::from_setting(&value))
            .unwrap_or(args.agent);
        Ok(Self {
            store,
            args,
            selected_session_id,
            input: String::new(),
            overlay,
            selected_row: 0,
            setup_complete,
            account,
            model,
            model_configured,
            provider_model,
            browser,
            browser_notice: None,
            agent_backend,
            quit_hint_until: None,
        })
    }

    fn workbench_state(&self) -> Result<WorkbenchState> {
        let sessions = self.store.list_sessions()?;
        let current_id = self.selected_session_id.as_deref();
        let current_events = current_id
            .map(|id| self.store.events_for_session(id))
            .transpose()?
            .unwrap_or_default();
        let all_events = sessions
            .iter()
            .map(|session| {
                self.store
                    .events_for_session(&session.id)
                    .map(|events| (session.id.clone(), events))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(project_workbench(
            &sessions,
            &current_events,
            &all_events,
            current_id,
            self.browser.clone(),
        ))
    }

    fn open_overlay(&mut self, overlay: Overlay) {
        self.overlay = overlay;
        self.selected_row = 0;
        if overlay != Overlay::Browser {
            self.browser_notice = None;
        }
    }

    fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
        self.selected_row = 0;
        self.browser_notice = None;
    }

    fn submit(&mut self) -> Result<()> {
        let text = self.input.trim().to_string();
        self.input.clear();
        if text.is_empty() {
            if let Some(session) = self
                .selected_session_id
                .as_deref()
                .and_then(|id| self.store.load_session(id).ok().flatten())
            {
                if session.status == SessionStatus::Failed {
                    self.start_agent_for_session(session.id)?;
                }
            }
            return Ok(());
        }
        if text == "/" {
            self.open_overlay(Overlay::Actions);
            return Ok(());
        }
        if let Some(session) = self
            .selected_session_id
            .as_deref()
            .and_then(|id| self.store.load_session(id).ok().flatten())
        {
            if session.status.is_active() {
                self.store.append_event(
                    &session.id,
                    "session.followup",
                    serde_json::json!({ "text": text }),
                )?;
                return Ok(());
            }
            self.store.append_event(
                &session.id,
                "session.followup",
                serde_json::json!({ "text": text }),
            )?;
            self.start_agent_for_session(session.id)?;
            return Ok(());
        }
        let session = self.store.create_session(None, std::env::current_dir()?)?;
        self.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({ "text": text }),
        )?;
        self.store.append_event(
            &session.id,
            "browser.page",
            serde_json::json!({ "url": "about:blank", "title": "Browser ready" }),
        )?;
        self.selected_session_id = Some(session.id.clone());
        self.start_agent_for_session(session.id)?;
        Ok(())
    }

    fn start_agent_for_session(&self, session_id: String) -> Result<()> {
        if matches!(self.agent_backend, AgentBackend::None) {
            return Ok(());
        }
        let state_dir = self.args.state_dir.clone();
        let backend = self.agent_backend;
        let model = self.provider_model.clone();
        let browser = self.browser.clone();
        thread::Builder::new()
            .name(format!("browser-use-agent-{session_id}"))
            .spawn(move || {
                if let Err(error) = run_agent_thread(state_dir, session_id, backend, model, browser)
                {
                    eprintln!("agent thread failed: {error:#}");
                }
            })
            .context("spawn agent thread")?;
        Ok(())
    }

    fn complete_demo_result(&mut self) -> Result<()> {
        let Some(id) = self.selected_session_id.clone() else {
            return Ok(());
        };
        self.store.append_event(
            &id,
            "session.done",
            serde_json::json!({"result": "Demo result from the Rust event store.\n\nThe browser task state is now rendered from SQLite."}),
        )?;
        Ok(())
    }

    fn cancel_current_task(&mut self) -> Result<bool> {
        let Some(id) = self.selected_session_id.clone() else {
            return Ok(false);
        };
        let Some(session) = self.store.load_session(&id)? else {
            return Ok(false);
        };
        if !session.status.is_active() {
            return Ok(false);
        }
        self.store.request_cancel(&id, "stopped from terminal")?;
        Ok(true)
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key {
            KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => return Ok(true),
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                if !self.input.is_empty() {
                    self.input.clear();
                } else if self.cancel_current_task()? {
                    self.quit_hint_until = None;
                } else if self
                    .quit_hint_until
                    .is_some_and(|until| Instant::now() <= until)
                {
                    return Ok(true);
                } else {
                    self.quit_hint_until = Some(Instant::now() + Duration::from_millis(1500));
                }
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => self.close_overlay(),
            KeyEvent {
                code: KeyCode::Tab, ..
            } => self.open_overlay(Overlay::History),
            KeyEvent {
                code: KeyCode::F(1),
                ..
            } => self.open_overlay(Overlay::Help),
            KeyEvent {
                code: KeyCode::F(2),
                ..
            } => self.open_overlay(Overlay::Browser),
            KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.open_overlay(Overlay::Developer),
            KeyEvent {
                code: KeyCode::Char('/'),
                modifiers: KeyModifiers::NONE,
                ..
            } if self.input.is_empty() => self.open_overlay(Overlay::Actions),
            KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
                ..
            } if self.overlay == Overlay::History => self.execute_overlay_selection()?,
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.complete_demo_result()?,
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } if self.is_first_run_setup_visible()? => self.open_overlay(Overlay::Account),
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } if self.overlay == Overlay::Setup => self.open_overlay(Overlay::Account),
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } if self.overlay != Overlay::None => self.execute_overlay_selection()?,
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => self.submit()?,
            KeyEvent {
                code: KeyCode::Backspace,
                ..
            } => {
                self.input.pop();
            }
            KeyEvent {
                code: KeyCode::Up, ..
            } => self.selected_row = self.selected_row.saturating_sub(1),
            KeyEvent {
                code: KeyCode::Down,
                ..
            } => self.selected_row = self.selected_row.saturating_add(1),
            KeyEvent {
                code: KeyCode::Char(ch),
                modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
                ..
            } => self.input.push(ch),
            _ => {}
        }
        Ok(false)
    }

    fn is_first_run_setup_visible(&self) -> Result<bool> {
        Ok(!self.setup_complete
            && self.overlay == Overlay::None
            && self.selected_session_id.is_none()
            && self.input.is_empty()
            && self.store.list_sessions()?.is_empty())
    }

    fn execute_overlay_selection(&mut self) -> Result<()> {
        match self.overlay {
            Overlay::Actions => match self.selected_row.min(5) {
                0 => {
                    self.selected_session_id = None;
                    self.close_overlay();
                }
                1 => self.open_overlay(Overlay::Browser),
                2 => self.open_overlay(Overlay::History),
                3 => self.open_overlay(Overlay::Setup),
                4 => self.open_overlay(Overlay::Model),
                _ => self.open_overlay(Overlay::Account),
            },
            Overlay::History => {
                let sessions = self.store.list_sessions()?;
                if let Some(session) =
                    sessions.get(self.selected_row.min(sessions.len().saturating_sub(1)))
                {
                    self.selected_session_id = Some(session.id.clone());
                }
                self.close_overlay();
            }
            Overlay::Setup => match self.selected_row.min(2) {
                0 => self.open_overlay(Overlay::Account),
                1 => self.open_overlay(Overlay::Model),
                _ => self.open_overlay(Overlay::BrowserChoice),
            },
            Overlay::Account => {
                self.account = ACCOUNT_CHOICES
                    .get(
                        self.selected_row
                            .min(ACCOUNT_CHOICES.len().saturating_sub(1)),
                    )
                    .unwrap_or(&ACCOUNT_CHOICES[0])
                    .to_string();
                self.persist_runtime_settings()?;
                self.open_overlay(Overlay::Model);
            }
            Overlay::Model => {
                let choice = MODEL_CHOICES
                    .get(self.selected_row.min(MODEL_CHOICES.len().saturating_sub(1)))
                    .unwrap_or(&MODEL_CHOICES[0]);
                self.model = choice.display.to_string();
                self.account = choice.account.to_string();
                self.provider_model = choice.provider_model.to_string();
                self.agent_backend = choice.backend;
                self.model_configured = true;
                self.persist_runtime_settings()?;
                self.open_overlay(Overlay::BrowserChoice);
            }
            Overlay::Browser => match self.selected_row.min(2) {
                0 => self.request_open_browser()?,
                1 => self.request_reconnect_browser()?,
                _ => self.open_overlay(Overlay::BrowserChoice),
            },
            Overlay::BrowserChoice => {
                let choice = BROWSER_CHOICES
                    .get(
                        self.selected_row
                            .min(BROWSER_CHOICES.len().saturating_sub(1)),
                    )
                    .unwrap_or(&BROWSER_CHOICES[0]);
                self.browser = (*choice).to_string();
                self.persist_runtime_settings()?;
                if self.selected_session_id.is_none() && self.store.list_sessions()?.is_empty() {
                    self.open_overlay(Overlay::SetupComplete);
                } else {
                    self.close_overlay();
                }
            }
            Overlay::SetupComplete => {
                self.setup_complete = true;
                self.store.set_setting("setup.complete", "1")?;
                self.persist_runtime_settings()?;
                self.close_overlay();
            }
            Overlay::Help | Overlay::Developer | Overlay::None => self.close_overlay(),
        }
        Ok(())
    }

    fn request_open_browser(&mut self) -> Result<()> {
        let Some(session_id) = self.selected_session_id.clone() else {
            self.browser_notice = Some("No current browser task yet.".to_string());
            return Ok(());
        };
        let state = self.workbench_state()?;
        let target = state
            .browser
            .live_url
            .as_deref()
            .or(state.browser.url.as_deref())
            .unwrap_or("about:blank");
        self.store.append_event(
            &session_id,
            "browser.open_requested",
            serde_json::json!({ "target": target }),
        )?;
        self.browser_notice = Some(format!("Open requested for {target}"));
        Ok(())
    }

    fn request_reconnect_browser(&mut self) -> Result<()> {
        let Some(session_id) = self.selected_session_id.clone() else {
            self.browser_notice = Some("No current browser task yet.".to_string());
            return Ok(());
        };
        self.store.append_event(
            &session_id,
            "browser.reconnect_requested",
            serde_json::json!({ "browser": self.browser }),
        )?;
        self.browser_notice = Some("Reconnect requested.".to_string());
        Ok(())
    }

    fn persist_runtime_settings(&self) -> Result<()> {
        self.store.set_setting("account", &self.account)?;
        self.store.set_setting("model", &self.model)?;
        self.store
            .set_setting("provider.model", &self.provider_model)?;
        self.store.set_setting("browser", &self.browser)?;
        self.store
            .set_setting("agent.backend", self.agent_backend.as_setting())?;
        Ok(())
    }
}

fn run_agent_thread(
    state_dir: PathBuf,
    session_id: String,
    backend: AgentBackend,
    model: String,
    browser: String,
) -> Result<()> {
    let store = Store::open(&state_dir)?;
    let result = match backend {
        AgentBackend::Codex => {
            let provider = codex_provider(&store, model)?;
            run_existing_session_with_provider(
                &store,
                &provider,
                &session_id,
                tui_agent_options(&browser),
            )
        }
        AgentBackend::Openai => {
            let provider = openai_provider(&store, model)?;
            run_existing_session_with_provider(
                &store,
                &provider,
                &session_id,
                tui_agent_options(&browser),
            )
        }
        AgentBackend::Anthropic => {
            let provider = anthropic_provider(&store, model)?;
            run_existing_session_with_provider(
                &store,
                &provider,
                &session_id,
                tui_agent_options(&browser),
            )
        }
        AgentBackend::Openrouter => {
            let provider = openrouter_provider(&store, model)?;
            run_existing_session_with_provider(
                &store,
                &provider,
                &session_id,
                tui_agent_options(&browser),
            )
        }
        AgentBackend::Fake => {
            let provider = FakeProvider::with_text("Fake result from the Rust TUI agent loop.");
            run_existing_session_with_provider(
                &store,
                &provider,
                &session_id,
                tui_agent_options(&browser),
            )
        }
        AgentBackend::None => Ok(session_id.clone()),
    };
    if let Err(error) = result {
        let _ = store.append_event(
            &session_id,
            "session.failed",
            serde_json::json!({ "error": error.to_string() }),
        );
        return Err(error);
    }
    Ok(())
}

fn tui_agent_options(browser: &str) -> AgentRunOptions {
    match browser {
        "Headless Chromium" => AgentRunOptions::default().with_browser_mode("headless"),
        "Browser Use cloud" => AgentRunOptions::default().with_browser_mode("cloud"),
        _ => AgentRunOptions::default(),
    }
}

fn openai_provider(store: &Store, model: String) -> Result<OpenAIResponsesProvider> {
    let api_key = stored_or_env(
        store,
        "auth.openai.api_key",
        &["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"],
    )?
    .context("run `browser-use-terminal auth login openai --api-key ...` or set LLM_BROWSER_OPENAI_API_KEY")?;
    let base_url = setting_or_env_or_default(
        store,
        "auth.openai.base_url",
        &["LLM_BROWSER_OPENAI_BASE_URL"],
        "https://api.openai.com/v1",
    )?;
    Ok(OpenAIResponsesProvider::with_base_url(
        api_key, model, base_url,
    ))
}

fn codex_provider(store: &Store, model: String) -> Result<CodexResponsesProvider> {
    let auth = match stored_codex_auth(store)? {
        Some(auth) => auth,
        None => load_codex_auth()?,
    };
    let base_url = setting_or_env_or_default(
        store,
        "auth.codex.base_url",
        &["LLM_BROWSER_CODEX_BASE_URL"],
        "https://chatgpt.com/backend-api",
    )?;
    Ok(CodexResponsesProvider::with_base_url(auth, model, base_url))
}

fn anthropic_provider(store: &Store, model: String) -> Result<AnthropicMessagesProvider> {
    let api_key = stored_or_env(
        store,
        "auth.anthropic.api_key",
        &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
    )?
    .context("run `browser-use-terminal auth login anthropic --api-key ...` or set LLM_BROWSER_ANTHROPIC_API_KEY")?;
    let base_url = setting_or_env_or_default(
        store,
        "auth.anthropic.base_url",
        &["LLM_BROWSER_ANTHROPIC_BASE_URL"],
        "https://api.anthropic.com/v1",
    )?;
    Ok(AnthropicMessagesProvider::with_base_url(
        api_key, model, base_url,
    ))
}

fn openrouter_provider(store: &Store, model: String) -> Result<OpenAICompatibleChatProvider> {
    let api_key = stored_or_env(
        store,
        "auth.openrouter.api_key",
        &["LLM_BROWSER_OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY"],
    )?
    .context(
        "run `browser-use-terminal auth login openrouter --api-key ...` or set OPENROUTER_API_KEY",
    )?;
    let base_url = setting_or_env_or_default(
        store,
        "auth.openrouter.base_url",
        &["LLM_BROWSER_OPENAI_COMPAT_BASE_URL", "OPENROUTER_BASE_URL"],
        "https://openrouter.ai/api/v1",
    )?;
    Ok(OpenAICompatibleChatProvider::with_base_url(
        api_key, model, base_url,
    ))
}

fn stored_codex_auth(store: &Store) -> Result<Option<CodexAuth>> {
    let Some(access_token) = store.get_setting("auth.codex.access_token")? else {
        return Ok(None);
    };
    let Some(account_id) = store.get_setting("auth.codex.account_id")? else {
        return Ok(None);
    };
    if access_token.trim().is_empty() || account_id.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(CodexAuth {
        access_token,
        account_id,
    }))
}

fn stored_or_env(store: &Store, setting_key: &str, env_names: &[&str]) -> Result<Option<String>> {
    if let Some(value) = store.get_setting(setting_key)? {
        if !value.trim().is_empty() {
            return Ok(Some(value));
        }
    }
    Ok(env_names
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty()))
}

fn setting_or_env_or_default(
    store: &Store,
    setting_key: &str,
    env_names: &[&str],
    default: &str,
) -> Result<String> {
    Ok(stored_or_env(store, setting_key, env_names)?.unwrap_or_else(|| default.to_string()))
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.dump_screen {
        let mut app = App::new(args)?;
        let text = render_dump(&mut app)?;
        print!("{text}");
        return Ok(());
    }
    run_terminal(App::new(args)?)
}

fn run_terminal(mut app: App) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = loop {
        terminal.draw(|frame| render(frame, &mut app))?;
        if event::poll(Duration::from_millis(100))? {
            if let TermEvent::Key(key) = event::read()? {
                if app.handle_key(key)? {
                    break Ok(());
                }
            }
        }
    };
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn render_dump(app: &mut App) -> Result<String> {
    let backend = TestBackend::new(app.args.width, app.args.height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| render(frame, app))?;
    Ok(buffer_to_string(terminal.backend().buffer()))
}

fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in area.y..area.y.saturating_add(area.height) {
        let mut line = String::new();
        for x in area.x..area.x.saturating_add(area.width) {
            line.push_str(buffer[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let state = app.workbench_state().unwrap_or_else(|_| WorkbenchState {
        setup_complete: false,
        current_session: None,
        task: None,
        result: None,
        failure: Some("Could not load state.".to_string()),
        activity: Vec::new(),
        browser: Default::default(),
        history: Vec::new(),
    });

    let is_first_run =
        !app.setup_complete && state.history.is_empty() && state.current_session.is_none();
    if is_first_run && app.overlay == Overlay::None {
        render_setup(frame, area, app, true);
    } else if is_first_run
        && matches!(
            app.overlay,
            Overlay::Account | Overlay::Model | Overlay::BrowserChoice | Overlay::SetupComplete
        )
    {
        // Setup steps are full-screen product states, not modals over a workbench.
    } else {
        render_workbench(frame, area, app, &state);
    }

    match app.overlay {
        Overlay::None => {}
        Overlay::Setup => render_setup(frame, centered_rect(78, 20, area), app, false),
        Overlay::Account => render_account_overlay(frame, centered_rect(78, 18, area), app),
        Overlay::Model => render_model_overlay(frame, centered_rect(92, 22, area), app),
        Overlay::Browser => render_browser_overlay(frame, centered_rect(84, 18, area), app, &state),
        Overlay::BrowserChoice => {
            render_browser_choice_overlay(frame, centered_rect(84, 18, area), app)
        }
        Overlay::SetupComplete => render_setup_complete(frame, centered_rect(78, 16, area), app),
        Overlay::History => render_history_overlay(frame, centered_rect(94, 20, area), app, &state),
        Overlay::Actions => render_actions_overlay(frame, centered_rect(72, 16, area), app),
        Overlay::Help => render_help_overlay(frame, centered_rect(78, 14, area)),
        Overlay::Developer => {
            render_developer_overlay(frame, centered_rect(96, 24, area), app, &state)
        }
    }
}

fn render_workbench(frame: &mut Frame<'_>, area: Rect, app: &App, state: &WorkbenchState) {
    let block = Block::bordered()
        .title(workbench_title(app, state, area.width))
        .style(Style::default().fg(text()).bg(background()));
    frame.render_widget(block, area);

    let outer = area.inner(Margin {
        vertical: 1,
        horizontal: 2,
    });
    let composer_h = 3u16;
    let footer_h = 1u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),
            Constraint::Length(composer_h),
            Constraint::Length(footer_h),
        ])
        .split(outer);

    let content = if let Some(session) = state.current_session.as_ref() {
        if session.status.is_active() {
            running_lines(state)
        } else if session.status == SessionStatus::Cancelled {
            cancelled_lines()
        } else if let Some(error) = state.failure.as_ref() {
            failure_lines(error)
        } else {
            result_lines(state)
        }
    } else {
        ready_lines(state)
    };
    frame.render_widget(
        Paragraph::new(content)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );
    render_composer(frame, chunks[1], app, state.current_session.as_ref());
    render_footer(frame, chunks[2], app, state);
}

fn ready_lines(state: &WorkbenchState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("What should the browser do?", bold())),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled("Recent", muted())),
        Line::from(""),
    ];
    if state.history.is_empty() {
        lines.push(Line::from(Span::styled("  No previous work yet.", dim())));
    } else {
        for row in state.history.iter().take(3) {
            lines.push(history_line(row, 74));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Ready  ", muted()),
        Span::styled("signed in", text_style()),
        Span::raw("      "),
        Span::styled("browser connected", text_style()),
    ]));
    lines
}

fn running_lines(state: &WorkbenchState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("")];
    let activity = if state.activity.is_empty() {
        vec!["starting browser task".to_string()]
    } else {
        state
            .activity
            .iter()
            .rev()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
    };
    for item in activity.into_iter().rev() {
        lines.push(Line::from(vec![
            Span::styled("* ", accent()),
            Span::styled(item, text_style()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Browser", bold())));
    lines.push(kv_line(
        "page",
        state.browser.url.as_deref().unwrap_or("connecting"),
    ));
    lines.push(kv_line(
        "open",
        state
            .browser
            .live_url
            .as_deref()
            .map(|_| "live browser")
            .unwrap_or("not available yet"),
    ));
    lines
}

fn result_lines(state: &WorkbenchState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled("Result", bold())), Line::from("")];
    if let Some(result) = state.result.as_ref() {
        for line in result.lines().take(18) {
            lines.push(Line::from(line.to_string()));
        }
    } else {
        lines.push(Line::from(Span::styled("No result yet.", dim())));
    }
    if let Some(source) = state
        .browser
        .url
        .as_ref()
        .or(state.browser.live_url.as_ref())
    {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Source", bold())));
        lines.push(Line::from(Span::styled(source.clone(), link())));
    }
    lines
}

fn failure_lines(error: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled("The agent could not finish the task.", bold())),
        Line::from(""),
        Line::from(Span::styled(first_line(error), muted())),
        Line::from(""),
        Line::from("> Retry"),
        Line::from("  Sign in"),
        Line::from("  Choose model"),
        Line::from("  Change browser"),
        Line::from(""),
        Line::from(Span::styled("Work preserved in history.", muted())),
    ]
}

fn cancelled_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled("The task was stopped.", bold())),
        Line::from(""),
        Line::from(Span::styled("Work preserved in history.", muted())),
        Line::from(""),
        Line::from("> Start a follow-up"),
        Line::from("  Previous work"),
        Line::from("  Setup"),
    ]
}

fn workbench_title(app: &App, state: &WorkbenchState, width: u16) -> String {
    let max_title = width.saturating_sub(4) as usize;
    if let Some(session) = state.current_session.as_ref() {
        let status = session.status.as_str();
        let max_task = max_title.saturating_sub(status.len() + 4).max(12);
        let task = truncate(state.task.as_deref().unwrap_or("browser task"), max_task);
        truncate(&format!(" {task}  {status} "), max_title)
    } else {
        let prefix = " browser-use";
        let details = format!("{}  {}", app.browser, app.model);
        let details = truncate(&details, max_title.saturating_sub(prefix.len() + 2).max(12));
        let spaces = max_title.saturating_sub(prefix.len() + details.len());
        format!("{prefix}{}{}", " ".repeat(spaces), details)
    }
}

fn render_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    current_session: Option<&SessionMeta>,
) {
    let placeholder = if current_session.is_some_and(|session| session.status.is_active()) {
        "Type to steer the agent..."
    } else if current_session.is_some() {
        "Ask a follow-up..."
    } else {
        "Tell the browser what to do..."
    };
    let text = if app.input.is_empty() {
        vec![Line::from(vec![
            Span::styled("> ", dim()),
            Span::styled(placeholder, dim()),
        ])]
    } else {
        vec![Line::from(vec![
            Span::styled("> ", accent()),
            Span::styled(app.input.clone(), bold()),
        ])]
    };
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::bordered().style(Style::default().bg(composer_bg())))
            .style(Style::default().bg(composer_bg()))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App, state: &WorkbenchState) {
    let label = if app
        .quit_hint_until
        .is_some_and(|until| Instant::now() <= until)
    {
        "ctrl+c again to quit"
    } else if state
        .current_session
        .as_ref()
        .is_some_and(|session| session.status.is_active())
    {
        "enter steer     ctrl+c stop     f2 browser     / actions"
    } else if state.current_session.is_some() {
        "enter follow-up     f2 browser     tab history     / actions"
    } else {
        "enter run     tab history     / actions     f1 keys"
    };
    frame.render_widget(
        Paragraph::new(label)
            .style(muted())
            .alignment(Alignment::Right),
        area,
    );
}

fn render_setup(frame: &mut Frame<'_>, area: Rect, app: &App, first_run: bool) {
    if !first_run {
        frame.render_widget(Clear, area);
    }
    let inner = if first_run {
        modal(frame, centered_rect(80, 18, area), "browser-use")
    } else {
        modal(frame, area, "Setup")
    };
    let lines = vec![
        if first_run {
            Line::from(Span::styled("Set up the browser agent", bold()))
        } else {
            Line::from(Span::styled("The browser agent needs attention.", bold()))
        },
        Line::from(""),
        if first_run {
            setup_line("1", "Sign in", "Not connected")
        } else {
            setup_status_line("ok", "Browser", &format!("{} found", app.browser))
        },
        Line::from(""),
        if first_run {
            setup_line("2", "Choose model", "No model selected")
        } else {
            setup_status_line("ok", "Sign in", &app.account)
        },
        Line::from(""),
        if first_run {
            setup_line("3", "Choose browser", &format!("{} available", app.browser))
        } else {
            setup_status_line("ok", "Model", &app.model)
        },
        Line::from(""),
        if first_run {
            Line::from("> Start setup")
        } else {
            selected("Sign in", 0, app.selected_row)
        },
        if first_run {
            Line::from("")
        } else {
            selected("Choose model", 1, app.selected_row)
        },
        if first_run {
            Line::from(Span::styled("enter continue", muted()))
        } else {
            selected("Change browser", 2, app.selected_row)
        },
        if first_run {
            Line::from("")
        } else {
            Line::from("")
        },
        if first_run {
            Line::from("")
        } else {
            Line::from(Span::styled("enter fix     esc back", muted()))
        },
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_setup_complete(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(Clear, area);
    let inner = modal(frame, area, "Ready");
    let lines = vec![
        setup_status_line("ok", "Signed in", &app.account),
        setup_status_line("ok", "Model", &app.model),
        setup_status_line("ok", "Browser", &app.browser),
        Line::from(""),
        Line::from("> Start using browser-use"),
        Line::from(""),
        Line::from(Span::styled("enter continue", muted())),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_account_overlay(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(Clear, area);
    let inner = modal(frame, area, "Sign in");
    let lines = vec![
        Line::from("Choose how the agent should connect to a model."),
        Line::from(""),
        selected("Codex login", 0, app.selected_row),
        selected("Claude Code login", 1, app.selected_row),
        selected("OpenAI API key", 2, app.selected_row),
        selected("Anthropic API key", 3, app.selected_row),
        selected("OpenRouter API key", 4, app.selected_row),
        Line::from(""),
        Line::from(Span::styled("enter select     esc back", muted())),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_model_overlay(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(Clear, area);
    let inner = modal(frame, area, "Choose model");
    let mut lines = vec![
        Line::from(Span::styled("Recommended", bold())),
        Line::from(""),
    ];
    for (idx, choice) in MODEL_CHOICES.iter().enumerate() {
        lines.push(selected(choice.row, idx, app.selected_row));
    }
    lines.extend([
        Line::from(""),
        Line::from(Span::styled("Current", muted())),
        Line::from(if app.model_configured {
            format!("  {} via {}", app.model, app.account)
        } else {
            "  none".to_string()
        }),
        Line::from(""),
        Line::from(Span::styled("enter select     esc back", muted())),
    ]);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_browser_overlay(frame: &mut Frame<'_>, area: Rect, app: &App, state: &WorkbenchState) {
    frame.render_widget(Clear, area);
    let inner = modal(frame, area, "Browser");
    let mut lines = vec![
        Line::from(Span::styled("Current", bold())),
        kv_line("backend", &app.browser),
        kv_line("title", state.browser.title.as_deref().unwrap_or("unknown")),
        kv_line(
            "page",
            state.browser.url.as_deref().unwrap_or("no page yet"),
        ),
        kv_line("status", &state.browser.status),
        kv_line(
            "live",
            state.browser.live_url.as_deref().unwrap_or("not available"),
        ),
        kv_line(
            "tabs",
            &state
                .browser
                .tabs
                .map(|tabs| format!("{tabs} open"))
                .unwrap_or_else(|| "unknown".to_string()),
        ),
        kv_line(
            "viewport",
            state.browser.viewport.as_deref().unwrap_or("unknown"),
        ),
        Line::from(""),
        selected("Open browser", 0, app.selected_row),
        selected("Reconnect", 1, app.selected_row),
        selected("Change browser", 2, app.selected_row),
        Line::from(""),
        Line::from(Span::styled("enter select     esc close", muted())),
    ];
    if let Some(notice) = app.browser_notice.as_ref() {
        lines.insert(lines.len().saturating_sub(1), Line::from(""));
        lines.insert(
            lines.len().saturating_sub(1),
            Line::from(Span::styled(notice.clone(), muted())),
        );
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_browser_choice_overlay(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(Clear, area);
    let inner = modal(frame, area, "Choose browser");
    let lines = vec![
        selected(
            "Local Chrome                 visible browser on this machine",
            0,
            app.selected_row,
        ),
        selected(
            "Browser Use cloud            remote browser with live view",
            1,
            app.selected_row,
        ),
        selected(
            "Headless Chromium            background browser",
            2,
            app.selected_row,
        ),
        Line::from(""),
        Line::from(Span::styled("Current", muted())),
        Line::from(format!("  {} available", app.browser)),
        Line::from(""),
        Line::from(Span::styled("enter select     esc back", muted())),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_history_overlay(frame: &mut Frame<'_>, area: Rect, app: &App, state: &WorkbenchState) {
    frame.render_widget(Clear, area);
    let inner = modal(frame, area, "Previous work");
    let mut lines = if state.history.is_empty() {
        vec![Line::from(Span::styled("No previous work yet.", dim()))]
    } else {
        state
            .history
            .iter()
            .enumerate()
            .map(|(idx, row)| {
                let marker = if idx == app.selected_row { "> " } else { "  " };
                history_overlay_line(row, marker, inner.width.saturating_sub(4) as usize)
            })
            .collect()
    };
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "enter open     r resume     esc close",
        muted(),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_actions_overlay(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(Clear, area);
    let inner = modal(frame, area, "Actions");
    let items = [
        "New task",
        "Open browser",
        "Previous work",
        "Setup",
        "Choose model",
        "Sign in",
    ];
    let rows = items
        .iter()
        .enumerate()
        .map(|(idx, item)| ListItem::new(selected(item, idx, app.selected_row)))
        .collect::<Vec<_>>();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(List::new(rows), chunks[0]);
    frame.render_widget(
        Paragraph::new("type to search     enter select     esc close").style(muted()),
        chunks[1],
    );
}

fn render_help_overlay(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(Clear, area);
    let inner = modal(frame, area, "Keyboard");
    let rows = vec![
        ("enter", "run, follow up, confirm"),
        ("tab", "previous work"),
        ("f2", "browser"),
        ("/", "actions"),
        ("ctrl+c", "clear input, stop task, or quit"),
        ("esc", "close overlay"),
    ];
    frame.render_widget(
        Paragraph::new(
            rows.into_iter()
                .map(|(k, v)| kv_line(k, v))
                .collect::<Vec<_>>(),
        ),
        inner,
    );
}

fn render_developer_overlay(frame: &mut Frame<'_>, area: Rect, app: &App, state: &WorkbenchState) {
    frame.render_widget(Clear, area);
    let inner = modal(frame, area, "Developer");
    let mut lines = vec![Line::from(Span::styled("Events", bold())), Line::from("")];
    let Some(session) = state.current_session.as_ref() else {
        lines.push(Line::from(Span::styled("No task selected.", dim())));
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    };
    match app.store.events_for_session(&session.id) {
        Ok(events) => {
            for event in events.iter().rev().take(12).rev() {
                let payload = truncate(&event.payload.to_string(), 44);
                lines.push(Line::from(vec![
                    Span::styled(format!("{:>4}  ", event.seq), muted()),
                    Span::styled(
                        format!("{:<24}", truncate(&event.event_type, 24)),
                        text_style(),
                    ),
                    Span::styled(payload, dim()),
                ]));
            }
        }
        Err(err) => lines.push(Line::from(Span::styled(err.to_string(), dim()))),
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("esc close", muted())));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn seed_demo_if_requested(store: &Store, mode: Option<&str>) -> Result<()> {
    let Some(mode) = mode else {
        return Ok(());
    };
    if !store.list_sessions()?.is_empty() {
        return Ok(());
    }
    let session = store.create_session(None, std::env::current_dir()?)?;
    store.append_event(
        &session.id,
        "session.input",
        serde_json::json!({"text": "Find the top 5 Hacker News posts"}),
    )?;
    store.append_event(
        &session.id,
        "browser.page",
        serde_json::json!({
            "url": "https://news.ycombinator.com",
            "title": "Hacker News",
            "tabs": 1,
            "viewport": {"w": 1440, "h": 900},
        }),
    )?;
    store.append_event(
        &session.id,
        "browser.live_url",
        serde_json::json!({"live_url": "https://live.browser-use.com/?wss=example"}),
    )?;
    if mode == "done" {
        store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Top 5 Hacker News posts\n\n1. Example story\n2. Another story\n3. Browser agents in practice"}),
        )?;
    }
    Ok(())
}

fn setup_line(prefix: &str, label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("[{prefix}] "), accent()),
        Span::styled(format!("{label:<16}"), bold()),
        Span::styled(value.to_string(), muted()),
    ])
}

fn setup_status_line(prefix: &str, label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("[{prefix}] "), accent()),
        Span::styled(format!("{label:<14}"), bold()),
        Span::styled(value.to_string(), muted()),
    ])
}

fn kv_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), muted()),
        Span::styled(value.to_string(), text_style()),
    ])
}

fn history_line(row: &HistoryRow, width: usize) -> Line<'static> {
    let task_width = width.saturating_sub(20).max(12);
    Line::from(vec![
        Span::styled("> ", dim()),
        Span::styled(
            format!("{:<task_width$}", truncate(&row.task, task_width)),
            text_style(),
        ),
        Span::styled(
            format!("{:<10}", row.status.as_str()),
            status_style(row.status.as_str()),
        ),
        Span::styled("recent", muted()),
    ])
}

fn history_overlay_line(row: &HistoryRow, marker: &str, width: usize) -> Line<'static> {
    let task_width = width.saturating_sub(20).max(12);
    Line::from(vec![
        Span::styled(marker.to_string(), dim()),
        Span::styled(
            format!("{:<task_width$}", truncate(&row.task, task_width)),
            text_style(),
        ),
        Span::styled(
            format!("{:<10}", row.status.as_str()),
            status_style(row.status.as_str()),
        ),
        Span::styled("recent", muted()),
    ])
}

fn selected(text: &str, idx: usize, selected: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if idx == selected { "> " } else { "  " },
            if idx == selected { accent() } else { dim() },
        ),
        Span::styled(
            text.to_string(),
            if idx == selected {
                bold()
            } else {
                text_style()
            },
        ),
    ])
}

fn modal(frame: &mut Frame<'_>, area: Rect, title: &str) -> Rect {
    let block = Block::bordered()
        .title(title.to_string())
        .style(Style::default().fg(text()).bg(panel()));
    frame.render_widget(block, area);
    area.inner(Margin {
        vertical: 1,
        horizontal: 2,
    })
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    if max <= 3 {
        return value.chars().take(max).collect();
    }
    let mut out = value.chars().take(max - 3).collect::<String>();
    out.push_str("...");
    out
}

fn first_line(value: &str) -> String {
    value.lines().next().unwrap_or(value).to_string()
}

fn text() -> Color {
    Color::Rgb(230, 230, 235)
}

fn muted_color() -> Color {
    Color::Rgb(156, 158, 168)
}

fn dim_color() -> Color {
    Color::Rgb(102, 105, 116)
}

fn accent_color() -> Color {
    Color::Rgb(92, 156, 245)
}

fn panel() -> Color {
    Color::Rgb(30, 32, 38)
}

fn background() -> Color {
    Color::Rgb(18, 19, 23)
}

fn composer_bg() -> Color {
    Color::Rgb(28, 30, 36)
}

fn text_style() -> Style {
    Style::default().fg(text())
}

fn bold() -> Style {
    text_style().add_modifier(Modifier::BOLD)
}

fn muted() -> Style {
    Style::default().fg(muted_color())
}

fn dim() -> Style {
    Style::default().fg(dim_color())
}

fn accent() -> Style {
    Style::default()
        .fg(accent_color())
        .add_modifier(Modifier::BOLD)
}

fn link() -> Style {
    Style::default().fg(Color::Rgb(125, 180, 255))
}

fn status_style(status: &str) -> Style {
    match status {
        "done" => Style::default().fg(Color::Rgb(126, 192, 143)),
        "failed" => Style::default().fg(Color::Rgb(230, 126, 126)),
        "running" | "created" => Style::default().fg(Color::Rgb(215, 168, 79)),
        _ => muted(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_screen_starts_with_setup_when_empty() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Local Chrome".to_string(),
            dump_screen: true,
            width: 100,
            height: 28,
            select_latest: false,
            seed_demo: None,
            overlay: None,
            agent: AgentBackend::None,
        };
        let mut app = App::new(args)?;
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Set up the browser agent"));
        assert!(screen.contains("Choose model"));
        assert!(!screen.contains("session"));
        assert!(!screen.contains("artifact"));
        Ok(())
    }

    #[test]
    fn first_run_setup_flow_can_reach_ready_workbench() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Local Chrome".to_string(),
            dump_screen: true,
            width: 100,
            height: 28,
            select_latest: false,
            seed_demo: None,
            overlay: None,
            agent: AgentBackend::None,
        };
        let mut app = App::new(args)?;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.overlay, Overlay::Account);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.overlay, Overlay::Model);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.overlay, Overlay::BrowserChoice);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.overlay, Overlay::SetupComplete);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Start using browser-use"));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.overlay, Overlay::None);
        assert!(app.setup_complete);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("What should the browser do?"));
        assert!(!screen.contains("Set up the browser agent"));

        let args = Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Local Chrome".to_string(),
            dump_screen: true,
            width: 100,
            height: 28,
            select_latest: false,
            seed_demo: None,
            overlay: None,
            agent: AgentBackend::None,
        };
        let mut restarted = App::new(args)?;
        let screen = render_dump(&mut restarted)?;
        assert!(screen.contains("What should the browser do?"));
        assert!(!screen.contains("Set up the browser agent"));
        Ok(())
    }

    #[test]
    fn setup_flow_persists_account_model_and_browser_choices() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Local Chrome".to_string(),
            dump_screen: true,
            width: 100,
            height: 28,
            select_latest: false,
            seed_demo: None,
            overlay: None,
            agent: AgentBackend::None,
        };
        let mut app = App::new(args)?;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.overlay, Overlay::Account);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.account, "OpenAI API key");
        assert_eq!(
            app.store.get_setting("account")?.as_deref(),
            Some("OpenAI API key")
        );

        for _ in 0..6 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        }
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.model, "Qwen3.6 Plus");
        assert_eq!(app.account, "OpenRouter API key");
        assert_eq!(app.agent_backend, AgentBackend::Openrouter);
        assert_eq!(app.provider_model, "qwen/qwen3.6-plus");
        assert_eq!(app.overlay, Overlay::BrowserChoice);

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.overlay, Overlay::SetupComplete);
        assert_eq!(app.browser, "Browser Use cloud");
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert!(app.setup_complete);

        let restarted = App::new(Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Local Chrome".to_string(),
            dump_screen: true,
            width: 100,
            height: 28,
            select_latest: false,
            seed_demo: None,
            overlay: None,
            agent: AgentBackend::None,
        })?;
        assert_eq!(restarted.model, "Qwen3.6 Plus");
        assert_eq!(restarted.account, "OpenRouter API key");
        assert_eq!(restarted.browser, "Browser Use cloud");
        assert_eq!(restarted.agent_backend, AgentBackend::Openrouter);
        Ok(())
    }

    #[test]
    fn browser_overlay_actions_do_not_mutate_backend_choice() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Headless Chromium".to_string(),
            dump_screen: true,
            width: 120,
            height: 34,
            select_latest: false,
            seed_demo: None,
            overlay: None,
            agent: AgentBackend::None,
        };
        let mut app = App::new(args)?;
        app.setup_complete = true;
        app.store.set_setting("setup.complete", "1")?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect current page"}),
        )?;
        app.store.append_event(
            &session.id,
            "browser.live_url",
            serde_json::json!({"live_url": "https://live.browser-use.com/?wss=example"}),
        )?;
        app.store.append_event(
            &session.id,
            "browser.state",
            serde_json::json!({
                "url": "https://example.com",
                "title": "Example",
                "tabs": 2,
                "viewport": {"w": 1440, "h": 900},
            }),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.open_overlay(Overlay::Browser);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("2 open"));
        assert!(screen.contains("1440 x 900"));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.browser, "Headless Chromium");
        assert_eq!(app.overlay, Overlay::Browser);
        let events = app.store.events_for_session(&session.id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "browser.open_requested"
                && event.payload["target"] == "https://live.browser-use.com/?wss=example"
        }));

        app.selected_row = 1;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.browser, "Headless Chromium");
        let events = app.store.events_for_session(&session.id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "browser.reconnect_requested"));

        app.selected_row = 2;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.overlay, Overlay::BrowserChoice);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.browser, "Browser Use cloud");
        assert_eq!(app.overlay, Overlay::None);
        Ok(())
    }

    #[test]
    fn dump_screen_renders_result_from_sqlite_events() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Local Chrome".to_string(),
            dump_screen: true,
            width: 120,
            height: 34,
            select_latest: true,
            seed_demo: Some("done".to_string()),
            overlay: None,
            agent: AgentBackend::None,
        };
        let mut app = App::new(args)?;
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Find the top 5 Hacker News posts"));
        assert!(screen.contains("Result"));
        assert!(screen.contains("Hacker News"));
        assert!(!screen.contains("artifact"));
        assert!(!screen.contains("trace"));
        Ok(())
    }

    #[test]
    fn dump_screen_with_history_stays_on_ready_workbench_until_selected() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Local Chrome".to_string(),
            dump_screen: true,
            width: 120,
            height: 34,
            select_latest: false,
            seed_demo: Some("done".to_string()),
            overlay: None,
            agent: AgentBackend::None,
        };
        let mut app = App::new(args)?;
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("What should the browser do?"));
        assert!(screen.contains("Recent"));
        assert!(screen.contains("Find the top 5 Hacker News posts"));
        assert!(!screen.contains("Result"));
        Ok(())
    }

    #[test]
    fn history_overlay_r_resumes_selected_work() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Local Chrome".to_string(),
            dump_screen: true,
            width: 120,
            height: 34,
            select_latest: false,
            seed_demo: Some("done".to_string()),
            overlay: Some(OverlayArg::History),
            agent: AgentBackend::None,
        };
        let mut app = App::new(args)?;
        assert!(app.selected_session_id.is_none());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))?);
        assert!(app.selected_session_id.is_some());
        assert_eq!(app.overlay, Overlay::None);
        Ok(())
    }

    #[test]
    fn submitting_task_starts_background_agent_loop() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Local Chrome".to_string(),
            dump_screen: true,
            width: 120,
            height: 34,
            select_latest: false,
            seed_demo: None,
            overlay: None,
            agent: AgentBackend::Fake,
        };
        let mut app = App::new(args)?;
        app.setup_complete = true;
        app.store.set_setting("setup.complete", "1")?;
        app.input = "Open example.com".to_string();
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        let session_id = app
            .selected_session_id
            .clone()
            .context("new session selected")?;
        for _ in 0..50 {
            let session = app.store.load_session(&session_id)?.context("session")?;
            if session.status == SessionStatus::Done {
                let screen = render_dump(&mut app)?;
                assert!(screen.contains("Fake result from the Rust TUI agent loop."));
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::bail!("background fake agent did not finish");
    }

    #[test]
    fn result_composer_runs_followup_on_existing_task() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Local Chrome".to_string(),
            dump_screen: true,
            width: 120,
            height: 34,
            select_latest: true,
            seed_demo: Some("done".to_string()),
            overlay: None,
            agent: AgentBackend::Fake,
        };
        let mut app = App::new(args)?;
        app.setup_complete = true;
        app.store.set_setting("setup.complete", "1")?;
        let session_id = app
            .selected_session_id
            .clone()
            .context("seed session selected")?;
        app.input = "now summarize it shorter".to_string();
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.store.list_sessions()?.len(), 1);
        let events = app.store.events_for_session(&session_id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.followup"
                && event.payload["text"] == "now summarize it shorter"));
        for _ in 0..50 {
            let events = app.store.events_for_session(&session_id)?;
            if events
                .iter()
                .filter(|event| event.event_type == "session.done")
                .count()
                >= 2
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::bail!("follow-up fake agent did not finish");
    }

    #[test]
    fn enter_retries_failed_task_and_clears_old_failure_projection() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Local Chrome".to_string(),
            dump_screen: true,
            width: 120,
            height: 34,
            select_latest: false,
            seed_demo: None,
            overlay: None,
            agent: AgentBackend::Fake,
        };
        let mut app = App::new(args)?;
        app.setup_complete = true;
        app.store.set_setting("setup.complete", "1")?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "retry this task"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.failed",
            serde_json::json!({"error": "temporary failure"}),
        )?;
        app.selected_session_id = Some(session.id.clone());
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Retry"));
        assert!(screen.contains("temporary failure"));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        for _ in 0..50 {
            let session = app.store.load_session(&session.id)?.context("session")?;
            if session.status == SessionStatus::Done {
                let screen = render_dump(&mut app)?;
                assert!(screen.contains("Fake result from the Rust TUI agent loop."));
                assert!(!screen.contains("temporary failure"));
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::bail!("retry fake agent did not finish");
    }

    #[test]
    fn ctrl_c_stops_running_task() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Local Chrome".to_string(),
            dump_screen: true,
            width: 120,
            height: 34,
            select_latest: true,
            seed_demo: Some("running".to_string()),
            overlay: None,
            agent: AgentBackend::None,
        };
        let mut app = App::new(args)?;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))?);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("stopped"));
        let state = app.workbench_state()?;
        assert_eq!(
            state
                .current_session
                .as_ref()
                .map(|session| &session.status),
            Some(&SessionStatus::Cancelled)
        );
        Ok(())
    }

    #[test]
    fn hidden_developer_overlay_can_show_raw_events() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let args = Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Local Chrome".to_string(),
            dump_screen: true,
            width: 120,
            height: 34,
            select_latest: true,
            seed_demo: Some("done".to_string()),
            overlay: Some(OverlayArg::Developer),
            agent: AgentBackend::None,
        };
        let mut app = App::new(args)?;
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Developer"));
        assert!(screen.contains("Events"));
        assert!(screen.contains("session.input"));
        Ok(())
    }
}
