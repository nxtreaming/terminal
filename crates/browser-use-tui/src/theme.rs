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

pub(crate) fn panel() -> Color {
    Color::Rgb(30, 32, 38)
}

pub(crate) fn background() -> Color {
    Color::Rgb(18, 19, 23)
}

pub(crate) fn composer_bg() -> Color {
    Color::Rgb(28, 30, 36)
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

pub(crate) fn link() -> Style {
    Style::default().fg(Color::Rgb(125, 180, 255))
}

pub(crate) fn status_style(status: &str) -> Style {
    match status {
        "done" => Style::default().fg(Color::Rgb(126, 192, 143)),
        "failed" => Style::default().fg(Color::Rgb(230, 126, 126)),
        "running" | "created" => Style::default().fg(Color::Rgb(215, 168, 79)),
        _ => muted(),
    }
}
