//! Minimal markdown rendering for the transcript.
//!
//! The TUI is a reading surface: it needs the document structure and emphasis
//! the model writes, not a full CommonMark parser. Anything unrecognized falls
//! back to plain text, so no content is dropped. TS renders through opentui's
//! markdown widget; this is the same reading shape, with its own code-block
//! highlighter.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::highlight::Highlighter;
use crate::theme::Theme;

/// How a table column aligns its cells, from the separator row
/// (`:---`, `:---:`, `---:`; plain `---` is left).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAlign {
    Left,
    Center,
    Right,
}

/// Which row of a table a line is, so the pane can draw the rule itself and
/// offer a hover tip for the rows that carry cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Header,
    Rule,
    Body,
}

/// A table line's structure, kept through projection so the pane can lay the
/// table out at its own width instead of folding a pre-painted row.
#[derive(Clone, PartialEq)]
pub struct TableMeta {
    /// Cells of the header row, used as labels in a hover tip.
    pub header: Vec<String>,
    /// Cells of this row (empty for the rule).
    pub cells: Vec<String>,
    /// The widest cell in each column across the whole table, in display
    /// cells. Every row carries the same list, so each can be laid out on its
    /// own without grouping the table back together.
    pub natural: Vec<usize>,
    pub aligns: Vec<TableAlign>,
    pub kind: TableKind,
    /// The ink the row paints with (header/secondary, rule/border, body/text).
    pub style: Style,
}

/// A markdown line plus the table structure behind it, when it is a table row.
pub struct MarkdownLine {
    pub line: Line<'static>,
    pub table: Option<TableMeta>,
}

impl MarkdownLine {
    fn text(line: Line<'static>) -> Self {
        Self { line, table: None }
    }
}

/// Render a markdown document as styled transcript lines.
pub fn lines(markdown: &str, theme: &Theme) -> Vec<Line<'static>> {
    lines_meta(markdown, theme)
        .into_iter()
        .map(|line| line.line)
        .collect()
}

/// Render a markdown document, keeping a table's structure alongside the
/// pre-painted fallback line. The pane uses the structure to fit the table to
/// the width it actually has; [`lines`] keeps the old fixed rendering for
/// callers that only need the lines.
pub fn lines_meta(markdown: &str, theme: &Theme) -> Vec<MarkdownLine> {
    let palette = theme.palette;
    let markdown = &crate::tool::scrub_output(markdown);
    let mut out = Vec::new();
    let source: Vec<&str> = markdown.lines().collect();
    let mut index = 0;
    while index < source.len() {
        let line = source[index];
        let trimmed = line.trim_start();
        if let Some(fence) = fence_marker(trimmed) {
            index = push_code(&mut out, &source, index, fence, theme);
            continue;
        }
        if trimmed.is_empty() {
            out.push(MarkdownLine::text(Line::default()));
            index += 1;
            continue;
        }
        if let Some(level) = heading_level(trimmed) {
            let style = Style::new()
                .fg(match level {
                    1 => palette.primary,
                    2 => palette.secondary,
                    _ => palette.accent,
                })
                .add_modifier(Modifier::BOLD);
            let mut spans = Vec::new();
            parse_inline(heading_text(trimmed, level), style, theme, &mut spans);
            out.push(MarkdownLine::text(Line::from(spans)));
            index += 1;
            continue;
        }
        if is_rule(trimmed) {
            out.push(MarkdownLine::text(Line::from(Span::styled(
                "─".repeat(40),
                Style::new().fg(palette.border),
            ))));
            index += 1;
            continue;
        }
        if let Some(body) = trimmed.strip_prefix('>') {
            let body = body.strip_prefix(' ').unwrap_or(body);
            let mut spans = vec![Span::styled("│ ", Style::new().fg(palette.border))];
            parse_inline(body, Style::new().fg(palette.text_muted), theme, &mut spans);
            out.push(MarkdownLine::text(Line::from(spans)));
            index += 1;
            continue;
        }
        if let Some((indent, marker, body)) = list_item(line) {
            let mut spans = vec![
                Span::styled(indent.to_string(), Style::new()),
                Span::styled(marker, Style::new().fg(palette.primary)),
            ];
            parse_inline(body, Style::new().fg(palette.text), theme, &mut spans);
            out.push(MarkdownLine::text(Line::from(spans)));
            index += 1;
            continue;
        }
        if line.contains('|') && index + 1 < source.len() && is_table_separator(source[index + 1]) {
            index = push_table(&mut out, &source, index, theme);
            continue;
        }
        let mut spans = Vec::new();
        parse_inline(line, Style::new().fg(palette.text), theme, &mut spans);
        out.push(MarkdownLine::text(Line::from(spans)));
        index += 1;
    }
    out
}

fn fence_marker(line: &str) -> Option<char> {
    let fence = line.chars().next()?;
    if !matches!(fence, '`' | '~') {
        return None;
    }
    if line.chars().take_while(|c| *c == fence).count() < 3 {
        return None;
    }
    Some(fence)
}

fn push_code(
    out: &mut Vec<MarkdownLine>,
    source: &[&str],
    start: usize,
    fence: char,
    theme: &Theme,
) -> usize {
    let palette = theme.palette;
    let info = source[start]
        .trim_start()
        .trim_start_matches(fence)
        .trim()
        .to_string();
    out.push(MarkdownLine::text(Line::from(Span::styled(
        if info.is_empty() {
            "  ╭".to_string()
        } else {
            format!("  ╭ {info}")
        },
        Style::new().fg(palette.text_muted),
    ))));
    let closing = fence.to_string().repeat(3);
    let mut highlighter = Highlighter::new(&info, palette);
    let mut index = start + 1;
    while index < source.len() {
        let line = source[index];
        if line.trim_start().starts_with(&closing) {
            out.push(MarkdownLine::text(Line::from(Span::styled(
                "  ╰".to_string(),
                Style::new().fg(palette.text_muted),
            ))));
            return index + 1;
        }
        let mut spans = vec![Span::styled("  │ ", Style::new().fg(palette.border))];
        spans.extend(highlighter.line(line));
        out.push(MarkdownLine::text(Line::from(spans)));
        index += 1;
    }
    // An unterminated fence is a half-written document, not an error; show its
    // body rather than swallowing the tail of the message.
    out.push(MarkdownLine::text(Line::from(Span::styled(
        "  ╰".to_string(),
        Style::new().fg(palette.text_muted),
    ))));
    index
}

fn heading_level(line: &str) -> Option<usize> {
    let level = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    if line.chars().nth(level) != Some(' ') {
        return None;
    }
    Some(level)
}

fn heading_text(line: &str, level: usize) -> &str {
    line[level..].trim().trim_end_matches('#').trim_end()
}

fn is_rule(line: &str) -> bool {
    let mut chars = line.chars().filter(|c| !c.is_whitespace());
    let Some(first) = chars.next() else {
        return false;
    };
    if !matches!(first, '-' | '*' | '_') {
        return false;
    }
    let mut count = 1;
    for c in chars {
        if c != first {
            return false;
        }
        count += 1;
    }
    count >= 3
}

fn list_item(line: &str) -> Option<(&str, String, &str)> {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_len);
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'-' | b'*' | b'+') && bytes[1] == b' ' {
        return Some((indent, "• ".to_string(), &rest[2..]));
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let after = &rest[digits..];
    let body = after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))?;
    Some((indent, format!("{}. ", &rest[..digits]), body))
}

fn is_table_separator(line: &str) -> bool {
    let cells = split_row(line);
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let trimmed = cell.trim_start_matches(':').trim_end_matches(':');
            !trimmed.is_empty() && trimmed.chars().all(|c| c == '-')
        })
}

fn split_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

fn push_table(out: &mut Vec<MarkdownLine>, source: &[&str], start: usize, theme: &Theme) -> usize {
    let palette = theme.palette;
    let header = split_row(source[start]);
    let aligns = table_aligns(source[start + 1], header.len());
    let mut rows = Vec::new();
    let mut index = start + 2;
    while index < source.len() {
        let line = source[index];
        if line.trim().is_empty() || !line.contains('|') {
            break;
        }
        rows.push(split_row(line));
        index += 1;
    }
    let columns = header
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    let mut natural: Vec<usize> = (0..columns)
        .map(|column| header.get(column).map_or(0, |cell| cell.width()))
        .collect();
    for row in &rows {
        for (column, cell) in row.iter().enumerate() {
            natural[column] = natural[column].max(cell.width());
        }
    }
    // The fallback painting (what [`lines`] returns) still caps a column at 40
    // cells; the pane lays the table out against the width it really has.
    let widths: Vec<usize> = natural.iter().map(|width| (*width).min(40)).collect();
    let header_style = Style::new()
        .fg(palette.secondary)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::new().fg(palette.text);
    let rule_style = Style::new().fg(palette.border);
    let header = padded(&header, columns);
    out.push(MarkdownLine {
        line: table_row_line(&header, &widths, &aligns, header_style),
        table: Some(TableMeta {
            header: header.clone(),
            cells: header.clone(),
            natural: natural.clone(),
            aligns: aligns.clone(),
            kind: TableKind::Header,
            style: header_style,
        }),
    });
    let rule = widths
        .iter()
        .map(|width| "─".repeat(*width))
        .collect::<Vec<_>>()
        .join("─┼─");
    out.push(MarkdownLine {
        line: Line::from(Span::styled(rule, rule_style)),
        table: Some(TableMeta {
            header: header.clone(),
            cells: Vec::new(),
            natural: natural.clone(),
            aligns: aligns.clone(),
            kind: TableKind::Rule,
            style: rule_style,
        }),
    });
    for row in &rows {
        let cells = padded(row, columns);
        out.push(MarkdownLine {
            line: table_row_line(&cells, &widths, &aligns, body_style),
            table: Some(TableMeta {
                header: header.clone(),
                cells: cells.clone(),
                natural: natural.clone(),
                aligns: aligns.clone(),
                kind: TableKind::Body,
                style: body_style,
            }),
        });
    }
    index
}

fn padded(cells: &[String], columns: usize) -> Vec<String> {
    (0..columns)
        .map(|column| cells.get(column).cloned().unwrap_or_default())
        .collect()
}

fn table_aligns(separator: &str, columns: usize) -> Vec<TableAlign> {
    let cells = split_row(separator);
    (0..columns)
        .map(|column| {
            let cell = cells.get(column).map(String::as_str).unwrap_or("").trim();
            match (cell.starts_with(':'), cell.ends_with(':')) {
                (true, true) => TableAlign::Center,
                (false, true) => TableAlign::Right,
                _ => TableAlign::Left,
            }
        })
        .collect()
}

fn table_row_line(
    cells: &[String],
    widths: &[usize],
    aligns: &[TableAlign],
    style: Style,
) -> Line<'static> {
    let mut text = String::new();
    for (column, width) in widths.iter().enumerate() {
        if column > 0 {
            text.push_str(" │ ");
        }
        let cell = cells.get(column).map(String::as_str).unwrap_or("");
        let cell = clip_cell(cell, *width);
        let pad = width.saturating_sub(cell.width());
        let (left, right) = match aligns.get(column).copied().unwrap_or(TableAlign::Left) {
            TableAlign::Left => (0, pad),
            TableAlign::Right => (pad, 0),
            TableAlign::Center => (pad / 2, pad - pad / 2),
        };
        text.push_str(&" ".repeat(left));
        text.push_str(&cell);
        text.push_str(&" ".repeat(right));
    }
    Line::from(Span::styled(text.trim_end().to_string(), style))
}

/// Clip one cell to `width` display cells, marking the loss with `…`.
pub(crate) fn clip_cell(cell: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if cell.width() <= width {
        return cell.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for character in cell.chars() {
        let cells = character.width().unwrap_or(0);
        if used + cells > width.saturating_sub(1) {
            break;
        }
        used += cells;
        out.push(character);
    }
    out.push('…');
    out
}

fn parse_inline(text: &str, style: Style, theme: &Theme, spans: &mut Vec<Span<'static>>) {
    let palette = theme.palette;
    let chars: Vec<char> = text.chars().collect();
    let mut plain = String::new();
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if c == '\\' {
            // A backslash escapes the character that follows it. A line-final
            // one is the markdown hard break, and the reading flow re-wraps
            // paragraphs anyway, so neither may paint a literal backslash.
            index += 1;
            if index < chars.len() {
                plain.push(chars[index]);
                index += 1;
            }
            continue;
        }
        if c == '`' {
            if let Some(end) = find_char(&chars, index + 1, '`') {
                push_plain(spans, &mut plain, style);
                let code: String = chars[index + 1..end].iter().collect();
                spans.push(Span::styled(code, style.fg(palette.accent)));
                index = end + 1;
                continue;
            }
        }
        if let Some((end, delimiter)) = match_delimiter(&chars, index) {
            push_plain(spans, &mut plain, style);
            let inner: String = chars[index + delimiter.len()..end].iter().collect();
            let emphasis = match delimiter {
                "**" | "__" => Modifier::BOLD,
                "~~" => Modifier::CROSSED_OUT,
                _ => Modifier::ITALIC,
            };
            parse_inline(&inner, style.add_modifier(emphasis), theme, spans);
            index = end + delimiter.len();
            continue;
        }
        if c == '[' {
            if let Some((label_end, url_end)) = find_link(&chars, index) {
                push_plain(spans, &mut plain, style);
                let label: String = chars[index + 1..label_end].iter().collect();
                parse_inline(
                    &label,
                    style.fg(palette.accent).add_modifier(Modifier::UNDERLINED),
                    theme,
                    spans,
                );
                index = url_end + 1;
                continue;
            }
        }
        plain.push(c);
        index += 1;
    }
    push_plain(spans, &mut plain, style);
}

fn match_delimiter(chars: &[char], start: usize) -> Option<(usize, &'static str)> {
    for delimiter in ["**", "__", "~~", "*", "_"] {
        let marker: Vec<char> = delimiter.chars().collect();
        if !chars[start..].starts_with(&marker) {
            continue;
        }
        let inner_start = start + marker.len();
        if inner_start >= chars.len() || chars[inner_start].is_whitespace() {
            continue;
        }
        // `_` inside a word is an identifier (`snake_case`), not emphasis.
        if delimiter == "_" && start > 0 && chars[start - 1].is_alphanumeric() {
            continue;
        }
        if let Some(end) = find_sequence(chars, inner_start, &marker) {
            if end > inner_start {
                return Some((end, delimiter));
            }
        }
    }
    None
}

fn find_sequence(chars: &[char], from: usize, sequence: &[char]) -> Option<usize> {
    (from..chars.len().saturating_sub(sequence.len() - 1))
        .find(|&index| chars[index..].starts_with(sequence))
}

fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&index| chars[index] == target)
}

fn find_link(chars: &[char], start: usize) -> Option<(usize, usize)> {
    let close = find_char(chars, start + 1, ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = find_char(chars, close + 2, ')')?;
    Some((close, end))
}

fn push_plain(spans: &mut Vec<Span<'static>>, plain: &mut String, style: Style) {
    if plain.is_empty() {
        return;
    }
    spans.push(Span::styled(std::mem::take(plain), style));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;

    fn theme() -> Theme {
        Theme {
            name: "test".to_string(),
            palette: theme::builtin("miao", crate::theme::ThemeMode::Dark).expect("builtin theme"),
        }
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn control_characters_never_reach_the_render() {
        let painted = lines("\u{1b}[38;2;255;0;0mred\u{7}text\u{7}\nplain", &theme())
            .iter()
            .map(text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(painted.contains("redtext"));
        assert!(painted.contains("plain"));
        assert!(!painted.contains('\u{1b}'));
        assert!(!painted.contains('\u{7}'));
    }

    /// The model writes markdown hard breaks as a line-final backslash. It
    /// marks a break; the reading flow re-wraps paragraphs, so it must never
    /// paint as a literal glyph at the end of a row.
    #[test]
    fn a_hard_break_backslash_does_not_paint() {
        let rendered = lines("thought ends here \\\nnext paragraph", &theme())
            .iter()
            .map(text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.starts_with("thought ends here \nnext paragraph"));
        assert!(!rendered.contains('\\'));
    }

    #[test]
    fn headings_carry_level_colors_and_bold() {
        let lines = lines("# Title\n\n## Section\n\n### Detail", &theme());
        assert_eq!(text(&lines[0]), "Title");
        assert_eq!(lines[0].spans[0].style.fg, Some(theme().palette.primary));
        assert!(lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert_eq!(text(&lines[2]), "Section");
        assert_eq!(lines[2].spans[0].style.fg, Some(theme().palette.secondary));
        assert_eq!(text(&lines[4]), "Detail");
        assert_eq!(lines[4].spans[0].style.fg, Some(theme().palette.accent));
    }

    #[test]
    fn inline_emphasis_and_code_are_spans_of_the_same_line() {
        let lines = lines("a **bold** and `code` and ~~gone~~", &theme());
        let line = &lines[0];
        let joined = text(line);
        let bold = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "bold")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "code")
            .expect("code span");
        assert_eq!(code.style.fg, Some(theme().palette.accent));
        // Inline code used to carry a `surface` background, and a prose line
        // full of identifiers then read as patchy strips of highlight. Claude
        // Code marks inline code by ink alone, so the background is gone and
        // the accent color carries the distinction.
        assert_eq!(code.style.bg, None);
        let gone = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "gone")
            .expect("struck span");
        assert!(gone.style.add_modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(joined, "a bold and code and gone");
    }

    #[test]
    fn snake_case_does_not_become_emphasis() {
        let lines = lines("call read_file_then_write_file now", &theme());
        assert_eq!(text(&lines[0]), "call read_file_then_write_file now");
        assert_eq!(lines[0].spans.len(), 1);
    }

    #[test]
    fn a_link_keeps_its_label_and_drops_the_url() {
        let lines = lines("see [the guide](https://example.com/x) now", &theme());
        let line = &lines[0];
        let label = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "the guide")
            .expect("label span");
        assert_eq!(label.style.fg, Some(theme().palette.accent));
        assert!(!text(line).contains("example.com"));
    }

    #[test]
    fn lists_keep_indentation_and_replace_the_bullet() {
        let lines = lines("  - one\n1. first", &theme());
        assert_eq!(text(&lines[0]), "  • one");
        assert_eq!(text(&lines[1]), "1. first");
    }

    #[test]
    fn a_fence_keeps_every_body_line_verbatim() {
        let lines = lines(
            "before\n```rust\nfn main() {}\n  *not emphasis*\n```\nafter",
            &theme(),
        );
        let body = lines.iter().map(text).collect::<Vec<_>>();
        assert!(body.contains(&"  │ fn main() {}".to_string()));
        assert!(body.contains(&"  │   *not emphasis*".to_string()));
        assert_eq!(lines[0].spans[0].content.as_ref(), "before");
        assert_eq!(body.last().map(String::as_str), Some("after"));
    }

    #[test]
    fn a_fence_is_highlighted_by_its_language() {
        let lines = lines("```rust\nlet x = 1; // note\n```", &theme());
        let body = &lines[1];
        assert_eq!(text(body), "  │ let x = 1; // note");
        let colors: Vec<_> = body.spans.iter().map(|span| span.style.fg).collect();
        let syntax = theme().palette.syntax;
        assert!(colors.contains(&Some(syntax.keyword)), "{colors:?}");
        assert!(colors.contains(&Some(syntax.number)), "{colors:?}");
    }

    #[test]
    fn a_fence_without_a_language_stays_plain() {
        let lines = lines("```\nlet x = 1; // note\n```", &theme());
        let body = &lines[1];
        assert_eq!(text(body), "  │ let x = 1; // note");
        assert_eq!(body.spans.len(), 2);
        assert_eq!(body.spans[1].style.fg, Some(theme().palette.text));
    }

    #[test]
    fn a_quote_and_a_rule_get_their_own_shape() {
        let lines = lines("> quoted\n---", &theme());
        assert_eq!(text(&lines[0]), "│ quoted");
        assert_eq!(text(&lines[1]), "─".repeat(40));
    }

    #[test]
    fn a_table_is_aligned_by_column_widths() {
        let lines = lines(
            "| name | value |\n| --- | --- |\n| a | 1 |\n| longer | 22 |",
            &theme(),
        );
        let rendered: Vec<String> = lines.iter().map(text).collect();
        assert!(rendered[0].starts_with("name"));
        assert!(rendered[0].contains("value"));
        assert!(rendered[1].contains('┼'));
        assert!(rendered[2].starts_with("a "));
        assert!(rendered[3].starts_with("longer"));
    }

    /// The pane fits the table to the width it has, so the parse keeps the
    /// whole table's natural widths and each row's cells alongside the
    /// fallback painting. Without that structure the pane can only wrap the
    /// pre-painted line, which is what tore the grid apart.
    #[test]
    fn a_table_keeps_the_structure_the_pane_lays_out() {
        let parsed = lines_meta(
            "| name | value |\n| :--- | ---: |\n| a | 1 |\n| longer | 22 |",
            &theme(),
        );
        assert_eq!(parsed.len(), 4);
        let header = parsed[0].table.as_ref().expect("header structure");
        assert_eq!(header.kind, TableKind::Header);
        assert_eq!(header.header, vec!["name", "value"]);
        assert_eq!(header.natural, vec![6, 5]);
        assert_eq!(header.aligns, vec![TableAlign::Left, TableAlign::Right]);
        assert_eq!(
            parsed[1].table.as_ref().map(|row| row.kind),
            Some(TableKind::Rule)
        );
        let body = parsed[3].table.as_ref().expect("body structure");
        assert_eq!(body.kind, TableKind::Body);
        assert_eq!(body.cells, vec!["longer", "22"]);
        assert_eq!(body.header, vec!["name", "value"]);
    }

    /// A cell wider than the fallback's 40-cell cap is clipped with a visible
    /// marker instead of running past its column.
    #[test]
    fn a_capped_table_cell_is_marked_as_clipped() {
        let long = "x".repeat(60);
        let rendered = lines(&format!("| a |\n| --- |\n| {long} |"), &theme());
        let row = text(&rendered[2]);
        assert!(row.ends_with('…'), "{row:?}");
        assert!(
            unicode_width::UnicodeWidthStr::width(row.trim_end()) <= 40,
            "{row:?}"
        );
    }

    #[test]
    fn an_unterminated_fence_still_renders_its_body() {
        let lines = lines("```\nstill writing", &theme());
        let rendered: Vec<String> = lines.iter().map(text).collect();
        assert!(rendered.contains(&"  │ still writing".to_string()));
        assert_eq!(rendered.last().map(String::as_str), Some("  ╰"));
    }

    /// The renderer is deliberately not a CommonMark parser, so the fallback
    /// path is the contract: an HTML block, a template expression, an
    /// autolink and a tabbed line must each survive character for character.
    #[test]
    fn unknown_content_passes_through_without_loss() {
        let document = "<details open>\n{{template.render(x)}}\n<https://example.com/auto>\n宽字符与\t制表符保留";
        let painted: Vec<String> = lines(document, &theme()).iter().map(text).collect();
        assert_eq!(
            painted,
            vec![
                "<details open>".to_string(),
                "{{template.render(x)}}".to_string(),
                "<https://example.com/auto>".to_string(),
                "宽字符与\t制表符保留".to_string(),
            ]
        );
    }
}
