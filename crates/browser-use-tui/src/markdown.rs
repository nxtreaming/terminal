use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::{
    link, markdown_code, markdown_code_block, markdown_emphasis, markdown_heading, markdown_marker,
    markdown_quote, markdown_strong, muted, text_style,
};

pub(crate) fn render_markdown_lines(markdown: &str, width: u16) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let mut writer = MarkdownWriter::new(width as usize);
    for event in Parser::new_ext(markdown, options) {
        writer.handle_event(event);
    }
    writer.finish()
}

#[derive(Clone, Debug)]
struct ListState {
    next: Option<u64>,
}

#[derive(Clone, Debug)]
struct TableRow {
    cells: Vec<Vec<Span<'static>>>,
    is_header: bool,
}

#[derive(Clone, Debug)]
struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<TableRow>,
    current_row: Option<Vec<Vec<Span<'static>>>>,
    current_cell: Option<Vec<Span<'static>>>,
    in_header: bool,
}

struct MarkdownWriter {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<ListState>,
    link_stack: Vec<String>,
    table: Option<TableState>,
    pending_prefix: Option<Vec<Span<'static>>>,
    quote_depth: usize,
    in_code_block: bool,
    needs_blank: bool,
    width: usize,
}

impl MarkdownWriter {
    fn new(width: usize) -> Self {
        Self {
            lines: Vec::new(),
            current: Vec::new(),
            style_stack: Vec::new(),
            list_stack: Vec::new(),
            link_stack: Vec::new(),
            table: None,
            pending_prefix: None,
            quote_depth: 0,
            in_code_block: false,
            needs_blank: false,
            width: width.max(24),
        }
    }

    fn handle_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => {
                if self.in_code_block {
                    self.push_code_text(&text);
                } else {
                    self.push_text(&text);
                }
            }
            Event::Code(code) => {
                self.push_span(Span::styled(code.to_string(), markdown_code()));
            }
            Event::SoftBreak | Event::HardBreak => self.flush_current(),
            Event::Rule => {
                self.flush_current();
                self.push_blank();
                self.lines.push(Line::from(Span::styled("---", muted())));
                self.push_blank();
            }
            Event::Html(html) | Event::InlineHtml(html) => self.push_text(&html),
            Event::FootnoteReference(text) => self.push_text(&format!("[{text}]")),
            Event::TaskListMarker(checked) => {
                self.push_span(Span::styled(if checked { "[x] " } else { "[ ] " }, muted()));
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.needs_blank && self.list_stack.is_empty() {
                    self.push_blank();
                }
                self.needs_blank = false;
            }
            Tag::Heading { level, .. } => {
                self.flush_current();
                if !self.lines.is_empty() {
                    self.push_blank();
                }
                self.push_style(heading_text_style(level));
            }
            Tag::BlockQuote => {
                self.flush_current();
                if self.needs_blank {
                    self.push_blank();
                    self.needs_blank = false;
                }
                self.quote_depth += 1;
            }
            Tag::Table(alignments) => {
                self.flush_current();
                if !self.lines.is_empty() && !self.last_line_is_blank() {
                    self.push_blank();
                }
                self.table = Some(TableState {
                    alignments,
                    rows: Vec::new(),
                    current_row: None,
                    current_cell: None,
                    in_header: false,
                });
                self.needs_blank = false;
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = true;
                    table.current_row = Some(Vec::new());
                }
            }
            Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.current_row = Some(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.current_cell = Some(Vec::new());
                }
            }
            Tag::CodeBlock(kind) => {
                self.flush_current();
                if !self.lines.is_empty() && !self.last_line_is_blank() {
                    self.push_blank();
                }
                let _language = match kind {
                    CodeBlockKind::Fenced(language) if !language.is_empty() => {
                        Some(language.to_string())
                    }
                    _ => None,
                };
                self.in_code_block = true;
                self.needs_blank = false;
            }
            Tag::List(start) => {
                self.flush_current();
                self.list_stack.push(ListState { next: start });
                self.needs_blank = false;
            }
            Tag::Item => {
                self.flush_current();
                self.pending_prefix = Some(self.next_list_marker());
            }
            Tag::Emphasis => self.push_style(markdown_emphasis()),
            Tag::Strong => self.push_style(markdown_strong()),
            Tag::Strikethrough => self.push_style(muted()),
            Tag::Link { dest_url, .. } => {
                self.link_stack.push(dest_url.to_string());
                self.push_style(link());
            }
            Tag::Image {
                title, dest_url, ..
            } => {
                let label = if title.is_empty() {
                    dest_url.to_string()
                } else {
                    title.to_string()
                };
                self.push_span(Span::styled(format!("[image: {label}]"), muted()));
            }
            Tag::FootnoteDefinition(_) | Tag::HtmlBlock | Tag::MetadataBlock(_) => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_current();
                self.needs_blank = true;
            }
            TagEnd::Heading(_) => {
                self.flush_current();
                self.pop_style();
                self.needs_blank = true;
            }
            TagEnd::BlockQuote => {
                self.flush_current();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.needs_blank = true;
            }
            TagEnd::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    let cell = table.current_cell.take().unwrap_or_default();
                    if let Some(row) = table.current_row.as_mut() {
                        row.push(cell);
                    }
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    let cells = table.current_row.take().unwrap_or_default();
                    table.rows.push(TableRow {
                        cells,
                        is_header: table.in_header,
                    });
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    let cells = table.current_row.take().unwrap_or_default();
                    if !cells.is_empty() {
                        table.rows.push(TableRow {
                            cells,
                            is_header: true,
                        });
                    }
                    table.in_header = false;
                }
            }
            TagEnd::Table => {
                self.flush_table();
                self.needs_blank = true;
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                self.needs_blank = true;
            }
            TagEnd::List(_) => {
                self.flush_current();
                self.list_stack.pop();
                self.needs_blank = true;
            }
            TagEnd::Item => {
                self.flush_current();
                self.pending_prefix = None;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link => {
                self.pop_style();
                if let Some(dest) = self.link_stack.pop() {
                    self.push_span(Span::raw(" ("));
                    self.push_span(Span::styled(dest, link()));
                    self.push_span(Span::raw(")"));
                }
            }
            TagEnd::Image
            | TagEnd::FootnoteDefinition
            | TagEnd::HtmlBlock
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        if self.in_table_cell() {
            let normalized = text.replace(['\n', '\r'], " ");
            if !normalized.is_empty() {
                let style = self.current_style();
                self.push_span(Span::styled(normalized, style));
            }
            return;
        }
        for (idx, line) in text.lines().enumerate() {
            if idx > 0 {
                self.flush_current();
            }
            let style = self.current_style();
            if looks_like_bare_link(line) || looks_like_path(line) {
                self.push_span(Span::styled(line.to_string(), link()));
            } else {
                self.push_span(Span::styled(line.to_string(), style));
            }
        }
    }

    fn push_code_text(&mut self, text: &str) {
        for part in text.split_inclusive('\n') {
            let line = part.trim_end_matches(['\n', '\r']).to_string();
            self.lines
                .push(Line::from(Span::styled(line, markdown_code_block())));
        }
    }

    fn ensure_prefix(&mut self) {
        if !self.current.is_empty() {
            return;
        }
        for _ in 0..self.quote_depth {
            self.current
                .push(Span::styled("| ".to_string(), markdown_quote()));
        }
        if let Some(prefix) = self.pending_prefix.take() {
            self.current.extend(prefix);
        }
    }

    fn push_span(&mut self, span: Span<'static>) {
        if let Some(cell) = self
            .table
            .as_mut()
            .and_then(|table| table.current_cell.as_mut())
        {
            cell.push(span);
            return;
        }
        self.ensure_prefix();
        self.current.push(span);
    }

    fn flush_current(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let line = Line::from(std::mem::take(&mut self.current));
        self.push_rendered_line(line);
        self.pending_prefix = None;
    }

    fn push_rendered_line(&mut self, line: Line<'static>) {
        for wrapped in wrap_line_preserving_spans(line, self.width) {
            self.lines.push(wrapped);
        }
    }

    fn push_blank(&mut self) {
        if !self.last_line_is_blank() {
            self.lines.push(Line::from(""));
        }
    }

    fn last_line_is_blank(&self) -> bool {
        self.lines
            .last()
            .map(|line| line_plain_text(line).trim().is_empty())
            .unwrap_or(false)
    }

    fn next_list_marker(&mut self) -> Vec<Span<'static>> {
        let depth = self.list_stack.len().saturating_sub(1);
        let indent = "  ".repeat(depth);
        let Some(list) = self.list_stack.last_mut() else {
            return vec![
                Span::raw(indent),
                Span::styled("- ".to_string(), markdown_marker()),
            ];
        };
        match &mut list.next {
            Some(next) => {
                let marker = format!("{next}. ");
                *next += 1;
                vec![Span::raw(indent), Span::styled(marker, markdown_marker())]
            }
            None => vec![
                Span::raw(indent),
                Span::styled("- ".to_string(), markdown_marker()),
            ],
        }
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(style);
    }

    fn pop_style(&mut self) {
        self.style_stack.pop();
    }

    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or_else(text_style)
    }

    fn in_table_cell(&self) -> bool {
        self.table
            .as_ref()
            .is_some_and(|table| table.current_cell.is_some())
    }

    fn flush_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let rendered = render_table_lines(table, self.width);
        for line in rendered {
            self.lines.push(line);
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_current();
        if self.lines.is_empty() {
            return vec![Line::from("")];
        }
        while self.last_line_is_blank() && self.lines.len() > 1 {
            self.lines.pop();
        }
        self.lines
    }
}

fn wrap_line_preserving_spans(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let plain = line_plain_text(&line);
    if plain.trim().is_empty() || display_width(&plain) <= width {
        return vec![line];
    }

    let continuation_indent = continuation_indent_width(&plain);
    let mut out = Vec::new();
    let mut builder = StyledLineBuilder::new(width);
    let mut pending_space = false;

    for span in line.spans {
        let style = span.style;
        for token in styled_tokens(&span.content) {
            match token {
                Token::Space(text) => {
                    if builder.is_empty() && out.is_empty() {
                        builder.push_text(text, style);
                    } else {
                        pending_space = true;
                    }
                }
                Token::Word(text) => {
                    let needed_space = usize::from(pending_space && !builder.is_empty());
                    let word_width = display_width(text);
                    if !builder.is_empty() && builder.width + needed_space + word_width > width {
                        out.push(builder.finish());
                        builder = StyledLineBuilder::with_indent(width, continuation_indent);
                    } else if pending_space && !builder.is_empty() {
                        builder.push_text(" ", style);
                    }
                    push_word(&mut out, &mut builder, text, style, continuation_indent);
                    pending_space = false;
                }
            }
        }
    }

    if !builder.is_empty() || out.is_empty() {
        out.push(builder.finish());
    }
    out
}

fn push_word(
    out: &mut Vec<Line<'static>>,
    builder: &mut StyledLineBuilder,
    word: &str,
    style: Style,
    continuation_indent: usize,
) {
    if display_width(word) <= builder.available_width() {
        builder.push_text(word, style);
        return;
    }

    for ch in word.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if !builder.is_empty() && builder.width + ch_width > builder.max_width {
            out.push(builder.finish());
            *builder = StyledLineBuilder::with_indent(builder.max_width, continuation_indent);
        }
        builder.push_text(&ch.to_string(), style);
    }
}

#[derive(Clone, Copy)]
enum Token<'a> {
    Space(&'a str),
    Word(&'a str),
}

fn styled_tokens(text: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut current_is_space: Option<bool> = None;

    for (idx, ch) in text.char_indices() {
        let is_space = ch.is_whitespace();
        match current_is_space {
            None => current_is_space = Some(is_space),
            Some(kind) if kind == is_space => {}
            Some(kind) => {
                tokens.push(if kind {
                    Token::Space(&text[start..idx])
                } else {
                    Token::Word(&text[start..idx])
                });
                start = idx;
                current_is_space = Some(is_space);
            }
        }
    }

    if let Some(kind) = current_is_space {
        tokens.push(if kind {
            Token::Space(&text[start..])
        } else {
            Token::Word(&text[start..])
        });
    }

    tokens
}

struct StyledLineBuilder {
    spans: Vec<Span<'static>>,
    width: usize,
    max_width: usize,
}

impl StyledLineBuilder {
    fn new(max_width: usize) -> Self {
        Self {
            spans: Vec::new(),
            width: 0,
            max_width,
        }
    }

    fn with_indent(max_width: usize, indent: usize) -> Self {
        let mut builder = Self::new(max_width);
        if indent > 0 {
            builder.push_text(
                &" ".repeat(indent.min(max_width.saturating_sub(1))),
                text_style(),
            );
        }
        builder
    }

    fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    fn available_width(&self) -> usize {
        self.max_width.saturating_sub(self.width)
    }

    fn push_text(&mut self, text: &str, style: Style) {
        self.width += display_width(text);
        self.spans.push(Span::styled(text.to_string(), style));
    }

    fn finish(&mut self) -> Line<'static> {
        self.width = 0;
        Line::from(std::mem::take(&mut self.spans))
    }
}

fn continuation_indent_width(text: &str) -> usize {
    let leading = text.chars().take_while(|ch| ch.is_whitespace()).count();
    let trimmed = text.trim_start();
    let quote_prefix_width = if let Some(rest) = trimmed.strip_prefix("| ") {
        let nested = continuation_indent_width(rest);
        return leading + 2 + nested;
    } else {
        0
    };
    if let Some(rest) = trimmed.strip_prefix("- ") {
        return leading + quote_prefix_width + trimmed.len() - rest.len();
    }
    let marker_len = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if marker_len > 0 && trimmed.chars().nth(marker_len) == Some('.') {
        return leading + quote_prefix_width + marker_len + 2;
    }
    leading + quote_prefix_width
}

fn line_plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn render_table_lines(table: TableState, width: usize) -> Vec<Line<'static>> {
    let column_count = table
        .rows
        .iter()
        .map(|row| row.cells.len())
        .max()
        .unwrap_or(0)
        .max(table.alignments.len());
    if column_count == 0 {
        return Vec::new();
    }

    let column_widths = table_column_widths(&table.rows, column_count, width);
    let mut out = Vec::new();
    let mut rendered_header_separator = false;

    for (idx, row) in table.rows.iter().enumerate() {
        if !row.is_header && idx > 0 && !rendered_header_separator {
            out.push(table_separator_line(&column_widths));
            rendered_header_separator = true;
        }
        out.push(table_row_line(
            row,
            &table.alignments,
            &column_widths,
            row.is_header,
        ));
    }

    out
}

fn table_column_widths(rows: &[TableRow], column_count: usize, max_width: usize) -> Vec<usize> {
    let gap_width = column_count.saturating_sub(1) * 2;
    let available = max_width.saturating_sub(gap_width).max(column_count);
    let mut widths = vec![3; column_count];

    for row in rows {
        for (idx, cell) in row.cells.iter().enumerate().take(column_count) {
            widths[idx] = widths[idx].max(cell_width(cell));
        }
    }

    while widths.iter().sum::<usize>() > available {
        let Some((idx, width)) = widths
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, width)| *width > 3)
            .max_by_key(|(_, width)| *width)
        else {
            break;
        };
        widths[idx] = width.saturating_sub(1);
    }

    widths
}

fn table_row_line(
    row: &TableRow,
    alignments: &[Alignment],
    column_widths: &[usize],
    is_header: bool,
) -> Line<'static> {
    let mut spans = Vec::new();
    for (idx, width) in column_widths.iter().copied().enumerate() {
        if idx > 0 {
            spans.push(Span::raw("  "));
        }
        let cell = row.cells.get(idx).map(Vec::as_slice).unwrap_or(&[]);
        spans.extend(aligned_table_cell(
            cell,
            width,
            alignments.get(idx).copied().unwrap_or(Alignment::None),
            is_header,
        ));
    }
    Line::from(spans)
}

fn table_separator_line(column_widths: &[usize]) -> Line<'static> {
    let mut spans = Vec::new();
    for (idx, width) in column_widths.iter().copied().enumerate() {
        if idx > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled("-".repeat(width), muted()));
    }
    Line::from(spans)
}

fn aligned_table_cell(
    cell: &[Span<'static>],
    width: usize,
    alignment: Alignment,
    is_header: bool,
) -> Vec<Span<'static>> {
    let content = truncate_table_cell(cell, width, is_header);
    let content_width = spans_width(&content);
    let padding = width.saturating_sub(content_width);
    let (left, right) = match alignment {
        Alignment::Right => (padding, 0),
        Alignment::Center => (padding / 2, padding - (padding / 2)),
        Alignment::None | Alignment::Left => (0, padding),
    };

    let mut spans = Vec::new();
    if left > 0 {
        spans.push(Span::raw(" ".repeat(left)));
    }
    spans.extend(content);
    if right > 0 {
        spans.push(Span::raw(" ".repeat(right)));
    }
    spans
}

fn truncate_table_cell(
    cell: &[Span<'static>],
    width: usize,
    is_header: bool,
) -> Vec<Span<'static>> {
    let mut remaining = width;
    let mut out = Vec::new();

    for span in cell {
        if remaining == 0 {
            break;
        }
        let mut text = String::new();
        let mut text_width = 0;
        for ch in span.content.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if text_width + ch_width > remaining {
                break;
            }
            text.push(ch);
            text_width += ch_width;
        }
        if !text.is_empty() {
            let style = if is_header && span.style == text_style() {
                markdown_strong()
            } else {
                span.style
            };
            out.push(Span::styled(text, style));
            remaining = remaining.saturating_sub(text_width);
        }
    }

    out
}

fn cell_width(cell: &[Span<'static>]) -> usize {
    cell.iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

fn heading_text_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3 => markdown_heading(),
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => text_style(),
    }
}

fn looks_like_bare_link(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn looks_like_path(value: &str) -> bool {
    value.starts_with('/') || value.starts_with("~/") || value.starts_with("./")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    fn plain(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(line_plain_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn strips_markdown_markers_but_keeps_readable_text() {
        let lines = render_markdown_lines(
            "This is **important** with `coupon.json`.\n\n- [Example](https://example.com)",
            80,
        );
        let text = plain(&lines);
        assert!(text.contains("This is important with coupon.json."));
        assert!(text.contains("- Example (https://example.com)"));
        assert!(!text.contains("**important**"));
        assert!(!text.contains("`coupon.json`"));
    }

    #[test]
    fn code_fences_do_not_render_language_label_artifacts() {
        let lines = render_markdown_lines("```bash\ncargo test\n```\n", 80);
        let text = plain(&lines);
        assert!(text.contains("cargo test"));
        assert!(!text.contains("code bash"));
        assert!(!text.contains("```"));
    }

    #[test]
    fn headings_render_as_text_not_source_markers() {
        let lines = render_markdown_lines("### What it does\n\nBody", 80);
        let text = plain(&lines);
        assert!(text.contains("What it does"));
        assert!(!text.contains("###"));
    }

    #[test]
    fn blockquotes_render_as_quote_blocks_not_source_markers() {
        let lines = render_markdown_lines(
            "> Quote with **formatting**\n> - quoted list item\n>\n> > nested quote",
            80,
        );
        let text = plain(&lines);
        assert!(text.contains("| Quote with formatting"));
        assert!(text.contains("| - quoted list item"));
        assert!(text.contains("| | nested quote"));
        assert!(!text.contains("> Quote"));
    }

    #[test]
    fn tables_render_as_aligned_rows_not_pipe_source() {
        let lines = render_markdown_lines(
            "| Name | Count |\n| --- | ---: |\n| **Apples** | `12` |\n| Pears | 3 |",
            80,
        );
        let text = plain(&lines);
        assert!(text.contains("Name"), "{text:?}");
        assert!(text.contains("Count"));
        assert!(text.contains("Apples"));
        assert!(text.contains("12"));
        assert!(!text.contains("| Name | Count |"));
        assert!(!text.contains("---:"));
    }

    #[test]
    fn wraps_lists_without_losing_marker_or_span_styles() {
        let lines = render_markdown_lines(
            "- **browser-use-terminal** keeps state while a long sentence wraps cleanly",
            34,
        );
        let text = plain(&lines);
        assert!(text.starts_with("- browser-use-terminal keeps"));
        assert!(text.contains("\n  while a long sentence wraps"));
        let first = &lines[0];
        assert!(first
            .spans
            .iter()
            .any(|span| span.content == "browser-use-terminal"
                && span.style.add_modifier.contains(Modifier::BOLD)));
    }
}
