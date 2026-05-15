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

pub(crate) fn link() -> Style {
    text_style().add_modifier(Modifier::UNDERLINED)
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

pub(crate) fn status_style(status: &str) -> Style {
    match status {
        "done" => done(),
        "failed" => failed(),
        "running" | "created" => running(),
        _ => muted(),
    }
}
