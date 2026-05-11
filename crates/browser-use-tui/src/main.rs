use std::fmt;
use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use browser_use_protocol::{project_workbench, SessionStatus, WorkbenchState};
use browser_use_store::Store;
use clap::{Parser, ValueEnum};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::Command;
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
    input_cursor: usize,
    input_kill_buffer: String,
    overlay: Overlay,
    selected_row: usize,
    setup_complete: bool,
    account: String,
    model: String,
    model_configured: bool,
    provider_model: String,
    browser: String,
    browser_notice: Option<String>,
    status_notice: Option<String>,
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
            input_cursor: 0,
            input_kill_buffer: String::new(),
            overlay,
            selected_row: 0,
            setup_complete,
            account,
            model,
            model_configured,
            provider_model,
            browser,
            browser_notice: None,
            status_notice: None,
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
        self.clear_input();
        if text.is_empty() {
            if let Some(session) = self
                .selected_session_id
                .as_deref()
                .and_then(|id| self.store.load_session(id).ok().flatten())
            {
                if session.status == SessionStatus::Failed {
                    if !self.ensure_agent_ready()? {
                        return Ok(());
                    }
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
            if !self.ensure_agent_ready()? {
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
        if !self.ensure_agent_ready()? {
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

    fn ensure_agent_ready(&mut self) -> Result<bool> {
        if let Some(notice) = self.auth_notice()? {
            self.status_notice = Some(notice);
            self.open_overlay(Overlay::Account);
            return Ok(false);
        }
        self.status_notice = None;
        Ok(true)
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
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(false);
        }
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
                    self.clear_input();
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
            } if self.input.is_empty() => self.open_overlay(Overlay::Developer),
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
                code: KeyCode::Up, ..
            } if self.overlay != Overlay::None || self.is_first_run_setup_visible()? => {
                self.move_selection(-1)?
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            } if self.overlay != Overlay::None || self.is_first_run_setup_visible()? => {
                self.move_selection(1)?
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } if self.is_first_run_setup_visible()? => self.execute_first_run_setup_selection()?,
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
            _ if self.overlay == Overlay::None && self.handle_composer_key(key) => {}
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.complete_demo_result()?,
            _ => {}
        }
        Ok(false)
    }

    fn handle_composer_key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::Backspace
            && key
                .modifiers
                .intersects(KeyModifiers::META | KeyModifiers::SUPER)
        {
            self.kill_input_current_line();
            return true;
        }
        if key.code == KeyCode::Delete
            && key
                .modifiers
                .intersects(KeyModifiers::META | KeyModifiers::SUPER)
        {
            self.kill_input_current_line();
            return true;
        }
        if (key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL))
            || (matches!(key.code, KeyCode::Char('\n' | '\r'))
                && key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL))
            || key_pressed(key, KeyCode::Char('j'), KeyModifiers::CONTROL)
            || key_pressed(key, KeyCode::Char('m'), KeyModifiers::CONTROL)
        {
            self.insert_input_char('\n');
            return true;
        }
        if key_pressed(key, KeyCode::Backspace, KeyModifiers::ALT)
            || key_pressed(key, KeyCode::Backspace, KeyModifiers::CONTROL)
            || key_pressed(
                key,
                KeyCode::Backspace,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )
            || key_pressed(key, KeyCode::Char('w'), KeyModifiers::CONTROL)
            || key_pressed(
                key,
                KeyCode::Char('h'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )
        {
            self.delete_input_word_backward();
            return true;
        }
        if key_pressed(key, KeyCode::Backspace, KeyModifiers::NONE)
            || key_pressed(key, KeyCode::Backspace, KeyModifiers::SHIFT)
            || key_pressed(key, KeyCode::Char('h'), KeyModifiers::CONTROL)
        {
            self.delete_input_backward();
            return true;
        }
        if key_pressed(key, KeyCode::Delete, KeyModifiers::ALT)
            || key_pressed(key, KeyCode::Delete, KeyModifiers::CONTROL)
            || key_pressed(
                key,
                KeyCode::Delete,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )
            || key_pressed(key, KeyCode::Char('d'), KeyModifiers::ALT)
        {
            self.delete_input_word_forward();
            return true;
        }
        if key_pressed(key, KeyCode::Delete, KeyModifiers::NONE)
            || key_pressed(key, KeyCode::Delete, KeyModifiers::SHIFT)
            || (key_pressed(key, KeyCode::Char('d'), KeyModifiers::CONTROL)
                && !self.input.is_empty())
        {
            self.delete_input_forward();
            return true;
        }
        if key_pressed(key, KeyCode::Char('u'), KeyModifiers::CONTROL) {
            self.clear_input_current_line_before_cursor();
            return true;
        }
        if key_pressed(key, KeyCode::Char('k'), KeyModifiers::CONTROL) {
            self.clear_input_current_line_after_cursor();
            return true;
        }
        if key_pressed(key, KeyCode::Char('y'), KeyModifiers::CONTROL) {
            self.yank_input_kill_buffer();
            return true;
        }
        if key_pressed(key, KeyCode::Left, KeyModifiers::ALT)
            || key_pressed(key, KeyCode::Left, KeyModifiers::CONTROL)
            || key_pressed(key, KeyCode::Char('b'), KeyModifiers::ALT)
        {
            self.move_input_word_left();
            return true;
        }
        if key_pressed(key, KeyCode::Right, KeyModifiers::ALT)
            || key_pressed(key, KeyCode::Right, KeyModifiers::CONTROL)
            || key_pressed(key, KeyCode::Char('f'), KeyModifiers::ALT)
        {
            self.move_input_word_right();
            return true;
        }
        if key_pressed(key, KeyCode::Left, KeyModifiers::NONE)
            || key_pressed(key, KeyCode::Char('b'), KeyModifiers::CONTROL)
        {
            self.move_input_cursor(-1);
            return true;
        }
        if key_pressed(key, KeyCode::Right, KeyModifiers::NONE)
            || key_pressed(key, KeyCode::Char('f'), KeyModifiers::CONTROL)
        {
            self.move_input_cursor(1);
            return true;
        }
        if key_pressed(key, KeyCode::Char('p'), KeyModifiers::CONTROL) {
            self.move_input_line_up();
            return true;
        }
        if key_pressed(key, KeyCode::Char('n'), KeyModifiers::CONTROL) {
            self.move_input_line_down();
            return true;
        }
        if key_pressed(key, KeyCode::Home, KeyModifiers::NONE)
            || key_pressed(key, KeyCode::Char('a'), KeyModifiers::CONTROL)
        {
            self.move_input_line_start();
            return true;
        }
        if key_pressed(key, KeyCode::End, KeyModifiers::NONE)
            || key_pressed(key, KeyCode::Char('e'), KeyModifiers::CONTROL)
        {
            self.move_input_line_end();
            return true;
        }
        if let KeyEvent {
            code: KeyCode::Char(ch),
            modifiers,
            ..
        } = key
        {
            let (_, normalized_modifiers) = normalize_key_parts(KeyCode::Char(ch), modifiers);
            if normalized_modifiers == KeyModifiers::NONE
                || normalized_modifiers == KeyModifiers::SHIFT
            {
                self.insert_input_char(ch);
                return true;
            }
        }
        false
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
                self.status_notice = self.auth_notice()?;
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
                self.status_notice = self.auth_notice()?;
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

    fn execute_first_run_setup_selection(&mut self) -> Result<()> {
        match self.selected_row.min(2) {
            0 => self.open_overlay(Overlay::Account),
            1 => self.open_overlay(Overlay::Model),
            _ => self.open_overlay(Overlay::BrowserChoice),
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
        self.browser_notice = Some(match open_external_url(target) {
            Ok(()) => format!("Opened {target}"),
            Err(error) => format!("Could not open {target}: {error}"),
        });
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

    fn selectable_row_count(&self) -> Result<usize> {
        Ok(match self.overlay {
            Overlay::None => {
                if self.is_first_run_setup_visible()? {
                    3
                } else {
                    0
                }
            }
            Overlay::Setup => 3,
            Overlay::Account => ACCOUNT_CHOICES.len(),
            Overlay::Model => MODEL_CHOICES.len(),
            Overlay::Browser => 3,
            Overlay::BrowserChoice => BROWSER_CHOICES.len(),
            Overlay::SetupComplete => 1,
            Overlay::History => self.store.list_sessions()?.len(),
            Overlay::Actions => 6,
            Overlay::Help | Overlay::Developer => 0,
        })
    }

    fn move_selection(&mut self, delta: isize) -> Result<()> {
        let count = self.selectable_row_count()?;
        if count == 0 {
            self.selected_row = 0;
            return Ok(());
        }
        let max = count.saturating_sub(1) as isize;
        self.selected_row = (self.selected_row as isize + delta).clamp(0, max) as usize;
        Ok(())
    }

    fn input_len(&self) -> usize {
        self.input.chars().count()
    }

    fn input_chars(&self) -> Vec<char> {
        self.input.chars().collect()
    }

    fn composer_height(&self) -> u16 {
        let input_lines = if self.input.is_empty() {
            1
        } else {
            self.input.split('\n').count()
        };
        input_lines.clamp(1, 10) as u16 + 2
    }

    #[cfg(test)]
    fn set_input(&mut self, value: String) {
        self.input = value;
        self.input_cursor = self.input_len();
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
    }

    fn insert_input_char(&mut self, ch: char) {
        let mut chars = self.input.chars().collect::<Vec<_>>();
        self.input_cursor = self.input_cursor.min(chars.len());
        chars.insert(self.input_cursor, ch);
        self.input = chars.into_iter().collect();
        self.input_cursor += 1;
    }

    fn move_input_cursor(&mut self, delta: isize) {
        let len = self.input_len() as isize;
        self.input_cursor = (self.input_cursor as isize + delta).clamp(0, len) as usize;
    }

    fn move_input_word_left(&mut self) {
        let chars = self.input.chars().collect::<Vec<_>>();
        self.input_cursor = self.input_cursor.min(chars.len());
        let mut cursor = self.input_cursor;
        while cursor > 0 && chars[cursor - 1].is_whitespace() {
            cursor -= 1;
        }
        while cursor > 0 && !chars[cursor - 1].is_whitespace() {
            cursor -= 1;
        }
        self.input_cursor = cursor;
    }

    fn move_input_word_right(&mut self) {
        let chars = self.input.chars().collect::<Vec<_>>();
        let len = chars.len();
        let mut cursor = self.input_cursor.min(len);
        while cursor < len && chars[cursor].is_whitespace() {
            cursor += 1;
        }
        while cursor < len && !chars[cursor].is_whitespace() {
            cursor += 1;
        }
        self.input_cursor = cursor;
    }

    fn delete_input_backward(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let mut chars = self.input_chars();
        self.input_cursor = self.input_cursor.min(chars.len());
        self.store_input_kill(chars[self.input_cursor - 1].to_string());
        chars.remove(self.input_cursor - 1);
        self.input_cursor -= 1;
        self.input = chars.into_iter().collect();
    }

    fn delete_input_forward(&mut self) {
        let mut chars = self.input_chars();
        self.input_cursor = self.input_cursor.min(chars.len());
        if self.input_cursor >= chars.len() {
            return;
        }
        self.store_input_kill(chars[self.input_cursor].to_string());
        chars.remove(self.input_cursor);
        self.input = chars.into_iter().collect();
    }

    fn delete_input_word_backward(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let mut chars = self.input_chars();
        self.input_cursor = self.input_cursor.min(chars.len());
        let mut start = self.input_cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.store_input_kill(chars[start..self.input_cursor].iter().collect());
        chars.drain(start..self.input_cursor);
        self.input_cursor = start;
        self.input = chars.into_iter().collect();
    }

    fn delete_input_word_forward(&mut self) {
        let mut chars = self.input_chars();
        let cursor = self.input_cursor.min(chars.len());
        let mut end = cursor;
        while end < chars.len() && chars[end].is_whitespace() {
            end += 1;
        }
        while end < chars.len() && !chars[end].is_whitespace() {
            end += 1;
        }
        if end == cursor {
            return;
        }
        self.store_input_kill(chars[cursor..end].iter().collect());
        chars.drain(cursor..end);
        self.input = chars.into_iter().collect();
        self.input_cursor = cursor;
    }

    fn clear_input_current_line_before_cursor(&mut self) {
        let mut chars = self.input_chars();
        let cursor = self.input_cursor.min(chars.len());
        let line_start = line_start_for(&chars, cursor);
        let range = if cursor == line_start {
            if line_start > 0 {
                Some(line_start - 1..line_start)
            } else {
                None
            }
        } else {
            Some(line_start..cursor)
        };
        let Some(range) = range else {
            return;
        };
        self.store_input_kill(chars[range.clone()].iter().collect());
        let new_cursor = range.start;
        chars.drain(range);
        self.input = chars.into_iter().collect();
        self.input_cursor = new_cursor;
    }

    fn clear_input_current_line_after_cursor(&mut self) {
        let mut chars = self.input_chars();
        let cursor = self.input_cursor.min(chars.len());
        let line_end = line_end_for(&chars, cursor);
        let range = if cursor == line_end {
            if line_end < chars.len() {
                Some(cursor..line_end + 1)
            } else {
                None
            }
        } else {
            Some(cursor..line_end)
        };
        let Some(range) = range else {
            return;
        };
        self.store_input_kill(chars[range.clone()].iter().collect());
        chars.drain(range);
        self.input = chars.into_iter().collect();
        self.input_cursor = cursor;
    }

    fn kill_input_current_line(&mut self) {
        let mut chars = self.input_chars();
        if chars.is_empty() {
            return;
        }
        let cursor = self.input_cursor.min(chars.len());
        let line_start = line_start_for(&chars, cursor);
        let line_end = line_end_for(&chars, cursor);
        let start = if line_end == chars.len() && line_start > 0 {
            line_start - 1
        } else {
            line_start
        };
        let end = if line_end < chars.len() {
            line_end + 1
        } else {
            line_end
        };
        if start >= end {
            return;
        }
        self.store_input_kill(chars[start..end].iter().collect());
        chars.drain(start..end);
        self.input = chars.into_iter().collect();
        self.input_cursor = start.min(self.input_len());
    }

    fn yank_input_kill_buffer(&mut self) {
        if self.input_kill_buffer.is_empty() {
            return;
        }
        let text = self.input_kill_buffer.clone();
        for ch in text.chars() {
            self.insert_input_char(ch);
        }
    }

    fn store_input_kill(&mut self, text: String) {
        if !text.is_empty() {
            self.input_kill_buffer = text;
        }
    }

    fn move_input_line_start(&mut self) {
        let chars = self.input_chars();
        self.input_cursor = line_start_for(&chars, self.input_cursor.min(chars.len()));
    }

    fn move_input_line_end(&mut self) {
        let chars = self.input_chars();
        self.input_cursor = line_end_for(&chars, self.input_cursor.min(chars.len()));
    }

    fn move_input_line_up(&mut self) {
        let chars = self.input_chars();
        let cursor = self.input_cursor.min(chars.len());
        let line_start = line_start_for(&chars, cursor);
        if line_start == 0 {
            return;
        }
        let column = cursor - line_start;
        let previous_line_end = line_start - 1;
        let previous_line_start = line_start_for(&chars, previous_line_end);
        self.input_cursor =
            previous_line_start + column.min(previous_line_end - previous_line_start);
    }

    fn move_input_line_down(&mut self) {
        let chars = self.input_chars();
        let cursor = self.input_cursor.min(chars.len());
        let line_start = line_start_for(&chars, cursor);
        let line_end = line_end_for(&chars, cursor);
        if line_end >= chars.len() {
            return;
        }
        let column = cursor - line_start;
        let next_line_start = line_end + 1;
        let next_line_end = line_end_for(&chars, next_line_start);
        self.input_cursor = next_line_start + column.min(next_line_end - next_line_start);
    }

    fn auth_notice(&self) -> Result<Option<String>> {
        let notice = match self.agent_backend {
            AgentBackend::Openai
                if !self.has_stored_or_env(
                    "auth.openai.api_key",
                    &["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"],
                )? =>
            {
                Some("OpenAI API key required. Run `browser-use-terminal auth login openai --api-key ...` or set OPENAI_API_KEY.")
            }
            AgentBackend::Openrouter
                if !self.has_stored_or_env(
                    "auth.openrouter.api_key",
                    &["LLM_BROWSER_OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY"],
                )? =>
            {
                Some("OpenRouter API key required. Run `browser-use-terminal auth login openrouter --api-key ...` or set OPENROUTER_API_KEY.")
            }
            AgentBackend::Anthropic
                if self.account == "Claude Code login"
                    && !self.has_stored_or_env(
                        "auth.claude_code.auth_token",
                        &[
                            "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
                            "CLAUDE_CODE_OAUTH_TOKEN",
                            "ANTHROPIC_AUTH_TOKEN",
                        ],
                    )? =>
            {
                Some("Claude Code OAuth token required. Run `claude setup-token` then `browser-use-terminal auth login claude-code --access-token ...`.")
            }
            AgentBackend::Anthropic
                if self.account != "Claude Code login"
                    && !self.has_stored_or_env(
                        "auth.anthropic.api_key",
                        &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
                    )? =>
            {
                Some("Anthropic API key required. Run `browser-use-terminal auth login anthropic --api-key ...` or set ANTHROPIC_API_KEY.")
            }
            _ => None,
        };
        Ok(notice.map(str::to_string))
    }

    fn has_stored_or_env(&self, setting_key: &str, env_names: &[&str]) -> Result<bool> {
        if self
            .store
            .get_setting(setting_key)?
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Ok(true);
        }
        Ok(env_names
            .iter()
            .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty())))
    }

    fn auth_status_for_account(&self, account: &str) -> &'static str {
        let connected = match account {
            "OpenAI API key" => self
                .has_stored_or_env(
                    "auth.openai.api_key",
                    &["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"],
                )
                .unwrap_or(false),
            "OpenRouter API key" => self
                .has_stored_or_env(
                    "auth.openrouter.api_key",
                    &["LLM_BROWSER_OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY"],
                )
                .unwrap_or(false),
            "Anthropic API key" => self
                .has_stored_or_env(
                    "auth.anthropic.api_key",
                    &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
                )
                .unwrap_or(false),
            "Claude Code login" => self
                .has_stored_or_env(
                    "auth.claude_code.auth_token",
                    &[
                        "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
                        "CLAUDE_CODE_OAUTH_TOKEN",
                        "ANTHROPIC_AUTH_TOKEN",
                    ],
                )
                .unwrap_or(false),
            "Codex login" => return "uses Codex auth",
            _ => false,
        };
        if connected {
            "connected"
        } else {
            "needs sign in"
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KeyBinding {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyBinding {
    const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    fn is_press(self, event: KeyEvent) -> bool {
        normalize_key_parts(self.code, self.modifiers)
            == normalize_key_parts(event.code, event.modifiers)
            && matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat)
    }
}

fn key_pressed(event: KeyEvent, code: KeyCode, modifiers: KeyModifiers) -> bool {
    KeyBinding::new(code, modifiers).is_press(event)
}

fn normalize_key_parts(code: KeyCode, mut modifiers: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let KeyCode::Char(ch) = code else {
        return (code, modifiers);
    };
    if modifiers.is_empty() {
        if let Some(ctrl_char) = c0_control_char_to_ctrl_char(ch) {
            return (KeyCode::Char(ctrl_char), KeyModifiers::CONTROL);
        }
    }
    if ch.is_ascii_uppercase() {
        modifiers.insert(KeyModifiers::SHIFT);
        return (KeyCode::Char(ch.to_ascii_lowercase()), modifiers);
    }
    (code, modifiers)
}

fn c0_control_char_to_ctrl_char(ch: char) -> Option<char> {
    let code = u32::from(ch);
    match code {
        0x00 => Some(' '),
        0x01..=0x1a => char::from_u32(code - 0x01 + u32::from('a')),
        0x1c..=0x1f => char::from_u32(code - 0x1c + u32::from('4')),
        _ => None,
    }
}

fn line_start_for(chars: &[char], cursor: usize) -> usize {
    let cursor = cursor.min(chars.len());
    chars[..cursor]
        .iter()
        .rposition(|ch| *ch == '\n')
        .map(|idx| idx + 1)
        .unwrap_or(0)
}

fn line_end_for(chars: &[char], cursor: usize) -> usize {
    let cursor = cursor.min(chars.len());
    chars[cursor..]
        .iter()
        .position(|ch| *ch == '\n')
        .map(|idx| cursor + idx)
        .unwrap_or(chars.len())
}

#[cfg(not(test))]
fn open_external_url(target: &str) -> Result<()> {
    let target = target.trim();
    if target.is_empty() {
        anyhow::bail!("browser target is empty");
    }
    open::that_detached(target).with_context(|| format!("launch external browser for {target}"))
}

#[cfg(test)]
fn open_external_url(target: &str) -> Result<()> {
    if target.trim().is_empty() {
        anyhow::bail!("browser target is empty");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResetKeyboardEnhancementFlags;

impl Command for ResetKeyboardEnhancementFlags {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[<u")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "keyboard enhancement reset is not implemented for legacy Windows terminals",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DisableModifyOtherKeys;

impl Command for DisableModifyOtherKeys {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[>4;0m")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "modifyOtherKeys reset is not implemented for legacy Windows terminals",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
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
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    )?;
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
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        ResetKeyboardEnhancementFlags,
        DisableModifyOtherKeys,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
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
    fn composer_supports_cursor_word_and_line_editing() -> Result<()> {
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
        app.setup_complete = true;
        app.store.set_setting("setup.complete", "1")?;

        app.set_input("hello browser world".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT))?);
        assert_eq!(app.input, "hello browser ");
        assert_eq!(app.input_cursor, app.input_len());

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))?);
        assert_eq!(app.input, "");
        assert_eq!(app.input_cursor, 0);

        app.set_input("hello world".to_string());
        app.input_cursor = 5;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char(','), KeyModifiers::NONE))?);
        assert_eq!(app.input, "hello, world");
        assert_eq!(app.input_cursor, 6);

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))?);
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(app.input_cursor, app.input_len());

        app.set_input("first line\nprefix suffix".to_string());
        app.input_cursor = "first line\nprefix ".chars().count();
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))?);
        assert_eq!(app.input_cursor, "first line\n".chars().count());

        app.set_input("first line\nprefix suffix".to_string());
        app.input_cursor = "first line\nprefix ".chars().count();
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER))?);
        assert_eq!(app.input, "first line");
        assert_eq!(app.input_cursor, "first line".chars().count());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER))?);
        assert_eq!(app.input, "");
        assert_eq!(app.input_cursor, 0);

        app.set_input("first line\nprefix suffix".to_string());
        app.input_cursor = "first line\nprefix ".chars().count();
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))?);
        assert_eq!(app.input, "first line\nsuffix");
        assert_eq!(app.input_cursor, "first line\n".chars().count());

        app.set_input("first line\nprefix suffix\nlast line".to_string());
        app.input_cursor = "first line\nprefix".chars().count();
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::SUPER))?);
        assert_eq!(app.input, "first line\nlast line");
        assert_eq!(app.input_cursor, "first line\n".chars().count());

        app.set_input("first line\nprefix suffix\nlast line".to_string());
        app.input_cursor = "first line\nprefix".chars().count();
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL))?);
        assert_eq!(app.input, "first line\nprefix\nlast line");
        assert_eq!(app.input_cursor, "first line\nprefix".chars().count());

        app.clear_input();
        assert_eq!(app.composer_height(), 3);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE))?);
        assert_eq!(app.input, "a\nb");
        assert_eq!(app.composer_height(), 4);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))?);
        assert_eq!(app.input, "a\nb\nc");

        app.input_cursor = "a\n".chars().count();
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert_eq!(app.input_cursor, "a\n".chars().count());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
        assert_eq!(app.input_cursor, "a\n".chars().count());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))?);
        assert_eq!(app.input_cursor, "a\nb\n".chars().count());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))?);
        assert_eq!(app.input_cursor, "a\n".chars().count());

        let long_input = (0..20)
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.set_input(long_input);
        assert_eq!(app.composer_height(), 12);
        Ok(())
    }

    #[test]
    fn overlay_navigation_clamps_at_menu_bounds() -> Result<()> {
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

        app.open_overlay(Overlay::Model);
        for _ in 0..50 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        }
        assert_eq!(app.selected_row, MODEL_CHOICES.len() - 1);
        for _ in 0..50 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
        }
        assert_eq!(app.selected_row, 0);

        app.open_overlay(Overlay::Actions);
        for _ in 0..50 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        }
        assert_eq!(app.selected_row, 5);

        app.open_overlay(Overlay::BrowserChoice);
        for _ in 0..50 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        }
        assert_eq!(app.selected_row, BROWSER_CHOICES.len() - 1);
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
    fn missing_openrouter_key_blocks_run_before_creating_session() -> Result<()> {
        let env_names = ["LLM_BROWSER_OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY"];
        let saved = env_names
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect::<Vec<_>>();
        for name in env_names {
            std::env::remove_var(name);
        }

        let result = (|| -> Result<()> {
            let temp = tempfile::tempdir()?;
            let args = Args {
                state_dir: temp.path().to_path_buf(),
                model: "GLM-5.1".to_string(),
                account: "OpenRouter API key".to_string(),
                browser: "Local Chrome".to_string(),
                dump_screen: true,
                width: 100,
                height: 28,
                select_latest: false,
                seed_demo: None,
                overlay: None,
                agent: AgentBackend::Openrouter,
            };
            let mut app = App::new(args)?;
            app.setup_complete = true;
            app.store.set_setting("setup.complete", "1")?;
            app.provider_model = "z-ai/glm-5.1".to_string();
            app.set_input("go to hacker news".to_string());

            anyhow::ensure!(
                !app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?,
                "enter should not quit"
            );
            anyhow::ensure!(
                app.overlay == Overlay::Account,
                "account overlay should open"
            );
            anyhow::ensure!(
                app.store.list_sessions()?.is_empty(),
                "missing auth should not create a session"
            );
            anyhow::ensure!(app
                .status_notice
                .as_deref()
                .is_some_and(|notice| notice.contains("OpenRouter API key required")));
            anyhow::ensure!(app.input.is_empty(), "input should clear after submit");
            Ok(())
        })();

        for (name, value) in saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }

        result
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
        assert_eq!(
            app.browser_notice.as_deref(),
            Some("Opened https://live.browser-use.com/?wss=example")
        );

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
    fn result_view_renders_basic_markdown_and_visible_links() -> Result<()> {
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
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "make a markdown result"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({
                "result": "# Summary\n\n- Saved [Hacker News](https://news.ycombinator.com/news)\n- Wrote `hackernews_top5.json`\n- Cart has **14 items** with subtotal **$1,240.70**"
            }),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Summary"));
        assert!(screen.contains("Saved Hacker News (https://news.ycombinator.com/news)"));
        assert!(screen.contains("Wrote hackernews_top5.json"));
        assert!(screen.contains("Cart has 14 items with subtotal $1,240.70"));
        assert!(!screen.contains("# Summary"));
        assert!(!screen.contains("**14 items**"));
        assert!(!screen.contains("**$1,240.70**"));
        Ok(())
    }

    #[test]
    fn result_and_developer_views_render_laminar_trace() -> Result<()> {
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
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "smoke telemetry"}),
        )?;
        app.store.append_event(
            &session.id,
            "telemetry.trace",
            serde_json::json!({
                "trace_id": "trace123",
                "backend": "laminar",
                "endpoint": "https://api.lmnr.ai/v1/traces",
            }),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "ok"}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Telemetry"));
        assert!(screen.contains("trace123"));

        app.open_overlay(Overlay::Developer);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Telemetry"));
        assert!(screen.contains("https://api.lmnr.ai/v1/traces"));
        assert!(screen.contains("telemetry.trace"));
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
    fn history_selected_done_session_followup_stays_in_place() -> Result<()> {
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
            agent: AgentBackend::Fake,
        };
        let mut app = App::new(args)?;
        app.setup_complete = true;
        app.store.set_setting("setup.complete", "1")?;
        let session_id = app
            .store
            .list_sessions()?
            .first()
            .context("seeded session")?
            .id
            .clone();

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(
            app.selected_session_id.as_deref(),
            Some(session_id.as_str())
        );

        app.set_input("now summarize it shorter".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.store.list_sessions()?.len(), 1);
        let events = app.store.events_for_session(&session_id)?;
        assert!(events.iter().any(|event| {
            event.event_type == "session.followup"
                && event.payload["text"] == "now summarize it shorter"
        }));
        Ok(())
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
