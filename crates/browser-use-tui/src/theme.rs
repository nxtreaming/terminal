use ratatui::style::{Color, Modifier, Style};

pub(crate) fn text() -> Color {
    Color::Rgb(205, 214, 244)
}

fn muted_color() -> Color {
    Color::Rgb(166, 173, 200)
}

fn dim_color() -> Color {
    Color::Rgb(108, 112, 134)
}

fn accent_color() -> Color {
    Color::Rgb(137, 180, 250)
}

fn link_color() -> Color {
    Color::Rgb(137, 220, 235)
}

fn path_reference_color() -> Color {
    Color::Rgb(250, 179, 135)
}

fn code_color() -> Color {
    Color::Rgb(180, 190, 254)
}

fn code_background_color() -> Color {
    Color::Rgb(49, 50, 68)
}

fn heading_color() -> Color {
    Color::Rgb(250, 179, 135)
}

fn quote_color() -> Color {
    Color::Rgb(147, 153, 178)
}

fn border_color() -> Color {
    Color::Rgb(69, 71, 90)
}

fn done_color() -> Color {
    Color::Rgb(166, 227, 161)
}

fn running_color() -> Color {
    Color::Rgb(250, 179, 135)
}

fn failed_color() -> Color {
    Color::Rgb(243, 139, 168)
}

fn thought_color() -> Color {
    Color::Rgb(203, 166, 247)
}

pub(crate) fn text_style() -> Style {
    Style::default().fg(text())
}

pub(crate) fn bold() -> Style {
    text_style().add_modifier(Modifier::BOLD)
}

pub(crate) fn muted() -> Style {
    Style::default().fg(muted_color())
}

pub(crate) fn dim() -> Style {
    Style::default().fg(dim_color())
}

pub(crate) fn accent() -> Style {
    Style::default()
        .fg(accent_color())
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn border() -> Style {
    Style::default().fg(border_color())
}

fn user_prompt_background_color() -> Color {
    Color::Rgb(49, 50, 68)
}

/// Background fill for a user prompt block in the transcript, so the message
/// the user sent stands apart from the agent's replies.
pub(crate) fn user_prompt_text() -> Style {
    text_style().bg(user_prompt_background_color())
}

pub(crate) fn user_prompt_muted() -> Style {
    muted().bg(user_prompt_background_color())
}

/// The accent-colored `>` prefix on a user prompt, sharing the prompt's
/// highlight background.
pub(crate) fn user_prompt_accent() -> Style {
    accent().bg(user_prompt_background_color())
}

pub(crate) fn link() -> Style {
    Style::default()
        .fg(link_color())
        .add_modifier(Modifier::UNDERLINED)
}

pub(crate) fn path_reference() -> Style {
    Style::default().fg(path_reference_color())
}

pub(crate) fn markdown_code() -> Style {
    Style::default()
        .fg(code_color())
        .bg(code_background_color())
}

pub(crate) fn markdown_code_block() -> Style {
    Style::default().fg(Color::Rgb(186, 194, 222))
}

pub(crate) fn markdown_emphasis() -> Style {
    muted().add_modifier(Modifier::ITALIC)
}

pub(crate) fn markdown_strong() -> Style {
    bold()
}

pub(crate) fn markdown_marker() -> Style {
    muted()
}

pub(crate) fn markdown_quote() -> Style {
    Style::default()
        .fg(quote_color())
        .add_modifier(Modifier::ITALIC)
}

pub(crate) fn markdown_heading() -> Style {
    Style::default()
        .fg(heading_color())
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn done() -> Style {
    Style::default().fg(done_color())
}

pub(crate) fn running() -> Style {
    Style::default().fg(running_color())
}

pub(crate) fn failed() -> Style {
    Style::default().fg(failed_color())
}

pub(crate) fn thought() -> Style {
    Style::default().fg(thought_color())
}

pub(crate) fn activity_group() -> Style {
    Style::default()
        .fg(Color::Rgb(166, 227, 161))
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_read() -> Style {
    Style::default()
        .fg(Color::Rgb(137, 180, 250))
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_run() -> Style {
    Style::default()
        .fg(Color::Rgb(250, 179, 135))
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_list() -> Style {
    Style::default()
        .fg(Color::Rgb(148, 226, 213))
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_search() -> Style {
    Style::default()
        .fg(Color::Rgb(249, 226, 175))
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn activity_task() -> Style {
    Style::default()
        .fg(Color::Rgb(180, 190, 254))
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn selection() -> Style {
    Style::default().bg(Color::Rgb(45, 52, 66))
}

pub(crate) fn status_style(status: &str) -> Style {
    match status {
        "done" => done(),
        "failed" => failed(),
        "running" | "created" | "starting" => running(),
        _ => muted(),
    }
}
