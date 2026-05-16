use anyhow::Result;
use browser_use_protocol::{HistoryRow, SessionMeta, TelemetrySummary, WorkbenchState};
use ratatui::backend::TestBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::composer::composer_rule;
use crate::palette;
use crate::settings::{
    is_claude_code_account, ACCOUNT_ANTHROPIC, ACCOUNT_CHOICES, ACCOUNT_CLAUDE_CODE, ACCOUNT_CODEX,
    ACCOUNT_OPENAI, ACCOUNT_OPENROUTER, BROWSER_CHOICES, BROWSER_USE_CLOUD, MODEL_CHOICES,
};
use crate::theme::*;
use crate::transcript;

use super::{App, ProductState, Surface};

pub(crate) const APP_HORIZONTAL_MARGIN: u16 = 2;
const CONTENT_HORIZONTAL_MARGIN: u16 = 2;
pub(crate) const NATIVE_TRANSCRIPT_HORIZONTAL_MARGIN: u16 =
    APP_HORIZONTAL_MARGIN + CONTENT_HORIZONTAL_MARGIN;
const COMPOSER_HINT_GAP: u16 = 1;

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
    let mut lines = transcript::transcript_model(app, &state)
        .map(|model| {
            transcript::all_terminal_scrollback_lines(&model, width.saturating_sub(4).max(1))
        })
        .unwrap_or_default();
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
    let transcript_model = if native_scrollback_active {
        transcript::transcript_model(app, state)
    } else {
        None
    };
    let body = if app.surface.is_bottom_pane() {
        Vec::new()
    } else if native_scrollback_active {
        let mut lines =
            transcript::active_viewport_lines(transcript_model.as_ref(), body_width, max_body_h);
        if lines.is_empty() {
            if let Some(next) = next_action_lines(state, app, product_state) {
                lines = next;
            }
        }
        lines
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
    let pin_bottom = should_pin_main_bottom(product_state, native_scrollback_active)
        && !app.surface.is_bottom_pane();
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
        let top_gap = if native_scrollback_active
            && matches!(
                product_state,
                ProductState::Failed | ProductState::Cancelled
            ) {
            0
        } else {
            top_gap
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
    if native_scrollback_active {
        return false;
    }
    matches!(
        product_state,
        ProductState::Running | ProductState::Failed | ProductState::Cancelled
    )
}

pub(crate) fn main_viewport_height(app: &App, width: u16) -> u16 {
    let current = composer_pane_height(app, ProductState::Ready, width);
    let reserved_input_h = 1_u16;
    let palette_h = (palette::max_item_count() as u16).min(8).saturating_add(3);
    let max_palette = reserved_input_h.saturating_add(palette_h).saturating_add(1);
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
    let visual_input_lines = composer_visual_input_lines(app, composer_input_area_width(width));
    let palette_h = slash_palette_pane_height(app);
    if palette_h > 0 {
        visual_input_lines + palette_h + 1
    } else {
        visual_input_lines + COMPOSER_HINT_GAP + 2
    }
}

fn composer_input_area_width(width: u16) -> u16 {
    width.saturating_sub(4).max(1)
}

fn composer_visual_input_lines(app: &App, input_area_width: u16) -> u16 {
    let visual_input_lines = app
        .composer
        .visual_line_count_wrapped(input_area_width as usize);
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
    if cloud_browser_needs_key(app) {
        return format!("{} needs key", app.browser);
    }
    let status = if state.browser.status == "not connected" {
        if app.browser == BROWSER_USE_CLOUD {
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
    let input_h = composer_visual_input_lines(app, composer_input_area_width(area.width));
    let action_h = if palette_h > 0 { palette_h } else { 1 };
    let constraints = if palette_h > 0 {
        vec![
            Constraint::Length(1),
            Constraint::Length(input_h),
            Constraint::Length(action_h),
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Length(input_h),
            Constraint::Length(COMPOSER_HINT_GAP),
            Constraint::Length(action_h),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
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
    let action_chunk = if palette_h > 0 { chunks[2] } else { chunks[3] };
    let action_area = action_chunk.inner(Margin {
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
    "-".repeat(width as usize)
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
            x: area.x.saturating_add(cursor_x.min(area.width)),
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
    if account == BROWSER_USE_CLOUD {
        lines.extend([
            Line::from("  Browser Use cloud runs a remote browser with live view."),
            Line::from("  Add this key once, or export BROWSER_USE_API_KEY before launch."),
            Line::from(""),
        ]);
    } else if is_claude_code_account(account) {
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
            if account == BROWSER_USE_CLOUD {
                "  Stored locally and passed to browser worker as BROWSER_USE_API_KEY."
            } else if is_claude_code_account(account) {
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
    let cloud_description = if !app.browser_use_cloud_key_ready().unwrap_or(false) {
        "needs Browser Use key"
    } else {
        "remote browser with live view"
    };
    let descriptions = [
        cloud_description,
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
            Span::styled(
                format!(" . {}", browser_current_status_for_select(app)),
                browser_current_status_style(app),
            ),
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
    let mut out = transcript::transcript_model(app, state)
        .map(|model| {
            let mut lines = transcript::all_scrollback_lines(&model, width);
            if matches!(product_state, ProductState::Running) {
                let active = transcript::active_viewport_lines(Some(&model), width, u16::MAX);
                if !active.is_empty() {
                    if !lines.is_empty() {
                        lines.push(Line::from(""));
                    }
                    lines.extend(active);
                }
            }
            lines
        })
        .unwrap_or_default();
    if out.is_empty() {
        append_task_section(&mut out, state);
    }
    if let Some(next) = next_action_lines(state, app, product_state) {
        out.push(Line::from(""));
        out.extend(next);
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
    let mut out = vec![event_marker_line("next")];
    for (idx, label) in actions.iter().enumerate() {
        out.push(prefix_block_line(
            "  ",
            selected(label, idx, effective_selection),
        ));
    }
    Some(out)
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
    lines.push(Line::from(vec![
        Span::styled("> ", accent()),
        Span::styled(
            state
                .task
                .clone()
                .unwrap_or_else(|| "browser task".to_string()),
            text_style(),
        ),
    ]));
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

fn prefix_block_line(prefix: &'static str, line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix, dim())];
    spans.extend(line.spans);
    Line::from(spans)
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
    if cloud_browser_needs_key(app) {
        return format!("{} needs key", app.browser);
    }
    if state.browser.status == "not connected" {
        format!("{} ready", app.browser)
    } else {
        format!("{} {}", app.browser, state.browser.status)
    }
}

fn browser_current_status_for_select(app: &App) -> &'static str {
    if cloud_browser_needs_key(app) {
        "needs key"
    } else {
        "ready"
    }
}

fn browser_current_status_style(app: &App) -> Style {
    if cloud_browser_needs_key(app) {
        failed()
    } else {
        done()
    }
}

fn cloud_browser_needs_key(app: &App) -> bool {
    app.browser == BROWSER_USE_CLOUD && !app.browser_use_cloud_key_ready().unwrap_or(false)
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
        BROWSER_USE_CLOUD => "Browser Use cloud key",
        account if is_claude_code_account(account) => "Claude Code OAuth token",
        _ => "Credential",
    }
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
