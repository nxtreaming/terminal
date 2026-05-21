use anyhow::Result;
use browser_use_protocol::{HistoryRow, TelemetrySummary, WorkbenchState};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};
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
    let full_area = frame.area();
    let area = app_surface(full_area);
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
            if !app.native_scrollback_is_active() {
                render_active_modal_overlay(frame, full_area, app, &state);
            }
        }
        surface if surface.uses_main_view() => {
            if app.is_slash_palette_active() && !app.native_scrollback_is_active() {
                render_main(frame, area, app, &state, product_state);
                render_active_modal_overlay(frame, full_area, app, &state);
            } else {
                render_main(frame, area, app, &state, product_state);
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
    let modal_overlay_active = app.surface.is_popup() && !app.native_scrollback_is_active();
    let native_scrollback_active = app.native_scrollback_is_active() && !modal_overlay_active;
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
            ProductState::Ready => ready_lines(app, state, body_width, max_body_h),
            ProductState::Running
            | ProductState::Result
            | ProductState::Failed
            | ProductState::Cancelled => work_lines(state, app, body_width, product_state),
        }
    };
    let pin_bottom = should_pin_main_bottom(product_state, native_scrollback_active)
        && !layout_surface.is_bottom_pane();
    let attach_bottom_to_body =
        native_scrollback_active && !body.is_empty() && !layout_surface.is_bottom_pane();
    let (body_area, bottom_area, footer_area) = main_layout_areas(
        area,
        bottom_h,
        body.len(),
        show_footer,
        pin_bottom,
        attach_bottom_to_body,
    );
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
    let body_content_rect = content_area(body_render_area);
    if matches!(product_state, ProductState::Ready) && app.is_welcome_surface() {
        app.welcome_logo_rect
            .set(Some(crate::welcome::logo_screen_rect(
                body_content_rect,
                app.status_notice.is_some(),
            )));
    } else {
        app.welcome_logo_rect.set(None);
    }
    frame.render_widget(
        Paragraph::new(body)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        body_content_rect,
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
    attach_bottom_to_body: bool,
) -> (Rect, Rect, Rect) {
    let footer_h = u16::from(show_footer && area.height > bottom_h);
    let max_body_h = area
        .height
        .saturating_sub(bottom_h)
        .saturating_sub(footer_h);
    let body_h = (body_len as u16).min(max_body_h);
    // The composer is always pinned to the bottom of the terminal; the
    // optional footer is the very last row. The body either sits at the
    // top with a flex spacer pushing the composer down (welcome / setup),
    // or sits at the bottom just above the composer with the spacer
    // above it so it grows downward toward the composer as content
    // arrives (active sessions).
    let chunks = if pin_bottom {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Fill(1),
                Constraint::Length(body_h),
                Constraint::Length(bottom_h),
                Constraint::Length(footer_h),
            ])
            .split(area)
    } else if attach_bottom_to_body {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(body_h),
                Constraint::Length(bottom_h),
                Constraint::Fill(1),
                Constraint::Length(footer_h),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(body_h),
                Constraint::Fill(1),
                Constraint::Length(bottom_h),
                Constraint::Length(footer_h),
            ])
            .split(area)
    };
    if attach_bottom_to_body && !pin_bottom {
        (chunks[0], chunks[1], chunks[3])
    } else {
        let body_idx = if pin_bottom { 1 } else { 0 };
        (chunks[body_idx], chunks[2], chunks[3])
    }
}

fn should_pin_main_bottom(product_state: ProductState, native_scrollback_active: bool) -> bool {
    if native_scrollback_active {
        return false;
    }
    matches!(
        product_state,
        ProductState::Running
            | ProductState::Result
            | ProductState::Failed
            | ProductState::Cancelled
    )
}

pub(crate) fn main_viewport_height(app: &App, width: u16) -> u16 {
    // The palette is now a floating popup over the main view — it doesn't
    // grow the composer pane. So the composer's own height is the only
    // contributor to the bottom-pane reserve.
    composer_pane_height(app, ProductState::Ready, width)
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
    let line_count =
        surface_lines(surface, app, state, content_width(area.width) as usize).len() as u16;
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
    // top border + input rows + bottom border + status row beneath.
    visual_input_lines + 3
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

pub(crate) struct ModalOverlay {
    pub(crate) rect: Rect,
    pub(crate) buffer: Buffer,
    pub(crate) cursor: Option<Position>,
}

pub(crate) fn active_modal_overlay(
    app: &App,
    state: &WorkbenchState,
    area: Rect,
) -> Option<ModalOverlay> {
    if app.is_slash_palette_active() {
        return command_palette_overlay(app, area);
    }
    if app.surface.is_popup() {
        return surface_popup_overlay(app, state, area, app.surface);
    }
    None
}

fn render_active_modal_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    state: &WorkbenchState,
) {
    let Some(overlay) = active_modal_overlay(app, state, area) else {
        return;
    };
    overlay.buffer.merge_into(frame.buffer_mut(), overlay.rect);
    if let Some(cursor) = overlay.cursor {
        frame.set_cursor_position(cursor);
    }
}

trait BufferOverlayExt {
    fn merge_into(&self, target: &mut Buffer, target_rect: Rect);
}

impl BufferOverlayExt for Buffer {
    fn merge_into(&self, target: &mut Buffer, target_rect: Rect) {
        for y in 0..self.area.height {
            for x in 0..self.area.width {
                if let Some(target_cell) = target.cell_mut((
                    target_rect.x.saturating_add(x),
                    target_rect.y.saturating_add(y),
                )) {
                    *target_cell = self[(x, y)].clone();
                }
            }
        }
    }
}

fn surface_popup_overlay(
    app: &App,
    state: &WorkbenchState,
    area: Rect,
    surface: Surface,
) -> Option<ModalOverlay> {
    let rect = surface_popup_rect(app, state, area, surface)?;
    let local_rect = Rect::new(0, 0, rect.width, rect.height);
    let mut buffer = Buffer::empty(local_rect);
    let local_cursor = render_surface_popup_box(&mut buffer, local_rect, app, state, surface);
    let cursor = local_cursor.map(|position| Position {
        x: rect.x.saturating_add(position.x),
        y: rect.y.saturating_add(position.y),
    });
    Some(ModalOverlay {
        rect,
        buffer,
        cursor,
    })
}

/// Centered floating popup overlay for slash-command-launched surfaces
/// (history, browser, model, auth, telemetry, developer). Responsive: shrinks
/// to fit small terminals and caps to a comfortable max on large ones.
fn surface_popup_rect(
    app: &App,
    state: &WorkbenchState,
    area: Rect,
    surface: Surface,
) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
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
    let body_inner_width = popup_w
        .saturating_sub(2 + CONTENT_HORIZONTAL_MARGIN * 2)
        .max(1) as usize;
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
    Some(Rect {
        x: popup_x,
        y: popup_y,
        width: popup_w,
        height: popup_h,
    })
}

fn render_surface_popup_box(
    buffer: &mut Buffer,
    popup_rect: Rect,
    app: &App,
    state: &WorkbenchState,
    surface: Surface,
) -> Option<Position> {
    Clear.render(popup_rect, buffer);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border());
    let inner = block.inner(popup_rect);
    block.render(popup_rect, buffer);

    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    // Layout inside the popup: header lines, body, footer line.
    let header = surface_header_lines(surface, inner.width);
    let header_h = (header.len() as u16).min(inner.height);
    let footer_text = surface_footer(surface);
    let footer_h: u16 = if footer_text.is_empty() { 0 } else { 1 };
    let body_h = inner
        .height
        .saturating_sub(header_h)
        .saturating_sub(footer_h);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Length(body_h),
            Constraint::Length(footer_h),
        ])
        .split(inner);

    Paragraph::new(header).render(content_area(chunks[0]), buffer);

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
    let cursor_pos: Option<Position> = if surface.is_text_input_popup() {
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
        lines
            .iter()
            .take(visible_h)
            .enumerate()
            .find_map(|(row, line)| {
                let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                if plain.starts_with(&target) {
                    Some(Position {
                        x: body_area.x.saturating_add(cursor_col.min(body_area.width)),
                        y: body_area.y.saturating_add(row as u16),
                    })
                } else {
                    None
                }
            })
    } else {
        None
    };
    trim_trailing_whitespace(&mut lines);
    Paragraph::new(lines)
        .style(Style::default().fg(text()))
        .wrap(Wrap { trim: false })
        .render(body_area, buffer);

    if footer_h > 0 {
        Paragraph::new(footer_text)
            .style(muted())
            .alignment(Alignment::Right)
            .render(content_area(chunks[2]), buffer);
    }
    cursor_pos
}

pub(crate) fn command_palette_overlay(app: &App, area: Rect) -> Option<ModalOverlay> {
    let rect = command_palette_popup_rect(area)?;
    let local_rect = Rect::new(0, 0, rect.width, rect.height);
    let mut buffer = Buffer::empty(local_rect);
    let local_cursor = render_command_palette_box(&mut buffer, local_rect, app)?;
    let cursor = Position {
        x: rect.x.saturating_add(local_cursor.x),
        y: rect.y.saturating_add(local_cursor.y),
    };
    Some(ModalOverlay {
        rect,
        buffer,
        cursor: Some(cursor),
    })
}

fn command_palette_popup_rect(area: Rect) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    const MIN_W: u16 = 40;
    const MIN_H: u16 = 10;
    const MAX_W: u16 = 72;
    const H_MARGIN: u16 = 4;
    const V_MARGIN: u16 = 2;

    // The popup size is fixed at the full command count so the box never
    // resizes as the user filters — empty slots stay blank below the rows.
    // Chrome: border(2) + input row(1) + blank(1) + footer(1) = 5.
    let desired_h = (palette::max_item_count() as u16)
        .saturating_add(5)
        .max(MIN_H);

    let popup_w = if area.width <= MIN_W {
        area.width
    } else {
        area.width
            .saturating_sub(H_MARGIN.saturating_mul(2))
            .min(MAX_W)
            .max(MIN_W)
    };
    let available_h = area
        .height
        .saturating_sub(V_MARGIN.saturating_mul(2))
        .max(MIN_H.min(area.height));
    let popup_h = desired_h.min(available_h).max(MIN_H.min(available_h));
    let popup_x = area.x + area.width.saturating_sub(popup_w) / 2;
    let popup_y = area.y + area.height.saturating_sub(popup_h) / 2;
    Some(Rect {
        x: popup_x,
        y: popup_y,
        width: popup_w,
        height: popup_h,
    })
}

fn render_command_palette_box(
    buffer: &mut Buffer,
    popup_rect: Rect,
    app: &App,
) -> Option<Position> {
    Clear.render(popup_rect, buffer);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border());
    let inner = block.inner(popup_rect);
    block.render(popup_rect, buffer);
    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    // Layout inside the popup:
    //   input row       — `> filter` (with cursor)
    //   blank
    //   items body      — filtered command rows
    //   footer hint
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // input
            Constraint::Length(1), // blank
            Constraint::Min(1),    // items
            Constraint::Length(1), // footer hint
        ])
        .split(inner);

    // Input row. The popup owns its own filter — the composer underneath
    // is untouched while the palette is open.
    let typed = app.palette_filter().to_string();
    let input_area = chunks[0];
    let input_inner = Rect {
        x: input_area.x.saturating_add(2),
        y: input_area.y,
        width: input_area.width.saturating_sub(2),
        height: 1,
    };
    let input_line = Line::from(vec![
        Span::styled("> ", accent()),
        Span::styled(typed.clone(), text_style()),
    ]);
    Paragraph::new(input_line).render(input_area, buffer);
    let cursor_offset = typed.chars().count() as u16;
    let mut cursor = None;
    if input_inner.width > 0 {
        cursor = Some(Position {
            x: input_inner
                .x
                .saturating_add(cursor_offset.min(input_inner.width)),
            y: input_inner.y,
        });
    }

    let body_chunk = chunks[2];
    let footer_chunk = chunks[3];
    let items = app.slash_palette_items();

    if items.is_empty() {
        Paragraph::new(Line::from(Span::styled("  No commands match.", muted())))
            .render(body_chunk, buffer);
    } else {
        let rows = slash_palette_rows(app, body_chunk.width as usize);
        let mut visible = rows;
        let body_h = body_chunk.height as usize;
        if body_h > 0 && app.selected_row >= body_h {
            let skip = app.selected_row + 1 - body_h;
            visible = visible.into_iter().skip(skip).collect();
        }
        Paragraph::new(visible)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false })
            .render(body_chunk, buffer);
    }

    Paragraph::new(Line::from(Span::styled(
        " ↑↓ navigate · ⏎ select · esc close",
        muted(),
    )))
    .alignment(Alignment::Right)
    .render(footer_chunk, buffer);
    cursor
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
/// Bordered composer with the current git branch punched into the
/// bottom border, and a single muted status row beneath showing the
/// active model and the context-fill bar. No browser, no cwd, no key
/// hints — the only ambient metadata is what the user explicitly asked
/// to see.
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
    let input_inner_w = area.width.saturating_sub(4).max(1);
    let input_h = composer_visual_input_lines(app, input_inner_w);
    let box_h = input_h.saturating_add(2).min(area.height);
    let status_h: u16 = if area.height > box_h { 1 } else { 0 };

    let box_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: box_h,
    };

    // Top + sides via Block, bottom border drawn manually so the branch
    // tag punches through it in white while the dashes/corners keep the
    // same gray border() color as the rest of the box.
    let block = Block::default()
        .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
        .border_type(BorderType::Rounded)
        .border_style(border());
    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);

    // IMPORTANT: render the input FIRST. Ratatui's Paragraph::render fills the
    // entire area with its base style (`Style::default().fg(text())` for the
    // composer input), which would otherwise paint over the bottom-border row
    // and bleach our dim border to bright white.
    if inner.width > 2 && inner.height > 0 {
        let input_area = Rect {
            x: inner.x.saturating_add(1),
            y: inner.y,
            width: inner.width.saturating_sub(2),
            height: inner.height.saturating_sub(1),
        };
        render_composer_input(frame, input_area, app, state);
    }

    let bottom_area = Rect {
        x: box_area.x,
        y: box_area.y + box_area.height.saturating_sub(1),
        width: box_area.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(composer_bottom_border(box_area.width)).style(border()),
        bottom_area,
    );

    if status_h > 0 {
        let status_area = Rect {
            x: area.x,
            y: box_area.y + box_area.height,
            width: area.width,
            height: status_h,
        };
        let status_inner = status_area.inner(Margin {
            vertical: 0,
            horizontal: 2,
        });
        frame.render_widget(
            Paragraph::new(composer_status_line(
                app,
                state,
                status_inner.width as usize,
            )),
            status_inner,
        );
    }
}

/// Bottom border line for the composer, with the git branch tag punched
/// through it on the right. Corners and dashes use the same gray
/// `border()` style as the rest of the box; the branch text is white.
fn composer_bottom_border(width: u16) -> Line<'static> {
    if width < 2 {
        return Line::from("");
    }
    let inner_w = width.saturating_sub(2) as usize;
    let mut spans: Vec<Span<'static>> = vec![Span::styled("╰", border())];
    if let Some(branch) = git_branch() {
        // ` branch ` with one cell of dash padding on each side, so the
        // background dashes hug right up to the spaces around the tag.
        let tag: Vec<Span<'static>> = vec![
            Span::raw(" "),
            Span::styled(branch, text_style()),
            Span::raw(" "),
        ];
        let tag_w: usize = tag.iter().map(|s| s.content.chars().count()).sum();
        let trail = 2usize.min(inner_w.saturating_sub(tag_w));
        let lead = inner_w.saturating_sub(tag_w + trail);
        spans.push(Span::styled("─".repeat(lead), border()));
        spans.extend(tag);
        spans.push(Span::styled("─".repeat(trail), border()));
    } else {
        spans.push(Span::styled("─".repeat(inner_w), border()));
    }
    spans.push(Span::styled("╯", border()));
    Line::from(spans)
}

/// Status row below the composer: active model and context-fill bar,
/// plus running cost when there is one. The branch lives on the box's
/// bottom border, not here.
fn composer_status_line(app: &App, state: &WorkbenchState, _width: usize) -> Line<'static> {
    let usage = session_usage(app, state);
    let mut spans = vec![Span::styled(app.model.clone(), accent())];
    spans.push(status_separator());
    spans.extend(context_bar_spans(usage.context_tokens.unwrap_or(0)));
    if usage.cost_usd > 0.0 {
        spans.push(status_separator());
        spans.push(Span::styled(format!("${:.4}", usage.cost_usd), muted()));
    }
    Line::from(spans)
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

/// Token budget the context bar fills toward. `browser-use-core` compacts the
/// conversation at `max_context_chars` (240_000) / `APPROX_CHARS_PER_TOKEN` (4),
/// so the agent operates within ~60k tokens regardless of the underlying model.
const CONTEXT_BUDGET_TOKENS: i64 = 60_000;

/// Width, in cells, of the filled/empty context bar.
const CONTEXT_BAR_WIDTH: usize = 10;

/// A plain context bar — solid `█` fill over a `░` track — followed by the
/// `used/budget` token counts. Turns red as the conversation nears the
/// compaction budget.
fn context_bar_spans(used_tokens: i64) -> Vec<Span<'static>> {
    let used_tokens = used_tokens.max(0);
    let ratio = (used_tokens as f64 / CONTEXT_BUDGET_TOKENS as f64).clamp(0.0, 1.0);
    let fill_style = if ratio >= 0.9 { failed() } else { accent() };

    let filled = ((ratio * CONTEXT_BAR_WIDTH as f64).round() as usize).min(CONTEXT_BAR_WIDTH);
    let pct_left = ((1.0 - ratio) * 100.0).round() as i64;
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
        Span::raw("  "),
        Span::styled(format!("{pct_left}% context left"), muted()),
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

fn render_composer_input(frame: &mut Frame<'_>, area: Rect, app: &App, state: &WorkbenchState) {
    let current_session = state.current_session.as_ref();
    let placeholder = if current_session.is_some_and(|session| session.status.is_active()) {
        "Type to steer the agent..."
    } else if current_session.is_some() {
        "Ask a follow-up..."
    } else {
        "Tell the browser what to do..."
    };
    let max_lines = area.height.max(1) as usize;
    // While the slash palette is open, the popup is the input — render the
    // composer as if it were empty (just the placeholder, no `/text`) and
    // skip cursor placement here so the popup owns it.
    let palette_owns_input = app.is_slash_palette_active();
    let lines: Vec<Line<'static>> = if palette_owns_input {
        vec![Line::from(vec![
            Span::styled("> ", dim()),
            Span::styled(placeholder.to_string(), dim()),
        ])]
    } else {
        app.composer
            .render_lines_wrapped(max_lines, area.width as usize, placeholder)
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(text()))
            .wrap(Wrap { trim: false }),
        area,
    );
    if palette_owns_input {
        return;
    }
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
        "attach to already-open browser",
        "Rust-owned background browser",
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

fn ready_lines(app: &App, state: &WorkbenchState, width: u16, max_h: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(notice) = app.status_notice.as_ref() {
        lines.push(Line::from(Span::styled(notice.clone(), failed())));
        lines.push(Line::from(""));
    }
    // Pass the remaining body height to the welcome renderer so it can
    // balance the gap above the logo with the gap below the menu.
    let remaining = max_h.saturating_sub(lines.len() as u16);
    lines.extend(crate::welcome::welcome_lines(
        width,
        &app.welcome_anim,
        app.selected_row,
        remaining,
    ));
    let _ = state;
    lines
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
                        for _ in 0..transcript::gap_before_active(&model) {
                            lines.push(Line::from(""));
                        }
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
