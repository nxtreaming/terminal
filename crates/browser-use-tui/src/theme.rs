use ratatui::style::{Color, Modifier, Style};

pub(crate) fn text() -> Color {
    Color::Rgb(230, 230, 235)
}

fn muted_color() -> Color {
    Color::Rgb(156, 158, 168)
}

fn dim_color() -> Color {
    Color::Rgb(102, 105, 116)
}

fn accent_color() -> Color {
    Color::Rgb(92, 156, 245)
}

fn border_color() -> Color {
    Color::Rgb(74, 78, 92)
}

fn done_color() -> Color {
    Color::Rgb(126, 192, 143)
}

fn running_color() -> Color {
    Color::Rgb(215, 168, 79)
}

fn failed_color() -> Color {
    Color::Rgb(230, 126, 126)
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
    Style::default().fg(Color::Rgb(125, 180, 255))
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

pub(crate) fn status_style(status: &str) -> Style {
    match status {
        "done" => done(),
        "failed" => failed(),
        "running" | "created" => running(),
        _ => muted(),
    }
}
