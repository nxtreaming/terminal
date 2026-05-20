use anyhow::Result;
use browser_use_protocol::{HistoryRow, SessionMeta, TelemetrySummary, WorkbenchState};
use ratatui::backend::TestBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Strip trailing spaces from each line in place, so right-side column padding
/// stops counting toward the wrap budget. With this applied before a `Paragraph`
/// that has `Wrap` enabled, narrowing the terminal clips the empty tail off the
/// line rather than wrapping the padding to a new visual row.
fn trim_trailing_whitespace(lines: &mut Vec<Line<'static>>) {
    for line in lines.iter_mut() {
        while let Some(last) = line.spans.last_mut() {
            let trimmed_len = last.content.trim_end_matches(' ').len();
            if trimmed_len == 0 {
                line.spans.pop();
            } else {
                if trimmed_len != last.content.len() {
                    let style = last.style;
                    let trimmed = last.content[..trimmed_len].to_string();
                    *last = Span::styled(trimmed, style);
                }
                break;
            }
        }
    }
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

    if app.is_first_run_setup_visible().unwrap_or(false) {
        // First-run setup always renders full-screen, whatever step it is on.
        let surface = if app.surface == Surface::Main {
            Surface::Setup
        } else {
            app.surface
        };
        render_surface(frame, area, app, &state, surface);
        return;
    }

    match app.surface {
        surface if surface.is_popup() => {
            render_main(frame, area, app, &state, product_state);
            render_popup_overlay(frame, area, app, &state, surface);
        }
        surface if surface.uses_main_view() => {
            render_main(frame, area, app, &state, product_state);
            if app.is_slash_palette_active() {
                render_command_palette_popup(frame, area, app);
            }
        }
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
    // Popup surfaces float over the main view; the underlying main layout
    // should ignore them and render as if no surface were open.
    let layout_surface = if app.surface.is_popup() {
        Surface::Main
    } else {
        app.surface
    };
    let bottom_h = main_bottom_height_for(app, state, layout_surface, area, product_state);
    let body_width = content_width(area.width);
    let native_scrollback_active = app.native_scrollback_is_active();
    let show_footer = layout_surface.is_bottom_pane()
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
    let body = if native_scrollback_active {
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
        && !layout_surface.is_bottom_pane();
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
    trim_trailing_whitespace(&mut body);
    frame.render_widget(
        Paragraph::new(body)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        content_area(body_render_area),
    );
    if layout_surface.is_bottom_pane() {
        render_bottom_pane(frame, bottom_area, app, state, layout_surface);
    } else if app.surface.is_text_input_popup() {
        // The popup itself is the input — don't render the composer under it,
        // or the user sees their typing duplicated. Clear the area so nothing
        // bleeds through behind the floating popup.
        frame.render_widget(Clear, bottom_area);
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
    // Worst-case fused-composer height with the palette open: input floor +
    // dropdown rows (capped at 8) + top border + separator + bottom border +
    // hint = COMPOSER_INPUT_MIN_ROWS + items + 4. We reserve this so opening
    // the palette never resizes the viewport underneath the user.
    let palette_items = (palette::max_item_count() as u16).min(8);
    let max_palette_pane = COMPOSER_INPUT_MIN_ROWS
        .saturating_add(palette_items)
        .saturating_add(4);
    current.max(max_palette_pane)
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
    let line_count = surface_lines(surface, app, state, content_width(area.width) as usize)
        .len() as u16;
    let max_height = match surface {
        Surface::Model | Surface::History => area.height.saturating_sub(2).max(6),
        Surface::BrowserSelect => 22,
        _ => 18,
    };
    // Add room for the surface header (rule + title + description + spacer).
    let desired = line_count.saturating_add(6).clamp(8, max_height);
    let available = area.height.saturating_sub(2).max(4);
    desired.min(available)
}

fn composer_pane_height(app: &App, _product_state: ProductState, width: u16) -> u16 {
    let visual_input_lines = composer_visual_input_lines(app, width.saturating_sub(4).max(1));
    // top border + input + bottom border + hint. The slash palette floats as
    // a popup over the body, so the composer's own height never changes.
    visual_input_lines + 3
}

fn composer_input_area_width(width: u16) -> u16 {
    width.saturating_sub(4).max(1)
}

/// Visual rows the input area inside the fused composer should occupy.
/// Floored at 3 so the box has comfortable breathing room when empty, and
/// capped at 10 so a long pasted prompt doesn't push the rest of the UI
/// off-screen.
const COMPOSER_INPUT_MIN_ROWS: u16 = 3;
const COMPOSER_INPUT_MAX_ROWS: u16 = 10;

fn composer_visual_input_lines(app: &App, input_area_width: u16) -> u16 {
    let visual_input_lines = app
        .composer
        .visual_line_count_wrapped(input_area_width as usize) as u16;
    visual_input_lines.clamp(COMPOSER_INPUT_MIN_ROWS, COMPOSER_INPUT_MAX_ROWS)
}

/// Number of dropdown item rows the slash palette would render (0 when the
/// palette is closed). The fused composer absorbs its own chrome — top
/// border, separator, hint — so this is just the row count, not a pane
/// height including borders.
fn slash_palette_pane_height(app: &App) -> u16 {
    if !app.is_slash_palette_active() {
        return 0;
    }
    let items = app.slash_palette_items();
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
    let header = surface_header_lines(surface, content_width(area.width));
    let header_h = header.len() as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(header_h), Constraint::Min(1)])
        .split(area);
    frame.render_widget(Paragraph::new(header), content_area(chunks[0]));
    let body_area = content_area(chunks[1]);
    let body_width = body_area.width as usize;
    let mut lines = surface_lines(surface, app, state, body_width);
    // For surfaces whose body is a straight list of selectable rows indexed by
    // `selected_row` (currently just History), keep the selection in view by
    // dropping rows from the top once it would otherwise scroll off the bottom.
    if matches!(surface, Surface::History) {
        let body_h = body_area.height as usize;
        if body_h > 0 && app.selected_row >= body_h {
            let skip = app.selected_row + 1 - body_h;
            lines = lines.into_iter().skip(skip).collect();
        }
    }
    trim_trailing_whitespace(&mut lines);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        body_area,
    );
}

/// Centered floating popup overlay for slash-command-launched surfaces
/// (history, browser, model, auth, telemetry, developer). Responsive: shrinks
/// to fit small terminals and caps to a comfortable max on large ones.
fn render_popup_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &WorkbenchState,
    surface: Surface,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    const MIN_W: u16 = 40;
    const MIN_H: u16 = 10;
    const MAX_W: u16 = 84;
    const MAX_H: u16 = 26;
    const H_MARGIN: u16 = 4;
    const V_MARGIN: u16 = 2;

    let popup_w = if area.width <= MIN_W {
        area.width
    } else {
        area.width
            .saturating_sub(H_MARGIN.saturating_mul(2))
            .min(MAX_W)
            .max(MIN_W)
    };

    // Estimate desired height from body content length + chrome
    // (border 2 + header 4 + footer 2 = 8 lines).
    let body_inner_width = popup_w.saturating_sub(2 + CONTENT_HORIZONTAL_MARGIN * 2).max(1) as usize;
    let body_line_count = surface_lines(surface, app, state, body_inner_width).len() as u16;
    let desired_h = body_line_count.saturating_add(8);

    let popup_h = if area.height <= MIN_H {
        area.height
    } else {
        desired_h
            .clamp(MIN_H, MAX_H)
            .min(area.height.saturating_sub(V_MARGIN.saturating_mul(2)))
            .max(MIN_H.min(area.height))
    };

    let popup_x = area.x + area.width.saturating_sub(popup_w) / 2;
    let popup_y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup_rect = Rect {
        x: popup_x,
        y: popup_y,
        width: popup_w,
        height: popup_h,
    };

    frame.render_widget(Clear, popup_rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border());
    let inner = block.inner(popup_rect);
    frame.render_widget(block, popup_rect);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Layout inside the popup: header lines, body, footer line.
    let header = surface_header_lines(surface, inner.width);
    let header_h = (header.len() as u16).min(inner.height);
    let footer_text = surface_footer(surface);
    let footer_h: u16 = if footer_text.is_empty() { 0 } else { 1 };
    let body_h = inner.height.saturating_sub(header_h).saturating_sub(footer_h);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Length(body_h),
            Constraint::Length(footer_h),
        ])
        .split(inner);

    frame.render_widget(Paragraph::new(header), content_area(chunks[0]));

    let body_area = content_area(chunks[1]);
    let mut lines = surface_lines(surface, app, state, body_area.width as usize);
    if matches!(surface, Surface::History) {
        let body_h = body_area.height as usize;
        if body_h > 0 && app.selected_row >= body_h {
            let skip = app.selected_row + 1 - body_h;
            lines = lines.into_iter().skip(skip).collect();
        }
    }
    // For text-input popups, position the terminal cursor at the end of the
    // masked secret line so the user sees a blinking caret in the input field.
    let cursor_pos: Option<(u16, u16)> = if surface.is_text_input_popup() {
        let masked = match surface {
            Surface::Telemetry => masked_secret(app.composer.input()),
            Surface::ApiKey => {
                let account = app.api_key_account.as_deref().unwrap_or("");
                masked_secret_for_account(account, app.composer.input())
            }
            _ => String::new(),
        };
        let target = format!("  {masked}");
        let cursor_col = target.chars().count() as u16;
        let visible_h = body_area.height as usize;
        lines.iter().take(visible_h).enumerate().find_map(|(row, line)| {
            let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if plain.starts_with(&target) {
                Some((
                    body_area.x.saturating_add(cursor_col.min(body_area.width)),
                    body_area.y.saturating_add(row as u16),
                ))
            } else {
                None
            }
        })
    } else {
        None
    };
    trim_trailing_whitespace(&mut lines);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        body_area,
    );
    if let Some((x, y)) = cursor_pos {
        frame.set_cursor_position(Position { x, y });
    }

    if footer_h > 0 {
        frame.render_widget(
            Paragraph::new(footer_text)
                .style(muted())
                .alignment(Alignment::Right),
            content_area(chunks[2]),
        );
    }
}

/// Floating command palette: appears the moment the composer starts with
/// `/`, listing all slash commands (filtered by what the user has typed
/// after the slash). Centered horizontally, anchored just above the
/// composer so it reads as the dropdown for the input below.
fn render_command_palette_popup(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    const MIN_W: u16 = 40;
    const MIN_H: u16 = 8;
    const MAX_W: u16 = 72;
    const H_MARGIN: u16 = 4;

    let items = app.slash_palette_items();
    let item_count = items.len() as u16;

    // Chrome above/below the row list: border(2) + header(2) + footer(1) = 5.
    // Add the item count for the visible body.
    let desired_h = item_count.saturating_add(5).max(MIN_H);

    let popup_w = if area.width <= MIN_W {
        area.width
    } else {
        area.width
            .saturating_sub(H_MARGIN.saturating_mul(2))
            .min(MAX_W)
            .max(MIN_W)
    };
    let composer_h = composer_pane_height(app, ProductState::Ready, area.width);
    let composer_top = area.y + area.height.saturating_sub(composer_h);
    // Cap height so the popup never overlaps the composer below.
    let available_h = composer_top.saturating_sub(area.y).saturating_sub(1).max(MIN_H);
    let popup_h = desired_h.min(available_h).max(MIN_H.min(available_h));
    let popup_x = area.x + area.width.saturating_sub(popup_w) / 2;
    // Anchor to just above the composer.
    let popup_y = composer_top.saturating_sub(popup_h).saturating_sub(1).max(area.y);
    let popup_rect = Rect {
        x: popup_x,
        y: popup_y,
        width: popup_w,
        height: popup_h,
    };

    frame.render_widget(Clear, popup_rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border());
    let inner = block.inner(popup_rect);
    frame.render_widget(block, popup_rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Header: just the palette title + match-count summary. The user can
    // see what they typed in the composer below the popup — echoing the
    // filter text here too would be duplication.
    let summary = if item_count == palette::max_item_count() as u16 {
        format!(" {} commands", item_count)
    } else {
        format!(" {} of {}", item_count, palette::max_item_count())
    };
    let header_lines = vec![
        Line::from(vec![
            Span::raw("  "),
            Span::styled("/", accent()),
            Span::styled(" command palette", muted()),
            Span::styled(summary, dim()),
        ]),
        Line::from(""),
    ];
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    frame.render_widget(Paragraph::new(header_lines), chunks[0]);

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  No commands match.",
                muted(),
            ))),
            chunks[1],
        );
    } else {
        let rows = slash_palette_rows(app, chunks[1].width as usize);
        let mut visible = rows;
        let body_h = chunks[1].height as usize;
        if body_h > 0 && app.selected_row >= body_h {
            let skip = app.selected_row + 1 - body_h;
            visible = visible.into_iter().skip(skip).collect();
        }
        frame.render_widget(
            Paragraph::new(visible)
                .style(Style::default().fg(text()))
                .wrap(Wrap { trim: false }),
            chunks[1],
        );
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑↓ navigate · ⏎ select · esc close",
            muted(),
        )))
        .alignment(Alignment::Right),
        chunks[2],
    );
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
    let header = surface_header_lines(surface, area.width);
    let chrome_h = header.len() as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(chrome_h),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);
    frame.render_widget(Paragraph::new(header), chunks[0]);
    let body_width = content_area(chunks[1]).width as usize;
    let mut lines = surface_lines(surface, app, state, body_width);
    trim_trailing_whitespace(&mut lines);
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

/// Title and one-line description for a dropdown/settings surface header.
fn surface_heading(surface: Surface) -> (&'static str, &'static str) {
    match surface {
        Surface::Setup => ("Setup", "Get Browser Use ready to go"),
        Surface::Account => ("Authenticate", "Sign in to a model provider"),
        Surface::ApiKey => ("API key", "Enter your provider API key"),
        Surface::Telemetry => ("Laminar", "Configure Laminar telemetry"),
        Surface::Model => ("Model", "Choose the model and provider for this session"),
        Surface::Browser => ("Browser", "Change the browser backend"),
        Surface::BrowserSelect => ("Browser", "Choose a browser backend"),
        Surface::History => ("History", "Browse and resume previous tasks"),
        Surface::Developer => ("Developer", "Developer tools and diagnostics"),
        Surface::Main => ("", ""),
    }
}

/// A surface header: a full-width accent rule, the colored title, and a muted
/// one-line description — the shared chrome for every dropdown/settings view.
fn surface_header_lines(surface: Surface, width: u16) -> Vec<Line<'static>> {
    let (title, description) = surface_heading(surface);
    let indent = " ".repeat(CONTENT_HORIZONTAL_MARGIN as usize);
    vec![
        Line::from(Span::styled("─".repeat(width as usize), accent())),
        Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(title.to_string(), accent()),
        ]),
        Line::from(vec![
            Span::raw(indent),
            Span::styled(description.to_string(), muted()),
        ]),
        Line::from(""),
    ]
}

fn surface_footer(surface: Surface) -> &'static str {
    match surface {
        Surface::ApiKey => "Enter:save | Esc:cancel",
        Surface::Telemetry => "Enter:save | Esc:cancel",
        Surface::History => "",
        Surface::Setup => "Enter:continue | Esc:quit",
        Surface::Browser => "Enter:select | Esc:back",
        Surface::Developer => "Esc:close",
        _ => "Enter:select | Esc:back",
    }
}

fn surface_lines(
    surface: Surface,
    app: &App,
    state: &WorkbenchState,
    width: usize,
) -> Vec<Line<'static>> {
    match surface {
        Surface::Setup => setup_lines(app),
        Surface::Account => account_lines(app),
        Surface::ApiKey => api_key_lines(app),
        Surface::Telemetry => telemetry_key_lines(app),
        Surface::Model => model_lines(app),
        Surface::Browser => browser_panel_lines(app, state),
        Surface::BrowserSelect => browser_select_lines(app),
        Surface::History => history_lines(app, state, width),
        Surface::Developer => developer_lines(app, state),
        Surface::Main => Vec::new(),
    }
}

/// Fused bordered composer: a single rounded box that contains the input area
/// and — when the slash palette is open — the dropdown rows sitting above the
/// input, separated by a thin dashed rule. Session metadata is punched
/// through the box's borders: model + browser on the top edge (or moves to
/// the bottom when the dropdown takes over the top), cwd on the bottom-left,
/// branch on the bottom-right. A single hint/status row renders just below
/// the box.
fn render_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &WorkbenchState,
    _product_state: ProductState,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // The slash palette renders as a floating popup over the body now, so
    // the composer is always a single bordered input box — no inline
    // dropdown half stacked on top of it. Keeping the local name `palette_open`
    // false means the top border carries model+browser regardless and the
    // bottom border carries cwd+branch.
    let palette_open = false;
    let _ = palette_open;

    // Inside the box we reserve 1 cell of horizontal padding on each side,
    // plus the 1-cell border, so the input lives in `width - 4`.
    let input_inner_w = area.width.saturating_sub(4).max(1);
    let input_h = composer_visual_input_lines(app, input_inner_w);
    let inner_h = input_h;
    let box_h = inner_h.saturating_add(2).min(area.height);
    let hint_h: u16 = if area.height > box_h { 1 } else { 0 };

    let box_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: box_h,
    };

    // Top border, with metadata or palette tag punched through.
    let top_y = box_area.y;
    frame.render_widget(
        Paragraph::new(top_border_line(box_area.width, app, palette_open)),
        Rect {
            x: box_area.x,
            y: top_y,
            width: box_area.width,
            height: 1,
        },
    );

    // Bottom border, with cwd + branch (and model/browser when the palette
    // pushed it off the top).
    let bottom_y = box_area.y + box_area.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(bottom_border_line(box_area.width, app, palette_open)),
        Rect {
            x: box_area.x,
            y: bottom_y,
            width: box_area.width,
            height: 1,
        },
    );

    // Side borders for every row between top and bottom.
    if box_area.height > 2 {
        for row in 1..box_area.height.saturating_sub(1) {
            let y = box_area.y + row;
            frame.render_widget(
                Paragraph::new(Span::styled("│", border())),
                Rect {
                    x: box_area.x,
                    y,
                    width: 1,
                    height: 1,
                },
            );
            frame.render_widget(
                Paragraph::new(Span::styled("│", border())),
                Rect {
                    x: box_area.x + box_area.width.saturating_sub(1),
                    y,
                    width: 1,
                    height: 1,
                },
            );
        }
    }

    let row_y = box_area.y + 1;
    let inner_x = box_area.x + 1;
    let inner_w = box_area.width.saturating_sub(2);

    if input_h > 0 && inner_w > 2 {
        let input_area = Rect {
            x: inner_x + 1,
            y: row_y,
            width: inner_w.saturating_sub(2),
            height: input_h,
        };
        render_composer_input(frame, input_area, app, state.current_session.as_ref());
    }

    if hint_h > 0 {
        let hint_area = Rect {
            x: area.x,
            y: box_area.y + box_area.height,
            width: area.width,
            height: hint_h,
        };
        let hint_inner = hint_area.inner(Margin {
            vertical: 0,
            horizontal: 2,
        });
        if state.current_session.is_some() {
            frame.render_widget(
                Paragraph::new(status_bar_line(app, state, hint_inner.width as usize)),
                hint_inner,
            );
        } else {
            frame.render_widget(
                Paragraph::new(hint_row(hint_inner.width as usize)),
                hint_inner,
            );
        }
    }
}

/// Build the top border line for the fused composer. Punches one tag
/// through it: `/ command palette` when the palette is open, otherwise the
/// model + browser metadata. Each tag is dropped or shortened to fit when
/// the terminal narrows.
fn top_border_line(width: u16, app: &App, palette_open: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled("╭", border()));
    let inner_w = width.saturating_sub(2) as usize;
    if palette_open {
        let mut tag_spans = vec![
            Span::raw(" "),
            Span::styled("/", accent()),
            Span::styled(" command palette ", muted()),
        ];
        let tag_w: usize = tag_spans.iter().map(|s| s.content.chars().count()).sum();
        // 2-cell dash lead-in before the tag, fill the rest with dashes.
        let lead = 2usize.min(inner_w.saturating_sub(tag_w));
        spans.push(Span::styled("─".repeat(lead), border()));
        spans.append(&mut tag_spans);
        let trail = inner_w.saturating_sub(lead + tag_w);
        spans.push(Span::styled("─".repeat(trail), border()));
    } else {
        let tag = top_metadata_spans(app, inner_w);
        let tag_w: usize = tag.iter().map(|s| s.content.chars().count()).sum();
        // Right-align the metadata tag: dashes, then tag, then 2-dash trail.
        let trail = 2usize.min(inner_w.saturating_sub(tag_w));
        let lead = inner_w.saturating_sub(tag_w + trail);
        spans.push(Span::styled("─".repeat(lead), border()));
        spans.extend(tag);
        spans.push(Span::styled("─".repeat(trail), border()));
    }
    spans.push(Span::styled("╮", border()));
    Line::from(spans)
}

/// Build the bottom border line. Always shows cwd on the left and branch on
/// the right (when there is one). When the palette has displaced the
/// model/browser tag off the top, the bottom takes it too — inserted between
/// cwd and branch.
fn bottom_border_line(width: u16, app: &App, palette_open: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled("╰", border()));
    let inner_w = width.saturating_sub(2) as usize;

    let mut left_spans = bottom_left_spans();
    let mut right_spans = bottom_right_spans();
    let mut center_spans: Vec<Span<'static>> = Vec::new();
    if palette_open {
        center_spans = top_metadata_spans(app, inner_w);
    }

    let left_w: usize = left_spans.iter().map(|s| s.content.chars().count()).sum();
    let right_w: usize = right_spans.iter().map(|s| s.content.chars().count()).sum();
    let center_w: usize = center_spans.iter().map(|s| s.content.chars().count()).sum();

    // Floor: 2-cell dash lead-in on the left, 2 between segments. If we can't
    // fit a segment we drop it, smallest-priority first: branch, then
    // center (palette-displaced metadata), then cwd.
    let needed = |segments: &[usize]| -> usize {
        let dashes = 2 + segments.len().saturating_sub(1) * 2 + 2; // lead + gaps + trail
        segments.iter().sum::<usize>() + dashes
    };

    // Build present segments list (left, optional center, optional right).
    let mut segments: Vec<Vec<Span<'static>>> = vec![left_spans.clone()];
    if !center_spans.is_empty() {
        segments.push(center_spans.clone());
    }
    if !right_spans.is_empty() {
        segments.push(right_spans.clone());
    }
    let widths: Vec<usize> = segments
        .iter()
        .map(|s| s.iter().map(|sp| sp.content.chars().count()).sum())
        .collect();
    let mut drop_right = false;
    let mut drop_center = false;
    let mut drop_left = false;
    if needed(&widths) > inner_w {
        // Drop the rightmost-priority segment first.
        if widths.len() == 3 {
            drop_right = true;
            let trimmed: Vec<usize> = widths[..2].to_vec();
            if needed(&trimmed) > inner_w {
                drop_center = true;
                let trimmed2: Vec<usize> = vec![widths[0]];
                if needed(&trimmed2) > inner_w {
                    drop_left = true;
                }
            }
        } else if widths.len() == 2 {
            drop_right = true;
            if needed(&[widths[0]]) > inner_w {
                drop_left = true;
            }
        } else if widths.len() == 1 && needed(&widths) > inner_w {
            drop_left = true;
        }
    }
    if drop_right {
        right_spans.clear();
    }
    if drop_center {
        center_spans.clear();
    }
    if drop_left {
        left_spans.clear();
    }

    let left_w = left_spans.iter().map(|s| s.content.chars().count()).sum::<usize>();
    let right_w = right_spans.iter().map(|s| s.content.chars().count()).sum::<usize>();
    let center_w = center_spans.iter().map(|s| s.content.chars().count()).sum::<usize>();

    // Layout: [lead dashes] [left] [mid dashes] [center] [mid dashes] [right] [trail dashes]
    // collapse missing segments and their dash gaps.
    let lead = 2.min(inner_w.saturating_sub(left_w + center_w + right_w));
    spans.push(Span::styled("─".repeat(lead), border()));
    let mut used = lead;
    if !left_spans.is_empty() {
        spans.extend(left_spans.clone());
        used += left_w;
    }
    if !center_spans.is_empty() {
        let gap = inner_w
            .saturating_sub(used + center_w + right_w + 2)
            .min(usize::MAX);
        let gap = gap.max(2);
        spans.push(Span::styled("─".repeat(gap), border()));
        spans.extend(center_spans.clone());
        used += gap + center_w;
    }
    if !right_spans.is_empty() {
        let gap = inner_w.saturating_sub(used + right_w + 2).max(2);
        spans.push(Span::styled("─".repeat(gap), border()));
        spans.extend(right_spans.clone());
        used += gap + right_w;
    }
    let trail = inner_w.saturating_sub(used);
    spans.push(Span::styled("─".repeat(trail), border()));
    spans.push(Span::styled("╯", border()));
    Line::from(spans)
}

/// The dashed separator that sits between the slash-command dropdown and the
/// input area when the palette is open. `├╌╌...╌╌┤` so the box reads as one
/// continuous frame split into two halves.
fn separator_line(width: u16) -> Line<'static> {
    let inner = width.saturating_sub(2) as usize;
    Line::from(vec![
        Span::styled("├", border()),
        Span::styled("╌".repeat(inner), border()),
        Span::styled("┤", border()),
    ])
}

/// Model + browser, in their warm/cool accent colors, separated by a muted
/// `·`. Used on the top edge by default, or on the bottom edge when the
/// palette has taken over the top.
fn top_metadata_spans(app: &App, max_width: usize) -> Vec<Span<'static>> {
    let model = short_model(&app.model);
    let browser = short_browser(&app.browser);
    let model_color = Style::default().fg(ratatui::style::Color::Rgb(251, 191, 36));
    let browser_color = Style::default().fg(ratatui::style::Color::Rgb(96, 165, 250));
    let full = vec![
        Span::raw(" "),
        Span::styled(model.clone(), model_color),
        Span::styled(" · ", muted()),
        Span::styled(browser.clone(), browser_color),
        Span::raw(" "),
    ];
    let full_w: usize = full.iter().map(|s| s.content.chars().count()).sum();
    if full_w + 4 <= max_width {
        return full;
    }
    // Tight: drop the browser, keep just the model.
    let model_only = vec![Span::raw(" "), Span::styled(model.clone(), model_color), Span::raw(" ")];
    let model_w: usize = model_only.iter().map(|s| s.content.chars().count()).sum();
    if model_w + 4 <= max_width {
        return model_only;
    }
    Vec::new()
}

fn bottom_left_spans() -> Vec<Span<'static>> {
    let cwd = short_cwd(&cwd_label());
    vec![Span::raw(" "), Span::styled(cwd, muted()), Span::raw(" ")]
}

fn bottom_right_spans() -> Vec<Span<'static>> {
    if let Some(branch) = git_branch() {
        let branch_color = Style::default().fg(ratatui::style::Color::Rgb(192, 132, 252));
        vec![
            Span::raw(" "),
            Span::styled("⎇ ", muted()),
            Span::styled(branch, branch_color),
            Span::raw(" "),
        ]
    } else {
        Vec::new()
    }
}

/// Compact display name for the model, used on the input border.
fn short_model(model: &str) -> String {
    if model.is_empty() {
        return "—".to_string();
    }
    model.to_string()
}

/// Compact display name for the browser backend.
fn short_browser(browser: &str) -> String {
    if browser.is_empty() {
        return "—".to_string();
    }
    browser.to_string()
}

/// Path string for the bottom-left of the input. Replaces the home prefix
/// with `~` and, if the path is still long, keeps the last two path
/// components with a leading ellipsis.
fn short_cwd(cwd: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let with_tilde = if !home.is_empty() && cwd.starts_with(&home) {
        format!("~{}", &cwd[home.len()..])
    } else {
        cwd.to_string()
    };
    if with_tilde.chars().count() <= 48 {
        return with_tilde;
    }
    let parts: Vec<&str> = with_tilde.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        return with_tilde;
    }
    let tail = parts[parts.len() - 2..].join("/");
    format!("…/{tail}")
}

/// Dropdown rows used by the fused composer. No top/bottom rules and no
/// hint footer — those are provided by the box around it. Each row is
/// `marker · command · description` with the marker column reserved for the
/// `›` cursor on the active item.
fn slash_palette_rows(app: &App, width: usize) -> Vec<Line<'static>> {
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
            let marker = if is_selected { "› " } else { "  " };
            let cmd_style = if is_selected { accent() } else { text_style() };
            let desc_style = if is_selected { text_style() } else { muted() };
            let desc_max = width.saturating_sub(cmd_col + 4).max(4);
            let description = truncate(item.description, desc_max);
            highlight_selectable_row(
                vec![
                    Span::styled(marker, accent()),
                    Span::styled(format!("{:<cmd_col$}", item.command), cmd_style),
                    Span::raw("  "),
                    Span::styled(description, desc_style),
                ],
                is_selected,
                width,
            )
        })
        .collect()
}

fn input_box_rule(width: u16) -> String {
    "─".repeat(width as usize)
}

/// Token budget the context bar fills toward. `browser-use-core` compacts the
/// conversation at `max_context_chars` (240_000) / `APPROX_CHARS_PER_TOKEN` (4),
/// so the agent operates within ~60k tokens regardless of the underlying model.
const CONTEXT_BUDGET_TOKENS: i64 = 60_000;

/// Width, in cells, of the filled/empty context bar.
const CONTEXT_BAR_WIDTH: usize = 10;

/// Compact Claude-Code-style status bar rendered as the composer footer:
/// the active model, a context-fill bar, and accumulated session cost.
fn status_bar_line(app: &App, state: &WorkbenchState, _width: usize) -> Line<'static> {
    let usage = session_usage(app, state);
    let mut spans = vec![Span::styled(app.model.clone(), accent())];
    spans.push(status_separator());
    spans.extend(context_bar_spans(usage.context_tokens.unwrap_or(0)));
    if let Some(branch) = git_branch() {
        spans.push(status_separator());
        spans.push(Span::styled(branch, done()));
    }
    if usage.cost_usd > 0.0 {
        spans.push(status_separator());
        spans.push(Span::styled(format!("${:.4}", usage.cost_usd), muted()));
    }
    Line::from(spans)
}

/// A plain context bar — solid `█` fill over a `░` track — followed by the
/// `used/budget` token counts. Turns red as the conversation nears the
/// compaction budget.
fn context_bar_spans(used_tokens: i64) -> Vec<Span<'static>> {
    let used_tokens = used_tokens.max(0);
    let ratio = (used_tokens as f64 / CONTEXT_BUDGET_TOKENS as f64).clamp(0.0, 1.0);
    let fill_style = if ratio >= 0.9 { failed() } else { accent() };

    let filled = ((ratio * CONTEXT_BAR_WIDTH as f64).round() as usize).min(CONTEXT_BAR_WIDTH);
    vec![
        Span::styled("█".repeat(filled), fill_style),
        Span::styled("░".repeat(CONTEXT_BAR_WIDTH - filled), dim()),
        Span::raw("  "),
        Span::styled(
            format!(
                "{}/{}",
                format_token_count(used_tokens),
                format_token_count(CONTEXT_BUDGET_TOKENS)
            ),
            muted(),
        ),
    ]
}

fn status_separator() -> Span<'static> {
    Span::styled("  ·  ", dim())
}

/// Per-session token and cost totals derived from `model.usage` store events.
struct SessionUsage {
    /// Prompt tokens of the most recent model turn — i.e. current context occupancy.
    context_tokens: Option<i64>,
    /// Accumulated estimated cost across the whole session, in USD.
    cost_usd: f64,
}

fn session_usage(app: &App, state: &WorkbenchState) -> SessionUsage {
    let mut usage = SessionUsage {
        context_tokens: None,
        cost_usd: 0.0,
    };
    let Some(session) = state.current_session.as_ref() else {
        return usage;
    };
    for event in app.cached_events_for_session(&session.id) {
        if event.event_type != "model.usage" {
            continue;
        }
        if let Some(input_tokens) = event
            .payload
            .get("input_tokens")
            .and_then(serde_json::Value::as_i64)
        {
            usage.context_tokens = Some(input_tokens);
        }
        if let Some(cost) = event
            .payload
            .get("cost_usd")
            .and_then(serde_json::Value::as_f64)
        {
            usage.cost_usd += cost;
        }
    }
    usage
}

/// Current git branch of the working directory, or `None` outside a repo.
/// Walks up from the cwd to locate `.git` (directory or worktree pointer file).
fn git_branch() -> Option<String> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let git_path = dir.join(".git");
        if git_path.is_dir() {
            return branch_from_git_dir(&git_path);
        }
        if git_path.is_file() {
            // Worktree/submodule: `.git` is a file holding `gitdir: <path>`.
            let contents = std::fs::read_to_string(&git_path).ok()?;
            let gitdir = contents.strip_prefix("gitdir:")?.trim();
            return branch_from_git_dir(std::path::Path::new(gitdir));
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn branch_from_git_dir(git_dir: &std::path::Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref:") {
        let reference = reference.trim();
        return Some(
            reference
                .strip_prefix("refs/heads/")
                .unwrap_or(reference)
                .to_string(),
        );
    }
    // Detached HEAD — fall back to a short commit hash.
    (head.len() >= 7).then(|| head[..7].to_string())
}

fn format_token_count(tokens: i64) -> String {
    let tokens = tokens.max(0);
    if tokens < 1_000 {
        return tokens.to_string();
    }
    let thousands = tokens as f64 / 1_000.0;
    if thousands.fract().abs() < 0.05 {
        format!("{}k", thousands.round() as i64)
    } else {
        format!("{thousands:.1}k")
    }
}

/// Command key hints shown in the composer footer on the home screen, where
/// there is no active session and therefore no usage data to surface.
fn hint_row(width: usize) -> Line<'static> {
    let hints = [
        ("Enter", "send"),
        ("Tab", "history"),
        ("/", "commands"),
        ("Esc", "clear"),
    ];
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
    let is_selected = idx == app.selected_row;
    let current =
        app.model_configured && app.model == choice.display && app.account == choice.account;
    let name_style = if is_selected { bold() } else { text_style() };
    let access = access_label(choice.account);
    let descriptor = descriptor_for(idx);
    let descriptor_style = if descriptor == "needs key" {
        dim()
    } else {
        muted()
    };
    highlight_selectable_row(
        vec![
            Span::styled(format!("{:<20}", choice.display), name_style),
            Span::styled(format!("{:<22}", access), muted()),
            Span::styled(format!("{:<22}", descriptor), descriptor_style),
            Span::styled(if current { " *" } else { "" }.to_string(), done()),
        ],
        is_selected,
        // 2-space indent + 20 + 22 + 22 columns + " *" — width of the longest
        // possible row (one with the current-selection marker), so every row
        // highlights to the same end column.
        68,
    )
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

    let branch = git_branch().unwrap_or_else(|| "no branch".to_string());
    let cwd = cwd_label();
    let version = env!("CARGO_PKG_VERSION");
    let elapsed = app.startup_instant.elapsed().as_secs_f32();
    lines.extend(crate::welcome::welcome_lines(
        width,
        &branch,
        &cwd,
        version,
        elapsed,
        app.selected_row,
    ));
    let _ = state; // recent worktree list removed from welcome screen
    lines
}

/// The Browser Use config card, emitted once at the top of every session
/// transcript so the model/account/browser context stays visible per session.
pub(crate) fn session_header_lines(
    app: &App,
    state: &WorkbenchState,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = config_card_lines(app, state, width as usize);
    lines.push(Line::from(""));
    lines
}

fn config_card_lines(app: &App, state: &WorkbenchState, width: usize) -> Vec<Line<'static>> {
    // Cap at 94 cols so the card has a comfortable max on wide terminals, but
    // never enforce a min — when the terminal narrows below that cap the card
    // shrinks with it instead of overflowing and forcing a wrap.
    let card_w = width.min(94);
    let inner_w = card_w.saturating_sub(2);
    let rule = || Line::from(Span::styled(format!("+{}+", "-".repeat(inner_w)), border()));
    let mut lines = vec![rule()];
    lines.push(card_header_line("Browser Use Terminal", inner_w));
    lines.push(card_blank_line(inner_w));
    lines.push(card_kv_line("model", &app.model, "/model", inner_w));
    lines.push(card_kv_line(
        "account",
        &compact_account_label(&app.account),
        "/auth",
        inner_w,
    ));
    lines.push(card_kv_line(
        "browser",
        &browser_ready_label(app, state).replace(" ready", " idle"),
        "/browser",
        inner_w,
    ));
    lines.push(card_kv_line("directory", &cwd_label(), "", inner_w));
    lines.push(card_kv_line(
        "telemetry",
        &app.laminar_status()
            .unwrap_or_else(|_| "Laminar unavailable".to_string()),
        "/laminar",
        inner_w,
    ));
    lines.push(rule());
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

/// The bolded title row at the top of the config card.
fn card_header_line(title: &str, inner_w: usize) -> Line<'static> {
    let title = truncate(title, inner_w.saturating_sub(1));
    let title_len = title.chars().count();
    let trailing = inner_w.saturating_sub(title_len + 1);
    Line::from(vec![
        Span::styled("|", border()),
        Span::raw(" "),
        Span::styled(title, bold()),
        Span::raw(" ".repeat(trailing)),
        Span::styled("|", border()),
    ])
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

/// Status marker for a history row — conveys outcome at a glance, paired with
/// the status color, instead of a bare colon.
fn status_glyph(status: &str) -> char {
    match status {
        "done" => '✓',
        "failed" => '✗',
        "running" | "created" => '●',
        "cancelled" => '○',
        _ => '·',
    }
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
    let prefix = format!("{} {status_label:<8}", status_glyph(status));
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

fn history_lines(app: &App, state: &WorkbenchState, width: usize) -> Vec<Line<'static>> {
    if state.history.is_empty() {
        return vec![Line::from(Span::styled("No previous work yet.", dim()))];
    }
    state
        .history
        .iter()
        .enumerate()
        .map(|(idx, row)| history_overlay_line(row, idx, app.selected_row, width))
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
    // Layout priority is left-to-right: the task is the leftmost and most
    // important column, then status, then the relative timestamp. When the
    // terminal gets squished we drop time first, then status, so the task
    // stays visible instead of being squeezed to zero. Each row must render
    // as exactly one visual line — wrapping would throw off the History pane's
    // scroll math, which counts data rows.
    const INDENT: usize = 2;
    const STATUS_COL_W: usize = 10;
    const TASK_FLOOR: usize = 6;
    let time_str = relative_time(row.updated_ms);
    let time_w = time_str.chars().count();
    let full_task_w = width.saturating_sub(INDENT + STATUS_COL_W + time_w);
    let no_time_task_w = width.saturating_sub(INDENT + STATUS_COL_W);
    let task_only_w = width.saturating_sub(INDENT);
    let (task_w, show_status, show_time) = if full_task_w >= TASK_FLOOR {
        (full_task_w, true, true)
    } else if no_time_task_w >= TASK_FLOOR {
        (no_time_task_w, true, false)
    } else {
        (task_only_w, false, false)
    };
    let mut content = vec![Span::styled(
        format!("{:<task_w$}", truncate(&row.task, task_w)),
        text_style(),
    )];
    if show_status {
        content.push(Span::styled(
            format!("{:<STATUS_COL_W$}", row.status.as_str()),
            status_style(row.status.as_str()),
        ));
    }
    if show_time {
        content.push(Span::styled(time_str, muted()));
    }
    highlight_selectable_row(content, idx == selected_row, width)
}

/// The single source of truth for selectable-row styling: a 2-space indent and,
/// when selected, a full-width background highlight. Shared by the slash palette
/// and the history list so selection looks identical everywhere.
fn highlight_selectable_row(
    content: Vec<Span<'static>>,
    is_selected: bool,
    width: usize,
) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    spans.extend(content);
    let mut line = Line::from(spans);
    if is_selected {
        let used: usize = line
            .spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        if used < width {
            line.spans.push(Span::raw(" ".repeat(width - used)));
        }
        line = line.style(selection());
    }
    line
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
        let count = value.chars().count();
        let visible = count.min(8);
        let hidden = count.saturating_sub(8);
        let prefix: String = value.chars().take(visible).collect();
        format!("{prefix}{}", "*".repeat(hidden))
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
