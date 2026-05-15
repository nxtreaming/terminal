use anyhow::Result;
use browser_use_protocol::{
    EventRecord, HistoryRow, SessionMeta, TelemetrySummary, TranscriptTurn, WorkbenchState,
};
use ratatui::backend::TestBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::composer::composer_rule;
use crate::markdown::render_markdown_lines;
use crate::palette;
use crate::settings::{
    is_claude_code_account, ACCOUNT_ANTHROPIC, ACCOUNT_CHOICES, ACCOUNT_CLAUDE_CODE, ACCOUNT_CODEX,
    ACCOUNT_OPENAI, ACCOUNT_OPENROUTER, BROWSER_CHOICES, MODEL_CHOICES,
};
use crate::theme::*;

use super::{App, ProductState, Surface};

pub(crate) const APP_HORIZONTAL_MARGIN: u16 = 2;
const CONTENT_HORIZONTAL_MARGIN: u16 = 2;
pub(crate) const NATIVE_TRANSCRIPT_HORIZONTAL_MARGIN: u16 =
    APP_HORIZONTAL_MARGIN + CONTENT_HORIZONTAL_MARGIN;

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
    if let Some(timeline) = tool_aware_chronological_lines(app, &state, body_width, product_state) {
        lines.extend(timeline);
    } else if matches!(
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

#[cfg(test)]
pub(crate) fn native_scrollback_event_lines(
    events: &[EventRecord],
    state: &WorkbenchState,
    width: u16,
    last_group: &mut Option<String>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for event in events {
        append_native_timeline_event(&mut lines, last_group, None, state, event, width);
    }
    lines
}

pub(crate) fn native_scrollback_chronological_event_lines(
    app: &App,
    state: &WorkbenchState,
    session_id: &str,
    after_seq: i64,
    width: u16,
    last_group: &mut Option<String>,
) -> (Vec<Line<'static>>, i64) {
    let events = chronological_events_for_session(app, session_id)
        .into_iter()
        .filter(|event| event.seq > after_seq)
        .collect::<Vec<_>>();
    let last_seq = events
        .iter()
        .map(|event| event.seq)
        .max()
        .unwrap_or(after_seq);
    let mut lines = Vec::new();
    for event in events {
        append_native_timeline_event(&mut lines, last_group, Some(app), state, event, width);
    }
    (lines, last_seq)
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
        horizontal: APP_HORIZONTAL_MARGIN,
    })
}

fn content_area(area: Rect) -> Rect {
    area.inner(Margin {
        vertical: 0,
        horizontal: CONTENT_HORIZONTAL_MARGIN,
    })
}

fn content_width(width: u16) -> u16 {
    width
        .saturating_sub(CONTENT_HORIZONTAL_MARGIN.saturating_mul(2))
        .max(1)
}

fn render_main(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut App,
    state: &WorkbenchState,
    product_state: ProductState,
) {
    let bottom_h = main_bottom_height_for(app, state, app.surface, area, product_state);
    let body_width = content_width(area.width);
    let native_scrollback_active =
        app.native_scrollback_is_active() && !app.surface.is_bottom_pane();
    let show_footer = app.surface.is_bottom_pane()
        || app
            .quit_hint_until
            .is_some_and(|until| std::time::Instant::now() <= until)
        || app.escape_stop_is_pending();
    let footer_h = u16::from(show_footer && area.height > bottom_h);
    let max_body_h = area
        .height
        .saturating_sub(bottom_h)
        .saturating_sub(footer_h);
    let body = if app.surface.is_bottom_pane() {
        Vec::new()
    } else if native_scrollback_active {
        native_replay_live_lines(app, state, product_state, body_width, max_body_h)
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
    let pin_bottom = should_pin_main_bottom(product_state, native_scrollback_active);
    let (body_area, bottom_area, footer_area) =
        main_layout_areas(area, bottom_h, body.len(), show_footer, pin_bottom);
    let mut body = body;
    if body.len() > body_area.height as usize {
        body = visible_main_body_lines(body, body_area.height, product_state);
    }
    let body_render_area = if pin_bottom
        && !body.is_empty()
        && body.len() < body_area.height as usize
    {
        let empty_rows = body_area.height.saturating_sub(body.len() as u16);
        let top_gap = match product_state {
            ProductState::Result => empty_rows.saturating_sub(4).min(8),
            ProductState::Running | ProductState::Failed | ProductState::Cancelled => empty_rows,
            ProductState::Ready | ProductState::SetupNeeded => 0,
        };
        Rect {
            y: body_area.y.saturating_add(top_gap),
            height: body_area.height.saturating_sub(top_gap),
            ..body_area
        }
    } else {
        body_area
    };
    frame.render_widget(
        Paragraph::new(body)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        content_area(body_render_area),
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
    let body_h = if pin_bottom {
        max_body_h
    } else {
        (body_len as u16).min(max_body_h)
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

fn should_pin_main_bottom(product_state: ProductState, native_scrollback_active: bool) -> bool {
    (native_scrollback_active && product_state != ProductState::Result)
        || matches!(
            product_state,
            ProductState::Running | ProductState::Failed | ProductState::Cancelled
        )
}

pub(crate) fn main_viewport_height(app: &App, width: u16) -> u16 {
    let current = composer_pane_height(app, ProductState::Ready, width);
    let input_h = composer_visual_input_lines(app, width);
    let palette_h = (palette::max_item_count() as u16).min(8).saturating_add(3);
    let max_palette = input_h.saturating_add(palette_h).saturating_add(2);
    current.max(max_palette)
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

fn composer_pane_height(app: &App, _product_state: ProductState, width: u16) -> u16 {
    let visual_input_lines = composer_visual_input_lines(app, width);
    let palette_h = slash_palette_pane_height(app);
    if palette_h > 0 {
        visual_input_lines + palette_h + 2
    } else {
        visual_input_lines + 3
    }
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
    (items.len() as u16).min(8).saturating_add(3)
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
            if let Some(streaming_text) = current_streaming_text(state) {
                let mut lines = Vec::new();
                append_streaming_block(&mut lines, streaming_text, width);
                lines
            } else {
                let mut lines = Vec::new();
                append_ascii_text_block(
                    &mut lines,
                    "status",
                    &["running browser task".to_string()],
                    Some("live"),
                );
                lines
            }
        }
        ProductState::Failed => {
            let error = state.failure.as_deref().unwrap_or("The task failed.");
            let (primary, secondary) = failure_actions(error);
            let mut next_lines = Vec::new();
            append_ascii_lines_block(
                &mut next_lines,
                "next",
                vec![
                    selected(primary, 0, app.selected_row),
                    selected(secondary, 1, app.selected_row),
                    selected("Retry", 2, app.selected_row),
                    selected("New task", 3, app.selected_row),
                ],
                None,
            );
            next_lines
        }
        ProductState::Cancelled => {
            let mut next_lines = Vec::new();
            append_ascii_lines_block(
                &mut next_lines,
                "next",
                vec![
                    selected("Continue with a follow-up", 0, app.selected_row),
                    selected("Start a new task", 1, app.selected_row),
                    selected("Previous work", 2, app.selected_row),
                ],
                None,
            );
            next_lines
        }
        ProductState::Result => Vec::new(),
        ProductState::Ready => Vec::new(),
        ProductState::SetupNeeded => setup_lines(app),
    };
    if lines.len() > height as usize {
        visible_tail_lines(lines, height)
    } else {
        lines
    }
}

fn visible_tail_lines(mut lines: Vec<Line<'static>>, height: u16) -> Vec<Line<'static>> {
    let height = height as usize;
    if height == 0 {
        return Vec::new();
    }
    if lines.len() > height {
        lines = lines.split_off(lines.len() - height);
    }
    lines
}

fn visible_head_lines(mut lines: Vec<Line<'static>>, height: u16) -> Vec<Line<'static>> {
    let height = height as usize;
    if height == 0 {
        return Vec::new();
    }
    if lines.len() > height {
        lines.truncate(height);
    }
    lines
}

fn visible_main_body_lines(
    lines: Vec<Line<'static>>,
    height: u16,
    product_state: ProductState,
) -> Vec<Line<'static>> {
    match product_state {
        ProductState::Ready | ProductState::SetupNeeded => visible_head_lines(lines, height),
        ProductState::Running
        | ProductState::Result
        | ProductState::Failed
        | ProductState::Cancelled => visible_tail_lines(lines, height),
    }
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
        content_area(chunks[1]),
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
        Surface::ApiKey => "Enter:save | Esc:cancel",
        Surface::Telemetry => "Enter:save | Esc:cancel",
        Surface::History => "Enter:open | R:resume | Esc:back",
        Surface::Setup => "Enter:continue | Esc:quit",
        Surface::Browser => "Enter:select | Esc:back",
        Surface::Developer => "Esc:close",
        _ => "Enter:select | Esc:back",
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
    let input_h = composer_visual_input_lines(app, area.width.saturating_sub(4));
    let action_h = if palette_h > 0 { palette_h } else { 1 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(input_h),
            Constraint::Length(1),
            Constraint::Length(action_h),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(input_box_rule(chunks[0].width)).style(border()),
        chunks[0],
    );
    let input_area = chunks[1].inner(Margin {
        vertical: 0,
        horizontal: 2,
    });
    render_composer_input(frame, input_area, app, state.current_session.as_ref());
    frame.render_widget(
        Paragraph::new(input_box_rule(chunks[2].width)).style(border()),
        chunks[2],
    );
    let action_area = chunks[3].inner(Margin {
        vertical: 0,
        horizontal: 2,
    });
    if palette_h > 0 {
        frame.render_widget(
            Paragraph::new(slash_palette_lines(app, action_area.width as usize)),
            action_area,
        );
    } else {
        frame.render_widget(
            Paragraph::new(hint_row(product_state, action_area.width as usize)),
            action_area,
        );
    }
}

fn slash_palette_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let items = app.slash_palette_items();
    let cmd_col = items
        .iter()
        .map(|item| item.command.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);
    let rule_w = width.saturating_sub(28);
    let mut lines = vec![Line::from(vec![
        Span::styled("actions ", bold()),
        Span::styled("-".repeat(rule_w), dim()),
        Span::styled(" esc close", muted()),
    ])];
    for (idx, item) in items.iter().enumerate() {
        let is_selected = idx == app.selected_row;
        let cmd_style = if is_selected { accent() } else { text_style() };
        let desc_style = if is_selected { text_style() } else { muted() };
        let desc_max = width.saturating_sub(cmd_col + 4).max(8);
        let description = truncate(item.description, desc_max);
        lines.push(Line::from(vec![
            Span::styled(
                if is_selected { "> " } else { "  " },
                if is_selected { accent() } else { dim() },
            ),
            Span::styled(format!("{:<cmd_col$}", item.command), cmd_style),
            Span::raw("  "),
            Span::styled(description, desc_style),
        ]));
    }
    lines.push(Line::from(Span::styled("-".repeat(rule_w), dim())));
    lines.push(Line::from(Span::styled(
        "up/down navigate . enter select",
        muted(),
    )));
    lines
}

fn input_box_rule(width: u16) -> String {
    let width = width as usize;
    if width < 2 {
        return "+".repeat(width);
    }
    format!("+{}+", "-".repeat(width.saturating_sub(2)))
}

fn hint_row(product_state: ProductState, width: usize) -> Line<'static> {
    let hints: &[(&str, &str)] = match product_state {
        ProductState::Running => &[("Esc", "stop"), ("F2", "browser"), ("/", "commands")],
        ProductState::Result => &[
            ("Enter", "reply"),
            ("Tab", "history"),
            ("F2", "browser"),
            ("/", "commands"),
            ("Esc", "clear"),
        ],
        ProductState::Failed | ProductState::Cancelled => &[
            ("Enter", "action"),
            ("F2", "browser"),
            ("/", "commands"),
            ("Esc", "clear"),
        ],
        _ => &[
            ("Enter", "send"),
            ("Tab", "history"),
            ("/", "commands"),
            ("Esc", "clear"),
        ],
    };
    let mut spans = Vec::new();
    for (idx, (key, action)) in hints.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled(" | ", dim()));
        }
        let text_len = key.chars().count() + action.chars().count() + 1;
        let used: usize = spans
            .iter()
            .map(|span: &Span<'_>| span.content.chars().count())
            .sum();
        if used + text_len > width {
            break;
        }
        spans.push(Span::styled((*key).to_string(), bold()));
        spans.push(Span::styled(":".to_string(), dim()));
        spans.push(Span::styled((*action).to_string(), muted()));
    }
    Line::from(spans)
}

fn compact_account_label(account: &str) -> String {
    if account == ACCOUNT_CODEX {
        "Codex".to_string()
    } else if is_claude_code_account(account) {
        "Claude Code".to_string()
    } else {
        account.replace(" API key", "")
    }
}

fn fit_cell(value: &str, width: usize) -> String {
    format!("{:<width$}", truncate(value, width))
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
        let _ = (state, product_state);
        ""
    };
    frame.render_widget(
        Paragraph::new(label)
            .style(muted())
            .alignment(Alignment::Right),
        area,
    );
}

fn setup_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("browser-use setup", muted()),
        Span::styled(" / ", dim()),
        Span::styled("authenticate", bold()),
        Span::styled(
            " -------------------------------------------------------------- ",
            dim(),
        ),
        Span::styled("step 1/3", muted()),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  CHOOSE ACCOUNT", muted())));
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
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            "--------------------------------------------------------------------------------",
            dim(),
        )),
        Line::from(Span::styled("enter select     esc quit", muted())),
    ]);
    lines
}

fn setup_account_row(label: &str, hint: &str, idx: usize, selected_row: usize) -> Line<'static> {
    let is_selected = idx == selected_row;
    let chev = if is_selected { ">" } else { " " };
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
    let chev = if selected { ">" } else { " " };
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
        Span::styled(if current { "*" } else { "" }.to_string(), done()),
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
        Line::from(Span::styled("CHOOSE BROWSER", muted())),
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
        Line::from(Span::styled("CURRENT", muted())),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(app.browser.clone(), text_style()),
            Span::styled(" . ready", done()),
        ]),
    ]);
    lines
}

fn ready_lines(app: &App, state: &WorkbenchState, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(notice.clone(), failed())));
        lines.push(Line::from(""));
    }
    lines.push(header_status_line(
        "browser-use",
        &ready_status_label(app, state),
        width as usize,
    ));
    lines.push(Line::from(""));
    lines.extend(config_card_lines(app, state, width as usize));
    lines.push(Line::from(""));

    if !state.history.is_empty() {
        let total = state.history.len();
        let header_text = if total > 3 {
            format!("recent . {total} total")
        } else {
            "recent".to_string()
        };
        lines.push(Line::from(Span::styled(header_text, muted())));
        let rows: Vec<&HistoryRow> = state.history.iter().take(3).collect();
        for row in rows {
            lines.push(history_plain_row(row, width as usize));
        }
    }
    lines
}

fn config_card_lines(app: &App, state: &WorkbenchState, width: usize) -> Vec<Line<'static>> {
    let card_w = width.clamp(32, 94);
    let inner_w = card_w.saturating_sub(2);
    let mut lines = vec![
        Line::from(Span::styled(format!("+{}+", "-".repeat(inner_w)), border())),
        card_text_line("Browser Use", "", "", inner_w),
        card_blank_line(inner_w),
        card_kv_line("model", &app.model, "/model", inner_w),
        card_kv_line(
            "account",
            &compact_account_label(&app.account),
            "/auth",
            inner_w,
        ),
        card_kv_line(
            "browser",
            &browser_ready_label(app, state).replace(" ready", " idle"),
            "/browser",
            inner_w,
        ),
        card_kv_line("cwd", &cwd_label(), "", inner_w),
        card_kv_line(
            "telemetry",
            &app.laminar_status()
                .unwrap_or_else(|_| "Laminar unavailable".to_string()),
            "/laminar",
            inner_w,
        ),
    ];
    lines.push(Line::from(Span::styled(
        format!("+{}+", "-".repeat(inner_w)),
        border(),
    )));
    lines
}

fn card_blank_line(inner_w: usize) -> Line<'static> {
    card_text_line("", "", "", inner_w)
}

fn card_kv_line(label: &str, value: &str, action: &str, inner_w: usize) -> Line<'static> {
    let label_w = 10usize.min(inner_w.saturating_sub(2));
    let action_w = if action.is_empty() {
        0
    } else {
        action.chars().count().saturating_add(2)
    };
    let value_w = inner_w
        .saturating_sub(label_w)
        .saturating_sub(action_w)
        .saturating_sub(2)
        .max(4);
    let left = format!("{label:<label_w$}{}", truncate(value, value_w));
    card_text_line(&left, action, "", inner_w)
}

fn card_text_line(left: &str, right: &str, _extra: &str, inner_w: usize) -> Line<'static> {
    let right_len = right.chars().count();
    let left_w = inner_w.saturating_sub(right_len).saturating_sub(1);
    let left = truncate(left, left_w);
    let left_len = left.chars().count();
    let spaces = inner_w.saturating_sub(left_len + right_len);
    Line::from(vec![
        Span::styled("|", border()),
        Span::raw(" "),
        Span::styled(left, text_style()),
        Span::raw(" ".repeat(spaces.saturating_sub(1))),
        Span::styled(right.to_string(), accent()),
        Span::styled("|", border()),
    ])
}

fn cwd_label() -> String {
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    if let Some(home) = std::env::var_os("HOME").and_then(|home| home.into_string().ok()) {
        if let Some(rest) = cwd.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    cwd
}

fn header_status_line(left: &str, right: &str, width: usize) -> Line<'static> {
    let right = truncate(right, width.saturating_sub(left.chars().count() + 3).max(8));
    let rule_len = width.saturating_sub(left.chars().count() + right.chars().count() + 2);
    Line::from(vec![
        Span::styled(left.to_string(), bold()),
        Span::raw(" "),
        Span::styled("-".repeat(rule_len), dim()),
        Span::raw(" "),
        Span::styled(right, done()),
    ])
}

fn ready_status_label(app: &App, state: &WorkbenchState) -> String {
    format!(
        "{} . {} . {}",
        app.model,
        compact_account_label(&app.account),
        browser_ready_label(app, state).replace(" ready", " idle")
    )
}

fn history_plain_row(row: &HistoryRow, width: usize) -> Line<'static> {
    let time = relative_time(row.updated_ms);
    let status = row.status.as_str();
    let status_label = match status {
        "done" => "done",
        "running" | "created" => "running",
        "failed" => "failed",
        "cancelled" => "stopped",
        _ => status,
    };
    let prefix = format!(": {status_label:<8}");
    let prefix_len = prefix.chars().count() + 2;
    let time_len = time.chars().count();
    let task_w = width.saturating_sub(prefix_len + time_len + 2).max(12);
    Line::from(vec![
        Span::styled(prefix, status_style(row.status.as_str())),
        Span::raw("  "),
        Span::styled(fit_cell(&row.task, task_w), text_style()),
        Span::raw("  "),
        Span::styled(time, muted()),
    ])
}

fn work_lines(
    state: &WorkbenchState,
    app: &App,
    width: u16,
    product_state: ProductState,
) -> Vec<Line<'static>> {
    let mut out =
        tool_aware_chronological_lines(app, state, width, product_state).unwrap_or_else(|| {
            transcript_lines(
                state,
                width,
                product_state,
                matches!(product_state, ProductState::Running),
            )
        });
    if out.is_empty() {
        append_task_section(&mut out, state);
    }
    if let Some(next) = next_action_lines(state, app, product_state) {
        out.push(Line::from(""));
        out.extend(next);
    }
    out
}

fn tool_aware_chronological_lines(
    app: &App,
    state: &WorkbenchState,
    width: u16,
    product_state: ProductState,
) -> Option<Vec<Line<'static>>> {
    let session = state.current_session.as_ref()?;
    let events = chronological_events_for_session(app, &session.id);
    if events.is_empty() {
        return None;
    }

    let mut lines = Vec::new();
    append_task_section(&mut lines, state);

    let mut last_group = None;
    let mut wrote_event = false;
    let mut pending_delta = PendingDeltaBlock::default();
    for event in events {
        if event.event_type == "session.input" {
            continue;
        }
        if event.session_id != session.id {
            pending_delta.flush(&mut lines, &mut last_group, width);
            let before = lines.len();
            append_child_timeline_event(&mut lines, &mut last_group, app, state, event, width);
            wrote_event |= lines.len() != before;
            continue;
        }
        if event.event_type == "model.thinking_delta" {
            let label = thinking_delta_label(event);
            if pending_delta.should_flush_before(PendingDeltaKind::Thought, label.as_deref()) {
                pending_delta.flush(&mut lines, &mut last_group, width);
            }
            pending_delta.push(PendingDeltaKind::Thought, label, event);
            continue;
        }
        pending_delta.flush(&mut lines, &mut last_group, width);
        let before = lines.len();
        append_tool_aware_event(&mut lines, &mut last_group, app, state, event, width);
        wrote_event |= lines.len() != before;
    }
    pending_delta.flush(&mut lines, &mut last_group, width);

    match product_state {
        ProductState::Result => {
            if let Some(result) = state.result.as_ref() {
                push_gap_if_needed(&mut lines);
                append_answer_event_block(&mut lines, result, state, width);
            }
        }
        ProductState::Failed => {
            if let Some(error) = state.failure.as_ref() {
                push_gap_if_needed(&mut lines);
                append_ascii_text_block(
                    &mut lines,
                    "error",
                    &[friendly_error_message(error)],
                    None,
                );
            }
        }
        ProductState::Cancelled => {
            push_gap_if_needed(&mut lines);
            append_ascii_text_block(
                &mut lines,
                "stopped",
                &["Progress is saved in history.".to_string()],
                None,
            );
        }
        ProductState::Running => {
            if let Some(streaming_text) = current_streaming_text(state) {
                push_gap_if_needed(&mut lines);
                append_answer_plain_block(&mut lines, streaming_text.trim_end(), width);
            }
        }
        ProductState::Ready | ProductState::SetupNeeded => {}
    }

    (wrote_event || matches!(product_state, ProductState::Result | ProductState::Running))
        .then_some(lines)
}

fn chronological_events_for_session<'a>(
    app: &'a App,
    root_session_id: &str,
) -> Vec<&'a EventRecord> {
    let mut session_ids = vec![root_session_id.to_string()];
    let mut index = 0;
    while index < session_ids.len() {
        let parent_id = session_ids[index].clone();
        for session in app
            .state_cache
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

    let mut events = session_ids
        .iter()
        .flat_map(|session_id| app.state_cache.events_for_session(session_id))
        .collect::<Vec<_>>();
    events.sort_by_key(|event| event.seq);
    events
}

fn append_tool_aware_event(
    lines: &mut Vec<Line<'static>>,
    last_group: &mut Option<String>,
    app: &App,
    state: &WorkbenchState,
    event: &EventRecord,
    width: u16,
) {
    match event.event_type.as_str() {
        "session.followup" => {
            if let Some(prompt) = event
                .payload
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|prompt| !prompt.is_empty())
            {
                last_group.take();
                push_gap_if_needed(lines);
                append_prompt_section(lines, prompt);
            }
        }
        "agent.spawned" => {
            let label = event
                .payload
                .get("nickname")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    event
                        .payload
                        .get("role")
                        .and_then(serde_json::Value::as_str)
                })
                .unwrap_or("subagent");
            append_timeline_item(
                lines,
                last_group,
                "subagent",
                &format!("{label} started"),
                width,
                text_style(),
            );
        }
        "agent.completed" => {
            let child_id = event
                .payload
                .get("child_session_id")
                .and_then(serde_json::Value::as_str);
            let label = child_id
                .map(|id| helper_label_for_session(app, id))
                .unwrap_or_else(|| "subagent".to_string());
            append_timeline_item(
                lines,
                last_group,
                "subagent",
                &format!("{label} finished"),
                width,
                text_style(),
            );
            if let Some(result) = event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("result"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|result| !result.is_empty())
            {
                append_preview_markdown(lines, last_group, "subagent", result, width, 3);
            }
        }
        "agent.failed" => {
            let error = event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("error"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("subagent failed");
            append_timeline_item(lines, last_group, "error", error, width, failed());
        }
        "agent.cancelled" => {
            append_timeline_item(
                lines,
                last_group,
                "subagent",
                "subagent stopped",
                width,
                muted(),
            );
        }
        "model.tool_call" => append_tool_call_intent(lines, last_group, app, event, width),
        "model.thinking_delta" => {}
        "tool.failed" => {
            let name = event
                .payload
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool");
            let error = event
                .payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool failed");
            append_timeline_item(
                lines,
                last_group,
                "error",
                &format!("{name} failed: {error}"),
                width,
                failed(),
            );
        }
        "model.turn.request" => {
            let model = event
                .payload
                .get("model")
                .and_then(serde_json::Value::as_str)
                .filter(|model| !model.trim().is_empty())
                .unwrap_or("model");
            append_timeline_item(
                lines,
                last_group,
                "thinking",
                &format!("waiting for {model}"),
                width,
                muted(),
            );
        }
        "model.turn.retry" => {
            append_timeline_item(
                lines,
                last_group,
                "thinking",
                "retrying model request",
                width,
                muted(),
            );
        }
        "model.turn.error" => {
            append_timeline_item(
                lines,
                last_group,
                "thinking",
                "model request hit an error",
                width,
                failed(),
            );
        }
        "file.list" => {
            let path = event
                .payload
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(|path| display_path(path, state))
                .unwrap_or_else(|| ".".to_string());
            append_timeline_item(lines, last_group, "list", &path, width, text_style());
        }
        "file.read" => {
            if let Some(path) = event
                .payload
                .get("path")
                .and_then(serde_json::Value::as_str)
            {
                append_timeline_item(
                    lines,
                    last_group,
                    "read",
                    &display_path(path, state),
                    width,
                    text_style(),
                );
            }
        }
        "file.search" => {
            let query = event
                .payload
                .get("query")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("files");
            let matches = event
                .payload
                .get("matches")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            append_timeline_item(
                lines,
                last_group,
                "search",
                &format!("Search {query:?} ({matches} matches)"),
                width,
                text_style(),
            );
        }
        "command.started" => {
            let cmd = event
                .payload
                .get("cmd")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("command");
            append_timeline_item(lines, last_group, "run", cmd, width, text_style());
        }
        "command.output" => {
            if let Some(text) = event
                .payload
                .get("text")
                .and_then(serde_json::Value::as_str)
            {
                append_preview_text(lines, last_group, "run", text, width, 4);
            }
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
                append_timeline_item(
                    lines,
                    last_group,
                    "run",
                    &format!("failed with exit {code}"),
                    width,
                    failed(),
                );
            }
        }
        "tool.output_spilled" => {
            let path = event
                .payload
                .get("artifact")
                .and_then(|artifact| artifact.get("path"))
                .and_then(serde_json::Value::as_str)
                .map(|path| display_path(path, state))
                .unwrap_or_else(|| "artifact".to_string());
            append_timeline_item(
                lines,
                last_group,
                "run",
                &format!("Full output saved to {path}"),
                width,
                muted(),
            );
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
                .map(|path| display_path(path, state))
                .unwrap_or_else(|| "file".to_string());
            append_timeline_item(
                lines,
                last_group,
                "edit",
                &format!("{kind} {path}"),
                width,
                text_style(),
            );
        }
        "browser.connected" | "browser.reconnected" | "browser.target_changed" => {
            append_timeline_item(
                lines,
                last_group,
                "browser",
                "browser connected",
                width,
                text_style(),
            );
        }
        "browser.disconnected" => {
            append_timeline_item(
                lines,
                last_group,
                "browser",
                "browser disconnected",
                width,
                muted(),
            );
        }
        "browser.live_url" => {
            append_timeline_item(
                lines,
                last_group,
                "browser",
                "live view available",
                width,
                text_style(),
            );
        }
        "browser.page" | "browser.state" => {
            if let Some(url) = event.payload.get("url").and_then(serde_json::Value::as_str) {
                append_timeline_item(
                    lines,
                    last_group,
                    "browser",
                    &format!("opened {}", compact_activity_url_for_render(url)),
                    width,
                    text_style(),
                );
            }
        }
        "plan.updated" => {
            append_timeline_item(
                lines,
                last_group,
                "plan",
                "updated plan",
                width,
                text_style(),
            );
        }
        _ => {}
    }
}

fn append_native_timeline_event(
    lines: &mut Vec<Line<'static>>,
    last_group: &mut Option<String>,
    app: Option<&App>,
    state: &WorkbenchState,
    event: &EventRecord,
    width: u16,
) {
    if !is_root_session_event(state, event) {
        if let Some(app) = app {
            append_child_timeline_event(lines, last_group, app, state, event, width);
        }
        return;
    }
    match event.event_type.as_str() {
        "session.input" | "session.followup" => {
            if let Some(prompt) = event
                .payload
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|prompt| !prompt.is_empty())
            {
                last_group.take();
                push_gap_if_needed(lines);
                append_prompt_section(lines, prompt);
            }
        }
        "session.done" => {
            if let Some(result) = event
                .payload
                .get("result")
                .and_then(serde_json::Value::as_str)
                .filter(|result| !result.trim().is_empty())
            {
                last_group.take();
                push_gap_if_needed(lines);
                append_answer_event_block(lines, result, state, width);
            }
        }
        "session.failed" => {
            let error = event
                .payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("The task failed.");
            last_group.take();
            push_gap_if_needed(lines);
            append_ascii_text_block(lines, "error", &[friendly_error_message(error)], None);
        }
        "session.cancelled" => {
            last_group.take();
            push_gap_if_needed(lines);
            append_ascii_text_block(
                lines,
                "stopped",
                &["Progress is saved in history.".to_string()],
                None,
            );
        }
        "agent.spawned" => {
            let label = event
                .payload
                .get("nickname")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    event
                        .payload
                        .get("role")
                        .and_then(serde_json::Value::as_str)
                })
                .unwrap_or("subagent");
            append_timeline_item(
                lines,
                last_group,
                "subagent",
                &format!("{label} started"),
                width,
                text_style(),
            );
        }
        "agent.completed" => {
            append_timeline_item(
                lines,
                last_group,
                "subagent",
                "subagent finished",
                width,
                text_style(),
            );
            if let Some(result) = event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("result"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|result| !result.is_empty())
            {
                append_preview_markdown(lines, last_group, "subagent", result, width, 3);
            }
        }
        "agent.failed" => {
            let error = event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("error"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("subagent failed");
            append_timeline_item(lines, last_group, "error", error, width, failed());
        }
        "agent.cancelled" => {
            append_timeline_item(
                lines,
                last_group,
                "subagent",
                "subagent stopped",
                width,
                muted(),
            );
        }
        "model.tool_call" | "model.thinking_delta" => {}
        "tool.failed" => {
            let name = event
                .payload
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool");
            let error = event
                .payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool failed");
            append_timeline_item(
                lines,
                last_group,
                "error",
                &format!("{name} failed: {error}"),
                width,
                failed(),
            );
        }
        "model.turn.request" | "model.turn.retry" => {}
        "model.turn.error" => {
            append_timeline_item(
                lines,
                last_group,
                "error",
                "model request hit an error",
                width,
                failed(),
            );
        }
        "file.list" => {
            let path = event
                .payload
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(|path| display_path(path, state))
                .unwrap_or_else(|| ".".to_string());
            append_timeline_item(lines, last_group, "list", &path, width, text_style());
        }
        "file.read" => {
            if let Some(path) = event
                .payload
                .get("path")
                .and_then(serde_json::Value::as_str)
            {
                append_timeline_item(
                    lines,
                    last_group,
                    "read",
                    &display_path(path, state),
                    width,
                    text_style(),
                );
            }
        }
        "file.search" => {
            let query = event
                .payload
                .get("query")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("files");
            let matches = event
                .payload
                .get("matches")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            append_timeline_item(
                lines,
                last_group,
                "search",
                &format!("Search {query:?} ({matches} matches)"),
                width,
                text_style(),
            );
        }
        "command.started" => {
            let cmd = event
                .payload
                .get("cmd")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("command");
            append_timeline_item(lines, last_group, "run", cmd, width, text_style());
        }
        "command.output" => {
            if let Some(text) = event
                .payload
                .get("text")
                .and_then(serde_json::Value::as_str)
            {
                append_preview_text(lines, last_group, "run", text, width, 4);
            }
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
                append_timeline_item(
                    lines,
                    last_group,
                    "run",
                    &format!("failed with exit {code}"),
                    width,
                    failed(),
                );
            }
        }
        "tool.output_spilled" => {
            let path = event
                .payload
                .get("artifact")
                .and_then(|artifact| artifact.get("path"))
                .and_then(serde_json::Value::as_str)
                .map(|path| display_path(path, state))
                .unwrap_or_else(|| "artifact".to_string());
            append_timeline_item(
                lines,
                last_group,
                "run",
                &format!("Full output saved to {path}"),
                width,
                muted(),
            );
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
                .map(|path| display_path(path, state))
                .unwrap_or_else(|| "file".to_string());
            append_timeline_item(
                lines,
                last_group,
                "edit",
                &format!("{kind} {path}"),
                width,
                text_style(),
            );
        }
        "model.stream_delta" => {}
        "browser.connected" | "browser.reconnected" | "browser.target_changed" => {
            append_timeline_item(
                lines,
                last_group,
                "browser",
                "browser connected",
                width,
                text_style(),
            );
        }
        "browser.disconnected" => {
            append_timeline_item(
                lines,
                last_group,
                "browser",
                "browser disconnected",
                width,
                muted(),
            );
        }
        "browser.live_url" => {
            append_timeline_item(
                lines,
                last_group,
                "browser",
                "live view available",
                width,
                text_style(),
            );
        }
        "browser.page" | "browser.state" => {
            if let Some(url) = event.payload.get("url").and_then(serde_json::Value::as_str) {
                append_timeline_item(
                    lines,
                    last_group,
                    "browser",
                    &format!("opened {}", compact_activity_url_for_render(url)),
                    width,
                    text_style(),
                );
            }
        }
        _ => {}
    }
}

fn append_child_timeline_event(
    lines: &mut Vec<Line<'static>>,
    last_group: &mut Option<String>,
    app: &App,
    state: &WorkbenchState,
    event: &EventRecord,
    width: u16,
) {
    let label = helper_label_for_session(app, &event.session_id);
    let group = format!("subagent {label}");
    match event.event_type.as_str() {
        "session.created"
        | "session.input"
        | "session.status"
        | "session.done"
        | "agent.context"
        | "model.config"
        | "model.delta"
        | "model.stream_delta"
        | "model.thinking_delta"
        | "model.turn.response"
        | "model.usage"
        | "model.tool_call"
        | "tool.started"
        | "tool.finished"
        | "tool.batch_started"
        | "tool.batch_result"
        | "tool.batch_finished" => {}
        "model.turn.request" => {
            let model = event
                .payload
                .get("model")
                .and_then(serde_json::Value::as_str)
                .filter(|model| !model.trim().is_empty())
                .unwrap_or("model");
            append_timeline_item(
                lines,
                last_group,
                &group,
                &format!("waiting for {model}"),
                width,
                muted(),
            );
        }
        "model.turn.retry" => {
            append_timeline_item(
                lines,
                last_group,
                &group,
                "retrying model request",
                width,
                muted(),
            );
        }
        "model.turn.error" => {
            append_timeline_item(
                lines,
                last_group,
                &group,
                "model request hit an error",
                width,
                failed(),
            );
        }
        "session.failed" => {
            let error = event
                .payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("subagent failed");
            append_timeline_item(lines, last_group, &group, error, width, failed());
        }
        "session.cancelled" => {
            append_timeline_item(lines, last_group, &group, "stopped", width, muted());
        }
        "file.list" => {
            let path = event
                .payload
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(|path| display_path(path, state))
                .unwrap_or_else(|| ".".to_string());
            let item = event
                .payload
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .map(|count| format!("list {path} ({count} items)"))
                .unwrap_or_else(|| format!("list {path}"));
            append_timeline_item(lines, last_group, &group, &item, width, text_style());
        }
        "file.read" => {
            if let Some(path) = event
                .payload
                .get("path")
                .and_then(serde_json::Value::as_str)
            {
                append_timeline_item(
                    lines,
                    last_group,
                    &group,
                    &format!("read {}", display_path(path, state)),
                    width,
                    text_style(),
                );
            }
        }
        "file.search" => {
            let query = event
                .payload
                .get("query")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("files");
            let matches = event
                .payload
                .get("matches")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            append_timeline_item(
                lines,
                last_group,
                &group,
                &format!("search {query:?} ({matches} matches)"),
                width,
                text_style(),
            );
        }
        "command.started" => {
            let cmd = event
                .payload
                .get("cmd")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("command");
            append_timeline_item(
                lines,
                last_group,
                &group,
                &format!("run {cmd}"),
                width,
                text_style(),
            );
        }
        "command.output" => {
            if let Some(text) = event
                .payload
                .get("text")
                .and_then(serde_json::Value::as_str)
            {
                append_preview_text(lines, last_group, &group, text, width, 4);
            }
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
                append_timeline_item(
                    lines,
                    last_group,
                    &group,
                    &format!("failed with exit {code}"),
                    width,
                    failed(),
                );
            }
        }
        "tool.failed" => {
            let name = event
                .payload
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool");
            let error = event
                .payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool failed");
            append_timeline_item(
                lines,
                last_group,
                &group,
                &format!("{name} failed: {error}"),
                width,
                failed(),
            );
        }
        "tool.output_spilled" => {
            let path = event
                .payload
                .get("artifact")
                .and_then(|artifact| artifact.get("path"))
                .and_then(serde_json::Value::as_str)
                .map(|path| display_path(path, state))
                .unwrap_or_else(|| "artifact".to_string());
            append_timeline_item(
                lines,
                last_group,
                &group,
                &format!("full output saved to {path}"),
                width,
                muted(),
            );
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
                .map(|path| display_path(path, state))
                .unwrap_or_else(|| "file".to_string());
            append_timeline_item(
                lines,
                last_group,
                &group,
                &format!("{kind} {path}"),
                width,
                text_style(),
            );
        }
        "browser.connected" | "browser.reconnected" | "browser.target_changed" => {
            append_timeline_item(
                lines,
                last_group,
                &group,
                "browser connected",
                width,
                text_style(),
            );
        }
        "browser.disconnected" => {
            append_timeline_item(
                lines,
                last_group,
                &group,
                "browser disconnected",
                width,
                muted(),
            );
        }
        "browser.live_url" => {
            append_timeline_item(
                lines,
                last_group,
                &group,
                "live view available",
                width,
                text_style(),
            );
        }
        "browser.page" | "browser.state" => {
            if let Some(url) = event.payload.get("url").and_then(serde_json::Value::as_str) {
                append_timeline_item(
                    lines,
                    last_group,
                    &group,
                    &format!("opened {}", compact_activity_url_for_render(url)),
                    width,
                    text_style(),
                );
            }
        }
        "plan.updated" => {
            append_timeline_item(
                lines,
                last_group,
                &group,
                "updated plan",
                width,
                text_style(),
            );
        }
        _ => {}
    }
}

fn is_root_session_event(state: &WorkbenchState, event: &EventRecord) -> bool {
    state
        .current_session
        .as_ref()
        .is_some_and(|session| session.id == event.session_id)
}

fn append_tool_call_intent(
    lines: &mut Vec<Line<'static>>,
    last_group: &mut Option<String>,
    app: &App,
    event: &EventRecord,
    width: u16,
) {
    let Some(name) = event
        .payload
        .get("name")
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };
    let arguments = event
        .payload
        .get("arguments")
        .unwrap_or(&serde_json::Value::Null);
    match name {
        "spawn_agent" => {}
        "wait_agent" => {
            append_timeline_item(lines, last_group, "subagent", "wait", width, muted());
        }
        "send_input" | "send_message" | "followup_task" => {
            let target = arguments
                .get("target")
                .and_then(serde_json::Value::as_str)
                .map(|target| helper_label_for_session(app, target))
                .unwrap_or_else(|| "subagent".to_string());
            append_timeline_item(
                lines,
                last_group,
                "subagent",
                &format!("send input to {target}"),
                width,
                muted(),
            );
        }
        "close_agent" => {
            append_timeline_item(lines, last_group, "subagent", "close", width, muted());
        }
        "python" => {
            append_timeline_item(
                lines,
                last_group,
                "python",
                "run browser Python",
                width,
                muted(),
            );
        }
        "view_image" => {
            append_timeline_item(lines, last_group, "image", "inspect image", width, muted());
        }
        "update_plan" => {
            append_timeline_item(lines, last_group, "plan", "update plan", width, muted());
        }
        "done" => {}
        "exec_command" | "write_stdin" | "read_file" | "search_files" | "list_files"
        | "apply_patch" => {}
        _ => append_timeline_item(
            lines,
            last_group,
            "tool",
            &format!("call {name}"),
            width,
            muted(),
        ),
    }
}

fn helper_label_for_session(app: &App, session_id: &str) -> String {
    for events in app.state_cache.events_by_session.values() {
        for event in events {
            if event.event_type == "agent.spawned"
                && event
                    .payload
                    .get("child_session_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(session_id)
            {
                if let Some(label) = event
                    .payload
                    .get("nickname")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| {
                        event
                            .payload
                            .get("role")
                            .and_then(serde_json::Value::as_str)
                    })
                {
                    return label.to_string();
                }
            }
        }
    }
    for event in app.state_cache.events_for_session(session_id) {
        if event.event_type == "agent.context" {
            if let Some(label) = event
                .payload
                .get("nickname")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    event
                        .payload
                        .get("role")
                        .and_then(serde_json::Value::as_str)
                })
                .or_else(|| {
                    event
                        .payload
                        .get("agent_path")
                        .and_then(serde_json::Value::as_str)
                })
            {
                return label.trim_start_matches("/root/").to_string();
            }
        }
    }
    compact_path_for_render(session_id)
}

fn display_path(path: &str, state: &WorkbenchState) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return ".".to_string();
    }
    if let Some(cwd) = state
        .current_session
        .as_ref()
        .map(|session| session.cwd.as_str())
    {
        let cwd = cwd.trim_end_matches('/');
        if let Some(relative) = trimmed
            .strip_prefix(cwd)
            .and_then(|path| path.strip_prefix('/'))
        {
            if !relative.is_empty() {
                return relative.to_string();
            }
        }
    }
    trimmed.to_string()
}

fn append_timeline_item(
    lines: &mut Vec<Line<'static>>,
    last_group: &mut Option<String>,
    group: &str,
    item: &str,
    width: u16,
    style: Style,
) {
    if last_group.as_deref() != Some(group) {
        push_gap_if_needed(lines);
        lines.push(event_marker_line(group));
        *last_group = Some(group.to_string());
    }
    let body_width = width.saturating_sub(4).max(24) as usize;
    let wrapped = wrap_plain(item.trim_end(), body_width, "");
    if wrapped.is_empty() {
        return;
    }
    for line in wrapped {
        lines.push(prefix_block_line(
            "  ",
            Line::from(Span::styled(line, style)),
        ));
    }
}

fn append_preview_text(
    lines: &mut Vec<Line<'static>>,
    last_group: &mut Option<String>,
    group: &str,
    text: &str,
    width: u16,
    max_lines: usize,
) {
    let preview_lines = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    for line in preview_lines.iter().take(max_lines) {
        append_timeline_item(lines, last_group, group, line, width, muted());
    }
    if preview_lines.len() > max_lines {
        append_timeline_item(
            lines,
            last_group,
            group,
            &format!("... +{} lines", preview_lines.len() - max_lines),
            width,
            dim(),
        );
    }
}

fn append_preview_markdown(
    lines: &mut Vec<Line<'static>>,
    last_group: &mut Option<String>,
    group: &str,
    text: &str,
    width: u16,
    max_lines: usize,
) {
    let preview_lines = render_markdown_lines(text, width.saturating_sub(4).max(24))
        .into_iter()
        .map(trim_default_markdown_indent)
        .filter(|line| {
            line.spans
                .iter()
                .any(|span| !span.content.trim().is_empty())
        })
        .collect::<Vec<_>>();
    for line in preview_lines.iter().take(max_lines).cloned() {
        append_timeline_line(lines, last_group, group, line);
    }
    if preview_lines.len() > max_lines {
        append_timeline_item(
            lines,
            last_group,
            group,
            &format!("... +{} lines", preview_lines.len() - max_lines),
            width,
            dim(),
        );
    }
}

fn append_timeline_line(
    lines: &mut Vec<Line<'static>>,
    last_group: &mut Option<String>,
    group: &str,
    line: Line<'static>,
) {
    if last_group.as_deref() != Some(group) {
        push_gap_if_needed(lines);
        lines.push(event_marker_line(group));
        *last_group = Some(group.to_string());
    }
    lines.push(prefix_block_line("  ", line));
}

fn append_answer_event_block(
    lines: &mut Vec<Line<'static>>,
    result: &str,
    state: &WorkbenchState,
    width: u16,
) {
    append_answer_plain_block(lines, result, width);
    if let Some(source) = state
        .browser
        .url
        .as_ref()
        .or(state.browser.live_url.as_ref())
        .filter(|source| is_useful_source(source))
    {
        append_source_line(lines, source);
    }
}

fn current_streaming_text(state: &WorkbenchState) -> Option<&str> {
    state
        .transcript
        .last()
        .and_then(|turn| turn.streaming_text.as_deref())
        .map(str::trim_end)
        .filter(|text| !text.trim().is_empty())
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
    let mut out = vec![event_marker_line("next")];
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
                    "answer",
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
                    append_answer_plain_block(&mut lines, result, width);
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
                append_answer_plain_block(lines, result, width);
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
        append_ascii_text_block(lines, "status", &[fallback.to_string()], Some("pending"));
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
                    "helper finished"
                        | "helper failed"
                        | "helper stopped"
                        | "subagent finished"
                        | "subagent failed"
                        | "subagent stopped"
                )
        })
        .cloned()
        .collect()
}

fn append_activity_blocks(lines: &mut Vec<Line<'static>>, activity: &[String]) {
    let mut browser = Vec::new();
    let mut status_items = Vec::new();
    let mut tool = Vec::new();
    let mut run = Vec::new();
    let mut edit = Vec::new();
    let mut other = Vec::new();

    for item in activity {
        let formatted = format_activity_item(item);
        if is_browser_activity(item) {
            browser.push(formatted);
        } else if is_thinking_activity(item) {
            status_items.push(formatted);
        } else if is_subagent_activity(item) {
            tool.push(formatted);
        } else if is_command_activity(item) {
            run.push(formatted);
        } else if is_change_activity(item) {
            edit.push(formatted);
        } else if is_explore_activity(item) {
            tool.push(formatted);
        } else {
            other.push(formatted);
        }
    }

    let mut wrote = false;
    for (title, items) in [
        ("browser", browser),
        ("status", status_items),
        ("tool", tool),
        ("run", run),
        ("edit", edit),
        ("step", other),
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
    append_answer_plain_block(lines, result, width);
    if let Some(source) = state
        .browser
        .url
        .as_ref()
        .or(state.browser.live_url.as_ref())
        .filter(|source| is_useful_source(source))
    {
        append_source_line(lines, source);
    }
}

fn append_streaming_block(lines: &mut Vec<Line<'static>>, text: &str, width: u16) {
    append_answer_plain_block(lines, text.trim_end(), width);
}

fn append_thinking_delta_event_lines(
    lines: &mut Vec<Line<'static>>,
    last_group: &mut Option<String>,
    text: &str,
    label: Option<&str>,
    width: u16,
) {
    let title = label
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(|label| format!("thought {label}"))
        .unwrap_or_else(|| "thought".to_string());
    append_delta_event_lines(lines, last_group, "thought", &title, text, width);
}

fn append_delta_event_lines(
    lines: &mut Vec<Line<'static>>,
    last_group: &mut Option<String>,
    group: &str,
    title: &str,
    text: &str,
    width: u16,
) {
    if last_group.as_deref() != Some(group) {
        push_gap_if_needed(lines);
        lines.push(event_marker_line(title));
        *last_group = Some(group.to_string());
    }
    let body_width = width.saturating_sub(4).max(24) as usize;
    for raw_line in text.lines() {
        if raw_line.trim().is_empty() {
            continue;
        }
        let wrapped = wrap_plain(raw_line.trim_end(), body_width, "");
        if wrapped.is_empty() {
            continue;
        }
        for line in wrapped {
            lines.push(prefix_block_line(
                "  ",
                Line::from(Span::styled(line, text_style())),
            ));
        }
    }
}

#[derive(Default)]
struct PendingDeltaBlock {
    kind: Option<PendingDeltaKind>,
    label: Option<String>,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingDeltaKind {
    Thought,
}

impl PendingDeltaBlock {
    fn should_flush_before(&self, kind: PendingDeltaKind, label: Option<&str>) -> bool {
        self.kind.is_some() && (self.kind != Some(kind) || self.label.as_deref() != label)
    }

    fn push(&mut self, kind: PendingDeltaKind, label: Option<String>, event: &EventRecord) {
        let Some(text) = event_text_payload(event) else {
            return;
        };
        if self.kind.is_none() {
            self.kind = Some(kind);
            self.label = label;
        }
        append_live_delta_text(&mut self.text, text);
    }

    fn flush(
        &mut self,
        lines: &mut Vec<Line<'static>>,
        last_group: &mut Option<String>,
        width: u16,
    ) {
        let Some(kind) = self.kind.take() else {
            return;
        };
        let text = self.text.trim_end().to_string();
        let label = self.label.take();
        self.text.clear();
        if text.trim().is_empty() {
            return;
        }
        match kind {
            PendingDeltaKind::Thought => {
                append_thinking_delta_event_lines(lines, last_group, &text, label.as_deref(), width)
            }
        }
    }
}

fn thinking_delta_label(event: &EventRecord) -> Option<String> {
    event
        .payload
        .get("label")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(ToOwned::to_owned)
}

fn event_text_payload(event: &EventRecord) -> Option<&str> {
    event
        .payload
        .get("text")
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

fn append_live_delta_text(current: &mut String, incoming: &str) {
    if current.is_empty() {
        current.push_str(incoming);
        return;
    }
    if incoming == current || incoming.trim() == current.trim() {
        return;
    }
    if let Some(suffix) = incoming.strip_prefix(current.as_str()) {
        current.push_str(suffix);
        return;
    }
    if incoming.chars().count() >= 24 && current.ends_with(incoming) {
        return;
    }
    current.push_str(incoming);
}

fn append_answer_plain_block(lines: &mut Vec<Line<'static>>, markdown: &str, width: u16) {
    let body_width = width.saturating_sub(2).max(24);
    for line in render_markdown_lines(markdown, body_width)
        .into_iter()
        .map(trim_default_markdown_indent)
    {
        lines.push(line);
    }
}

fn append_source_line(lines: &mut Vec<Line<'static>>, source: &str) {
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("source ", muted()),
        Span::styled(source.to_string(), link()),
    ]));
}

fn is_useful_source(source: &str) -> bool {
    let source = source.trim();
    !source.is_empty() && source != "about:blank"
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
    lines.push(event_marker_line(title));
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

fn event_marker_line(title: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(": ", event_marker_style(title)),
        Span::styled(title.to_string(), event_marker_style(title)),
    ])
}

fn event_marker_style(title: &str) -> Style {
    if title.starts_with("thought")
        || title.starts_with("thinking")
        || title.starts_with("status")
        || title.starts_with("edit")
    {
        thought()
    } else if title.starts_with("browser")
        || title == "run"
        || title == "image"
        || title == "plan"
        || title == "tool"
        || title == "python"
    {
        accent()
    } else if title.starts_with("answer")
        || title == "done"
        || title == "source"
        || title == "subagent"
        || title == "list"
        || title == "read"
        || title == "search"
    {
        done()
    } else if title == "error" || title == "stopped" {
        failed()
    } else {
        muted()
    }
}

fn push_gap_if_needed(lines: &mut Vec<Line<'static>>) {
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
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

fn format_activity_item(item: &str) -> String {
    item.strip_prefix("browsing ")
        .map(|url| format!("opened {}", compact_activity_url_for_render(url)))
        .or_else(|| {
            item.strip_prefix("started ")
                .and_then(|text| text.strip_suffix(" helper"))
                .map(|label| format!("{label} started"))
        })
        .or_else(|| {
            if matches!(
                item,
                "helper finished"
                    | "helper failed"
                    | "helper stopped"
                    | "subagent finished"
                    | "subagent failed"
                    | "subagent stopped"
            ) {
                Some(item.replacen("helper", "subagent", 1))
            } else {
                item.strip_prefix("helper ")
                    .or_else(|| item.strip_prefix("subagent "))
                    .map(|text| text.to_string())
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

fn compact_path_for_render(path: &str) -> String {
    let trimmed = path.trim();
    trimmed
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .filter(|tail| !tail.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

fn is_browser_activity(item: &str) -> bool {
    item.starts_with("browsing ")
        || item.starts_with("browser ")
        || item == "connected live browser"
}

fn is_thinking_activity(item: &str) -> bool {
    item.starts_with("thinking ")
}

fn is_subagent_activity(item: &str) -> bool {
    item.starts_with("helper ")
        || item.starts_with("subagent ")
        || (item.starts_with("started ") && item.contains(" helper"))
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
