use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use browser_use_protocol::{
    project_workbench, EventRecord, SessionMeta, SessionStatus, WorkbenchState,
};
use browser_use_store::{Store, StoreNotification};
use clap::{Parser, ValueEnum};
use crossterm::cursor::MoveTo;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::Command;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Position, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthStr;

mod composer;
mod markdown;
mod palette;
mod render;
mod runtime;
mod settings;
mod theme;

use composer::Composer;
use palette::PaletteAction;
#[cfg(test)]
use render::native_scrollback_event_lines;
use render::{
    lines_plain_text, main_viewport_height, native_scrollback_chronological_event_lines,
    native_scrollback_lines, render, render_dump, APP_HORIZONTAL_MARGIN,
    NATIVE_TRANSCRIPT_HORIZONTAL_MARGIN,
};
use runtime::run_agent_thread;
use settings::{
    is_claude_code_account, provider_model_for_display, AgentBackend, ACCOUNT_ANTHROPIC,
    ACCOUNT_CHOICES, ACCOUNT_CODEX, ACCOUNT_OPENAI, ACCOUNT_OPENROUTER, BROWSER_CHOICES,
    MODEL_CHOICES,
};

const DOUBLE_ESCAPE_STOP_WINDOW: Duration = Duration::from_millis(1500);
const STORE_FALLBACK_REFRESH_INTERVAL: Duration = Duration::from_millis(750);
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const RESIZE_DEBOUNCE_INTERVAL: Duration = Duration::from_millis(80);

#[derive(Debug, Parser)]
#[command(name = "but", bin_name = "but")]
struct Args {
    #[arg(long, default_value = ".browser-use-terminal")]
    state_dir: PathBuf,
    #[arg(long, default_value = "GPT-5.5")]
    model: String,
    #[arg(long, default_value = "Codex login")]
    account: String,
    #[arg(long, default_value = "Browser Use cloud")]
    browser: String,
    #[arg(long)]
    dump_screen: bool,
    #[arg(long, default_value_t = 120)]
    width: u16,
    #[arg(long, default_value_t = 28)]
    height: u16,
    #[arg(long)]
    select_latest: bool,
    #[arg(long)]
    seed_demo: Option<String>,
    #[arg(long, value_enum)]
    overlay: Option<ScreenArg>,
    #[arg(long, value_enum, default_value = "codex", hide = true)]
    agent: AgentBackend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Surface {
    Main,
    Setup,
    Account,
    ApiKey,
    Telemetry,
    Model,
    Browser,
    BrowserSelect,
    History,
    Developer,
}

impl Surface {
    fn is_bottom_pane(self) -> bool {
        false
    }

    fn uses_main_view(self) -> bool {
        self == Self::Main
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ScreenArg {
    Setup,
    Account,
    Telemetry,
    Model,
    Browser,
    History,
    Developer,
}

impl From<ScreenArg> for Surface {
    fn from(value: ScreenArg) -> Self {
        match value {
            ScreenArg::Setup => Self::Setup,
            ScreenArg::Account => Self::Account,
            ScreenArg::Telemetry => Self::Telemetry,
            ScreenArg::Model => Self::Model,
            ScreenArg::Browser => Self::Browser,
            ScreenArg::History => Self::History,
            ScreenArg::Developer => Self::Developer,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProductState {
    SetupNeeded,
    Ready,
    Running,
    Result,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AppCommand {
    StartTask(String),
    SendFollowup { session_id: String, text: String },
    RetryTask(String),
    OpenBrowser,
    ReconnectBrowser,
    NewTask,
    OpenHistory,
    SelectHistory(String),
    ChangeModel,
    SignIn,
    ConfigureTelemetry,
    ChangeBrowser,
    SaveAccount(String),
    SaveModel(usize),
    SaveBrowser(usize),
    SaveAuth(String),
    SaveTelemetry(String),
}

struct App {
    store: Store,
    store_rx: mpsc::Receiver<StoreNotification>,
    state_cache: AppStateCache,
    args: Args,
    selected_session_id: Option<String>,
    composer: Composer,
    surface: Surface,
    selected_row: usize,
    setup_complete: bool,
    account: String,
    model: String,
    model_configured: bool,
    provider_model: String,
    browser: String,
    api_key_account: Option<String>,
    pending_model_after_auth: Option<usize>,
    browser_notice: Option<String>,
    status_notice: Option<String>,
    agent_backend: AgentBackend,
    quit_hint_until: Option<Instant>,
    escape_stop_until: Option<Instant>,
    native_history: NativeHistoryState,
}

#[derive(Debug)]
struct AppStateCache {
    sessions: Vec<SessionMeta>,
    events_by_session: HashMap<String, Vec<EventRecord>>,
    last_seq_by_session: HashMap<String, i64>,
    projected: WorkbenchState,
    projection_key: Option<ProjectionKey>,
    dirty_projection: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProjectionKey {
    selected_session_id: Option<String>,
    browser: String,
    history_tasks_visible: bool,
}

impl AppStateCache {
    fn hydrate(store: &Store, browser: &str) -> Result<Self> {
        let sessions = store.list_sessions()?;
        let mut events_by_session = HashMap::new();
        let mut last_seq_by_session = HashMap::new();
        for session in &sessions {
            let events = store.events_for_session(&session.id)?;
            let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
            last_seq_by_session.insert(session.id.clone(), last_seq);
            events_by_session.insert(session.id.clone(), events);
        }
        Ok(Self {
            sessions,
            events_by_session,
            last_seq_by_session,
            projected: empty_workbench_state(browser),
            projection_key: None,
            dirty_projection: true,
        })
    }

    fn apply_notification(
        &mut self,
        store: &Store,
        notification: StoreNotification,
    ) -> Result<bool> {
        match notification {
            StoreNotification::SessionsChanged => self.refresh_sessions(store),
            StoreNotification::SessionChanged { session_id } => {
                self.refresh_session(store, &session_id)
            }
            StoreNotification::EventsChanged { session_id, seq: _ } => {
                self.refresh_events_after_seq(store, &session_id)
            }
            StoreNotification::SettingsChanged => Ok(false),
        }
    }

    fn refresh_all(&mut self, store: &Store) -> Result<bool> {
        let mut changed = self.refresh_sessions(store)?;
        let session_ids = self
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        for session_id in session_ids {
            changed |= self.refresh_events_after_seq(store, &session_id)?;
        }
        Ok(changed)
    }

    fn refresh_sessions(&mut self, store: &Store) -> Result<bool> {
        let sessions = store.list_sessions()?;
        let sessions_changed = self.sessions != sessions;
        self.sessions = sessions;
        let live_ids = self
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let old_event_count = self.events_by_session.len();
        self.events_by_session
            .retain(|session_id, _| live_ids.contains(session_id.as_str()));
        self.last_seq_by_session
            .retain(|session_id, _| live_ids.contains(session_id.as_str()));
        let removed_events = self.events_by_session.len() != old_event_count;
        let unknown_ids = self
            .sessions
            .iter()
            .filter(|session| !self.events_by_session.contains_key(&session.id))
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        let loaded_events = !unknown_ids.is_empty();
        for session_id in unknown_ids {
            let events = store.events_for_session(&session_id)?;
            let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
            self.last_seq_by_session
                .insert(session_id.clone(), last_seq);
            self.events_by_session.insert(session_id, events);
        }
        let changed = sessions_changed || removed_events || loaded_events;
        if changed {
            self.dirty_projection = true;
        }
        Ok(changed)
    }

    fn refresh_session(&mut self, store: &Store, session_id: &str) -> Result<bool> {
        let changed = match store.load_session(session_id)? {
            Some(session) => self.upsert_session(session),
            None => {
                let old_len = self.sessions.len();
                self.sessions.retain(|session| session.id != session_id);
                let removed_events = self.events_by_session.remove(session_id).is_some();
                let removed_seq = self.last_seq_by_session.remove(session_id).is_some();
                old_len != self.sessions.len() || removed_events || removed_seq
            }
        };
        if changed {
            self.dirty_projection = true;
        }
        Ok(changed)
    }

    fn refresh_events_after_seq(&mut self, store: &Store, session_id: &str) -> Result<bool> {
        let after_seq = self
            .last_seq_by_session
            .get(session_id)
            .copied()
            .unwrap_or_default();
        let events = store.events_after_seq(session_id, after_seq)?;
        if events.is_empty() {
            return Ok(false);
        }
        let last_seq = events.last().map(|event| event.seq).unwrap_or(after_seq);
        self.events_by_session
            .entry(session_id.to_string())
            .or_default()
            .extend(events);
        self.last_seq_by_session
            .insert(session_id.to_string(), last_seq);
        self.dirty_projection = true;
        Ok(true)
    }

    fn upsert_session(&mut self, session: SessionMeta) -> bool {
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|candidate| candidate.id == session.id)
        {
            if *existing == session {
                return false;
            }
            *existing = session;
        } else {
            self.sessions.push(session);
        }
        self.sessions
            .sort_by(|left, right| right.updated_ms.cmp(&left.updated_ms));
        true
    }

    fn project_if_needed(
        &mut self,
        selected_session_id: Option<&str>,
        browser: &str,
        history_tasks_visible: bool,
    ) -> &WorkbenchState {
        let key = ProjectionKey {
            selected_session_id: selected_session_id.map(ToOwned::to_owned),
            browser: browser.to_string(),
            history_tasks_visible,
        };
        if !self.dirty_projection && self.projection_key.as_ref() == Some(&key) {
            return &self.projected;
        }

        let current_events = selected_session_id
            .and_then(|id| self.events_by_session.get(id))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let all_events = if history_tasks_visible {
            self.sessions
                .iter()
                .map(|session| {
                    (
                        session.id.clone(),
                        self.events_by_session
                            .get(&session.id)
                            .cloned()
                            .unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
        } else if let Some(id) = selected_session_id {
            let mut session_ids = vec![id.to_string()];
            let mut index = 0;
            while index < session_ids.len() {
                let parent_id = session_ids[index].clone();
                for session in self
                    .sessions
                    .iter()
                    .filter(|session| session.parent_id.as_deref() == Some(parent_id.as_str()))
                {
                    if !session_ids.iter().any(|id| id == &session.id) {
                        session_ids.push(session.id.clone());
                    }
                }
                index += 1;
            }
            session_ids
                .into_iter()
                .map(|session_id| {
                    (
                        session_id.clone(),
                        self.events_by_session
                            .get(&session_id)
                            .cloned()
                            .unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        self.projected = project_workbench(
            &self.sessions,
            current_events,
            &all_events,
            selected_session_id,
            browser.to_string(),
        );
        self.projection_key = Some(key);
        self.dirty_projection = false;
        &self.projected
    }

    fn events_for_session(&self, session_id: &str) -> &[EventRecord] {
        self.events_by_session
            .get(session_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

fn empty_workbench_state(browser: &str) -> WorkbenchState {
    WorkbenchState {
        setup_complete: false,
        current_session: None,
        task: None,
        result: None,
        failure: None,
        activity: Vec::new(),
        transcript: Vec::new(),
        browser: browser_use_protocol::BrowserSummary {
            backend: browser.to_string(),
            status: "not connected".to_string(),
            ..Default::default()
        },
        telemetry: Default::default(),
        history: Vec::new(),
    }
}

#[derive(Debug, Default)]
struct NativeHistoryState {
    session_id: Option<String>,
    last_seq: i64,
    last_group: Option<String>,
    clear_before_replay: bool,
}

impl NativeHistoryState {
    fn reset(&mut self) {
        self.session_id = None;
        self.last_seq = 0;
        self.last_group = None;
        self.clear_before_replay = false;
    }

    fn reset_with_clear(&mut self) {
        self.reset();
        self.clear_before_replay = true;
    }

    #[cfg(test)]
    fn reset_for_session(&mut self, session_id: String, last_seq: i64) {
        self.reset_for_session_with_group(session_id, last_seq, None);
    }

    fn reset_for_session_with_group(
        &mut self,
        session_id: String,
        last_seq: i64,
        last_group: Option<String>,
    ) {
        self.session_id = Some(session_id);
        self.last_seq = last_seq;
        self.last_group = last_group;
        self.clear_before_replay = false;
    }

    fn is_active_for(&self, session_id: Option<&str>) -> bool {
        self.session_id.as_deref().is_some() && self.session_id.as_deref() == session_id
    }

    fn take_clear_before_replay(&mut self) -> bool {
        let should_clear = self.clear_before_replay;
        self.clear_before_replay = false;
        should_clear
    }
}

impl App {
    fn new(args: Args) -> Result<Self> {
        let (store_tx, store_rx) = mpsc::channel();
        let store = Store::open_with_notifier(&args.state_dir, store_tx)?;
        seed_demo_if_requested(&store, args.seed_demo.as_deref())?;
        let state_cache = AppStateCache::hydrate(&store, &args.browser)?;
        let selected_session_id = if args.select_latest {
            state_cache
                .sessions
                .first()
                .map(|session| session.id.clone())
        } else {
            None
        };
        let surface = args.overlay.map(Into::into).unwrap_or(Surface::Main);
        let setup_complete = store.get_setting("setup.complete")?.as_deref() == Some("1");
        let account = store
            .get_setting("account")?
            .unwrap_or_else(|| args.account.clone());
        let stored_model = store.get_setting("model")?;
        let had_stored_model = stored_model.is_some();
        let model_configured = had_stored_model || setup_complete;
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
        let selected_row = 0;
        let _ = had_stored_model;
        let mut app = Self {
            store,
            store_rx,
            state_cache,
            args,
            selected_session_id,
            composer: Composer::default(),
            surface,
            selected_row,
            setup_complete,
            account,
            model,
            model_configured,
            provider_model,
            browser,
            api_key_account: None,
            pending_model_after_auth: None,
            browser_notice: None,
            status_notice: None,
            agent_backend,
            quit_hint_until: None,
            escape_stop_until: None,
            native_history: NativeHistoryState::default(),
        };
        app.refresh_cached_projection();
        Ok(app)
    }

    fn workbench_state(&mut self) -> Result<WorkbenchState> {
        Ok(self.refresh_cached_projection().clone())
    }

    fn refresh_cached_projection(&mut self) -> &WorkbenchState {
        let selected_session_id = self.selected_session_id.clone();
        let browser = self.browser.clone();
        let history_tasks_visible = self.history_tasks_are_visible();
        self.state_cache.project_if_needed(
            selected_session_id.as_deref(),
            &browser,
            history_tasks_visible,
        )
    }

    fn drain_store_notifications(&mut self) -> Result<bool> {
        let mut changed = false;
        while let Ok(notification) = self.store_rx.try_recv() {
            changed |= self
                .state_cache
                .apply_notification(&self.store, notification)?;
        }
        if changed {
            self.refresh_cached_projection();
        }
        Ok(changed)
    }

    fn refresh_state_cache_from_store(&mut self) -> Result<bool> {
        let changed = self.state_cache.refresh_all(&self.store)?;
        if changed {
            self.refresh_cached_projection();
        }
        Ok(changed)
    }

    fn cached_events_for_session(&self, session_id: &str) -> &[EventRecord] {
        self.state_cache.events_for_session(session_id)
    }

    fn empty_workbench_state_with_failure(&self) -> WorkbenchState {
        let mut state = empty_workbench_state(&self.browser);
        state.failure = Some("Could not load state.".to_string());
        state
    }

    fn history_tasks_are_visible(&self) -> bool {
        self.surface == Surface::History || self.selected_session_id.is_none()
    }

    fn open_surface(&mut self, surface: Surface) {
        self.surface = surface;
        self.selected_row = 0;
        if surface != Surface::Browser {
            self.browser_notice = None;
        }
    }

    fn close_surface(&mut self) {
        self.surface = Surface::Main;
        self.selected_row = 0;
        self.browser_notice = None;
    }

    fn submit(&mut self) -> Result<()> {
        let text = self.composer.take_trimmed();
        if text.is_empty() {
            if let Some(session) = self
                .selected_session_id
                .as_deref()
                .and_then(|id| {
                    self.state_cache
                        .sessions
                        .iter()
                        .find(|session| session.id == id)
                })
                .cloned()
            {
                if session.status == SessionStatus::Failed {
                    self.execute_failed_selection(session.id)?;
                } else if session.status == SessionStatus::Cancelled {
                    self.execute_cancelled_selection()?;
                }
            }
            return Ok(());
        }
        if let Some(session) = self
            .selected_session_id
            .as_deref()
            .and_then(|id| {
                self.state_cache
                    .sessions
                    .iter()
                    .find(|session| session.id == id)
            })
            .cloned()
        {
            self.dispatch(AppCommand::SendFollowup {
                session_id: session.id,
                text,
            })?;
            return Ok(());
        }
        self.dispatch(AppCommand::StartTask(text))?;
        Ok(())
    }

    fn ensure_agent_ready(&mut self) -> Result<bool> {
        if let Some(notice) = self.auth_notice()? {
            self.status_notice = Some(notice);
            self.open_surface(Surface::Account);
            return Ok(false);
        }
        self.status_notice = None;
        Ok(true)
    }

    fn dispatch(&mut self, command: AppCommand) -> Result<()> {
        match command {
            AppCommand::StartTask(text) => {
                if !self.ensure_agent_ready()? {
                    return Ok(());
                }
                let session = self.store.create_session(None, std::env::current_dir()?)?;
                self.store.append_event(
                    &session.id,
                    "session.input",
                    serde_json::json!({ "text": text }),
                )?;
                self.selected_session_id = Some(session.id.clone());
                self.native_history.reset_with_clear();
                self.start_agent_for_session(session.id)?;
            }
            AppCommand::SendFollowup { session_id, text } => {
                let active = self
                    .store
                    .load_session(&session_id)?
                    .is_some_and(|session| session.status.is_active());
                if !active && !self.ensure_agent_ready()? {
                    return Ok(());
                }
                self.store.append_event(
                    &session_id,
                    "session.followup",
                    serde_json::json!({ "text": text }),
                )?;
                if !active {
                    self.start_agent_for_session(session_id)?;
                }
            }
            AppCommand::RetryTask(session_id) => {
                if !self.ensure_agent_ready()? {
                    return Ok(());
                }
                self.store.append_event(
                    &session_id,
                    "session.status",
                    serde_json::json!({ "status": "running" }),
                )?;
                self.start_agent_for_session(session_id)?;
            }
            AppCommand::OpenBrowser => self.request_open_browser()?,
            AppCommand::ReconnectBrowser => self.request_reconnect_browser()?,
            AppCommand::NewTask => {
                self.selected_session_id = None;
                self.native_history.reset_with_clear();
                self.close_surface();
            }
            AppCommand::OpenHistory => self.open_surface(Surface::History),
            AppCommand::SelectHistory(session_id) => {
                self.selected_session_id = Some(session_id);
                self.native_history.reset_with_clear();
                self.close_surface();
            }
            AppCommand::ChangeModel => self.open_surface(Surface::Model),
            AppCommand::SignIn => self.open_surface(Surface::Account),
            AppCommand::ConfigureTelemetry => self.start_telemetry_entry(),
            AppCommand::ChangeBrowser => self.open_surface(Surface::BrowserSelect),
            AppCommand::SaveAccount(account) => self.save_account(account)?,
            AppCommand::SaveModel(index) => self.save_model(index)?,
            AppCommand::SaveBrowser(index) => self.save_browser(index)?,
            AppCommand::SaveAuth(secret) => self.save_auth(secret)?,
            AppCommand::SaveTelemetry(secret) => self.save_telemetry(secret)?,
        }
        self.drain_store_notifications()?;
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
        let notifier = self.store.notifier();
        thread::Builder::new()
            .name(format!("browser-use-agent-{session_id}"))
            .spawn(move || {
                if let Err(error) =
                    run_agent_thread(state_dir, session_id, backend, model, browser, notifier)
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
        if !self.current_task_is_active()? {
            return Ok(false);
        }
        self.store.request_cancel(&id, "stopped from terminal")?;
        Ok(true)
    }

    fn current_task_is_active(&self) -> Result<bool> {
        let Some(id) = self.selected_session_id.as_deref() else {
            return Ok(false);
        };
        Ok(self
            .state_cache
            .sessions
            .iter()
            .find(|session| session.id == id)
            .is_some_and(|session| session.status.is_active()))
    }

    fn escape_stop_is_pending(&self) -> bool {
        self.escape_stop_until
            .is_some_and(|until| Instant::now() <= until)
    }

    fn handle_main_escape(&mut self) -> Result<()> {
        if self.escape_stop_is_pending() {
            if self.cancel_current_task()? {
                self.escape_stop_until = None;
                self.quit_hint_until = None;
                return Ok(());
            }
        }
        self.escape_stop_until = if self.current_task_is_active()? {
            Some(Instant::now() + DOUBLE_ESCAPE_STOP_WINDOW)
        } else {
            None
        };
        self.close_surface();
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(false);
        }
        self.drain_store_notifications()?;
        if key.code != KeyCode::Esc || !key.modifiers.is_empty() {
            self.escape_stop_until = None;
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
                if !self.composer.is_empty() {
                    self.composer.clear();
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
            } if self.is_slash_palette_active() => {
                self.escape_stop_until = None;
                self.composer.clear();
                self.selected_row = 0;
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } if self.surface == Surface::ApiKey => {
                self.escape_stop_until = None;
                self.cancel_auth_entry();
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } if self.surface == Surface::Telemetry => {
                self.escape_stop_until = None;
                self.cancel_secret_entry();
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } if self.surface == Surface::Main => self.handle_main_escape()?,
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                self.escape_stop_until = None;
                self.close_surface();
            }
            KeyEvent {
                code: KeyCode::Tab, ..
            } => self.open_surface(Surface::History),
            KeyEvent {
                code: KeyCode::F(1),
                ..
            } => {}
            KeyEvent {
                code: KeyCode::F(2),
                ..
            } => self.open_surface(Surface::Browser),
            KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } if self.composer.is_empty() => self.open_surface(Surface::Developer),
            KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
                ..
            } if self.surface == Surface::History => self.resume_selected_history()?,
            KeyEvent {
                code: KeyCode::Up, ..
            } if self.is_first_run_setup_visible()? => self.move_selection(-1)?,
            KeyEvent {
                code: KeyCode::Down,
                ..
            } if self.is_first_run_setup_visible()? => self.move_selection(1)?,
            KeyEvent {
                code: KeyCode::Up, ..
            } if self.is_slash_palette_active() => self.move_slash_palette_selection(-1),
            KeyEvent {
                code: KeyCode::Down,
                ..
            } if self.is_slash_palette_active() => self.move_slash_palette_selection(1),
            KeyEvent {
                code: KeyCode::Up, ..
            } if self.surface == Surface::Main
                && !(self.composer.is_empty() && self.main_selection_count()? > 0)
                && self.composer.handle_key(key) => {}
            KeyEvent {
                code: KeyCode::Down,
                ..
            } if self.surface == Surface::Main
                && !(self.composer.is_empty() && self.main_selection_count()? > 0)
                && self.composer.handle_key(key) => {}
            KeyEvent {
                code: KeyCode::Up, ..
            } if self.surface != Surface::Main
                || self.is_first_run_setup_visible()?
                || (self.composer.is_empty() && self.main_selection_count()? > 0) =>
            {
                self.move_selection(-1)?
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            } if self.surface != Surface::Main
                || self.is_first_run_setup_visible()?
                || (self.composer.is_empty() && self.main_selection_count()? > 0) =>
            {
                self.move_selection(1)?
            }
            KeyEvent {
                code: KeyCode::Up, ..
            } if self.surface == Surface::Main => {}
            KeyEvent {
                code: KeyCode::Down,
                ..
            } if self.surface == Surface::Main => {}
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } if self.is_slash_palette_active() => self.execute_slash_palette_selection()?,
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } if self.is_first_run_setup_visible()? => self.execute_first_run_setup_selection()?,
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } if self.surface != Surface::Main => self.execute_surface_selection()?,
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => self.submit()?,
            _ if matches!(self.surface, Surface::ApiKey | Surface::Telemetry)
                && self.handle_api_key_key(key) => {}
            _ if self.surface == Surface::Main && self.composer.handle_key(key) => {
                if self.is_slash_palette_active() {
                    self.clamp_slash_palette_selection();
                }
            }
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.complete_demo_result()?,
            _ => {}
        }
        self.drain_store_notifications()?;
        Ok(false)
    }

    fn handle_paste(&mut self, text: &str) {
        match self.surface {
            Surface::Main => {
                self.composer.insert_paste(text);
                if self.is_slash_palette_active() {
                    self.clamp_slash_palette_selection();
                }
            }
            Surface::ApiKey | Surface::Telemetry => {
                self.composer.insert_paste(text);
                self.selected_row = 0;
            }
            _ => {}
        }
    }

    fn is_first_run_setup_visible(&self) -> Result<bool> {
        Ok(!self.setup_complete
            && self.surface == Surface::Main
            && self.selected_session_id.is_none()
            && self.composer.is_empty()
            && self.state_cache.sessions.is_empty())
    }

    fn execute_surface_selection(&mut self) -> Result<()> {
        match self.surface {
            Surface::History => {
                let state = self.workbench_state()?.clone();
                if let Some(row) = state
                    .history
                    .get(self.selected_row.min(state.history.len().saturating_sub(1)))
                {
                    self.dispatch(AppCommand::SelectHistory(row.session_id.clone()))?;
                }
            }
            Surface::Setup => self.execute_first_run_setup_selection()?,
            Surface::Account => {
                let account = ACCOUNT_CHOICES
                    .get(
                        self.selected_row
                            .min(ACCOUNT_CHOICES.len().saturating_sub(1)),
                    )
                    .unwrap_or(&ACCOUNT_CHOICES[0])
                    .to_string();
                self.dispatch(AppCommand::SaveAccount(account))?;
            }
            Surface::ApiKey => match self.selected_row.min(1) {
                0 => {
                    let secret = self.composer.take_trimmed();
                    self.dispatch(AppCommand::SaveAuth(secret))?;
                }
                _ => self.cancel_auth_entry(),
            },
            Surface::Telemetry => match self.selected_row.min(1) {
                0 => {
                    let secret = self.composer.take_trimmed();
                    self.dispatch(AppCommand::SaveTelemetry(secret))?;
                }
                _ => self.cancel_secret_entry(),
            },
            Surface::Model => {
                self.dispatch(AppCommand::SaveModel(self.selected_row))?;
            }
            Surface::Browser => match self.selected_row.min(2) {
                0 => self.dispatch(AppCommand::OpenBrowser)?,
                1 => self.dispatch(AppCommand::ReconnectBrowser)?,
                _ => self.dispatch(AppCommand::ChangeBrowser)?,
            },
            Surface::BrowserSelect => {
                self.dispatch(AppCommand::SaveBrowser(self.selected_row))?;
            }
            Surface::Developer => match self.selected_row.min(1) {
                0 => self.dispatch(AppCommand::ConfigureTelemetry)?,
                _ => self.close_surface(),
            },
            Surface::Main => {
                self.close_surface();
            }
        }
        Ok(())
    }

    fn execute_first_run_setup_selection(&mut self) -> Result<()> {
        let idx = self
            .selected_row
            .min(ACCOUNT_CHOICES.len().saturating_sub(1));
        let account = ACCOUNT_CHOICES[idx].to_string();
        self.dispatch(AppCommand::SaveAccount(account))
    }

    fn resume_selected_history(&mut self) -> Result<()> {
        let state = self.workbench_state()?.clone();
        if let Some(row) = state
            .history
            .get(self.selected_row.min(state.history.len().saturating_sub(1)))
        {
            self.dispatch(AppCommand::SelectHistory(row.session_id.clone()))?;
        }
        Ok(())
    }

    fn execute_failed_selection(&mut self, session_id: String) -> Result<()> {
        let state = self.workbench_state()?;
        let error = state.failure.as_deref().unwrap_or_default();
        match self.selected_row.min(3) {
            0 if error.to_ascii_lowercase().contains("browser") => {
                self.open_surface(Surface::Browser)
            }
            0 if self.auth_notice()?.is_some() => self.open_surface(Surface::Account),
            0 => self.dispatch(AppCommand::RetryTask(session_id))?,
            1 if error.to_ascii_lowercase().contains("browser") => {
                self.open_surface(Surface::BrowserSelect)
            }
            1 => self.open_surface(Surface::Model),
            2 => self.dispatch(AppCommand::RetryTask(session_id))?,
            _ => self.dispatch(AppCommand::NewTask)?,
        }
        Ok(())
    }

    fn execute_cancelled_selection(&mut self) -> Result<()> {
        match self.selected_row.min(2) {
            0 => {}
            1 => self.dispatch(AppCommand::NewTask)?,
            _ => self.dispatch(AppCommand::OpenHistory)?,
        }
        Ok(())
    }

    fn execute_palette_action(&mut self, action: PaletteAction) -> Result<()> {
        match action {
            PaletteAction::NewTask => self.dispatch(AppCommand::NewTask)?,
            PaletteAction::ChangeBrowser => self.dispatch(AppCommand::ChangeBrowser)?,
            PaletteAction::PreviousWork => self.dispatch(AppCommand::OpenHistory)?,
            PaletteAction::ChooseModel => self.dispatch(AppCommand::ChangeModel)?,
            PaletteAction::Authenticate => self.dispatch(AppCommand::SignIn)?,
            PaletteAction::ConfigureLaminar => self.dispatch(AppCommand::ConfigureTelemetry)?,
        }
        Ok(())
    }

    fn save_account(&mut self, account: String) -> Result<()> {
        self.account = account.clone();
        if self.account == ACCOUNT_CODEX {
            self.persist_runtime_settings()?;
            self.status_notice = Some("Codex login selected.".to_string());
            self.advance_after_auth()?;
            return Ok(());
        }
        self.start_auth_entry(account);
        Ok(())
    }

    fn models_for_account(account: &str) -> Vec<usize> {
        MODEL_CHOICES
            .iter()
            .enumerate()
            .filter(|(_, choice)| choice.account == account)
            .map(|(idx, _)| idx)
            .collect()
    }

    fn advance_after_auth(&mut self) -> Result<()> {
        let indices = Self::models_for_account(&self.account);
        if indices.len() == 1 {
            return self.save_model(indices[0]);
        }
        self.selected_row = 0;
        self.open_surface(Surface::Model);
        Ok(())
    }

    fn save_model(&mut self, index: usize) -> Result<()> {
        let choice = MODEL_CHOICES
            .get(index.min(MODEL_CHOICES.len().saturating_sub(1)))
            .unwrap_or(&MODEL_CHOICES[0]);
        self.model = choice.display.to_string();
        self.account = choice.account.to_string();
        self.provider_model = choice.provider_model.to_string();
        self.agent_backend = choice.backend;
        self.model_configured = true;
        self.persist_runtime_settings()?;
        if !self.account_ready(&self.account)? {
            self.pending_model_after_auth = Some(index);
            self.start_auth_entry(self.account.clone());
            return Ok(());
        }
        self.status_notice = Some(format!("Model set to {}.", self.model));
        if !self.setup_complete {
            self.setup_complete = true;
            self.store.set_setting("setup.complete", "1")?;
            self.persist_runtime_settings()?;
        }
        self.close_surface();
        Ok(())
    }

    fn save_browser(&mut self, index: usize) -> Result<()> {
        let choice = BROWSER_CHOICES
            .get(index.min(BROWSER_CHOICES.len().saturating_sub(1)))
            .unwrap_or(&BROWSER_CHOICES[0]);
        self.browser = (*choice).to_string();
        self.persist_runtime_settings()?;
        self.status_notice = Some(format!("Browser set to {}.", self.browser));
        if !self.setup_complete && self.model_configured && self.account_ready(&self.account)? {
            self.setup_complete = true;
            self.store.set_setting("setup.complete", "1")?;
            self.close_surface();
        } else if !self.setup_complete {
            self.open_surface(Surface::Setup);
        } else {
            self.close_surface();
        }
        Ok(())
    }

    fn save_auth(&mut self, secret: String) -> Result<()> {
        let Some(account) = self.api_key_account.clone() else {
            self.open_surface(Surface::Account);
            return Ok(());
        };
        if secret.trim().is_empty() {
            self.status_notice = Some(format!("{} is required.", auth_secret_label(&account)));
            self.open_surface(Surface::ApiKey);
            return Ok(());
        }
        self.store
            .set_setting(auth_setting_key(&account), secret.trim())?;
        self.account = account.clone();
        self.persist_runtime_settings()?;
        self.api_key_account = None;
        self.status_notice = Some(format!("Saved {}.", auth_secret_label(&account)));
        if let Some(index) = self.pending_model_after_auth.take() {
            return self.save_model(index);
        }
        self.advance_after_auth()
    }

    fn start_auth_entry(&mut self, account: String) {
        self.api_key_account = Some(account);
        self.composer.clear();
        self.open_surface(Surface::ApiKey);
    }

    fn cancel_auth_entry(&mut self) {
        self.api_key_account = None;
        self.pending_model_after_auth = None;
        self.cancel_secret_entry();
    }

    fn start_telemetry_entry(&mut self) {
        self.composer.clear();
        self.open_surface(Surface::Telemetry);
    }

    fn cancel_secret_entry(&mut self) {
        self.composer.clear();
        self.close_surface();
    }

    fn save_telemetry(&mut self, secret: String) -> Result<()> {
        if secret.trim().is_empty() {
            self.status_notice = Some("Laminar API key is required.".to_string());
            self.open_surface(Surface::Telemetry);
            return Ok(());
        }
        self.store
            .set_setting(LAMINAR_API_KEY_SETTING, secret.trim())?;
        self.status_notice = Some("Saved Laminar API key.".to_string());
        self.open_surface(Surface::Developer);
        Ok(())
    }

    fn handle_api_key_key(&mut self, key: KeyEvent) -> bool {
        let handled = self.composer.handle_key(key);
        if handled {
            self.selected_row = 0;
        }
        handled
    }

    fn setup_row_count(&self) -> usize {
        ACCOUNT_CHOICES.len()
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

    fn selectable_row_count(&mut self) -> Result<usize> {
        Ok(match self.surface {
            Surface::Main => {
                if self.is_first_run_setup_visible()? {
                    self.setup_row_count()
                } else {
                    self.main_selection_count()?
                }
            }
            Surface::Setup => self.setup_row_count(),
            Surface::Account => ACCOUNT_CHOICES.len(),
            Surface::ApiKey | Surface::Telemetry => 2,
            Surface::Model => MODEL_CHOICES.len(),
            Surface::Browser => 3,
            Surface::BrowserSelect => BROWSER_CHOICES.len(),
            Surface::History => self.workbench_state()?.history.len(),
            Surface::Developer => 1,
        })
    }

    fn is_slash_palette_active(&self) -> bool {
        self.surface == Surface::Main && palette::is_slash_input(self.composer.input())
    }

    fn slash_palette_items(&self) -> Vec<palette::PaletteItem> {
        palette::items_filtered(self.composer.input())
    }

    fn move_slash_palette_selection(&mut self, delta: isize) {
        let count = self.slash_palette_items().len();
        if count == 0 {
            self.selected_row = 0;
            return;
        }
        let max = count.saturating_sub(1) as isize;
        self.selected_row = (self.selected_row as isize + delta).clamp(0, max) as usize;
    }

    fn clamp_slash_palette_selection(&mut self) {
        let count = self.slash_palette_items().len();
        if count == 0 {
            self.selected_row = 0;
        } else if self.selected_row >= count {
            self.selected_row = count - 1;
        }
    }

    fn execute_slash_palette_selection(&mut self) -> Result<()> {
        let action = palette::selected_action(self.composer.input(), self.selected_row);
        if let Some(action) = action {
            self.composer.clear();
            self.selected_row = 0;
            self.execute_palette_action(action)?;
        }
        Ok(())
    }

    fn main_selection_count(&mut self) -> Result<usize> {
        let state = self.workbench_state()?;
        Ok(match self.product_state(&state) {
            ProductState::Failed => 4,
            ProductState::Cancelled => 3,
            _ => 0,
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

    #[cfg(test)]
    fn composer_height(&self) -> u16 {
        self.composer.height()
    }

    fn live_viewport_height(&self) -> u16 {
        self.args.height.clamp(8, 10)
    }

    fn native_scrollback_is_active(&self) -> bool {
        self.surface.uses_main_view()
            && self
                .native_history
                .is_active_for(self.selected_session_id.as_deref())
    }

    #[cfg(test)]
    fn set_input(&mut self, value: String) {
        self.composer.set_input(value);
    }

    #[cfg(test)]
    fn set_input_cursor(&mut self, cursor: usize) {
        self.composer.set_cursor(cursor);
    }

    fn product_state(&self, state: &WorkbenchState) -> ProductState {
        if !self.setup_complete && state.history.is_empty() && state.current_session.is_none() {
            return ProductState::SetupNeeded;
        }
        let Some(session) = state.current_session.as_ref() else {
            return ProductState::Ready;
        };
        if session.status.is_active() {
            ProductState::Running
        } else if session.status == SessionStatus::Cancelled {
            ProductState::Cancelled
        } else if state.failure.is_some() {
            ProductState::Failed
        } else {
            ProductState::Result
        }
    }

    fn should_print_and_exit(&mut self) -> Result<bool> {
        if self.surface != Surface::Main || self.is_first_run_setup_visible()? {
            return Ok(false);
        }
        let state = self.workbench_state()?;
        Ok(matches!(
            self.product_state(&state),
            ProductState::Result | ProductState::Failed | ProductState::Cancelled
        ))
    }

    fn account_ready(&self, account: &str) -> Result<bool> {
        Ok(match account {
            ACCOUNT_OPENAI => self.has_stored_or_env(
                "auth.openai.api_key",
                &["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"],
            )?,
            ACCOUNT_OPENROUTER => self.has_stored_or_env(
                "auth.openrouter.api_key",
                &["LLM_BROWSER_OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY"],
            )?,
            ACCOUNT_ANTHROPIC => self.has_stored_or_env(
                "auth.anthropic.api_key",
                &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
            )?,
            account if is_claude_code_account(account) => self.has_claude_code_oauth()?,
            ACCOUNT_CODEX => true,
            _ => false,
        })
    }

    fn auth_notice(&self) -> Result<Option<String>> {
        let notice = match self.agent_backend {
            AgentBackend::Openai
                if !self.has_stored_or_env(
                    "auth.openai.api_key",
                    &["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"],
                )? =>
            {
                Some("OpenAI API key is missing. Authenticate here before retrying.")
            }
            AgentBackend::Openrouter
                if !self.has_stored_or_env(
                    "auth.openrouter.api_key",
                    &["LLM_BROWSER_OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY"],
                )? =>
            {
                Some("OpenRouter API key is missing. Authenticate here before retrying.")
            }
            AgentBackend::Anthropic
                if is_claude_code_account(&self.account)
                    && !self.has_claude_code_oauth()? =>
            {
                Some("Claude Code login is missing. Run `browser-use-terminal auth login claude-code`.")
            }
            AgentBackend::Anthropic
                if !is_claude_code_account(&self.account)
                    && !self.has_stored_or_env(
                        "auth.anthropic.api_key",
                        &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
                    )? =>
            {
                Some("Anthropic API key is missing. Authenticate here before retrying.")
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

    fn has_claude_code_oauth(&self) -> Result<bool> {
        Ok(self.has_stored_or_env(
            "auth.claude_code.access_token",
            &[
                "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "LLM_BROWSER_ANTHROPIC_OAUTH_TOKEN",
                "ANTHROPIC_OAUTH_TOKEN",
                "ANTHROPIC_AUTH_TOKEN",
            ],
        )? || self.has_stored_or_env("auth.claude_code.auth_token", &[])?)
    }

    fn laminar_status(&self) -> Result<String> {
        if self
            .store
            .get_setting(LAMINAR_API_KEY_SETTING)?
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Ok("connected via TUI config".to_string());
        }
        if std::env::var("LMNR_PROJECT_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
            return Ok("connected via LMNR_PROJECT_API_KEY".to_string());
        }
        Ok("not connected".to_string())
    }
}

const LAMINAR_API_KEY_SETTING: &str = "telemetry.laminar.api_key";

fn auth_setting_key(account: &str) -> &'static str {
    match account {
        ACCOUNT_OPENAI => "auth.openai.api_key",
        ACCOUNT_OPENROUTER => "auth.openrouter.api_key",
        ACCOUNT_ANTHROPIC => "auth.anthropic.api_key",
        account if is_claude_code_account(account) => "auth.claude_code.access_token",
        _ => "auth.codex.placeholder",
    }
}

fn auth_secret_label(account: &str) -> &'static str {
    match account {
        ACCOUNT_OPENAI => "OpenAI API key",
        ACCOUNT_OPENROUTER => "OpenRouter API key",
        ACCOUNT_ANTHROPIC => "Anthropic API key",
        account if is_claude_code_account(account) => "Claude Code OAuth token",
        _ => "credential",
    }
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
    load_dotenv()?;
    let args = Args::parse();
    if args.dump_screen {
        let mut app = App::new(args)?;
        let text = render_dump(&mut app)?;
        print!("{text}");
        return Ok(());
    }
    let mut app = App::new(args)?;
    if app.should_print_and_exit()? {
        print_native_transcript(&mut app)?;
        return Ok(());
    }
    run_terminal(app)
}

fn load_dotenv() -> Result<()> {
    load_dotenv_path(Path::new(".env"))
}

fn load_dotenv_path(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || std::env::var_os(key).is_some() {
            continue;
        }
        let value = unquote_env_value(value.trim());
        unsafe {
            std::env::set_var(key, value);
        }
    }
    Ok(())
}

fn unquote_env_value(value: &str) -> String {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn print_native_transcript(app: &mut App) -> Result<()> {
    let width = crossterm::terminal::size()
        .map(|(width, _)| width)
        .unwrap_or(app.args.width);
    let lines = native_scrollback_lines(app, width)?;
    print!("{}", lines_plain_text(&lines));
    io::stdout().flush()?;
    Ok(())
}

fn run_terminal(mut app: App) -> Result<()> {
    let mut viewport_height = desired_terminal_viewport_height(&mut app)?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        Clear(ClearType::All),
        MoveTo(0, 0),
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    )?;
    let mut terminal = new_inline_terminal(viewport_height)?;
    let result = (|| -> Result<()> {
        let mut draw_needed = true;
        let mut last_fallback_refresh = Instant::now();
        let mut pending_resize_at: Option<Instant> = None;
        loop {
            draw_needed |= app.drain_store_notifications()?;
            if last_fallback_refresh.elapsed() >= STORE_FALLBACK_REFRESH_INTERVAL {
                draw_needed |= app.refresh_state_cache_from_store()?;
                last_fallback_refresh = Instant::now();
            }
            if let Some(resize_at) = pending_resize_at {
                if resize_at.elapsed() >= RESIZE_DEBOUNCE_INTERVAL {
                    settle_terminal_resize(&mut terminal, &mut app)?;
                    pending_resize_at = None;
                    draw_needed = true;
                }
            }
            if pending_resize_at.is_none() && draw_needed {
                let desired_height = desired_terminal_viewport_height(&mut app)?;
                if desired_height != viewport_height {
                    reset_terminal_screen(terminal.backend_mut(), ClearType::Purge)?;
                    terminal = new_inline_terminal(desired_height)?;
                    viewport_height = desired_height;
                    app.native_history.reset();
                }
                draw_terminal_frame(&mut terminal, &mut app)?;
                draw_needed = false;
            }
            let poll_interval = pending_resize_at
                .map(|resize_at| {
                    RESIZE_DEBOUNCE_INTERVAL
                        .saturating_sub(resize_at.elapsed())
                        .min(INPUT_POLL_INTERVAL)
                })
                .unwrap_or(INPUT_POLL_INTERVAL);
            if !event::poll(poll_interval)? {
                continue;
            }
            let event = event::read()?;
            if matches!(event, TermEvent::Resize(_, _)) {
                pending_resize_at = Some(Instant::now());
                continue;
            }
            if handle_terminal_event(event, &mut app, &mut terminal)? {
                break Ok(());
            }
            draw_needed = true;
        }
    })();
    let restore_result = restore_terminal(terminal.backend_mut());
    let cursor_result = terminal.show_cursor();
    restore_result?;
    cursor_result?;
    result?;
    Ok(())
}

fn new_inline_terminal(height: u16) -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    let backend = CrosstermBackend::new(io::stdout());
    Ok(Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?)
}

fn desired_terminal_viewport_height(app: &mut App) -> Result<u16> {
    let (terminal_width, terminal_height) =
        crossterm::terminal::size().unwrap_or((app.args.width, app.args.height));
    let state = app.workbench_state()?;
    let product_state = app.product_state(&state);
    let full_height = terminal_height
        .saturating_sub(1)
        .max(app.live_viewport_height());
    let app_width = terminal_width
        .saturating_sub(APP_HORIZONTAL_MARGIN.saturating_mul(2))
        .max(1);
    let dock_height = main_viewport_height(app, app_width);
    let inactive_height = match product_state {
        ProductState::Failed | ProductState::Cancelled => {
            dock_height.max(app.live_viewport_height())
        }
        _ => dock_height,
    };
    let selected_status = app.selected_session_id.as_deref().and_then(|id| {
        app.state_cache
            .sessions
            .iter()
            .find(|session| session.id == id)
            .map(|session| &session.status)
    });
    if app.surface != Surface::Main
        || app.is_first_run_setup_visible()?
        || app.selected_session_id.is_none()
        || selected_status.is_some_and(SessionStatus::is_active)
    {
        return Ok(full_height);
    }
    Ok(inactive_height)
}

fn handle_terminal_event(
    event: TermEvent,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<bool> {
    match event {
        TermEvent::Key(key) if is_escape_prefix_candidate(key, app) => {
            handle_escape_prefix_key(key, app, terminal)
        }
        TermEvent::Key(key) => app.handle_key(key),
        TermEvent::Paste(text) => {
            app.handle_paste(&text);
            Ok(false)
        }
        TermEvent::Resize(_, _) => Ok(false),
        _ => Ok(false),
    }
}

fn settle_terminal_resize(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    reset_inline_terminal_after_resize(terminal)?;
    app.native_history.reset();
    Ok(())
}

fn reset_inline_terminal_after_resize(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    reset_terminal_screen(terminal.backend_mut(), ClearType::Purge)?;
    reset_inline_viewport_origin(terminal)?;
    terminal.autoresize()?;
    terminal.clear()?;
    Ok(())
}

fn is_escape_prefix_candidate(key: KeyEvent, app: &App) -> bool {
    app.surface == Surface::Main
        && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key.code == KeyCode::Esc
        && key.modifiers.is_empty()
}

fn handle_escape_prefix_key(
    escape_key: KeyEvent,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<bool> {
    if event::poll(Duration::ZERO)? {
        let next_event = event::read()?;
        if is_unmodified_enter_event(&next_event) {
            return app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        }
        let should_quit = app.handle_key(escape_key)?;
        if should_quit {
            return Ok(true);
        }
        return handle_terminal_event(next_event, app, terminal);
    }
    app.handle_key(escape_key)
}

fn is_unmodified_enter_event(event: &TermEvent) -> bool {
    matches!(
        event,
        TermEvent::Key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press | KeyEventKind::Repeat,
            ..
        })
    )
}

fn draw_terminal_frame(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    maybe_emit_native_transcript(terminal, app)?;
    terminal.draw(|frame| render(frame, app))?;
    Ok(())
}

fn maybe_emit_native_transcript(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let size = terminal.size()?;
    let state = app.workbench_state()?;
    if !app.surface.uses_main_view() || app.is_first_run_setup_visible()? {
        return Ok(());
    }
    if app.is_slash_palette_active()
        && state
            .current_session
            .as_ref()
            .is_none_or(|session| session.status.is_active())
    {
        return Ok(());
    }
    let should_clear = app.native_history.take_clear_before_replay();
    if should_clear {
        clear_native_transcript_screen(terminal)?;
    }
    let Some(session) = state.current_session.as_ref() else {
        return Ok(());
    };

    let session_id = session.id.clone();
    let width = native_scrollback_width(size.width);

    if !app.native_history.is_active_for(Some(&session_id)) {
        let mut last_group = None;
        let (lines, last_seq) = native_scrollback_chronological_event_lines(
            app,
            &state,
            &session_id,
            0,
            width,
            &mut last_group,
        );
        insert_initial_native_lines(terminal, lines)?;
        app.native_history
            .reset_for_session_with_group(session_id, last_seq, last_group);
        return Ok(());
    }

    let after_seq = app.native_history.last_seq;
    let mut last_group = app.native_history.last_group.take();
    let (lines, last_seq) = native_scrollback_chronological_event_lines(
        app,
        &state,
        &session_id,
        after_seq,
        width,
        &mut last_group,
    );
    if last_seq <= after_seq {
        app.native_history.last_group = last_group;
        return Ok(());
    }
    app.native_history.last_seq = last_seq;
    app.native_history.last_group = last_group;
    insert_native_lines(terminal, lines)?;
    Ok(())
}

fn native_scrollback_width(terminal_width: u16) -> u16 {
    terminal_width
        .saturating_sub(NATIVE_TRANSCRIPT_HORIZONTAL_MARGIN.saturating_mul(2))
        .max(1)
}

fn clear_native_transcript_screen(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    reset_terminal_screen(terminal.backend_mut(), ClearType::Purge)?;
    reset_inline_viewport_origin(terminal)?;
    terminal.clear()?;
    Ok(())
}

fn reset_terminal_screen(
    target: &mut CrosstermBackend<io::Stdout>,
    clear_type: ClearType,
) -> Result<()> {
    execute!(
        target,
        Clear(ClearType::All),
        Clear(clear_type),
        MoveTo(0, 0)
    )?;
    Ok(())
}

fn reset_inline_viewport_origin(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    terminal.set_cursor_position(Position::ORIGIN)?;
    let size = terminal.size()?;
    let area = Rect::new(0, 0, size.width, size.height);
    terminal.resize(area)?;
    Ok(())
}

fn insert_native_lines(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    lines: Vec<Line<'static>>,
) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let height = lines.len().try_into().unwrap_or(u16::MAX).max(1);
    let hyperlinks = collect_native_hyperlink_segments(&lines);
    terminal.insert_before(height, |buf| {
        let area = buf.area.inner(Margin {
            vertical: 0,
            horizontal: NATIVE_TRANSCRIPT_HORIZONTAL_MARGIN,
        });
        Paragraph::new(lines).render(area, buf);
        apply_native_hyperlinks(buf, area, &hyperlinks);
    })?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NativeHyperlinkSegment {
    line: usize,
    start_col: usize,
    width: usize,
    target: String,
}

#[derive(Clone, Debug)]
struct PendingNativeHyperlink {
    target: String,
    segments: Vec<NativeHyperlinkSegment>,
}

#[derive(Clone, Debug)]
struct LinkSpanFragment {
    start_col: usize,
    width: usize,
    text: String,
}

fn collect_native_hyperlink_segments(lines: &[Line<'static>]) -> Vec<NativeHyperlinkSegment> {
    let mut out = Vec::new();
    let mut pending: Option<PendingNativeHyperlink> = None;

    for (line_idx, line) in lines.iter().enumerate() {
        let fragments = link_span_fragments(line);
        let line_is_wrapped_link = !fragments.is_empty() && line_has_only_link_text(line);

        if !line_is_wrapped_link {
            flush_pending_hyperlink(&mut out, &mut pending);
            for fragment in fragments {
                let trimmed = fragment.text.trim();
                if looks_like_clickable_url(trimmed) {
                    out.push(NativeHyperlinkSegment {
                        line: line_idx,
                        start_col: fragment.start_col,
                        width: fragment.width,
                        target: trimmed.to_string(),
                    });
                }
            }
            continue;
        }

        let Some(first_fragment) = fragments.first() else {
            continue;
        };
        let first_text = first_fragment.text.trim();
        if looks_like_clickable_url(first_text) {
            flush_pending_hyperlink(&mut out, &mut pending);
            pending = Some(PendingNativeHyperlink {
                target: String::new(),
                segments: Vec::new(),
            });
        } else if pending.is_none() {
            continue;
        }

        if let Some(group) = pending.as_mut() {
            for fragment in fragments {
                group.target.push_str(fragment.text.trim());
                group.segments.push(NativeHyperlinkSegment {
                    line: line_idx,
                    start_col: fragment.start_col,
                    width: fragment.width,
                    target: String::new(),
                });
            }
        }
    }

    flush_pending_hyperlink(&mut out, &mut pending);
    out
}

fn flush_pending_hyperlink(
    out: &mut Vec<NativeHyperlinkSegment>,
    pending: &mut Option<PendingNativeHyperlink>,
) {
    let Some(group) = pending.take() else {
        return;
    };
    if !looks_like_clickable_url(&group.target) {
        return;
    }
    out.extend(
        group
            .segments
            .into_iter()
            .map(|segment| NativeHyperlinkSegment {
                target: group.target.clone(),
                ..segment
            }),
    );
}

fn link_span_fragments(line: &Line<'static>) -> Vec<LinkSpanFragment> {
    let mut fragments = Vec::new();
    let mut col = 0;
    for span in &line.spans {
        let text = span.content.as_ref();
        let width = UnicodeWidthStr::width(text);
        if span.style == theme::link() && !text.trim().is_empty() && width > 0 {
            fragments.push(LinkSpanFragment {
                start_col: col,
                width,
                text: text.to_string(),
            });
        }
        col += width;
    }
    fragments
}

fn line_has_only_link_text(line: &Line<'static>) -> bool {
    line.spans
        .iter()
        .all(|span| span.content.trim().is_empty() || span.style == theme::link())
}

fn looks_like_clickable_url(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://")
}

fn apply_native_hyperlinks(buf: &mut Buffer, area: Rect, hyperlinks: &[NativeHyperlinkSegment]) {
    for segment in hyperlinks {
        let Some(y) = area.y.checked_add(segment.line as u16) else {
            continue;
        };
        if y >= area.bottom() {
            continue;
        }
        let Some(start_x) = area.x.checked_add(segment.start_col as u16) else {
            continue;
        };
        if start_x >= area.right() {
            continue;
        }
        let visible_width = segment
            .width
            .min(area.right().saturating_sub(start_x) as usize);
        if visible_width == 0 {
            continue;
        }
        let end_x = start_x + visible_width as u16 - 1;
        let Some(target) = osc8_safe_url(&segment.target) else {
            continue;
        };
        let open = format!("\x1b]8;;{target}\x1b\\");
        let close = "\x1b]8;;\x1b\\";

        if start_x == end_x {
            let symbol = buf[(start_x, y)].symbol().to_string();
            buf[(start_x, y)].set_symbol(&format!("{open}{symbol}{close}"));
            continue;
        }

        let first_symbol = buf[(start_x, y)].symbol().to_string();
        buf[(start_x, y)].set_symbol(&format!("{open}{first_symbol}"));

        let last_symbol = buf[(end_x, y)].symbol().to_string();
        buf[(end_x, y)].set_symbol(&format!("{last_symbol}{close}"));
    }
}

fn osc8_safe_url(value: &str) -> Option<String> {
    let value = value.trim();
    if !looks_like_clickable_url(value) || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.replace('\\', "%5C"))
}

fn insert_initial_native_lines(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    lines: Vec<Line<'static>>,
) -> Result<()> {
    insert_native_lines(terminal, lines)
}

fn restore_terminal(mut target: impl io::Write) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        target,
        PopKeyboardEnhancementFlags,
        ResetKeyboardEnhancementFlags,
        DisableModifyOtherKeys,
        DisableBracketedPaste,
    )?;
    Ok(())
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
    if mode == "running" {
        store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Reading the page and preparing the next browser action..."}),
        )?;
    } else if mode == "done" || mode == "followup" {
        store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Top 5 Hacker News posts\n\n1. Example story\n2. Another story\n3. Browser agents in practice"}),
        )?;
        if mode == "followup" {
            store.append_event(
                &session.id,
                "session.followup",
                serde_json::json!({"text": "Which one should I read first?"}),
            )?;
            store.append_event(
                &session.id,
                "session.done",
                serde_json::json!({"result": "Read Example story first. It has the strongest discussion and enough context to decide whether to open the others."}),
            )?;
        }
    } else if mode == "long" {
        let result = (1..=60)
            .map(|idx| format!("- scroll check line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({ "result": result }),
        )?;
    } else if mode == "failed" {
        store.append_event(
            &session.id,
            "session.failed",
            serde_json::json!({"error": "OpenRouter API key is missing"}),
        )?;
    } else if mode == "cancelled" || mode == "stopped" {
        store.request_cancel(&session.id, "stopped from terminal")?;
    }
    Ok(())
}

#[cfg(test)]
mod redesign_tests {
    use super::*;

    fn args(temp: &tempfile::TempDir) -> Args {
        Args {
            state_dir: temp.path().to_path_buf(),
            model: "GPT-5.5".to_string(),
            account: "Codex login".to_string(),
            browser: "Browser Use cloud".to_string(),
            dump_screen: true,
            width: 100,
            height: 28,
            select_latest: false,
            seed_demo: None,
            overlay: None,
            agent: AgentBackend::None,
        }
    }

    fn ready_app(temp: &tempfile::TempDir) -> Result<App> {
        let mut app = App::new(args(temp))?;
        app.setup_complete = true;
        app.model_configured = true;
        app.store.set_setting("setup.complete", "1")?;
        Ok(app)
    }

    fn row_containing(screen: &str, needle: &str) -> usize {
        screen
            .lines()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("screen did not contain {needle:?}\n{screen}"))
    }

    #[test]
    fn dotenv_loader_sets_missing_env_vars_without_overriding_existing_values() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let loaded_key = format!("BUT_DOTENV_LOADED_{}", std::process::id());
        let existing_key = format!("BUT_DOTENV_EXISTING_{}", std::process::id());
        unsafe {
            std::env::remove_var(&loaded_key);
            std::env::set_var(&existing_key, "already-exported");
        }
        let result = (|| -> Result<()> {
            let path = temp.path().join(".env");
            std::fs::write(
                &path,
                format!(
                    "# comments are ignored\n{loaded_key}=\"from dotenv\"\n{existing_key}=from-file\nMALFORMED_LINE\n",
                ),
            )?;

            load_dotenv_path(&path)?;

            assert_eq!(std::env::var(&loaded_key).as_deref(), Ok("from dotenv"));
            assert_eq!(
                std::env::var(&existing_key).as_deref(),
                Ok("already-exported")
            );
            Ok(())
        })();
        unsafe {
            std::env::remove_var(&loaded_key);
            std::env::remove_var(&existing_key);
        }
        result
    }

    #[test]
    fn first_run_setup_is_activation_not_completion_modal() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = App::new(args(&temp))?;
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("step 1/3"));
        assert!(screen.contains("CHOOSE ACCOUNT"));
        assert!(screen.contains("Codex login"));
        assert!(screen.contains("Claude Code subscription"));
        assert!(screen.contains("OpenRouter API key"));
        assert!(!screen.contains("[needs]"));

        // Up/Down must navigate the 5 onboarding rows and clamp at edges.
        assert_eq!(app.selected_row, 0);
        for _ in 0..50 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        }
        assert_eq!(app.selected_row, ACCOUNT_CHOICES.len() - 1);
        for _ in 0..50 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
        }
        assert_eq!(app.selected_row, 0);

        // Default row 0 = Codex login -> single-model account -> auto-pick GPT-5.5 and finish setup.
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::Main);
        assert!(app.setup_complete);
        assert_eq!(app.account, "Codex login");
        assert_eq!(app.model, "GPT-5.5");
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Tell the browser what to do..."));
        assert!(screen.contains("Browser Use cloud"));
        Ok(())
    }

    #[test]
    fn account_flow_collects_api_key_inline() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = App::new(args(&temp))?;
        app.open_surface(Surface::Account);
        app.selected_row = 4;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::ApiKey);
        for ch in "sk-or-v1-test".chars() {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?);
        }
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("OpenRouter API key"));
        assert!(screen.contains("sk-or-v1"));
        assert!(!screen.contains("sk-or-v1-test"));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(
            app.store.get_setting("auth.openrouter.api_key")?.as_deref(),
            Some("sk-or-v1-test")
        );
        assert_eq!(app.surface, Surface::Model);
        Ok(())
    }

    #[test]
    fn model_selection_routes_to_required_sign_in() -> Result<()> {
        let saved = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::remove_var("OPENROUTER_API_KEY");
        let result = (|| -> Result<()> {
            let temp = tempfile::tempdir()?;
            let mut app = App::new(args(&temp))?;
            app.open_surface(Surface::Model);
            app.selected_row = 7;
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
            assert_eq!(app.model, "Kimi K2.5");
            assert_eq!(app.account, "OpenRouter API key");
            assert_eq!(app.surface, Surface::ApiKey);
            Ok(())
        })();
        if let Some(value) = saved {
            std::env::set_var("OPENROUTER_API_KEY", value);
        }
        result
    }

    #[test]
    fn result_screen_is_transcript_first_and_markdown_is_clean() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect cart"}),
        )?;
        app.store.append_event(
            &session.id,
            "browser.state",
            serde_json::json!({"url": "https://example.com/cart", "title": "Cart", "tabs": 1, "viewport": {"w": 1440, "h": 900}}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Your cart has **14 items**.\n\n- [Example item](https://example.com/item) with `coupon.json`\n- /tmp/cart.json"}),
        )?;
        app.selected_session_id = Some(session.id);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("inspect cart"));
        assert!(screen.contains(": browser"));
        assert!(!screen.contains(": answer"));
        assert!(screen.contains("source https://example.com/cart"));
        assert!(screen.contains("Your cart has 14 items."));
        assert!(screen.contains("Example item (https://example.com/item)"));
        assert!(screen.contains("/tmp/cart.json"));
        assert!(!screen.contains("**14 items**"));
        assert!(!screen.contains("`coupon.json`"));
        assert!(!screen.contains("┌"));
        Ok(())
    }

    #[test]
    fn idle_and_completed_screens_stay_top_aligned() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.args.height = 44;
        let running_session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &running_session.id,
            "session.input",
            serde_json::json!({"text": "run near the top"}),
        )?;
        app.store.append_event(
            &running_session.id,
            "browser.state",
            serde_json::json!({"url": "https://example.com", "title": "Example"}),
        )?;
        app.selected_session_id = Some(running_session.id);
        let running_screen = render_dump(&mut app)?;
        assert!(running_screen.contains("> run near the top"));
        assert!(running_screen.contains(": browser"));
        assert!(!running_screen.contains(": thought"));
        let running_composer_row = row_containing(&running_screen, "Type to steer the agent...");
        assert!(!running_screen.contains("Processing browser task"));
        assert!(!running_screen.contains("AI ENGINE"));
        let running_activity_row = row_containing(&running_screen, "opened example.com");
        assert!(running_composer_row > running_activity_row);
        assert!(running_composer_row.saturating_sub(running_activity_row) <= 8);

        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect top alignment"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Everything should sit near the top."}),
        )?;

        app.selected_session_id = None;
        let ready_screen = render_dump(&mut app)?;
        assert!(ready_screen.contains("browser-use"));
        assert!(ready_screen.contains("GPT-5.5 . Codex . Browser Use cloud idle"));
        assert!(ready_screen.contains("Browser Use"));
        assert!(ready_screen.contains("/model"));
        assert!(ready_screen.contains("/browser"));
        assert!(row_containing(&ready_screen, "recent") <= 14);
        assert!(ready_screen.contains("Tell the browser what to do..."));
        assert!(ready_screen.contains("Enter:send"));
        assert!(ready_screen.contains("Tab:history"));
        assert!(!ready_screen.contains("[ new task ]"));

        app.selected_session_id = Some(session.id);
        let completed_screen = render_dump(&mut app)?;
        assert!(completed_screen.contains("inspect top alignment"));
        assert!(!completed_screen.contains(": answer"));
        assert!(!completed_screen.contains(": done"));
        let composer_row = row_containing(&completed_screen, "Ask a follow-up...");
        let result_row = row_containing(&completed_screen, "Everything should sit near the top.");
        assert!(composer_row > result_row);
        Ok(())
    }

    #[test]
    fn helper_completion_renders_as_result_not_activity_blob() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "what is in this repo?"}),
        )?;
        app.store.append_event(
            &session.id,
            "agent.spawned",
            serde_json::json!({"child_session_id": "child", "nickname": "repo-explorer"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.followup",
            serde_json::json!({"text": "whats happening"}),
        )?;
        app.store.append_event(
            &session.id,
            "agent.completed",
            serde_json::json!({
                "child_session_id": "child",
                "payload": {
                    "result": "Repository summary:\n\n- **Purpose:** Rust-first terminal workbench\n- `crates/browser-use-tui` owns the UI"
                },
            }),
        )?;
        app.selected_session_id = Some(session.id);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("whats happening"));
        assert!(screen.contains(": subagent repo-explorer"));
        assert!(!screen.contains(": answer"));
        assert!(screen.contains("Purpose: Rust-first terminal workbench"));
        assert!(screen.contains("crates/browser-use-tui"));
        assert!(screen.contains("started"));
        assert!(screen.contains("finished"));
        assert!(!screen.contains("repo-explorer started"));
        assert!(!screen.contains("repo-explorer finished"));
        assert!(!screen.contains("helper finished: Repository summary"));
        assert!(!screen.contains("**Purpose:**"));
        Ok(())
    }

    #[test]
    fn command_palette_filters_and_exposes_only_product_actions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?);
        assert!(app.is_slash_palette_active());
        let screen = render_dump(&mut app)?;
        let input_row = row_containing(&screen, "> /");
        assert!(screen
            .lines()
            .enumerate()
            .any(|(idx, line)| idx >= input_row && line.contains("/task")));
        assert!(screen.contains("/task"));
        assert!(screen.contains("/history"));
        assert!(screen.contains("/browser"));
        assert!(screen.contains("/model"));
        assert!(screen.contains("/auth"));
        assert!(screen.contains("/laminar"));
        assert!(screen.contains("start a new task"));
        assert!(screen.contains("change browser backend"));
        assert!(!screen.contains("filter actions"));
        assert!(!screen.contains("tab history"));
        assert!(!screen.contains("Open browser"));
        assert!(!screen.contains("Reconnect browser"));
        for ch in "mo".chars() {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))?);
        }
        let screen = render_dump(&mut app)?;
        let input_row = row_containing(&screen, "> /mo");
        assert!(screen
            .lines()
            .enumerate()
            .any(|(idx, line)| idx >= input_row && line.contains("/model")));
        assert!(screen.contains("/model"));
        Ok(())
    }

    #[test]
    fn history_selection_uses_projected_root_task_rows() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let parent = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &parent.id,
            "session.input",
            serde_json::json!({"text": "parent task"}),
        )?;
        let child = app.store.create_child_session(
            &parent.id,
            std::env::current_dir()?,
            Some("/root/repo-explorer"),
            Some("repo-explorer"),
            Some("explorer"),
        )?;
        app.store.append_event(
            &child.id,
            "session.input",
            serde_json::json!({"text": "child helper task"}),
        )?;
        app.store.append_event(
            &child.id,
            "session.cancelled",
            serde_json::json!({"reason": "test"}),
        )?;
        app.drain_store_notifications()?;
        app.open_surface(Surface::History);

        let state = app.workbench_state()?.clone();
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].session_id, parent.id);

        app.resume_selected_history()?;
        assert_eq!(app.selected_session_id.as_deref(), Some(parent.id.as_str()));

        app.selected_session_id = None;
        app.open_surface(Surface::History);
        app.execute_surface_selection()?;
        assert_eq!(app.selected_session_id.as_deref(), Some(parent.id.as_str()));
        Ok(())
    }

    #[test]
    fn provider_auth_surfaces_explain_required_credentials() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;

        app.start_auth_entry(settings::ACCOUNT_OPENROUTER.to_string());
        assert_eq!(app.surface, Surface::ApiKey);
        assert_eq!(
            app.api_key_account.as_deref(),
            Some(settings::ACCOUNT_OPENROUTER)
        );
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("OpenRouter API key"));

        app.start_auth_entry(settings::ACCOUNT_CLAUDE_CODE.to_string());
        assert_eq!(app.surface, Surface::ApiKey);
        assert_eq!(
            app.api_key_account.as_deref(),
            Some(settings::ACCOUNT_CLAUDE_CODE)
        );
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Claude Code OAuth token"));
        assert!(screen.contains("Claude Code uses Browser Use's Anthropic OAuth login"));
        assert!(screen.contains("browser-use-terminal auth login claude-code"));
        assert!(screen.contains("refreshable Claude Code credential"));
        assert!(screen.contains("optional legacy access token"));
        Ok(())
    }

    #[test]
    fn credential_action_rows_are_real_menu_choices() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;

        app.start_auth_entry(settings::ACCOUNT_CLAUDE_CODE.to_string());
        assert_eq!(app.surface, Surface::ApiKey);
        assert_eq!(app.selected_row, 0);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("> Save key"));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert_eq!(app.selected_row, 1);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("> Cancel"));
        assert!(!screen.contains("> Save key"));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
        assert_eq!(app.selected_row, 0);
        app.selected_row = 1;
        app.handle_paste("legacy_token");
        assert_eq!(app.selected_row, 0);

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::Main);
        assert_eq!(app.api_key_account, None);
        assert!(app.composer.is_empty());
        assert_eq!(
            app.store.get_setting("auth.claude_code.access_token")?,
            None
        );

        app.open_surface(Surface::Developer);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::Telemetry);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("> Cancel"));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        assert_eq!(app.surface, Surface::Main);
        assert_eq!(app.store.get_setting(LAMINAR_API_KEY_SETTING)?, None);
        Ok(())
    }

    #[test]
    fn setup_surface_enter_matches_visible_account_choice() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.open_surface(Surface::Setup);
        app.selected_row = 1;

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Claude Code subscription"));
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);

        assert_eq!(app.surface, Surface::ApiKey);
        assert_eq!(
            app.api_key_account.as_deref(),
            Some(settings::ACCOUNT_CLAUDE_CODE)
        );
        Ok(())
    }

    #[test]
    fn up_down_keys_navigate_every_choice_menu() -> Result<()> {
        fn assert_nav(app: &mut App, expected_count: usize) -> Result<()> {
            app.selected_row = 0;
            for _ in 0..50 {
                assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
            }
            assert_eq!(app.selected_row, expected_count - 1);
            for _ in 0..50 {
                assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
            }
            assert_eq!(app.selected_row, 0);
            Ok(())
        }

        let first_run_temp = tempfile::tempdir()?;
        let mut first_run_app = App::new(args(&first_run_temp))?;
        assert_nav(&mut first_run_app, ACCOUNT_CHOICES.len())?;

        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        for surface in [
            Surface::Setup,
            Surface::Account,
            Surface::Model,
            Surface::Browser,
            Surface::BrowserSelect,
        ] {
            app.open_surface(surface);
            let count = match surface {
                Surface::Setup | Surface::Account => ACCOUNT_CHOICES.len(),
                Surface::Model => MODEL_CHOICES.len(),
                Surface::Browser | Surface::BrowserSelect => BROWSER_CHOICES.len(),
                _ => unreachable!(),
            };
            assert_nav(&mut app, count)?;
        }

        app.start_auth_entry(settings::ACCOUNT_CLAUDE_CODE.to_string());
        assert_nav(&mut app, 2)?;
        app.cancel_auth_entry();
        app.start_telemetry_entry();
        assert_nav(&mut app, 2)?;
        app.cancel_secret_entry();

        for idx in 0..3 {
            let session = app.store.create_session(None, std::env::current_dir()?)?;
            app.store.append_event(
                &session.id,
                "session.input",
                serde_json::json!({"text": format!("history task {idx}")}),
            )?;
        }
        app.open_surface(Surface::History);
        assert_nav(&mut app, 3)?;
        app.close_surface();

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?);
        let slash_palette_count = app.slash_palette_items().len();
        assert_nav(&mut app, slash_palette_count)?;
        app.composer.clear();
        app.selected_row = 0;

        let failed = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &failed.id,
            "session.input",
            serde_json::json!({"text": "failed task"}),
        )?;
        app.store.append_event(
            &failed.id,
            "session.failed",
            serde_json::json!({"error": "OpenRouter API key is missing"}),
        )?;
        app.selected_session_id = Some(failed.id);
        assert_nav(&mut app, 4)?;

        let cancelled = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &cancelled.id,
            "session.input",
            serde_json::json!({"text": "cancelled task"}),
        )?;
        app.store.request_cancel(&cancelled.id, "test cancel")?;
        app.selected_session_id = Some(cancelled.id);
        assert_nav(&mut app, 3)?;
        Ok(())
    }

    #[test]
    fn action_and_model_selection_clamp_at_edges() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?);
        for _ in 0..50 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        }
        assert_eq!(app.selected_row, app.slash_palette_items().len() - 1);
        for _ in 0..50 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
        }
        assert_eq!(app.selected_row, 0);
        app.composer.clear();
        app.selected_row = 0;

        app.open_surface(Surface::Model);
        for _ in 0..50 {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        }
        assert_eq!(app.selected_row, MODEL_CHOICES.len() - 1);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("> DeepSeek V4 Pro"));
        Ok(())
    }

    #[test]
    fn browser_panel_actions_record_explicit_events() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect"}),
        )?;
        app.store.append_event(
            &session.id,
            "browser.live_url",
            serde_json::json!({"live_url": "https://live.browser-use.com/?wss=example"}),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.open_surface(Surface::Browser);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        app.selected_row = 1;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        let events = app.store.events_for_session(&session.id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "browser.open_requested"));
        assert!(events
            .iter()
            .any(|event| event.event_type == "browser.reconnect_requested"));
        Ok(())
    }

    #[test]
    fn laminar_key_can_be_saved_from_developer_surface() -> Result<()> {
        let saved = std::env::var("LMNR_PROJECT_API_KEY").ok();
        std::env::remove_var("LMNR_PROJECT_API_KEY");
        let result = (|| -> Result<()> {
            let temp = tempfile::tempdir()?;
            let mut app = ready_app(&temp)?;
            app.open_surface(Surface::Developer);
            let screen = render_dump(&mut app)?;
            assert!(screen.contains("not connected"));
            assert!(screen.contains("Configure Laminar"));

            assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
            assert_eq!(app.surface, Surface::Telemetry);
            app.handle_paste("lmnr_test_key");
            let screen = render_dump(&mut app)?;
            assert!(screen.contains("Laminar API key"));
            assert!(screen.contains("lmnr_tes"));
            assert!(!screen.contains("lmnr_test_key"));

            assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
            assert_eq!(
                app.store.get_setting(LAMINAR_API_KEY_SETTING)?.as_deref(),
                Some("lmnr_test_key")
            );
            assert_eq!(app.surface, Surface::Developer);
            let screen = render_dump(&mut app)?;
            assert!(screen.contains("connected via TUI config"));
            Ok(())
        })();
        if let Some(value) = saved {
            std::env::set_var("LMNR_PROJECT_API_KEY", value);
        }
        result
    }

    #[test]
    fn composer_keeps_codex_like_multiline_behavior() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.set_input("hello browser world".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT))?);
        assert_eq!(app.composer.input(), "hello browser ");
        assert_eq!(app.composer.cursor(), app.composer.input_len());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))?);
        assert_eq!(app.composer.input(), "");

        app.set_input("first line\nprefix suffix".to_string());
        app.set_input_cursor("first line\nprefix ".chars().count());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER))?);
        assert_eq!(app.composer.input(), "first line");

        app.set_input("a\nb".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))?);
        assert_eq!(app.composer.input(), "a\n");
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))?);
        assert_eq!(app.composer.input(), "a");

        app.set_input("a".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE))?);
        assert_eq!(app.composer.input(), "a\nb");

        app.set_input("option".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))?);
        assert_eq!(app.composer.input(), "option\nn");

        app.set_input("meta".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::META))?);
        assert_eq!(app.composer.input(), "meta\n");

        app.set_input("alt-cr".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('\r'), KeyModifiers::ALT))?);
        assert_eq!(app.composer.input(), "alt-cr\n");

        app.set_input("a\nb".to_string());
        assert_eq!(app.composer_height(), 4);
        let rendered_input = lines_plain_text(&app.composer.render_lines(10, "placeholder"));
        assert!(rendered_input.contains("> a"));
        assert!(rendered_input.contains("  b"));
        assert!(!rendered_input.contains('|'));

        app.handle_paste(" pasted\ntext");
        assert_eq!(app.composer.input(), "a\nb pasted\ntext");
        let rendered_paste = lines_plain_text(&app.composer.render_lines(10, "placeholder"));
        assert!(rendered_paste.contains("  b pasted"));
        assert!(!rendered_paste.contains('|'));

        app.set_input("first\nsecond".to_string());
        app.set_input_cursor(app.composer.input_len());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))?);
        assert_eq!(app.composer.cursor(), "first".chars().count());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))?);
        assert_eq!(app.composer.cursor(), app.composer.input_len());
        Ok(())
    }

    #[test]
    fn long_results_use_terminal_scrollback_not_internal_scroll() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let app_args = Args {
            height: 12,
            width: 80,
            ..args(&temp)
        };
        let mut app = App::new(app_args)?;
        app.setup_complete = true;
        app.model_configured = true;
        app.store.set_setting("setup.complete", "1")?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "summarize a long page"}),
        )?;
        let result = (1..=40)
            .map(|idx| format!("- line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({ "result": result }),
        )?;
        app.selected_session_id = Some(session.id);
        let lines = native_scrollback_lines(&mut app, 80)?;
        let text = format!("{lines:?}");
        assert!(lines.len() > app.args.height as usize);
        assert!(text.contains("line 1"));
        assert!(text.contains("line 40"));
        Ok(())
    }

    #[test]
    fn activity_rendering_does_not_cap_or_compact_steps() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "exercise all activity rows"}),
        )?;
        for idx in 1..=14 {
            app.store.append_event(
                &session.id,
                "browser.state",
                serde_json::json!({"url": format!("https://example.com/page-{idx}")}),
            )?;
        }
        app.store.append_event(
            &session.id,
            "model.delta",
            serde_json::json!({"text": "result token"}),
        )?;
        app.selected_session_id = Some(session.id);
        let lines = native_scrollback_lines(&mut app, 120)?;
        let text = lines_plain_text(&lines);
        assert!(!text.contains("earlier steps"));
        assert!(!text.contains("writing result ("));
        assert!(!text.contains("writing result"));
        assert!(!text.contains("using browser"));
        assert_eq!(text.matches("opened example.com/page-").count(), 14);
        Ok(())
    }

    #[test]
    fn model_waits_are_visible_as_thinking_activity() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "wait on the model"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains(": thinking"));
        assert!(!screen.contains(": thought"));
        assert!(screen.contains("waiting for GPT-5.5"));
        Ok(())
    }

    #[test]
    fn provider_thinking_deltas_render_as_thought_not_status() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "think visibly"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.thinking_delta",
            serde_json::json!({"text": "Checking ", "label": "inspecting context"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.thinking_delta",
            serde_json::json!({"text": "Checking the repository structure.", "label": "inspecting context"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "This is the answer draft."}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains(": thinking"));
        assert!(screen.contains(": thought inspecting context"));
        assert!(screen.contains("Checking the repository structure."));
        assert!(!screen.contains("Checking \n"));
        assert!(!screen.contains(": answer draft"));
        assert!(screen.contains("This is the answer draft."));
        Ok(())
    }

    #[test]
    fn streaming_model_text_is_visible_while_task_runs() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "write as it streams"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Streaming "}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Streaming draft answer"}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(!screen.contains(": answer draft"));
        assert!(screen.contains("Streaming draft answer"));
        assert!(!screen.contains("Streaming \n"));
        Ok(())
    }

    #[test]
    fn pre_tool_streaming_text_renders_as_model_note_not_answer() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect the repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex", "turn_idx": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Need", "turn_idx": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": " more targeted.", "turn_idx": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.response",
            serde_json::json!({"turn_idx": 0, "tool_call_count": 1, "text_delta_chars": 19}),
        )?;
        app.store.append_event(
            &session.id,
            "model.tool_call",
            serde_json::json!({"name": "read_file", "arguments": {"path": "README.md"}}),
        )?;
        app.store.append_event(
            &session.id,
            "file.read",
            serde_json::json!({"path": "/repo/README.md"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Final answer from session.done."}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains(": note"));
        assert!(screen.contains("note: Need more targeted."));
        assert!(screen.contains("Final answer from session.done."));
        assert!(!screen.contains(": answer draft"));
        Ok(())
    }

    #[test]
    fn completed_final_stream_does_not_duplicate_session_done_answer() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "answer directly"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex", "turn_idx": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Draft answer that should not replay.", "turn_idx": 0}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.response",
            serde_json::json!({"turn_idx": 0, "tool_call_count": 0, "text_delta_chars": 36}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Canonical final answer."}),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("Canonical final answer."));
        assert!(!screen.contains("Draft answer that should not replay."));
        Ok(())
    }

    #[test]
    fn native_scrollback_filters_transient_model_events() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "write as it streams"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.turn.request",
            serde_json::json!({"model": "GPT-5.5", "provider": "codex"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.tool_call",
            serde_json::json!({"name": "spawn_agent", "arguments": {"nickname": "repo-explorer"}}),
        )?;
        app.store.append_event(
            &session.id,
            "agent.spawned",
            serde_json::json!({"nickname": "repo-explorer"}),
        )?;
        app.store.append_event(
            &session.id,
            "model.stream_delta",
            serde_json::json!({"text": "Live draft chunk"}),
        )?;
        app.selected_session_id = Some(session.id.clone());
        app.drain_store_notifications()?;
        let state = app.workbench_state()?.clone();
        let events = app.cached_events_for_session(&session.id).to_vec();
        let mut last_group = None;

        let lines = native_scrollback_event_lines(&events, &state, 100, &mut last_group);
        let text = lines_plain_text(&lines);

        assert!(text.contains("write as it streams"));
        assert!(text.contains(": subagent repo-explorer"));
        assert!(text.contains("started"));
        assert!(!text.contains("repo-explorer started"));
        assert!(!text.contains("waiting for GPT-5.5"));
        assert!(!text.contains("start repo-explorer helper"));
        assert!(!text.contains(": answer draft"));
        assert!(!text.contains("Live draft chunk"));
        Ok(())
    }

    #[test]
    fn wrapped_native_links_use_the_full_url_for_each_visible_fragment() {
        let lines = vec![
            Line::from(ratatui::text::Span::styled(
                "https://en.wikiped",
                theme::link(),
            )),
            Line::from(ratatui::text::Span::styled(
                "ia.org/wiki/Apple_Inc.",
                theme::link(),
            )),
        ];

        let hyperlinks = collect_native_hyperlink_segments(&lines);
        assert_eq!(hyperlinks.len(), 2);
        assert_eq!(
            hyperlinks[0].target,
            "https://en.wikipedia.org/wiki/Apple_Inc."
        );
        assert_eq!(hyperlinks[1].target, hyperlinks[0].target);
        assert_eq!(hyperlinks[0].line, 0);
        assert_eq!(hyperlinks[1].line, 1);
    }

    #[test]
    fn native_link_escape_annotation_keeps_visible_symbols_clickable() {
        let lines = vec![
            Line::from(ratatui::text::Span::styled(
                "https://example",
                theme::link(),
            )),
            Line::from(ratatui::text::Span::styled(".com/docs", theme::link())),
        ];
        let hyperlinks = collect_native_hyperlink_segments(&lines);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 2));
        let area = buffer.area;
        Paragraph::new(lines).render(area, &mut buffer);
        apply_native_hyperlinks(&mut buffer, area, &hyperlinks);

        assert!(buffer[(0, 0)]
            .symbol()
            .starts_with("\x1b]8;;https://example.com/docs\x1b\\h"));
        assert!(buffer[(14, 0)].symbol().ends_with("\x1b]8;;\x1b\\"));
        assert!(buffer[(0, 1)]
            .symbol()
            .starts_with("\x1b]8;;https://example.com/docs\x1b\\."));
        assert!(buffer[(8, 1)].symbol().ends_with("\x1b]8;;\x1b\\"));
    }

    #[test]
    fn child_agent_progress_is_summarized_in_parent_live_view() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let parent = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &parent.id,
            "session.input",
            serde_json::json!({"text": "explain this repo"}),
        )?;
        let child = app.store.create_child_session(
            &parent.id,
            std::env::current_dir()?,
            Some("/root/repo-explorer"),
            Some("repo-explorer"),
            Some("explorer"),
        )?;
        app.store.append_event(
            &child.id,
            "agent.context",
            serde_json::json!({"nickname": "repo-explorer", "role": "explorer"}),
        )?;
        app.store.append_event(
            &parent.id,
            "agent.spawned",
            serde_json::json!({"child_session_id": child.id, "nickname": "repo-explorer", "role": "explorer"}),
        )?;
        app.store.append_event(
            &child.id,
            "file.read",
            serde_json::json!({"path": "/repo/README.md"}),
        )?;
        app.store.append_event(
            &child.id,
            "model.stream_delta",
            serde_json::json!({"text": "Mapping the main crates."}),
        )?;
        app.selected_session_id = Some(parent.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains(": subagent"));
        assert!(screen.contains("repo-explorer"));
        assert!(screen.contains(": subagent repo-explorer"));
        assert!(screen.contains("read /repo/README.md"));
        assert!(!screen.contains("Mapping the main crates."));
        Ok(())
    }

    #[test]
    fn native_parent_scrollback_does_not_replay_child_session_turns() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let parent = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &parent.id,
            "session.input",
            serde_json::json!({"text": "explain this repo"}),
        )?;
        let child = app.store.create_child_session(
            &parent.id,
            std::env::current_dir()?,
            Some("/root/repo-explorer"),
            Some("repo-explorer"),
            Some("explorer"),
        )?;
        app.store.append_event(
            &parent.id,
            "agent.spawned",
            serde_json::json!({"child_session_id": child.id, "nickname": "repo-explorer"}),
        )?;
        app.store.append_event(
            &child.id,
            "session.input",
            serde_json::json!({"text": "read every repo file"}),
        )?;
        app.store.append_event(
            &child.id,
            "file.read",
            serde_json::json!({"path": "/repo/README.md"}),
        )?;
        app.store.append_event(
            &child.id,
            "session.done",
            serde_json::json!({"result": "CHILD FULL DETAILS SHOULD NOT BE TOP LEVEL"}),
        )?;
        app.store.append_event(
            &parent.id,
            "agent.completed",
            serde_json::json!({
                "child_session_id": child.id,
                "payload": {"result": "Short helper summary"}
            }),
        )?;
        app.selected_session_id = Some(parent.id.clone());
        app.drain_store_notifications()?;
        let state = app.workbench_state()?.clone();
        let mut last_group = None;

        let (lines, _) = native_scrollback_chronological_event_lines(
            &app,
            &state,
            &parent.id,
            0,
            100,
            &mut last_group,
        );
        let text = lines_plain_text(&lines);

        assert!(text.contains("explain this repo"));
        assert!(text.contains("subagent repo-explorer"));
        assert!(text.contains("started"));
        assert!(text.contains("read /repo/README.md"));
        assert!(text.contains("finished"));
        assert!(!text.contains("repo-explorer started"));
        assert!(!text.contains("subagent finished"));
        assert!(text.contains("Short helper summary"));
        assert!(!text.contains("read every repo file"));
        assert!(!text.contains("CHILD FULL DETAILS SHOULD NOT BE TOP LEVEL"));
        Ok(())
    }

    #[test]
    fn long_browser_urls_do_not_overrun_the_timeline_column() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "what do you see"}),
        )?;
        app.store.append_event(
            &session.id,
            "browser.state",
            serde_json::json!({
                "title": "Amazon Web Services Sign-In",
                "tabs": 2,
                "url": "https://signin.aws.amazon.com/signin?redirect_uri=https%3A%2F%2Faws.amazon.com%2Fmarketplace%2Fmanagement%2Fseller-settings%2Faccount%2Fcustom-notification-submitted%3F%26isauthcode%3Dtrue&client_id=arn%3Aaws%3Aiam%3A%3A015428540659%3Auser%2Fawsmp-contessa&forceMobileApp=0",
            }),
        )?;
        app.selected_session_id = Some(session.id);

        let screen = render_dump(&mut app)?;
        assert!(screen.contains("signin.aws.amazon.com/signin?..."));
        assert!(!screen.contains("redirect_uri=https"));
        Ok(())
    }

    #[test]
    fn native_scrollback_live_view_does_not_replay_completed_transcript() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        app.args.height = 44;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "describe this repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "It is a Rust browser-agent workbench."}),
        )?;
        app.store.append_event(
            &session.id,
            "session.followup",
            serde_json::json!({"text": "go say hi to aitor"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Hi Aitor - this is the short summary."}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history.reset_for_session(session.id, last_seq);

        let screen = render_dump(&mut app)?;
        let composer_row = row_containing(&screen, "Ask a follow-up");
        let hint_row = row_containing(&screen, "Enter:reply");
        assert!(hint_row >= composer_row + 2);
        assert!(!screen.contains("describe this repo"));
        assert!(!screen.contains("go say hi to aitor"));
        assert!(!screen.contains("It is a Rust browser-agent workbench."));
        assert!(!screen.contains("Hi Aitor"));
        Ok(())
    }

    #[test]
    fn slash_palette_does_not_resize_completed_history_viewport() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "describe this repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "It is a Rust browser-agent workbench."}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history.reset_for_session(session.id, last_seq);

        let before = desired_terminal_viewport_height(&mut app)?;
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))?);
        assert!(app.is_slash_palette_active());
        let after = desired_terminal_viewport_height(&mut app)?;
        assert_eq!(before, after);
        Ok(())
    }

    #[test]
    fn multiline_composer_does_not_resize_completed_history_viewport() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "describe this repo"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "It is a Rust browser-agent workbench."}),
        )?;
        let events = app.store.events_for_session(&session.id)?;
        let last_seq = events.last().map(|event| event.seq).unwrap_or_default();
        app.selected_session_id = Some(session.id.clone());
        app.native_history.reset_for_session(session.id, last_seq);

        let before = desired_terminal_viewport_height(&mut app)?;
        app.set_input("first line".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))?);
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))?);
        let after = desired_terminal_viewport_height(&mut app)?;
        assert_eq!(before, after);
        Ok(())
    }

    #[test]
    fn followups_render_as_transcript_turns() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let session = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &session.id,
            "session.input",
            serde_json::json!({"text": "inspect repository"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "It is a Rust TUI."}),
        )?;
        app.store.append_event(
            &session.id,
            "session.followup",
            serde_json::json!({"text": "which files matter most?"}),
        )?;
        app.store.append_event(
            &session.id,
            "session.done",
            serde_json::json!({"result": "Cargo.toml and crates/browser-use-tui/src/main.rs."}),
        )?;
        app.selected_session_id = Some(session.id);
        let screen = render_dump(&mut app)?;
        assert!(!screen.contains(": answer"));
        assert!(screen.contains("Cargo.toml"));
        Ok(())
    }

    #[test]
    fn followup_and_retry_enter_running_state_before_agent_events() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let done = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &done.id,
            "session.input",
            serde_json::json!({"text": "first task"}),
        )?;
        app.store.append_event(
            &done.id,
            "session.done",
            serde_json::json!({"result": "done"}),
        )?;
        app.dispatch(AppCommand::SendFollowup {
            session_id: done.id.clone(),
            text: "continue".to_string(),
        })?;
        assert_eq!(
            app.store
                .load_session(&done.id)?
                .map(|session| session.status),
            Some(SessionStatus::Running)
        );

        let failed = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &failed.id,
            "session.input",
            serde_json::json!({"text": "retry me"}),
        )?;
        app.store.append_event(
            &failed.id,
            "session.failed",
            serde_json::json!({"error": "read Codex SSE line"}),
        )?;
        app.dispatch(AppCommand::RetryTask(failed.id.clone()))?;
        assert_eq!(
            app.store
                .load_session(&failed.id)?
                .map(|session| session.status),
            Some(SessionStatus::Running)
        );
        Ok(())
    }

    #[test]
    fn followup_retry_cancel_and_developer_surface_work() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let app_args = Args {
            select_latest: true,
            seed_demo: Some("done".to_string()),
            agent: AgentBackend::Fake,
            ..args(&temp)
        };
        let mut app = App::new(app_args)?;
        app.setup_complete = true;
        app.store.set_setting("setup.complete", "1")?;
        let session_id = app.selected_session_id.clone().context("seed session")?;
        app.set_input("shorter".to_string());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?);
        let events = app.store.events_for_session(&session_id)?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "session.followup"));

        let running = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &running.id,
            "session.input",
            serde_json::json!({"text": "run"}),
        )?;
        app.selected_session_id = Some(running.id.clone());
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))?);
        assert_eq!(
            app.store
                .load_session(&running.id)?
                .map(|session| session.status),
            Some(SessionStatus::Cancelled)
        );

        app.open_surface(Surface::Developer);
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("developer"));
        assert!(screen.contains("Events"));
        Ok(())
    }

    #[test]
    fn escape_twice_stops_running_task() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut app = ready_app(&temp)?;
        let running = app.store.create_session(None, std::env::current_dir()?)?;
        app.store.append_event(
            &running.id,
            "session.input",
            serde_json::json!({"text": "run"}),
        )?;
        app.selected_session_id = Some(running.id.clone());

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?);
        assert!(app.escape_stop_is_pending());
        assert_eq!(
            app.store
                .load_session(&running.id)?
                .map(|session| session.status),
            Some(SessionStatus::Running)
        );
        let screen = render_dump(&mut app)?;
        assert!(screen.contains("esc again to stop"));

        assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?);
        assert!(!app.escape_stop_is_pending());
        assert_eq!(
            app.store
                .load_session(&running.id)?
                .map(|session| session.status),
            Some(SessionStatus::Cancelled)
        );
        Ok(())
    }
}
