use anyhow::Result;
use browser_use_protocol::{
    HistoryRow, SessionMeta, SessionStatus, TelemetrySummary, WorkbenchState,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::backend::TestBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::time::Instant;

use crate::settings::MODEL_CHOICES;
use crate::theme::*;

use super::{App, Overlay};

pub(crate) fn render_dump(app: &mut App) -> Result<String> {
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

pub(crate) fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let state = app.workbench_state().unwrap_or_else(|_| WorkbenchState {
        setup_complete: false,
        current_session: None,
        task: None,
        result: None,
        failure: Some("Could not load state.".to_string()),
        activity: Vec::new(),
        browser: Default::default(),
        telemetry: Default::default(),
        history: Vec::new(),
    });

    let is_first_run =
        !app.setup_complete && state.history.is_empty() && state.current_session.is_none();
    let surface = app_surface(area);

    if is_first_run && app.overlay == Overlay::None {
        render_setup(frame, surface, app, true);
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
        Overlay::Setup => render_setup(frame, surface, app, false),
        Overlay::Account => render_account_overlay(frame, surface, app),
        Overlay::Model => render_model_overlay(frame, surface, app),
        Overlay::Browser => render_browser_overlay(frame, surface, app, &state),
        Overlay::BrowserChoice => render_browser_choice_overlay(frame, surface, app),
        Overlay::SetupComplete => render_setup_complete(frame, surface, app),
        Overlay::History => render_history_overlay(frame, surface, app, &state),
        Overlay::Actions => render_actions_overlay(frame, surface, app),
        Overlay::Help => render_help_overlay(frame, surface),
        Overlay::Developer => {
            render_developer_overlay(frame, centered_rect(96, 24, area), app, &state)
        }
    }
}

fn app_surface(area: Rect) -> Rect {
    area.inner(Margin {
        vertical: 0,
        horizontal: 2,
    })
}

fn render_workbench(frame: &mut Frame<'_>, area: Rect, app: &App, state: &WorkbenchState) {
    let outer = app_surface(area);
    let composer_h = app.composer_height();
    let footer_h = 1u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(8),
            Constraint::Length(composer_h),
            Constraint::Length(footer_h),
        ])
        .split(outer);

    render_workbench_header(frame, chunks[0], app, state);
    let content = if let Some(session) = state.current_session.as_ref() {
        if session.status.is_active() {
            running_lines(state)
        } else if session.status == SessionStatus::Cancelled {
            cancelled_lines(&state.telemetry)
        } else if let Some(error) = state.failure.as_ref() {
            failure_lines(error, &state.telemetry)
        } else {
            result_lines(state)
        }
    } else {
        ready_lines(app, state)
    };
    frame.render_widget(
        Paragraph::new(content)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
    render_composer(frame, chunks[2], app, state.current_session.as_ref());
    render_footer(frame, chunks[3], app, state);
}

fn render_workbench_header(frame: &mut Frame<'_>, area: Rect, app: &App, state: &WorkbenchState) {
    let width = area.width as usize;
    if width == 0 {
        return;
    }
    let left = if let Some(session) = state.current_session.as_ref() {
        let task = truncate(state.task.as_deref().unwrap_or("browser task"), width / 2);
        format!("{task}  {}", session.status.as_str())
    } else {
        "browser-use".to_string()
    };
    let right = format!("{}  {}", app.browser, app.model);
    let max_left = width.saturating_sub(right.chars().count() + 2).max(10);
    let left = truncate(&left, max_left);
    let right = truncate(&right, width.saturating_sub(left.chars().count() + 2));
    let spaces = width.saturating_sub(left.chars().count() + right.chars().count());
    let lines = vec![
        Line::from(vec![
            Span::styled(left, bold()),
            Span::raw(" ".repeat(spaces)),
            Span::styled(right, muted()),
        ]),
        Line::from(Span::styled("─".repeat(width), dim())),
    ];
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(text())),
        area,
    );
}

fn ready_lines(app: &App, state: &WorkbenchState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("What should the browser do?", bold())),
        Line::from(""),
    ];
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(
            notice.clone(),
            status_style("failed"),
        )));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled("Recent", muted())));
    lines.push(Line::from(""));
    if state.history.is_empty() {
        lines.push(Line::from(Span::styled("  No previous work yet.", dim())));
    } else {
        for row in state.history.iter().take(3) {
            lines.push(history_line(row, 74));
        }
    }
    lines.push(Line::from(""));
    let auth_status = if app.auth_notice().ok().flatten().is_some() {
        "needs sign in"
    } else {
        "ready"
    };
    lines.push(Line::from(Span::styled("Ready", muted())));
    lines.push(kv_line("account", auth_status));
    lines.push(kv_line("browser", "connected"));
    lines
}

fn running_lines(state: &WorkbenchState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    append_task_section(&mut lines, "You", state);
    lines.push(Line::from(""));
    append_activity_section(&mut lines, "Agent is working", state, true);
    lines.push(Line::from(""));
    append_browser_section(&mut lines, state);
    append_telemetry_lines(&mut lines, &state.telemetry);
    lines
}

fn result_lines(state: &WorkbenchState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    append_task_section(&mut lines, "Task", state);
    lines.push(Line::from(""));
    append_activity_section(&mut lines, "Steps", state, false);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Result", bold())));
    lines.push(Line::from(""));
    if let Some(result) = state.result.as_ref() {
        lines.extend(markdown_result_lines(result));
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
    append_telemetry_lines(&mut lines, &state.telemetry);
    lines
}

fn failure_lines(error: &str, telemetry: &TelemetrySummary) -> Vec<Line<'static>> {
    let message = friendly_error_message(error);
    let mut lines = vec![
        Line::from(Span::styled("The agent could not finish the task.", bold())),
        Line::from(""),
        Line::from(Span::styled(message, muted())),
        Line::from(""),
        Line::from("> Retry"),
        Line::from("  Sign in"),
        Line::from("  Choose model"),
        Line::from("  Change browser"),
        Line::from(""),
        Line::from(Span::styled("Work preserved in history.", muted())),
    ];
    append_telemetry_lines(&mut lines, telemetry);
    lines
}

fn cancelled_lines(telemetry: &TelemetrySummary) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("The task was stopped.", bold())),
        Line::from(""),
        Line::from(Span::styled("Work preserved in history.", muted())),
        Line::from(""),
        Line::from("> Start a follow-up"),
        Line::from("  Previous work"),
        Line::from("  Setup"),
    ];
    append_telemetry_lines(&mut lines, telemetry);
    lines
}

fn append_task_section(lines: &mut Vec<Line<'static>>, title: &str, state: &WorkbenchState) {
    lines.push(Line::from(Span::styled(title.to_string(), bold())));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            state
                .task
                .clone()
                .unwrap_or_else(|| "browser task".to_string()),
            text_style(),
        ),
    ]));
}

fn append_activity_section(
    lines: &mut Vec<Line<'static>>,
    title: &str,
    state: &WorkbenchState,
    running: bool,
) {
    lines.push(Line::from(Span::styled(title.to_string(), bold())));
    if state.activity.is_empty() {
        let fallback = if running {
            "starting browser task"
        } else {
            "no recorded steps"
        };
        lines.push(bullet_line(fallback));
        return;
    }
    let max_items = 10usize;
    let skipped = state.activity.len().saturating_sub(max_items);
    if skipped > 0 {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("... {skipped} earlier steps"), dim()),
        ]));
    }
    for item in state.activity.iter().skip(skipped) {
        lines.push(bullet_line(item));
    }
}

fn append_browser_section(lines: &mut Vec<Line<'static>>, state: &WorkbenchState) {
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
}

fn bullet_line(value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  • ", accent()),
        Span::styled(value.to_string(), text_style()),
    ])
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
            Span::styled("▌ ", accent()),
            Span::styled(placeholder, dim()),
        ])]
    } else {
        let max_lines = area.height.saturating_sub(2).max(1) as usize;
        visible_composer_lines(
            composer_input_lines(&app.input, app.input_cursor),
            max_lines,
        )
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
        modal(frame, area, "browser-use")
    } else {
        modal(frame, area, "Setup")
    };
    let mut lines = vec![
        if first_run {
            Line::from(Span::styled("Set up the browser agent", bold()))
        } else {
            Line::from(Span::styled("The browser agent needs attention.", bold()))
        },
        Line::from(""),
    ];
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(
            notice.clone(),
            status_style("failed"),
        )));
        lines.push(Line::from(""));
    }
    if first_run {
        lines.extend([
            selected(
                &format!(
                    "[1] Sign in              {}",
                    app.auth_status_for_account(&app.account)
                ),
                0,
                app.selected_row,
            ),
            Line::from(""),
            selected(
                &format!(
                    "[2] Choose model         {}",
                    if app.model_configured {
                        app.model.as_str()
                    } else {
                        "No model selected"
                    }
                ),
                1,
                app.selected_row,
            ),
            Line::from(""),
            selected(
                &format!("[3] Choose browser       {}", app.browser),
                2,
                app.selected_row,
            ),
            Line::from(""),
            Line::from(Span::styled(
                "enter continue     tab history     / actions",
                muted(),
            )),
        ]);
    } else {
        lines.extend([
            setup_status_line("ok", "Browser", &format!("{} found", app.browser)),
            Line::from(""),
            setup_status_line("ok", "Sign in", &app.account),
            Line::from(""),
            setup_status_line("ok", "Model", &app.model),
            Line::from(""),
            selected("Sign in", 0, app.selected_row),
            selected("Choose model", 1, app.selected_row),
            selected("Change browser", 2, app.selected_row),
            Line::from(""),
            Line::from(Span::styled("enter fix     esc back", muted())),
        ]);
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_setup_complete(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(Clear, area);
    let inner = modal(frame, area, "Ready");
    let auth_state = app
        .auth_notice()
        .ok()
        .flatten()
        .unwrap_or_else(|| format!("Signed in with {}", app.account));
    let lines = vec![
        setup_status_line("ok", "Sign in", &auth_state),
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
    let mut lines = vec![
        Line::from("Choose how the agent should connect to a model."),
        Line::from(""),
    ];
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(
            notice.clone(),
            status_style("failed"),
        )));
        lines.push(Line::from(""));
    }
    for (idx, account) in super::settings::ACCOUNT_CHOICES.iter().enumerate() {
        lines.push(selected(
            &format!("{account:<24} {}", app.auth_status_for_account(account)),
            idx,
            app.selected_row,
        ));
    }
    lines.extend([
        Line::from(""),
        Line::from(Span::styled("enter select     esc back", muted())),
    ]);
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
        Paragraph::new("enter select     esc close").style(muted()),
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
    let mut lines = vec![
        Line::from(Span::styled("Telemetry", bold())),
        Line::from(""),
    ];
    let Some(session) = state.current_session.as_ref() else {
        lines.push(Line::from(Span::styled("No task selected.", dim())));
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    };
    append_telemetry_detail_lines(&mut lines, &state.telemetry);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Events", bold())));
    lines.push(Line::from(""));
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

fn append_telemetry_lines(lines: &mut Vec<Line<'static>>, telemetry: &TelemetrySummary) {
    if telemetry.trace_id.is_none() && telemetry.failure.is_none() {
        return;
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Telemetry", bold())));
    if let Some(trace_id) = telemetry.trace_id.as_ref() {
        lines.push(Line::from(vec![
            Span::styled("laminar  ", muted()),
            Span::styled(trace_id.clone(), link()),
        ]));
    }
    if let Some(error) = telemetry.failure.as_ref() {
        lines.push(Line::from(vec![
            Span::styled("laminar  ", muted()),
            Span::styled(
                format!("disabled: {}", truncate(&first_line(error), 96)),
                status_style("failed"),
            ),
        ]));
    }
}

fn append_telemetry_detail_lines(lines: &mut Vec<Line<'static>>, telemetry: &TelemetrySummary) {
    if telemetry.trace_id.is_none() && telemetry.failure.is_none() {
        lines.push(Line::from(Span::styled(
            "No Laminar event for this task.",
            dim(),
        )));
        return;
    }
    if let Some(trace_id) = telemetry.trace_id.as_ref() {
        lines.push(kv_line("trace", trace_id));
    }
    if let Some(backend) = telemetry.backend.as_ref() {
        lines.push(kv_line("backend", backend));
    }
    if let Some(endpoint) = telemetry.endpoint.as_ref() {
        lines.push(kv_line("endpoint", endpoint));
    }
    if let Some(error) = telemetry.failure.as_ref() {
        lines.push(kv_line(
            "status",
            &format!("disabled: {}", truncate(&first_line(error), 120)),
        ));
    }
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

fn composer_input_lines(input: &str, cursor: usize) -> Vec<Line<'static>> {
    let chars = input.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    let mut out = Vec::new();
    let mut global = 0usize;

    for (idx, source_line) in input.split('\n').enumerate() {
        let line_len = source_line.chars().count();
        let prefix = if idx == 0 { "> " } else { "  " };
        if cursor >= global && cursor <= global + line_len {
            let local = cursor - global;
            let before = source_line.chars().take(local).collect::<String>();
            let after = source_line.chars().skip(local).collect::<String>();
            out.push(Line::from(vec![
                Span::styled(prefix, accent()),
                Span::styled(before, bold()),
                Span::styled("▌", accent()),
                Span::styled(after, bold()),
            ]));
        } else {
            out.push(Line::from(vec![
                Span::styled(prefix, accent()),
                Span::styled(source_line.to_string(), bold()),
            ]));
        }
        global += line_len + 1;
    }

    if out.is_empty() {
        out.push(Line::from(vec![
            Span::styled("> ", accent()),
            Span::styled("▌", accent()),
        ]));
    }

    out
}

fn visible_composer_lines(mut lines: Vec<Line<'static>>, max_lines: usize) -> Vec<Line<'static>> {
    if lines.len() <= max_lines {
        return lines;
    }
    let start = lines.len().saturating_sub(max_lines);
    lines.drain(0..start);
    lines
}

fn markdown_result_lines(markdown: &str) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(markdown, options);
    let mut writer = MarkdownWriter::default();
    for event in parser {
        writer.handle_event(event);
    }
    writer.finish()
}

#[derive(Clone, Debug)]
struct ListState {
    next: Option<u64>,
}

#[derive(Default)]
struct MarkdownWriter {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<ListState>,
    link_stack: Vec<String>,
    pending_prefix: Option<String>,
    in_code_block: bool,
}

impl MarkdownWriter {
    fn handle_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(code) => {
                self.ensure_prefix();
                self.current.push(Span::styled(code.to_string(), muted()));
            }
            Event::SoftBreak | Event::HardBreak => self.flush_current(),
            Event::Rule => {
                self.flush_current();
                self.push_non_duplicate_blank();
                self.lines.push(Line::from(Span::styled("---", muted())));
                self.push_non_duplicate_blank();
            }
            Event::Html(html) | Event::InlineHtml(html) => self.push_text(&html),
            Event::FootnoteReference(text) => self.push_text(&format!("[{text}]")),
            Event::TaskListMarker(checked) => {
                self.ensure_prefix();
                self.current
                    .push(Span::styled(if checked { "[x] " } else { "[ ] " }, muted()));
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_current();
                self.style_stack.push(heading_style(level));
            }
            Tag::BlockQuote => {
                self.flush_current();
                self.pending_prefix = Some("> ".to_string());
            }
            Tag::CodeBlock(kind) => {
                self.flush_current();
                let label = match kind {
                    CodeBlockKind::Fenced(language) if !language.is_empty() => {
                        format!("code {language}")
                    }
                    _ => "code".to_string(),
                };
                self.lines.push(Line::from(Span::styled(label, muted())));
                self.in_code_block = true;
            }
            Tag::List(start) => {
                self.flush_current();
                self.list_stack.push(ListState { next: start });
            }
            Tag::Item => {
                self.flush_current();
                self.pending_prefix = Some(self.next_list_marker());
            }
            Tag::Emphasis => self.style_stack.push(muted()),
            Tag::Strong => self.style_stack.push(bold()),
            Tag::Strikethrough => self.style_stack.push(muted()),
            Tag::Link { dest_url, .. } => {
                self.link_stack.push(dest_url.to_string());
                self.style_stack.push(link());
            }
            Tag::Image {
                title, dest_url, ..
            } => {
                self.ensure_prefix();
                let label = if title.is_empty() {
                    dest_url.to_string()
                } else {
                    title.to_string()
                };
                self.current
                    .push(Span::styled(format!("[image: {label}]"), muted()));
            }
            Tag::FootnoteDefinition(_)
            | Tag::HtmlBlock
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::BlockQuote => self.flush_current(),
            TagEnd::CodeBlock => {
                self.flush_current();
                self.in_code_block = false;
            }
            TagEnd::List(_) => {
                self.flush_current();
                self.list_stack.pop();
            }
            TagEnd::Item => self.flush_current(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.style_stack.pop();
            }
            TagEnd::Link => {
                self.style_stack.pop();
                if let Some(dest) = self.link_stack.pop() {
                    self.current.push(Span::raw(" ("));
                    self.current.push(Span::styled(dest, link()));
                    self.current.push(Span::raw(")"));
                }
            }
            TagEnd::Image
            | TagEnd::FootnoteDefinition
            | TagEnd::HtmlBlock
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        for (idx, line) in text.lines().enumerate() {
            if idx > 0 {
                self.flush_current();
            }
            self.ensure_prefix();
            let style = if self.in_code_block {
                muted()
            } else {
                self.style_stack.last().copied().unwrap_or_else(text_style)
            };
            if looks_like_bare_link(line) {
                self.current.push(Span::styled(line.to_string(), link()));
            } else {
                self.current.push(Span::styled(line.to_string(), style));
            }
        }
    }

    fn ensure_prefix(&mut self) {
        if self.current.is_empty() {
            if let Some(prefix) = self.pending_prefix.take() {
                self.current.push(Span::styled(prefix, accent()));
            }
        }
    }

    fn flush_current(&mut self) {
        if self.current.is_empty() {
            return;
        }
        self.lines
            .push(Line::from(std::mem::take(&mut self.current)));
        self.pending_prefix = None;
    }

    fn push_non_duplicate_blank(&mut self) {
        if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(Line::from(""));
        }
    }

    fn next_list_marker(&mut self) -> String {
        let depth = self.list_stack.len().saturating_sub(1);
        let indent = "  ".repeat(depth);
        let Some(list) = self.list_stack.last_mut() else {
            return format!("{indent}• ");
        };
        match &mut list.next {
            Some(next) => {
                let marker = format!("{indent}{next}. ");
                *next += 1;
                marker
            }
            None => format!("{indent}• "),
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_current();
        if self.lines.is_empty() {
            self.lines.push(Line::from(""));
        }
        self.lines
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3 => bold(),
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => text_style(),
    }
}

fn looks_like_bare_link(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
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

fn friendly_error_message(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if lower.contains("auth login openrouter") || lower.contains("openrouter_api_key") {
        return "OpenRouter API key is missing. Sign in before retrying.".to_string();
    }
    if lower.contains("auth login openai") || lower.contains("openai_api_key") {
        return "OpenAI API key is missing. Sign in before retrying.".to_string();
    }
    if lower.contains("auth login anthropic") || lower.contains("anthropic_api_key") {
        return "Anthropic API key is missing. Sign in before retrying.".to_string();
    }
    if lower.contains("claude setup-token") || lower.contains("claude_code_oauth_token") {
        return "Claude Code login needs an OAuth token before retrying.".to_string();
    }
    truncate(&first_line(value), 96)
}
