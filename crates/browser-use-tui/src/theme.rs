use ratatui::style::{Color, Modifier, Style};

pub(crate) fn text() -> Color {
    Color::Rgb(236, 238, 232)
}

fn muted_color() -> Color {
    Color::Rgb(138, 144, 136)
}

fn dim_color() -> Color {
    Color::Rgb(84, 91, 84)
}

fn accent_color() -> Color {
    Color::Rgb(126, 158, 255)
}

fn link_color() -> Color {
    Color::Rgb(119, 196, 255)
}

fn path_reference_color() -> Color {
    Color::Rgb(197, 177, 116)
}

fn code_color() -> Color {
    Color::Rgb(173, 215, 190)
}

fn code_background_color() -> Color {
    Color::Rgb(39, 47, 42)
}

fn heading_color() -> Color {
    Color::Rgb(220, 171, 78)
}

fn quote_color() -> Color {
    Color::Rgb(154, 160, 151)
}

fn border_color() -> Color {
    Color::Rgb(53, 61, 52)
}

fn done_color() -> Color {
    Color::Rgb(142, 202, 129)
}

fn running_color() -> Color {
    Color::Rgb(220, 171, 78)
}

fn failed_color() -> Color {
    Color::Rgb(255, 112, 132)
}

fn thought_color() -> Color {
    Color::Rgb(178, 141, 255)
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
    Color::Rgb(38, 42, 38)
}

/// Background fill for a user prompt block in the transcript, so the message
/// the user sent stands apart from the agent's replies.
pub(crate) fn user_prompt_text() -> Style {
    text_style().bg(user_prompt_background_color())
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
    Style::default().fg(Color::Rgb(206, 213, 201))
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
    Style::default()
        .fg(thought_color())
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn selection() -> Style {
    Style::default().bg(Color::Rgb(45, 52, 66))
}

pub(crate) fn status_style(status: &str) -> Style {
    match status {
        "done" => done(),
        "failed" => failed(),
        "running" | "created" => running(),
        _ => muted(),
    }
}
