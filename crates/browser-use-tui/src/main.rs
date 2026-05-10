use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use browser_use_protocol::{project_workbench, SessionStatus, WorkbenchState};
use browser_use_store::Store;
use clap::{Parser, ValueEnum};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

mod render;
mod runtime;
mod settings;
mod theme;

use render::{render, render_dump};
use runtime::run_agent_thread;
use settings::{
    provider_model_for_display, AgentBackend, ACCOUNT_CHOICES, BROWSER_CHOICES, MODEL_CHOICES,
};

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
    fn dump_screen_projects_checked_in_legacy_events() -> Result<()> {
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
            agent: AgentBackend::None,
        };
        let mut app = App::new(args)?;
        app.setup_complete = true;
        app.store.set_setting("setup.complete", "1")?;
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/golden-events/legacy-session");
        let session = app.store.import_legacy_session(&fixture)?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Find the top Hacker News post"));
        assert!(screen.contains("Top story found"));
        assert!(screen.contains("Hacker News"));
        assert!(!screen.contains("artifact"));
        assert!(!screen.contains("trace"));

        app.open_overlay(Overlay::Browser);
        let browser_screen = render_dump(&mut app)?;
        assert!(browser_screen.contains("1 open"));
        assert!(browser_screen.contains("1440 x 900"));
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
