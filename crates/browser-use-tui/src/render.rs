use anyhow::Result;
use browser_use_protocol::{
    EventRecord, HistoryRow, SessionMeta, TelemetrySummary, TranscriptTurn, WorkbenchState,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::backend::TestBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::composer::composer_rule;
use crate::settings::{
    is_claude_code_account, ACCOUNT_ANTHROPIC, ACCOUNT_CHOICES, ACCOUNT_CLAUDE_CODE, ACCOUNT_CODEX,
    ACCOUNT_OPENAI, ACCOUNT_OPENROUTER, BROWSER_CHOICES, MODEL_CHOICES,
};
use crate::theme::*;

use super::{App, ProductState, Surface};

pub(crate) fn render_dump(app: &mut App) -> Result<String> {
    app.drain_store_notifications()?;
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

pub(crate) fn native_scrollback_lines(app: &mut App, width: u16) -> Result<Vec<Line<'static>>> {
    app.drain_store_notifications()?;
    let state = app.workbench_state()?;
    let product_state = app.product_state(&state);
    let body_width = width.saturating_sub(4).max(1);
    let mut lines = Vec::new();
    if matches!(
        product_state,
        ProductState::Failed | ProductState::Cancelled
    ) {
        lines.extend(native_plain_transcript_lines(
            &state,
            body_width,
            product_state,
        ));
    } else {
        lines.extend(transcript_lines(&state, body_width, product_state, false));
    }
    lines.push(Line::from(""));
    Ok(lines)
}

pub(crate) fn lines_plain_text(lines: &[Line<'static>]) -> String {
    let mut out = String::new();
    for line in lines {
        for span in &line.spans {
            out.push_str(&span.content);
        }
        out.push('\n');
    }
    out
}

pub(crate) fn native_scrollback_event_lines(
    events: &[EventRecord],
    state: &WorkbenchState,
    width: u16,
    last_group: &mut Option<String>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for event in events {
        match event.event_type.as_str() {
            "session.input" | "session.followup" => {
                let Some(prompt) = event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                else {
                    continue;
                };
                last_group.take();
                push_gap_if_needed(&mut lines);
                append_prompt_section(&mut lines, prompt);
            }
            "session.done" => {
                last_group.take();
                if let Some(result) = event
                    .payload
                    .get("result")
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                {
                    push_gap_if_needed(&mut lines);
                    append_result_block(&mut lines, result, state, width);
                }
            }
            "session.failed" => {
                last_group.take();
                let error = event
                    .payload
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("The task failed.");
                push_gap_if_needed(&mut lines);
                append_ascii_lines_block(
                    &mut lines,
                    "error",
                    vec![Line::from(Span::styled(
                        friendly_error_message(error),
                        muted(),
                    ))],
                    Some("saved"),
                );
            }
            "session.cancelled" => {
                last_group.take();
                push_gap_if_needed(&mut lines);
                append_ascii_lines_block(
                    &mut lines,
                    "stopped",
                    vec![Line::from(Span::styled(
                        "Progress is saved in history.",
                        muted(),
                    ))],
                    Some("saved"),
                );
            }
            "agent.completed" => {
                if let Some(result) = event
                    .payload
                    .get("payload")
                    .and_then(|payload| payload.get("result"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                {
                    last_group.take();
                    push_gap_if_needed(&mut lines);
                    append_result_block(&mut lines, result, state, width);
                } else {
                    append_grouped_event_line(
                        &mut lines,
                        last_group,
                        "explored",
                        "helper finished",
                    );
                }
            }
            "agent.failed" => {
                last_group.take();
                let error = event
                    .payload
                    .get("payload")
                    .and_then(|payload| payload.get("error"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("The agent could not start.");
                push_gap_if_needed(&mut lines);
                append_ascii_lines_block(
                    &mut lines,
                    "error",
                    vec![Line::from(Span::styled(error.to_string(), muted()))],
                    Some("saved"),
                );
            }
            "agent.cancelled" => {
                append_grouped_event_line(&mut lines, last_group, "explored", "helper stopped");
            }
            "browser.connected" => {
                append_grouped_event_line(&mut lines, last_group, "browser", "browser connected");
            }
            "browser.reconnected" => {
                append_grouped_event_line(&mut lines, last_group, "browser", "browser reconnected");
            }
            "browser.target_changed" => {
                append_grouped_event_line(
                    &mut lines,
                    last_group,
                    "browser",
                    "browser target changed",
                );
            }
            "browser.disconnected" => {
                append_grouped_event_line(
                    &mut lines,
                    last_group,
                    "browser",
                    "browser disconnected",
                );
            }
            "browser.live_url" => {
                append_grouped_event_line(
                    &mut lines,
                    last_group,
                    "browser",
                    "connected live browser",
                );
            }
            "browser.page" | "browser.state" => {
                if let Some(url) = event.payload.get("url").and_then(serde_json::Value::as_str) {
                    append_grouped_event_line(
                        &mut lines,
                        last_group,
                        "browser",
                        &format!("opened {}", compact_url_for_render(url)),
                    );
                }
            }
            "command.started" => {
                let text = event
                    .payload
                    .get("cmd")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("command");
                append_grouped_event_line(&mut lines, last_group, "ran", text);
            }
            "command.finished" => {
                if event
                    .payload
                    .get("success")
                    .and_then(serde_json::Value::as_bool)
                    .is_some_and(|success| !success)
                {
                    let code = event
                        .payload
                        .get("exit_code")
                        .and_then(serde_json::Value::as_i64)
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    append_grouped_event_line(
                        &mut lines,
                        last_group,
                        "ran",
                        &format!("command failed with exit {code}"),
                    );
                }
            }
            "file.read" => {
                if let Some(path) = event
                    .payload
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                {
                    append_grouped_event_line(
                        &mut lines,
                        last_group,
                        "explored",
                        &format!("read {}", compact_path_for_render(path)),
                    );
                }
            }
            "file.search" => {
                let query = event
                    .payload
                    .get("query")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("files");
                append_grouped_event_line(
                    &mut lines,
                    last_group,
                    "explored",
                    &format!("searched {query:?}"),
                );
            }
            "file.list" => {
                append_grouped_event_line(&mut lines, last_group, "explored", "listed files");
            }
            "patch.file_changed" => {
                let kind = event
                    .payload
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("changed");
                let path = event
                    .payload
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(compact_path_for_render)
                    .unwrap_or_else(|| "file".to_string());
                append_grouped_event_line(
                    &mut lines,
                    last_group,
                    "changed",
                    &format!("{kind} {path}"),
                );
            }
            "agent.spawned" => {
                append_grouped_event_line(
                    &mut lines,
                    last_group,
                    "explored",
                    &agent_started_text_for_render(&event.payload),
                );
            }
            _ => {}
        }
    }
    lines
}

pub(crate) fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = app_surface(frame.area());
    let state = app
        .workbench_state()
        .unwrap_or_else(|_| app.empty_workbench_state_with_failure());
    let product_state = app.product_state(&state);

    if app.is_first_run_setup_visible().unwrap_or(false) && app.surface == Surface::Main {
        render_surface(frame, area, app, &state, Surface::Setup);
        return;
    }

    match app.surface {
        surface if surface.uses_main_view() => render_main(frame, area, app, &state, product_state),
        surface => render_surface(frame, area, app, &state, surface),
    }
}

fn app_surface(area: Rect) -> Rect {
    area.inner(Margin {
        vertical: 0,
        horizontal: 2,
    })
}

fn render_main(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut App,
    state: &WorkbenchState,
    product_state: ProductState,
) {
    let bottom_h = main_bottom_height_for(app, state, app.surface, area, product_state);
    let body_width = area.width;
    let body = if app.surface.is_bottom_pane() {
        Vec::new()
    } else if app.native_scrollback_is_active() {
        Vec::new()
    } else {
        match product_state {
            ProductState::SetupNeeded => setup_lines(app),
            ProductState::Ready => ready_lines(app, state, body_width),
            ProductState::Running
            | ProductState::Result
            | ProductState::Failed
            | ProductState::Cancelled => work_lines(state, app, body_width, product_state),
        }
    };
    let show_footer = !(app.is_slash_palette_active() && !app.surface.is_bottom_pane());
    let pin_bottom =
        app.native_scrollback_is_active() && matches!(product_state, ProductState::Result);
    let (body_area, bottom_area, footer_area) =
        main_layout_areas(area, bottom_h, body.len(), show_footer, pin_bottom);
    let body = if app.native_scrollback_is_active() && !app.surface.is_bottom_pane() {
        native_replay_live_lines(app, state, product_state, body_width, body_area.height)
    } else {
        body
    };
    frame.render_widget(
        Paragraph::new(body)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        body_area,
    );
    if app.surface.is_bottom_pane() {
        render_bottom_pane(frame, bottom_area, app, state, app.surface);
    } else {
        render_composer(frame, bottom_area, app, state, product_state);
    }
    if show_footer {
        render_footer(frame, footer_area, app, state, product_state);
    }
}

fn main_layout_areas(
    area: Rect,
    bottom_h: u16,
    body_len: usize,
    show_footer: bool,
    pin_bottom: bool,
) -> (Rect, Rect, Rect) {
    let footer_h = u16::from(show_footer && area.height > bottom_h);
    let max_body_h = area
        .height
        .saturating_sub(bottom_h)
        .saturating_sub(footer_h);
    let body_h = if max_body_h == 0 {
        0
    } else if pin_bottom {
        max_body_h
    } else {
        (body_len as u16).clamp(1, max_body_h)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(body_h),
            Constraint::Length(bottom_h),
            Constraint::Length(footer_h),
            Constraint::Min(0),
        ])
        .split(area);
    (chunks[0], chunks[1], chunks[2])
}

fn main_bottom_height_for(
    app: &App,
    state: &WorkbenchState,
    surface: Surface,
    area: Rect,
    product_state: ProductState,
) -> u16 {
    if !surface.is_bottom_pane() {
        return composer_pane_height(app, product_state, area.width);
    }
    let line_count = surface_lines(surface, app, state).len() as u16;
    let max_height = match surface {
        Surface::Model => area.height.saturating_sub(2).max(4),
        Surface::BrowserSelect => 18,
        _ => 14,
    };
    let desired = line_count.saturating_add(2).clamp(4, max_height);
    let available = area.height.saturating_sub(2).max(4);
    desired.min(available)
}

fn composer_pane_height(app: &App, product_state: ProductState, width: u16) -> u16 {
    let visual_input_lines = composer_visual_input_lines(app, width);
    let palette_h = slash_palette_pane_height(app);
    let live_status_h = u16::from(composer_live_status_visible(product_state));
    if palette_h > 0 {
        visual_input_lines + palette_h + live_status_h + 1
    } else {
        visual_input_lines + live_status_h + 3
    }
}

fn composer_live_status_visible(product_state: ProductState) -> bool {
    matches!(product_state, ProductState::Running)
}

fn composer_visual_input_lines(app: &App, width: u16) -> u16 {
    let input_width = width.saturating_sub(4).max(1) as usize;
    let visual_input_lines = if app.composer.input().is_empty() {
        1
    } else {
        app.composer
            .input()
            .split('\n')
            .map(|line| {
                let len = line.chars().count();
                len.saturating_add(input_width.saturating_sub(1)) / input_width
            })
            .map(|lines| lines.max(1))
            .sum::<usize>()
    };
    visual_input_lines.clamp(1, 10) as u16
}

fn slash_palette_pane_height(app: &App) -> u16 {
    if !app.is_slash_palette_active() {
        return 0;
    }
    let items = app.slash_palette_items();
    if items.is_empty() {
        return 0;
    }
    (items.len() as u16).min(8)
}

fn render_bottom_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &WorkbenchState,
    surface: Surface,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);
    let title = surface_title(surface);
    let header = pane_header_line(title, chunks[0].width);
    frame.render_widget(Paragraph::new(header), chunks[0]);
    let body = chunks[1].inner(Margin {
        vertical: 0,
        horizontal: 2,
    });
    frame.render_widget(
        Paragraph::new(surface_lines(surface, app, state))
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        body,
    );
}

fn pane_header_line(title: &str, width: u16) -> Line<'static> {
    let width = width as usize;
    let title = format!(" {title} ");
    let title_len = title.chars().count();
    if width <= title_len {
        return Line::from(Span::styled(truncate(&title, width), muted()));
    }
    Line::from(vec![
        Span::styled(title, bold()),
        Span::styled("-".repeat(width.saturating_sub(title_len)), dim()),
    ])
}

fn native_replay_live_lines(
    app: &App,
    state: &WorkbenchState,
    product_state: ProductState,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let lines = match product_state {
        ProductState::Running => {
            let mut lines = Vec::new();
            append_ascii_text_block(
                &mut lines,
                "working",
                &[format!("{} running browser task", spinner_frame())],
                Some("live"),
            );
            if let Some(streaming_text) = current_streaming_text(state) {
                lines.push(Line::from(""));
                append_streaming_block(&mut lines, streaming_text, width);
            }
            lines
        }
        ProductState::Failed => {
            let error = state.failure.as_deref().unwrap_or("The task failed.");
            let (primary, secondary) = failure_actions(error);
            let mut lines = native_plain_transcript_lines(state, width, product_state);
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            append_ascii_text_block(&mut lines, "error", &[friendly_error_message(error)], None);
            lines.push(Line::from(""));
            append_ascii_lines_block(
                &mut lines,
                "next",
                vec![
                    selected(primary, 0, app.selected_row),
                    selected(secondary, 1, app.selected_row),
                    selected("Retry", 2, app.selected_row),
                    selected("New task", 3, app.selected_row),
                ],
                None,
            );
            lines
        }
        ProductState::Cancelled => {
            let mut lines = native_plain_transcript_lines(state, width, product_state);
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            append_ascii_text_block(
                &mut lines,
                "stopped",
                &["Progress is saved in history.".to_string()],
                None,
            );
            lines.push(Line::from(""));
            append_ascii_lines_block(
                &mut lines,
                "next",
                vec![
                    selected("Continue with a follow-up", 0, app.selected_row),
                    selected("Start a new task", 1, app.selected_row),
                    selected("Previous work", 2, app.selected_row),
                ],
                None,
            );
            lines
        }
        ProductState::Result => native_plain_transcript_lines(state, width, product_state),
        ProductState::Ready => Vec::new(),
        ProductState::SetupNeeded => setup_lines(app),
    };
    if matches!(product_state, ProductState::Result) {
        bottom_aligned_tail_lines(lines, height)
    } else {
        lines
    }
}

fn bottom_aligned_tail_lines(mut lines: Vec<Line<'static>>, height: u16) -> Vec<Line<'static>> {
    let height = height as usize;
    if height == 0 {
        return Vec::new();
    }
    if lines.len() > height {
        lines = lines.split_off(lines.len() - height);
    }
    let mut out = Vec::with_capacity(height);
    out.extend(std::iter::repeat_with(|| Line::from("")).take(height.saturating_sub(lines.len())));
    out.extend(lines);
    out
}

fn render_surface(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &WorkbenchState,
    surface: Surface,
) {
    frame.render_widget(Clear, frame.area());
    let chrome_h: u16 = if surface == Surface::Setup { 0 } else { 2 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(chrome_h),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);
    if chrome_h > 0 {
        render_header(frame, chunks[0], app, state, surface_title(surface));
    }
    let lines = surface_lines(surface, app, state);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(surface_footer(surface))
            .style(muted())
            .alignment(Alignment::Right),
        chunks[2],
    );
}

fn surface_title(surface: Surface) -> &'static str {
    match surface {
        Surface::Setup => "browser-use setup",
        Surface::Account => "browser-use setup / authenticate",
        Surface::ApiKey => "browser-use setup / authenticate",
        Surface::Telemetry => "browser-use / Laminar",
        Surface::Model => "browser-use setup / model",
        Surface::Browser => "browser-use / browser",
        Surface::BrowserSelect => "browser-use setup / browser",
        Surface::History => "browser-use / previous work",
        Surface::Developer => "browser-use / developer",
        Surface::Main => "browser-use",
    }
}

fn surface_footer(surface: Surface) -> &'static str {
    match surface {
        Surface::ApiKey => "enter save   esc cancel",
        Surface::Telemetry => "enter save   esc cancel",
        Surface::History => "enter open   r resume   esc back",
        Surface::Setup => "enter continue   esc quit",
        Surface::Browser => "enter select   esc back",
        Surface::Developer => "esc close",
        _ => "enter select   esc back",
    }
}

fn surface_lines(surface: Surface, app: &App, state: &WorkbenchState) -> Vec<Line<'static>> {
    match surface {
        Surface::Setup => setup_lines(app),
        Surface::Account => account_lines(app),
        Surface::ApiKey => api_key_lines(app),
        Surface::Telemetry => telemetry_key_lines(app),
        Surface::Model => model_lines(app),
        Surface::Browser => browser_panel_lines(app, state),
        Surface::BrowserSelect => browser_select_lines(app),
        Surface::History => history_lines(app, state),
        Surface::Developer => developer_lines(app, state),
        Surface::Main => Vec::new(),
    }
}

fn render_header(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &WorkbenchState,
    title: &str,
) {
    let lines = header_lines(app, state, title, area.width);
    frame.render_widget(Paragraph::new(lines), area);
}

fn header_lines(app: &App, state: &WorkbenchState, title: &str, width: u16) -> Vec<Line<'static>> {
    let width = width as usize;
    if width == 0 {
        return Vec::new();
    }
    let browser = browser_header_label(app, state);
    let right = format!("{browser}   {}", app.model);
    let max_left = width.saturating_sub(right.chars().count() + 2).max(10);
    let left = truncate(title, max_left);
    let right = truncate(&right, width.saturating_sub(left.chars().count() + 2));
    let spaces = width.saturating_sub(left.chars().count() + right.chars().count());
    vec![
        Line::from(vec![
            Span::styled(left, bold()),
            Span::raw(" ".repeat(spaces)),
            Span::styled(right, muted()),
        ]),
        Line::from(composer_rule(width as u16)),
    ]
}

fn browser_header_label(app: &App, state: &WorkbenchState) -> String {
    let status = if state.browser.status == "not connected" {
        if app.browser == "Browser Use cloud" {
            "ready"
        } else {
            "connected"
        }
    } else {
        state.browser.status.as_str()
    };
    format!("{} {status}", app.browser)
}

fn render_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &WorkbenchState,
    product_state: ProductState,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let palette_h = slash_palette_pane_height(app);
    let live_status_h = u16::from(composer_live_status_visible(product_state));
    if palette_h > 0 {
        let input_h = composer_visual_input_lines(app, area.width);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(live_status_h),
                Constraint::Length(input_h),
                Constraint::Length(palette_h),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(composer_rule(chunks[0].width)).style(dim()),
            chunks[0],
        );
        if live_status_h > 0 {
            let live_status_area = chunks[1].inner(Margin {
                vertical: 0,
                horizontal: 2,
            });
            frame.render_widget(
                Paragraph::new(composer_live_status_line(state, product_state)),
                live_status_area,
            );
        }
        let input_area = chunks[2].inner(Margin {
            vertical: 0,
            horizontal: 2,
        });
        render_composer_input(frame, input_area, app, state.current_session.as_ref());
        let palette_area = chunks[3].inner(Margin {
            vertical: 0,
            horizontal: 2,
        });
        frame.render_widget(
            Paragraph::new(slash_palette_lines(app, palette_area.width as usize)),
            palette_area,
        );
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(live_status_h),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(composer_rule(chunks[0].width)).style(dim()),
        chunks[0],
    );
    if live_status_h > 0 {
        let live_status_area = chunks[1].inner(Margin {
            vertical: 0,
            horizontal: 2,
        });
        frame.render_widget(
            Paragraph::new(composer_live_status_line(state, product_state)),
            live_status_area,
        );
    }
    let input_area = chunks[2].inner(Margin {
        vertical: 0,
        horizontal: 2,
    });
    render_composer_input(frame, input_area, app, state.current_session.as_ref());
    let status_area = chunks[4].inner(Margin {
        vertical: 0,
        horizontal: 2,
    });
    frame.render_widget(
        Paragraph::new(composer_status_line(
            app,
            state,
            product_state,
            status_area.width,
        ))
        .style(muted()),
        status_area,
    );
}

fn slash_palette_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let items = app.slash_palette_items();
    let cmd_col = items
        .iter()
        .map(|item| item.command.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);
    items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let is_selected = idx == app.selected_row;
            let cmd_style = if is_selected { done() } else { dim() };
            let desc_style = if is_selected { text_style() } else { muted() };
            let desc_max = width.saturating_sub(cmd_col + 4).max(8);
            let description = truncate(item.description, desc_max);
            Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:<cmd_col$}", item.command), cmd_style),
                Span::raw("  "),
                Span::styled(description, desc_style),
            ])
        })
        .collect()
}

fn render_composer_input(
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
    let max_lines = area.height.max(1) as usize;
    frame.render_widget(
        Paragraph::new(app.composer.render_lines_wrapped(
            max_lines,
            area.width as usize,
            placeholder,
        ))
        .style(Style::default().fg(text()))
        .wrap(Wrap { trim: false }),
        area,
    );
    if area.width > 0 && area.height > 0 {
        let (cursor_x, cursor_y) = app
            .composer
            .cursor_position_wrapped(max_lines, area.width as usize);
        frame.set_cursor_position(Position {
            x: area
                .x
                .saturating_add(cursor_x.min(area.width.saturating_sub(1))),
            y: area
                .y
                .saturating_add(cursor_y.min(area.height.saturating_sub(1))),
        });
    }
}

fn render_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &WorkbenchState,
    product_state: ProductState,
) {
    let label = if app
        .quit_hint_until
        .is_some_and(|until| std::time::Instant::now() <= until)
    {
        "ctrl+c again to quit"
    } else if app.escape_stop_is_pending() {
        "esc again to stop"
    } else if app.surface.is_bottom_pane() {
        surface_footer(app.surface)
    } else {
        match product_state {
            ProductState::Running => {
                "enter send   shift+enter newline   ctrl+c stop   esc esc stop   f2 browser   / actions"
            }
            ProductState::Ready | ProductState::SetupNeeded => {
                "enter run   tab history   / actions"
            }
            ProductState::Failed | ProductState::Cancelled => "enter select   / actions",
            ProductState::Result => {
                if state.current_session.is_some() {
                    "enter send   shift+enter newline   f2 browser   tab history   / actions"
                } else {
                    "enter run   tab history   / actions"
                }
            }
        }
    };
    frame.render_widget(
        Paragraph::new(label)
            .style(muted())
            .alignment(Alignment::Right),
        area,
    );
}

fn composer_live_status_line(state: &WorkbenchState, product_state: ProductState) -> Line<'static> {
    let elapsed = elapsed_label(state, product_state).unwrap_or_else(|| "0s".to_string());
    Line::from(vec![
        Span::styled("•", running()),
        Span::raw(" "),
        Span::styled("Working", bold()),
        Span::styled(format!(" ({elapsed} · esc to interrupt)"), muted()),
    ])
}

fn composer_status_line(
    app: &App,
    state: &WorkbenchState,
    product_state: ProductState,
    width: u16,
) -> Line<'static> {
    let mode = match product_state {
        ProductState::SetupNeeded => "Setup",
        ProductState::Ready => "Build",
        ProductState::Running => "",
        ProductState::Result => "Done",
        ProductState::Failed => "Failed",
        ProductState::Cancelled => "Stopped",
    };
    let left = if mode.is_empty() {
        format!("{}  {}", app.model, app.account)
    } else {
        format!("{mode}  {}  {}", app.model, app.account)
    };
    let mut right = browser_ready_label(app, state);
    let width = width as usize;
    let left_len = left.chars().count();
    right = truncate(&right, width.saturating_sub(left_len + 1));
    let right_len = right.chars().count();
    let gap = width.saturating_sub(left_len + right_len).max(1);
    Line::from(vec![
        Span::styled(left, status_style(product_state_label(product_state))),
        Span::raw(" ".repeat(gap)),
        Span::styled(right, muted()),
    ])
}

fn product_state_label(product_state: ProductState) -> &'static str {
    match product_state {
        ProductState::Running => "running",
        ProductState::Result => "done",
        ProductState::Failed => "failed",
        ProductState::Cancelled => "failed",
        _ => "ready",
    }
}

fn setup_lines(app: &App) -> Vec<Line<'static>> {
    let width = app.args.width;
    let mut lines = Vec::new();
    lines.extend(wordmark_lines(width));
    lines.push(Line::from(""));
    lines.push(centered_line(
        "a browser agent that lives in your terminal",
        width,
        muted(),
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("step 1 of ", muted()),
        Span::styled("2", bold()),
        Span::styled("  ·  how do you want to sign in?", muted()),
    ]));
    lines.push(Line::from(""));
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(notice.clone(), failed()),
        ]));
        lines.push(Line::from(""));
    }
    let options: [(&str, &str); 5] = [
        (ACCOUNT_CODEX, "uses your ChatGPT plan"),
        (ACCOUNT_CLAUDE_CODE, "uses your Claude Pro/Max"),
        (ACCOUNT_OPENAI, "bring your own key"),
        (ACCOUNT_ANTHROPIC, "bring your own key"),
        (ACCOUNT_OPENROUTER, "many models, one key"),
    ];
    for (idx, (label, hint)) in options.iter().enumerate() {
        lines.push(setup_account_row(*label, *hint, idx, app.selected_row));
    }
    lines
}

fn setup_account_row(label: &str, hint: &str, idx: usize, selected_row: usize) -> Line<'static> {
    let is_selected = idx == selected_row;
    let chev = if is_selected { "▸" } else { " " };
    let chev_style = if is_selected { accent() } else { dim() };
    let label_style = if is_selected { bold() } else { text_style() };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(chev.to_string(), chev_style),
        Span::raw("  "),
        Span::styled(format!("{label:<28}"), label_style),
        Span::styled(hint.to_string(), muted()),
    ])
}

fn account_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Authenticate", bold())),
        Line::from(""),
    ];
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(
            notice.clone(),
            status_style("failed"),
        )));
        lines.push(Line::from(""));
    }
    for (idx, account) in ACCOUNT_CHOICES.iter().enumerate() {
        let status = if *account == "Codex login" || app.account_ready(account).unwrap_or(false) {
            "connected"
        } else if account.contains("API key") {
            "needs key"
        } else {
            "needs auth"
        };
        lines.push(selected(
            &format!("{account:<24} {status}"),
            idx,
            app.selected_row,
        ));
    }
    lines
}

fn api_key_lines(app: &App) -> Vec<Line<'static>> {
    let account = app.api_key_account.as_deref().unwrap_or("selected account");
    let mut lines = vec![Line::from(Span::styled(auth_secret_label(account), bold()))];
    lines.push(Line::from(""));
    if is_claude_code_account(account) {
        lines.extend([
            Line::from("  Claude Code uses Browser Use's Anthropic OAuth login."),
            Line::from("  Run this in another terminal to open the browser sign-in:"),
            Line::from(Span::styled(
                "    browser-use-terminal auth login claude-code",
                text_style(),
            )),
            Line::from(Span::styled(
                "  This stores the refreshable Claude Code credential locally.",
                muted(),
            )),
            Line::from(""),
        ]);
    }
    lines.extend([
        Line::from(format!(
            "  {}",
            masked_secret_for_account(account, app.composer.input())
        )),
        Line::from(""),
        Line::from(Span::styled(
            if is_claude_code_account(account) {
                "  Pasted values are treated as legacy access tokens. Prefer the login command above."
            } else {
                "  This key is stored locally in browser-use state."
            },
            muted(),
        )),
        Line::from(""),
    ]);
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(
            notice.clone(),
            status_style("failed"),
        )));
        lines.push(Line::from(""));
    }
    lines.push(selected("Save key", 0, app.selected_row));
    lines.push(selected("Cancel", 1, app.selected_row));
    lines
}

fn telemetry_key_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Laminar API key", bold())),
        Line::from(""),
        Line::from(format!("  {}", masked_secret(app.composer.input()))),
        Line::from(""),
        Line::from(Span::styled(
            "  Stored locally and used by future agent runs.",
            muted(),
        )),
        Line::from(""),
    ];
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(
            notice.clone(),
            status_style("failed"),
        )));
        lines.push(Line::from(""));
    }
    lines.push(selected("Save key", 0, app.selected_row));
    lines.push(selected("Cancel", 1, app.selected_row));
    lines
}

fn model_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(notice.clone(), failed())));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled("recommended", muted())));
    for idx in 0..=2 {
        lines.push(model_row(idx, app));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("bring your own key", muted())));
    for idx in 3..=5 {
        lines.push(model_row(idx, app));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("openrouter", muted())));
    for idx in 6..=8 {
        lines.push(model_row(idx, app));
    }
    lines
}

fn model_row(idx: usize, app: &App) -> Line<'static> {
    let choice = MODEL_CHOICES[idx];
    let selected = idx == app.selected_row;
    let current =
        app.model_configured && app.model == choice.display && app.account == choice.account;
    let chev = if selected { "▸" } else { " " };
    let chev_style = if selected { accent() } else { dim() };
    let name_style = if selected { bold() } else { text_style() };
    let access = access_label(choice.account);
    let descriptor = descriptor_for(idx);
    let descriptor_style = if descriptor == "needs key" {
        dim()
    } else {
        muted()
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(chev.to_string(), chev_style),
        Span::raw(" "),
        Span::styled(format!("{:<20}", choice.display), name_style),
        Span::styled(format!("{:<22}", access), muted()),
        Span::styled(format!("{:<22}", descriptor), descriptor_style),
        Span::styled(if current { "★" } else { "" }.to_string(), done()),
    ])
}

fn access_label(account: &'static str) -> &'static str {
    if account == ACCOUNT_CODEX {
        "Codex login"
    } else if is_claude_code_account(account) {
        "Claude Code sub"
    } else {
        account
    }
}

fn descriptor_for(idx: usize) -> &'static str {
    match idx {
        0 => "best default",
        1 => "good browser agent",
        2 => "latest, strongest",
        7 => "vision + tools",
        _ => "needs key",
    }
}

fn browser_select_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Choose browser", bold())),
        Line::from(""),
    ];
    let descriptions = [
        "remote browser with live view",
        "visible browser on this machine",
        "background browser",
    ];
    for (idx, browser) in BROWSER_CHOICES.iter().enumerate() {
        lines.push(selected(
            &format!("{browser:<24} {}", descriptions[idx]),
            idx,
            app.selected_row,
        ));
    }
    lines.extend([
        Line::from(""),
        Line::from(Span::styled("Current", muted())),
        Line::from(format!("  {}", app.browser)),
    ]);
    lines
}

fn ready_lines(app: &App, state: &WorkbenchState, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(notice.clone(), failed())));
        lines.push(Line::from(""));
    }
    lines.extend(wordmark_lines(width));
    lines.push(Line::from(""));

    if !state.history.is_empty() {
        let total = state.history.len();
        let header_text = if total > 4 {
            format!("recent  ·  {total} total")
        } else {
            "recent".to_string()
        };
        lines.push(Line::from(Span::styled(header_text, muted())));
        let rows: Vec<&HistoryRow> = state.history.iter().take(4).collect();
        for chunk in rows.chunks(2) {
            lines.extend(history_card_row(chunk, width as usize));
        }
    }
    lines
}

fn history_card_row(rows: &[&HistoryRow], total_width: usize) -> Vec<Line<'static>> {
    const GAP: usize = 2;
    const MIN_CARD: usize = 36;
    let card_w = if rows.len() == 2 {
        ((total_width.saturating_sub(GAP)) / 2).max(MIN_CARD)
    } else {
        total_width.max(MIN_CARD)
    };
    let card_a = history_card(rows[0], card_w);
    let card_b = if rows.len() > 1 {
        Some(history_card(rows[1], card_w))
    } else {
        None
    };
    let mut out = Vec::with_capacity(card_a.len());
    for i in 0..card_a.len() {
        let mut spans = card_a[i].spans.clone();
        if let Some(b) = card_b.as_ref() {
            spans.push(Span::raw(" ".repeat(GAP)));
            spans.extend(b[i].spans.clone());
        }
        out.push(Line::from(spans));
    }
    out
}

fn history_card(row: &HistoryRow, card_w: usize) -> Vec<Line<'static>> {
    let inner = card_w.saturating_sub(2);
    let glyph = status_glyph(row.status.as_str());
    let glyph_style = status_style(row.status.as_str());
    let time = relative_time(row.updated_ms);
    let status_label = row.status.as_str();
    let task_max = inner.saturating_sub(6);
    let task = truncate(&row.task, task_max);
    let line1_used = 1 + glyph.chars().count() + 2 + task.chars().count();
    let line1_pad = inner.saturating_sub(line1_used);
    let meta = format!("{status_label} · {time}");
    let line2_used = 4 + meta.chars().count();
    let line2_pad = inner.saturating_sub(line2_used);
    let top = format!("╭{}╮", "─".repeat(inner));
    let bot = format!("╰{}╯", "─".repeat(inner));
    vec![
        Line::from(Span::styled(top, border())),
        Line::from(vec![
            Span::styled("│", border()),
            Span::raw(" "),
            Span::styled(glyph.to_string(), glyph_style),
            Span::raw("  "),
            Span::styled(task, text_style()),
            Span::raw(" ".repeat(line1_pad)),
            Span::styled("│", border()),
        ]),
        Line::from(vec![
            Span::styled("│", border()),
            Span::raw("    "),
            Span::styled(meta, muted()),
            Span::raw(" ".repeat(line2_pad)),
            Span::styled("│", border()),
        ]),
        Line::from(Span::styled(bot, border())),
    ]
}

fn status_glyph(status: &str) -> &'static str {
    match status {
        "done" => "✓",
        "running" | "created" => "●",
        "cancelled" => "⊘",
        "failed" => "✗",
        _ => "·",
    }
}

fn wordmark_lines(width: u16) -> Vec<Line<'static>> {
    const WORDMARK: [&str; 4] = [
        "▄",
        "█▀▀▄ █▀▀█ █▀▀█ █   █ █▀▀ █▀▀ █▀▀█   █  █ █▀▀ █▀▀",
        "█▀▀▄ █▄▄▀ █  █ █▄█▄█ ▀▀█ █▀▀ █▄▄▀   █  █ ▀▀█ █▀▀",
        "▀▀▀  ▀ ▀▀ ▀▀▀▀  ▀ ▀  ▀▀▀ ▀▀▀ ▀ ▀▀   ▀▀▀▀ ▀▀▀ ▀▀▀",
    ];
    WORDMARK
        .iter()
        .map(|line| centered_line(line, width, bold()))
        .collect()
}

fn centered_line(text: &str, width: u16, style: Style) -> Line<'static> {
    let width = width as usize;
    let len = text.chars().count();
    let pad = width.saturating_sub(len) / 2;
    Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(text.to_string(), style),
    ])
}

fn work_lines(
    state: &WorkbenchState,
    app: &App,
    width: u16,
    product_state: ProductState,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let prior_count = state.transcript.len().saturating_sub(1);
    for turn in state.transcript.iter().take(prior_count) {
        out.extend(prior_turn_lines(turn, width as usize));
        out.push(Line::from(""));
    }
    out.extend(work_header_lines(state, product_state, width as usize));
    out.push(Line::from(""));
    let tile_w: usize = 30;
    let gap: usize = 2;
    let left_w = (width as usize).saturating_sub(tile_w + gap).max(40);
    let timeline = timeline_lines(state, product_state, left_w);
    let tile = browser_tile_lines(&state.browser, tile_w);
    out.extend(zip_columns(timeline, tile, left_w, gap));
    if let Some(block) = outcome_lines(state, product_state, width as usize) {
        if !running_streaming_preview_is_outcome(state, product_state) {
            out.push(Line::from(""));
        }
        out.extend(block);
    }
    if let Some(next) = next_action_lines(state, app, product_state) {
        out.push(Line::from(""));
        out.extend(next);
    }
    out
}

fn prior_turn_lines(turn: &TranscriptTurn, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(vec![
        Span::styled("> ", accent()),
        Span::styled(turn.prompt.clone(), text_style()),
    ])];
    if let Some(result) = turn.result.as_ref() {
        out.push(Line::from(Span::styled("  result", muted())));
        let body_width = (width.saturating_sub(4).max(24)) as u16;
        let body = markdown_result_lines(result, body_width)
            .into_iter()
            .map(trim_default_markdown_indent)
            .collect::<Vec<_>>();
        for line in body.into_iter().take(3) {
            out.push(prefix_block_line("    ", line));
        }
    } else if let Some(failure) = turn.failure.as_ref() {
        out.push(Line::from(Span::styled("  error", muted())));
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(friendly_error_message(failure), failed()),
        ]));
    }
    out
}

fn work_header_lines(
    state: &WorkbenchState,
    product_state: ProductState,
    width: usize,
) -> Vec<Line<'static>> {
    let task = state
        .transcript
        .last()
        .map(|turn| turn.prompt.as_str())
        .or(state.task.as_deref())
        .unwrap_or("browser task")
        .to_string();
    let mut right_parts: Vec<(String, Style)> = Vec::new();
    if !matches!(product_state, ProductState::Running) {
        let (glyph, glyph_style, label) = state_pill(product_state);
        right_parts.extend([
            (glyph.to_string(), glyph_style),
            (" ".to_string(), muted()),
            (label.to_string(), glyph_style),
        ]);
        if let Some(elapsed) = elapsed_label(state, product_state) {
            right_parts.push((format!(" · {elapsed}"), muted()));
        }
    }
    let right_len: usize = right_parts.iter().map(|(s, _)| s.chars().count()).sum();
    let max_task = width.saturating_sub(right_len + 4).max(10);
    let task = truncate(&task, max_task);
    let task_len = task.chars().count();
    let pad = width.saturating_sub(2 + task_len + right_len);
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled("> ", accent()),
        Span::styled(task, bold()),
        Span::raw(" ".repeat(pad)),
    ];
    for (text, style) in right_parts {
        spans.push(Span::styled(text, style));
    }
    let rule = Line::from(Span::styled("─".repeat(width), border()));
    vec![Line::from(spans), rule]
}

fn state_pill(product_state: ProductState) -> (&'static str, Style, &'static str) {
    match product_state {
        ProductState::Running => ("●", running(), "working"),
        ProductState::Result => ("✓", done(), "done"),
        ProductState::Failed => ("✗", failed(), "failed"),
        ProductState::Cancelled => ("⊘", muted(), "stopped"),
        _ => ("·", muted(), "ready"),
    }
}

fn elapsed_label(state: &WorkbenchState, product_state: ProductState) -> Option<String> {
    let session = state.current_session.as_ref()?;
    let end_ms = if matches!(product_state, ProductState::Running) {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(session.updated_ms)
    } else {
        session.updated_ms
    };
    let elapsed_ms = end_ms.saturating_sub(session.created_ms).max(0);
    let secs = elapsed_ms / 1000;
    if secs < 60 {
        Some(format!("{secs}s"))
    } else if secs < 3600 {
        Some(format!("{}m {:02}s", secs / 60, secs % 60))
    } else {
        Some(format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60))
    }
}

fn zip_columns(
    left: Vec<Line<'static>>,
    right: Vec<Line<'static>>,
    left_w: usize,
    gap: usize,
) -> Vec<Line<'static>> {
    let n = left.len().max(right.len());
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let l = left.get(i).cloned().unwrap_or_else(|| Line::from(""));
        let l_chars: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
        let mut spans = l.spans;
        if let Some(r) = right.get(i) {
            let pad = left_w.saturating_sub(l_chars).saturating_add(gap);
            spans.push(Span::raw(" ".repeat(pad)));
            spans.extend(r.spans.clone());
        }
        out.push(Line::from(spans));
    }
    out
}

fn timeline_lines(
    state: &WorkbenchState,
    product_state: ProductState,
    w: usize,
) -> Vec<Line<'static>> {
    let activity = visible_activity(
        &state.activity,
        product_state == ProductState::Running,
        true,
    );
    let groups = group_activity_for_timeline(&activity);
    let mut out = Vec::new();
    let mut n: usize = 0;
    for (label, items) in groups {
        if items.is_empty() {
            continue;
        }
        n += 1;
        out.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(format!("{n:02}"), muted()),
            Span::raw("  "),
            Span::styled(truncate(label, w.saturating_sub(5)), bold()),
        ]));
        for item in items {
            let item = truncate(&item, w.saturating_sub(5).max(8));
            out.push(Line::from(vec![
                Span::raw("     "),
                Span::styled(item, text_style()),
            ]));
        }
        out.push(Line::from(""));
    }
    if out.is_empty() {
        let (text, style) = match product_state {
            ProductState::Running => (
                format!("{} starting browser task", spinner_frame()),
                running(),
            ),
            _ => ("no recorded steps".to_string(), dim()),
        };
        out.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("··", muted()),
            Span::raw("  "),
            Span::styled(truncate(&text, w.saturating_sub(5).max(8)), style),
        ]));
    } else if out
        .last()
        .is_some_and(|line| line.spans.iter().all(|s| s.content.trim().is_empty()))
    {
        out.pop();
    }
    out
}

fn group_activity_for_timeline(activity: &[String]) -> Vec<(&'static str, Vec<String>)> {
    let mut browser = Vec::new();
    let mut thinking = Vec::new();
    let mut helpers = Vec::new();
    let mut explored = Vec::new();
    let mut ran = Vec::new();
    let mut changed = Vec::new();
    let mut other = Vec::new();
    for item in activity {
        let formatted = format_activity_item(item);
        if is_browser_activity(item) {
            browser.push(formatted);
        } else if is_thinking_activity(item) {
            thinking.push(formatted);
        } else if is_helper_activity(item) {
            helpers.push(formatted);
        } else if is_command_activity(item) {
            ran.push(formatted);
        } else if is_change_activity(item) {
            changed.push(formatted);
        } else if is_explore_activity(item) {
            explored.push(formatted);
        } else {
            other.push(formatted);
        }
    }
    vec![
        ("browser", browser),
        ("thinking", thinking),
        ("helpers", helpers),
        ("explored", explored),
        ("ran", ran),
        ("changed", changed),
        ("activity", other),
    ]
}

fn browser_tile_lines(
    browser: &browser_use_protocol::BrowserSummary,
    w: usize,
) -> Vec<Line<'static>> {
    let inner = w.saturating_sub(2);
    let url = browser
        .url
        .as_deref()
        .or(browser.live_url.as_deref())
        .filter(|u| !u.is_empty())
        .map(compact_url_for_render)
        .unwrap_or_else(|| "idle".to_string());
    let tabs = browser
        .tabs
        .map(|t| format!("{t} tab{}", if t == 1 { "" } else { "s" }))
        .unwrap_or_else(|| "—".to_string());
    let status = if browser.status.is_empty() || browser.status == "not connected" {
        "idle".to_string()
    } else {
        browser.status.clone()
    };
    let mut lines = Vec::new();
    let title = " BROWSER ";
    let dashes = inner.saturating_sub(2 + title.chars().count());
    lines.push(Line::from(vec![
        Span::styled("╭".to_string(), border()),
        Span::styled("──".to_string(), border()),
        Span::styled(title.to_string(), bold()),
        Span::styled("─".repeat(dashes), border()),
        Span::styled("╮".to_string(), border()),
    ]));
    lines.push(tile_content(&url, link(), inner));
    lines.push(tile_content(
        &format!("{tabs} · {status}"),
        text_style(),
        inner,
    ));
    let hint = if browser.live_url.is_some() {
        "f2 · live view"
    } else {
        "f2 to open"
    };
    lines.push(tile_content(hint, muted(), inner));
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner)),
        border(),
    )));
    lines
}

fn tile_content(text: &str, style: Style, inner: usize) -> Line<'static> {
    let text = truncate(text, inner.saturating_sub(4));
    let used = 3 + text.chars().count();
    let pad = inner.saturating_sub(used);
    Line::from(vec![
        Span::styled("│".to_string(), border()),
        Span::raw("  "),
        Span::styled(text, style),
        Span::raw(" ".repeat(pad)),
        Span::raw(" "),
        Span::styled("│".to_string(), border()),
    ])
}

fn outcome_lines(
    state: &WorkbenchState,
    product_state: ProductState,
    width: usize,
) -> Option<Vec<Line<'static>>> {
    if matches!(product_state, ProductState::Failed) {
        let error = state.failure.as_deref().unwrap_or("The task failed.");
        return Some(vec![
            Line::from(Span::styled("error", muted())),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(friendly_error_message(error), failed()),
            ]),
        ]);
    }
    if matches!(product_state, ProductState::Cancelled) {
        return Some(vec![
            Line::from(Span::styled("stopped", muted())),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("Progress is saved in history.", muted()),
            ]),
        ]);
    }
    if let Some(result) = state
        .transcript
        .last()
        .and_then(|turn| turn.result.as_deref())
        .or(state.result.as_deref())
    {
        let mut out = result_collapsed_lines(result, width);
        if let Some(source) = state
            .browser
            .url
            .as_ref()
            .or(state.browser.live_url.as_ref())
            .filter(|source| is_useful_source(source))
        {
            let source = truncate(
                &compact_source_url_for_render(source),
                width.saturating_sub(2).max(8),
            );
            out.push(Line::from(""));
            out.push(Line::from(vec![
                Span::styled("source", muted()),
                Span::raw("  "),
                Span::styled(source, link()),
            ]));
        }
        return Some(out);
    }
    if matches!(product_state, ProductState::Running) {
        return current_streaming_text(state).map(|text| streaming_collapsed_lines(text, width));
    }
    None
}

fn running_streaming_preview_is_outcome(
    state: &WorkbenchState,
    product_state: ProductState,
) -> bool {
    matches!(product_state, ProductState::Running)
        && state
            .transcript
            .last()
            .and_then(|turn| turn.result.as_deref())
            .or(state.result.as_deref())
            .is_none()
        && current_streaming_text(state).is_some()
}

fn current_streaming_text(state: &WorkbenchState) -> Option<&str> {
    state
        .transcript
        .last()
        .and_then(|turn| turn.streaming_text.as_deref())
        .map(str::trim_end)
        .filter(|text| !text.trim().is_empty())
}

fn streaming_collapsed_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let body_width = width.saturating_sub(4).max(24) as u16;
    let body = markdown_result_lines(text, body_width)
        .into_iter()
        .map(trim_default_markdown_indent);
    let mut out = vec![Line::from(Span::styled("streaming", muted()))];
    for line in body {
        out.push(prefix_block_line("  ", line));
    }
    out
}

fn result_collapsed_lines(result: &str, width: usize) -> Vec<Line<'static>> {
    let body_width = width.saturating_sub(4).max(24) as u16;
    let body = markdown_result_lines(result, body_width)
        .into_iter()
        .map(trim_default_markdown_indent);
    let mut out = vec![Line::from(Span::styled("result", muted()))];
    for line in body {
        out.push(prefix_block_line("  ", line));
    }
    out
}

fn next_action_lines(
    state: &WorkbenchState,
    app: &App,
    product_state: ProductState,
) -> Option<Vec<Line<'static>>> {
    let actions: Vec<&str> = match product_state {
        ProductState::Failed => {
            let error = state.failure.as_deref().unwrap_or("");
            let (primary, secondary) = failure_actions(error);
            vec![primary, secondary, "Retry", "New task"]
        }
        ProductState::Cancelled => vec![
            "Continue with a follow-up",
            "Start a new task",
            "Previous work",
        ],
        _ => return None,
    };
    let effective_selection = if app.is_slash_palette_active() {
        usize::MAX
    } else {
        app.selected_row
    };
    let mut out = vec![Line::from(Span::styled("next", muted()))];
    for (idx, label) in actions.iter().enumerate() {
        out.push(prefix_block_line(
            "  ",
            selected(label, idx, effective_selection),
        ));
    }
    Some(out)
}

fn transcript_lines(
    state: &WorkbenchState,
    width: u16,
    product_state: ProductState,
    running: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if append_transcript_turns(&mut lines, state, width, running) {
        if product_state == ProductState::Cancelled {
            lines.push(Line::from(""));
            append_ascii_text_block(
                &mut lines,
                "stopped",
                &["Progress is saved in history.".to_string()],
                None,
            );
        }
        return lines;
    }

    append_task_section(&mut lines, state);
    lines.push(Line::from(""));
    append_activity_section(&mut lines, state, running);
    match product_state {
        ProductState::Result => {
            lines.push(Line::from(""));
            if let Some(result) = state.result.as_ref() {
                append_result_block(&mut lines, result, state, width);
            } else {
                append_ascii_text_block(
                    &mut lines,
                    "result",
                    &["No result yet.".to_string()],
                    None,
                );
            }
        }
        ProductState::Failed => {
            lines.push(Line::from(""));
            append_ascii_text_block(
                &mut lines,
                "error",
                &[friendly_error_message(
                    state.failure.as_deref().unwrap_or("The task failed."),
                )],
                None,
            );
        }
        ProductState::Cancelled => {
            lines.push(Line::from(""));
            append_ascii_text_block(
                &mut lines,
                "stopped",
                &["Progress is saved in history.".to_string()],
                None,
            );
        }
        _ => {}
    }
    lines
}

fn native_plain_transcript_lines(
    state: &WorkbenchState,
    width: u16,
    product_state: ProductState,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !state.transcript.is_empty() {
        for (idx, turn) in state.transcript.iter().enumerate() {
            if idx > 0 {
                lines.push(Line::from(""));
            }
            let is_current_turn = idx + 1 == state.transcript.len();
            let is_current_running = is_current_turn
                && product_state == ProductState::Running
                && turn.result.is_none()
                && turn.failure.is_none();
            append_prompt_section(&mut lines, &turn.prompt);
            append_turn_activity(&mut lines, turn, is_current_running, is_current_running);
            if let Some(result) = turn.result.as_ref() {
                lines.push(Line::from(""));
                if is_current_turn {
                    append_result_block(&mut lines, result, state, width);
                } else {
                    append_markdown_block(&mut lines, "result", result, width, Some("done"));
                }
            } else if let Some(failure) = turn.failure.as_ref() {
                if !(is_current_turn && product_state == ProductState::Failed) {
                    lines.push(Line::from(""));
                    append_ascii_lines_block(
                        &mut lines,
                        "error",
                        vec![Line::from(Span::styled(
                            friendly_error_message(failure),
                            muted(),
                        ))],
                        Some("saved"),
                    );
                }
            } else if is_current_turn && product_state == ProductState::Running {
                if let Some(streaming_text) = turn.streaming_text.as_deref() {
                    lines.push(Line::from(""));
                    append_streaming_block(&mut lines, streaming_text, width);
                }
            }
        }
        return lines;
    }

    append_task_section(&mut lines, state);
    if !state.activity.is_empty() {
        lines.push(Line::from(""));
        append_activity_blocks(&mut lines, &state.activity);
    }
    lines
}

fn browser_panel_lines(app: &App, state: &WorkbenchState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Current browser", bold())),
        Line::from(""),
        kv_line("backend", &app.browser),
        kv_line("status", &state.browser.status),
        kv_line("title", state.browser.title.as_deref().unwrap_or("unknown")),
        kv_line(
            "page",
            state.browser.url.as_deref().unwrap_or("no page yet"),
        ),
        kv_line(
            "live view",
            state
                .browser
                .live_url
                .as_deref()
                .map(|_| "available")
                .unwrap_or("not available"),
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
        selected("Open live browser", 0, app.selected_row),
        selected("Reconnect", 1, app.selected_row),
        selected("Change browser", 2, app.selected_row),
    ];
    if let Some(notice) = app.browser_notice.as_ref() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(notice.clone(), muted())));
    }
    lines
}

fn history_lines(app: &App, state: &WorkbenchState) -> Vec<Line<'static>> {
    if state.history.is_empty() {
        return vec![Line::from(Span::styled("No previous work yet.", dim()))];
    }
    state
        .history
        .iter()
        .enumerate()
        .map(|(idx, row)| history_overlay_line(row, idx, app.selected_row, 88))
        .collect()
}

fn developer_lines(app: &App, state: &WorkbenchState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Laminar", bold())),
        Line::from(""),
        kv_line(
            "status",
            &app.laminar_status()
                .unwrap_or_else(|_| "settings unavailable".to_string()),
        ),
    ];
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(notice.clone(), muted())));
    }
    lines.push(selected("Configure Laminar", 0, app.selected_row));
    lines.extend([
        Line::from(""),
        Line::from(Span::styled("Current task", bold())),
        Line::from(""),
    ]);
    let Some(session) = state.current_session.as_ref() else {
        lines.push(Line::from(Span::styled("No task selected.", dim())));
        return lines;
    };
    append_telemetry_detail_lines(&mut lines, &state.telemetry);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Events", bold())));
    lines.push(Line::from(""));
    for event in app
        .cached_events_for_session(&session.id)
        .iter()
        .rev()
        .take(12)
        .rev()
    {
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
    lines
}

fn append_task_section(lines: &mut Vec<Line<'static>>, state: &WorkbenchState) {
    append_prompt_section(
        lines,
        &state
            .task
            .clone()
            .unwrap_or_else(|| "browser task".to_string()),
    );
}

fn append_transcript_turns(
    lines: &mut Vec<Line<'static>>,
    state: &WorkbenchState,
    width: u16,
    running: bool,
) -> bool {
    if state.transcript.is_empty() {
        return false;
    }
    for (idx, turn) in state.transcript.iter().enumerate() {
        if idx > 0 {
            lines.push(Line::from(""));
        }
        let is_pending_running = running
            && idx + 1 == state.transcript.len()
            && turn.result.is_none()
            && turn.failure.is_none();
        append_prompt_section(lines, &turn.prompt);
        append_turn_activity(
            lines,
            turn,
            is_pending_running,
            is_pending_running || turn.result.is_none(),
        );
        if is_pending_running {
            if let Some(streaming_text) = turn.streaming_text.as_deref() {
                lines.push(Line::from(""));
                append_streaming_block(lines, streaming_text, width);
            }
            continue;
        }
        if let Some(failure) = turn.failure.as_ref() {
            lines.push(Line::from(""));
            append_ascii_lines_block(
                lines,
                "error",
                vec![Line::from(Span::styled(
                    friendly_error_message(failure),
                    muted(),
                ))],
                Some("saved"),
            );
        } else if let Some(result) = turn.result.as_ref() {
            lines.push(Line::from(""));
            if idx + 1 == state.transcript.len() {
                append_result_block(lines, result, state, width);
            } else {
                append_markdown_block(lines, "result", result, width, Some("done"));
            }
        }
    }
    true
}

fn append_prompt_section(lines: &mut Vec<Line<'static>>, prompt: &str) {
    lines.push(Line::from(vec![
        Span::styled("> ", accent()),
        Span::styled(prompt.to_string(), text_style()),
    ]));
}

fn append_turn_activity(
    lines: &mut Vec<Line<'static>>,
    turn: &TranscriptTurn,
    include_thinking: bool,
    include_completed_helper_markers: bool,
) {
    let activity = visible_activity(
        &turn.activity,
        include_thinking,
        include_completed_helper_markers,
    );
    if activity.is_empty() {
        return;
    }
    lines.push(Line::from(""));
    append_activity_blocks(lines, &activity);
}

fn append_activity_section(lines: &mut Vec<Line<'static>>, state: &WorkbenchState, running: bool) {
    let activity = visible_activity(&state.activity, running, running);
    if activity.is_empty() {
        let fallback = if running {
            "starting browser task"
        } else {
            "no recorded steps"
        };
        append_ascii_text_block(lines, "activity", &[fallback.to_string()], Some("pending"));
        return;
    }
    append_activity_blocks(lines, &activity);
}

fn visible_activity(
    activity: &[String],
    include_thinking: bool,
    include_completed_helper_markers: bool,
) -> Vec<String> {
    activity
        .iter()
        .filter(|item| include_thinking || !is_thinking_activity(item))
        .filter(|item| {
            include_completed_helper_markers
                || !matches!(
                    item.as_str(),
                    "helper finished" | "helper failed" | "helper stopped"
                )
        })
        .cloned()
        .collect()
}

fn append_activity_blocks(lines: &mut Vec<Line<'static>>, activity: &[String]) {
    let mut browser = Vec::new();
    let mut thinking = Vec::new();
    let mut helpers = Vec::new();
    let mut explored = Vec::new();
    let mut ran = Vec::new();
    let mut changed = Vec::new();
    let mut other = Vec::new();

    for item in activity {
        let formatted = format_activity_item(item);
        if is_browser_activity(item) {
            browser.push(formatted);
        } else if is_thinking_activity(item) {
            thinking.push(formatted);
        } else if is_helper_activity(item) {
            helpers.push(formatted);
        } else if is_command_activity(item) {
            ran.push(formatted);
        } else if is_change_activity(item) {
            changed.push(formatted);
        } else if is_explore_activity(item) {
            explored.push(formatted);
        } else {
            other.push(formatted);
        }
    }

    let mut wrote = false;
    for (title, items) in [
        ("browser", browser),
        ("thinking", thinking),
        ("helpers", helpers),
        ("explored", explored),
        ("ran", ran),
        ("changed", changed),
        ("activity", other),
    ] {
        if items.is_empty() {
            continue;
        }
        if wrote {
            lines.push(Line::from(""));
        }
        append_ascii_text_block(lines, title, &items, Some("done"));
        wrote = true;
    }
}

fn append_result_block(
    lines: &mut Vec<Line<'static>>,
    result: &str,
    state: &WorkbenchState,
    width: u16,
) {
    append_markdown_block(lines, "result", result, width, None);
    if let Some(source) = state
        .browser
        .url
        .as_ref()
        .or(state.browser.live_url.as_ref())
        .filter(|source| is_useful_source(source))
    {
        append_ascii_tail(
            lines,
            "source",
            vec![Line::from(Span::styled(source.clone(), link()))],
        );
    } else {
        append_ascii_tail(lines, "done", Vec::new());
    }
}

fn append_streaming_block(lines: &mut Vec<Line<'static>>, text: &str, width: u16) {
    append_markdown_block(lines, "streaming", text.trim_end(), width, None);
}

fn is_useful_source(source: &str) -> bool {
    let source = source.trim();
    !source.is_empty() && source != "about:blank"
}

fn append_markdown_block(
    lines: &mut Vec<Line<'static>>,
    title: &str,
    markdown: &str,
    width: u16,
    footer: Option<&str>,
) {
    let body_width = width.saturating_sub(8).max(24);
    let body = markdown_result_lines(markdown, body_width)
        .into_iter()
        .map(trim_default_markdown_indent)
        .collect();
    append_ascii_lines_block(lines, title, body, footer);
}

fn append_ascii_text_block(
    lines: &mut Vec<Line<'static>>,
    title: &str,
    items: &[String],
    footer: Option<&str>,
) {
    let body = items
        .iter()
        .map(|item| Line::from(Span::styled(item.clone(), text_style())))
        .collect();
    append_ascii_lines_block(lines, title, body, footer);
}

fn append_ascii_lines_block(
    lines: &mut Vec<Line<'static>>,
    title: &str,
    body: Vec<Line<'static>>,
    _footer: Option<&str>,
) {
    lines.push(Line::from(Span::styled(title.to_string(), muted())));
    if body.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("no details", dim()),
        ]));
    } else {
        for line in body {
            lines.push(prefix_block_line("  ", line));
        }
    }
}

fn append_ascii_tail(lines: &mut Vec<Line<'static>>, label: &str, body: Vec<Line<'static>>) {
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(label.to_string(), muted())));
    for line in body {
        lines.push(prefix_block_line("  ", line));
    }
}

fn push_gap_if_needed(lines: &mut Vec<Line<'static>>) {
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
}

fn append_grouped_event_line(
    lines: &mut Vec<Line<'static>>,
    last_group: &mut Option<String>,
    group: &str,
    item: &str,
) {
    if last_group.as_deref() != Some(group) {
        push_gap_if_needed(lines);
        lines.push(Line::from(Span::styled(group.to_string(), muted())));
        *last_group = Some(group.to_string());
    }
    lines.push(prefix_block_line(
        "  ",
        Line::from(Span::styled(item.to_string(), text_style())),
    ));
}

fn prefix_block_line(prefix: &'static str, line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix, dim())];
    spans.extend(line.spans);
    Line::from(spans)
}

fn trim_default_markdown_indent(mut line: Line<'static>) -> Line<'static> {
    if let Some(first) = line.spans.first_mut() {
        if let Some(rest) = first.content.strip_prefix("  ") {
            first.content = rest.to_string().into();
        }
    }
    line
}

fn format_activity_item(item: &str) -> String {
    item.strip_prefix("browsing ")
        .map(|url| format!("opened {}", compact_activity_url_for_render(url)))
        .or_else(|| {
            if matches!(item, "helper finished" | "helper failed" | "helper stopped") {
                Some(item.to_string())
            } else {
                item.strip_prefix("helper ").map(|text| text.to_string())
            }
        })
        .or_else(|| item.strip_prefix("thinking ").map(|text| text.to_string()))
        .or_else(|| item.strip_prefix("ran ").map(|cmd| cmd.to_string()))
        .or_else(|| {
            item.strip_prefix("read ")
                .map(|path| format!("read {path}"))
        })
        .or_else(|| {
            item.strip_prefix("searched ")
                .map(|query| format!("searched {query}"))
        })
        .or_else(|| {
            item.strip_prefix("modified ")
                .map(|path| format!("modified {path}"))
        })
        .or_else(|| {
            item.strip_prefix("added ")
                .map(|path| format!("added {path}"))
        })
        .or_else(|| {
            item.strip_prefix("deleted ")
                .map(|path| format!("deleted {path}"))
        })
        .unwrap_or_else(|| item.to_string())
}

fn compact_url_for_render(url: &str) -> String {
    url.trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))
        .unwrap_or_else(|| url.trim())
        .trim_end_matches('/')
        .to_string()
}

fn compact_activity_url_for_render(url: &str) -> String {
    let compact = compact_url_for_render(url);
    if let Some((prefix, _)) = compact.split_once('?') {
        format!("{prefix}?...")
    } else {
        compact
    }
}

fn compact_source_url_for_render(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if let Some((prefix, _)) = trimmed.split_once('?') {
        format!("{prefix}?...")
    } else {
        trimmed.to_string()
    }
}

fn compact_path_for_render(path: &str) -> String {
    let trimmed = path.trim();
    trimmed
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .filter(|tail| !tail.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

fn agent_started_text_for_render(payload: &serde_json::Value) -> String {
    let label = payload
        .get("nickname")
        .and_then(serde_json::Value::as_str)
        .or_else(|| payload.get("role").and_then(serde_json::Value::as_str))
        .unwrap_or("helper");
    format!("started {label} helper")
}

fn is_browser_activity(item: &str) -> bool {
    item.starts_with("browsing ")
        || item.starts_with("browser ")
        || item == "connected live browser"
}

fn is_thinking_activity(item: &str) -> bool {
    item.starts_with("thinking ")
}

fn is_helper_activity(item: &str) -> bool {
    item.starts_with("helper ")
}

fn is_command_activity(item: &str) -> bool {
    item.starts_with("ran ") || item.starts_with("command failed")
}

fn is_change_activity(item: &str) -> bool {
    item.starts_with("modified ") || item.starts_with("added ") || item.starts_with("deleted ")
}

fn is_explore_activity(item: &str) -> bool {
    item.starts_with("read ")
        || item.starts_with("searched ")
        || item == "listed files"
        || item.starts_with("started ")
}

fn spinner_frame() -> &'static str {
    const FRAMES: [&str; 4] = ["-", "\\", "|", "/"];
    let tick = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() / 160)
        .unwrap_or(0);
    FRAMES[tick as usize % FRAMES.len()]
}

fn kv_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{label:<10}"), muted()),
        Span::styled(value.to_string(), text_style()),
    ])
}

fn history_overlay_line(
    row: &HistoryRow,
    idx: usize,
    selected_row: usize,
    width: usize,
) -> Line<'static> {
    let task_width = width.saturating_sub(22).max(12);
    Line::from(vec![
        Span::styled(
            if idx == selected_row { "> " } else { "  " },
            if idx == selected_row { accent() } else { dim() },
        ),
        Span::styled(
            format!("{:<task_width$}", truncate(&row.task, task_width)),
            text_style(),
        ),
        Span::styled(
            format!("{:<10}", row.status.as_str()),
            status_style(row.status.as_str()),
        ),
        Span::styled(relative_time(row.updated_ms), muted()),
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

fn markdown_result_lines(markdown: &str, width: u16) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(markdown, options);
    let mut writer = MarkdownWriter::default();
    for event in parser {
        writer.handle_event(event);
    }
    wrap_markdown_lines(writer.finish(), width as usize)
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
            if looks_like_bare_link(line) || looks_like_path(line) {
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
            } else {
                self.current.push(Span::raw("  "));
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
        let indent = format!("  {}", "  ".repeat(depth));
        let Some(list) = self.list_stack.last_mut() else {
            return format!("{indent}* ");
        };
        match &mut list.next {
            Some(next) => {
                let marker = format!("{indent}{next}. ");
                *next += 1;
                marker
            }
            None => format!("{indent}* "),
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

fn wrap_markdown_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(24);
    let mut out = Vec::new();
    for line in lines {
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        if text.chars().count() <= width || text.trim().is_empty() {
            out.push(line);
            continue;
        }
        let continuation_indent = continuation_indent(&text);
        for (idx, part) in wrap_plain(&text, width, &continuation_indent)
            .into_iter()
            .enumerate()
        {
            let style = if idx == 0 {
                line.spans
                    .last()
                    .map(|span| span.style)
                    .unwrap_or_else(text_style)
            } else {
                text_style()
            };
            out.push(Line::from(Span::styled(part, style)));
        }
    }
    out
}

fn continuation_indent(text: &str) -> String {
    let leading = text.chars().take_while(|ch| ch.is_whitespace()).count();
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("* ") {
        return format!("{}  ", " ".repeat(leading + trimmed.len() - rest.len() - 2));
    }
    let marker_len = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if marker_len > 0 && trimmed.chars().nth(marker_len) == Some('.') {
        return " ".repeat(leading + marker_len + 2);
    }
    " ".repeat(leading)
}

fn wrap_plain(text: &str, width: usize, continuation_indent: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let limit = if lines.is_empty() {
            width
        } else {
            width.saturating_sub(continuation_indent.chars().count())
        };
        let current_len = current.chars().count();
        let word_len = word.chars().count();
        if current_len > 0 && current_len + 1 + word_len > limit {
            lines.push(current);
            current = continuation_indent.to_string();
            current.push_str(word);
        } else {
            if !current.is_empty() {
                current.push(' ');
            } else if !lines.is_empty() {
                current.push_str(continuation_indent);
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
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

fn looks_like_path(value: &str) -> bool {
    value.starts_with('/') || value.starts_with("~/")
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

fn browser_ready_label(app: &App, state: &WorkbenchState) -> String {
    if state.browser.status == "not connected" {
        format!("{} ready", app.browser)
    } else {
        format!("{} {}", app.browser, state.browser.status)
    }
}

fn masked_secret(value: &str) -> String {
    if value.is_empty() {
        "paste key here".to_string()
    } else {
        let prefix = value.chars().take(8).collect::<String>();
        format!(
            "{prefix}{}",
            "*".repeat(value.chars().count().saturating_sub(8).max(8))
        )
    }
}

fn masked_secret_for_account(account: &str, value: &str) -> String {
    if value.is_empty() && is_claude_code_account(account) {
        "optional legacy access token".to_string()
    } else {
        masked_secret(value)
    }
}

fn auth_secret_label(account: &str) -> &'static str {
    match account {
        ACCOUNT_OPENAI => "OpenAI API key",
        ACCOUNT_OPENROUTER => "OpenRouter API key",
        ACCOUNT_ANTHROPIC => "Anthropic API key",
        account if is_claude_code_account(account) => "Claude Code OAuth token",
        _ => "Credential",
    }
}

fn friendly_error_message(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if lower.contains("auth login openrouter") || lower.contains("openrouter_api_key") {
        return "OpenRouter API key is missing.".to_string();
    }
    if lower.contains("auth login openai") || lower.contains("openai_api_key") {
        return "OpenAI API key is missing.".to_string();
    }
    if lower.contains("auth login anthropic") || lower.contains("anthropic_api_key") {
        return "Anthropic API key is missing.".to_string();
    }
    if lower.contains("auth login claude-code")
        || lower.contains("claude_code_oauth_token")
        || lower.contains("claude code")
    {
        return "Claude Code login is missing.".to_string();
    }
    truncate(&first_line(value), 96)
}

fn failure_actions(error: &str) -> (&'static str, &'static str) {
    let lower = error.to_ascii_lowercase();
    if lower.contains("openrouter") {
        ("Authenticate with OpenRouter", "Choose a different model")
    } else if lower.contains("openai") {
        ("Authenticate with OpenAI", "Choose a different model")
    } else if lower.contains("anthropic") || lower.contains("claude") {
        ("Authenticate", "Choose a different model")
    } else if lower.contains("browser") || lower.contains("chrome") {
        ("Open browser settings", "Choose a different browser")
    } else {
        ("Retry", "Choose a different model")
    }
}

fn relative_time(ms: i64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(ms);
    let elapsed = now_ms.saturating_sub(ms);
    let seconds = elapsed / 1000;
    if seconds < 60 {
        return "recent".to_string();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days == 1 {
        "yesterday".to_string()
    } else {
        format!("{days}d ago")
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
