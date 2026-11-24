//! Tool rows for the transcript.
//!
//! The native engine names its tools (`read_file`, `run_command`, …) and its
//! `tool_result` content is the JSON-serialized result. The TS TUI never sees
//! that shape: `packages/miao/src/engine/bridge.ts` projects it onto the product
//! vocabulary first. A Rust client reads the wire directly, so the render-side
//! half of that projection lives here; without it every tool row would show raw
//! JSON.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::highlight::Highlighter;
use crate::session::{DisplayFile, ToolPart};
use crate::theme::Theme;

/// Output lines one command row shows before collapsing.
///
/// Three rows: enough for a command's real answer to be readable without
/// opening anything, few enough that a wall of output still folds. A mature
/// terminal agent keeps three; the row budget is a preview, not a transcript.
const TOOL_PREVIEW_LINES: usize = 3;
/// Diff lines one changed file shows before collapsing.
const DIFF_PREVIEW_LINES: usize = 12;
/// Characters one terminal read lays out per row.
const PAINT_ROW_CHARS: usize = 120;
/// Display cells one folded preview row keeps before clipping.
///
/// A tool result is not always many lines: a sqlite dump or a minified JSON
/// envelope is one line of tens of kilobytes. The line budget then folds
/// nothing, and the terminal's own wrapping spread that single row over the
/// whole viewport. Folding clips each kept row to a couple of normal-width
/// rows instead, and the expansion hint names what is out of view.
const FOLDED_ROW_CELLS: usize = 160;

/// How much of a tool result to paint, and the chord that changes it.
///
/// Folding is one decision, so the budget and the hint travel together: every
/// renderer used to recompute the same limit from `expanded`, and passing the
/// chord beside it on its own pushed three of them past clippy's argument
/// budget for no gain in clarity.
#[derive(Clone, Copy)]
struct Fold<'a> {
    expanded: bool,
    /// The chord the operator presses to unfold, as the keymap spells it.
    toggle: Option<&'a str>,
}

impl Fold<'_> {
    /// Rows a preview may paint out of `budget`, unlimited when expanded.
    fn limit(self, budget: usize) -> usize {
        match self.expanded {
            true => usize::MAX,
            false => budget,
        }
    }
}

pub fn lines(
    tool: &ToolPart,
    files: Option<&[DisplayFile]>,
    theme: &Theme,
    expanded: bool,
    toggle: Option<&str>,
) -> Vec<Line<'static>> {
    let name = product_name(&tool.name);
    let input = product_input(&tool.name, &tool.input);
    let raw = tool.output.as_deref();
    let result = raw.and_then(parse_json);
    let mut out = Vec::new();
    let fold = Fold { expanded, toggle };
    match name {
        "bash" => bash(&mut out, tool, &input, result.as_ref(), raw, theme, fold),
        "read" => read(&mut out, tool, &input, result.as_ref(), theme, fold),
        "glob" | "grep" => search(&mut out, tool, name, &input, result.as_ref(), theme, fold),
        "write" | "edit" | "apply_patch" => {
            file(&mut out, tool, name, &input, files, raw, theme, fold)
        }
        "todowrite" => todos(&mut out, tool, result.as_ref(), raw, theme, fold),
        "question" => question(&mut out, tool, &input, result.as_ref(), theme),
        "webfetch" => inline(&mut out, tool, "WebFetch", &text(&input, "url"), theme),
        "websearch" => inline(&mut out, tool, "WebSearch", &text(&input, "query"), theme),
        "skill" => inline(&mut out, tool, "Skill", &text(&input, "name"), theme),
        "task" => task(&mut out, tool, &input, raw, theme, fold),
        "task_list" => tasks(&mut out, tool, result.as_ref(), theme, fold),
        "task_result" => task_report(&mut out, tool, result.as_ref(), raw, theme, fold),
        "session_state" | "goal" => session_state(&mut out, tool, result.as_ref(), theme, fold),
        "schedule" => schedules(&mut out, tool, result.as_ref(), theme, fold),
        "job_list" | "job_wait" | "job_cancel" => {
            jobs(&mut out, tool, result.as_ref(), theme, fold)
        }
        "worktree_list" => worktree_rows(&mut out, tool, result.as_ref(), theme, fold),
        "send_message" => receipt(&mut out, tool, &input, raw, theme),
        "list_sessions" => sessions(&mut out, tool, result.as_ref(), theme, fold),
        "read_session_context" => out.push(header(
            tool,
            "read session",
            &compact(&text(&input, "to")),
            theme,
        )),
        "terminal_start" => terminal_started(&mut out, tool, &input, result.as_ref(), theme, fold),
        "terminal_write" | "terminal_resize" => {
            terminal_control(&mut out, tool, name, &input, theme, fold)
        }
        "terminal_read" => terminal_read(&mut out, tool, &input, result.as_ref(), theme, fold),
        "terminal_stop" => terminal_stopped(&mut out, tool, &input, result.as_ref(), theme, fold),
        "terminal_list" => terminal_list(&mut out, tool, result.as_ref(), theme, fold),
        _ => generic(
            &mut out,
            tool,
            name,
            &input,
            result.as_ref(),
            raw,
            theme,
            fold,
        ),
    }
    out
}

/// Engine tool name to the product vocabulary the TS renderers switch on.
pub fn product_name(engine: &str) -> &str {
    match engine {
        "read_file" | "read_image" | "list_files" => "read",
        "load_skill" => "skill",
        "write_file" => "write",
        "edit_file" => "edit",
        "run_command" | "start_job" => "bash",
        "job_status" | "job_list" => "job_list",
        "cancel_job" => "job_cancel",
        "job_wait" => "job_wait",
        "schedule_wakeup" | "cancel_wakeup" => "schedule",
        other if other.starts_with("cron_") => "schedule",
        other if other.starts_with("lsp_") => "lsp",
        other => other,
    }
}

/// Engine input to the field names the renderers read.
pub fn product_input(engine: &str, input: &Value) -> Value {
    let Some(record) = input.as_object() else {
        return input.clone();
    };
    let mut projected = record.clone();
    match engine {
        "read_file" | "read_image" | "list_files" | "write_file" | "edit_file" => {
            if !record.contains_key("filePath") {
                if let Some(path) = record.get("path").and_then(Value::as_str) {
                    projected.insert("filePath".into(), Value::String(path.to_string()));
                }
            }
            if engine == "write_file" && !record.contains_key("content") {
                if let Some(body) = record.get("text") {
                    projected.insert("content".into(), body.clone());
                }
            }
        }
        "run_command" | "bash" | "start_job" => {
            if !record.contains_key("command") {
                if let Some(argv) = record.get("argv").and_then(Value::as_array) {
                    let command = argv
                        .iter()
                        .filter_map(Value::as_str)
                        .map(quote_argument)
                        .collect::<Vec<_>>()
                        .join(" ");
                    projected.insert("command".into(), Value::String(command));
                }
            }
            if engine == "start_job" {
                projected.insert("run_in_background".into(), Value::Bool(true));
            }
        }
        "apply_patch" => {
            // A patch names its targets inside the payload, not in a field the
            // row can print: without this the header read as a bare
            // `apply_patch` with no subject at all.
            let patch = record
                .get("patch")
                .or_else(|| record.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let targets: Vec<&str> = patch
                .lines()
                .filter_map(|line| line.trim().strip_prefix("***"))
                .filter_map(|rest| rest.rsplit_once(':'))
                .map(|(_, path)| path.trim())
                .filter(|path| !path.is_empty())
                .collect();
            if let Some(subject) = match targets.as_slice() {
                [] => None,
                [only] => Some((*only).to_string()),
                [first, ..] => Some(format!("{first} +{} more", targets.len() - 1)),
            } {
                projected.insert("filePath".into(), Value::String(subject));
            }
        }
        "task" => {
            if let Some(agent) = record.get("agent") {
                projected
                    .entry("subagent_type")
                    .or_insert_with(|| agent.clone());
            }
            if !record.contains_key("description") {
                if let Some(line) = record
                    .get("prompt")
                    .and_then(Value::as_str)
                    .and_then(|prompt| prompt.lines().find(|line| !line.trim().is_empty()))
                {
                    projected.insert(
                        "description".into(),
                        Value::String(compact(line.trim()).chars().take(120).collect()),
                    );
                }
            }
        }
        _ => {}
    }
    Value::Object(projected)
}

#[cfg(test)]
mod patch_subject_tests {
    use super::product_input;
    use serde_json::json;

    /// A patch names its targets inside the payload. Without this projection
    /// the row read as a bare `apply_patch` with no subject, and a transcript
    /// of several calls showed the operator nothing at all.
    #[test]
    fn a_patch_row_names_what_it_touches() {
        let one = product_input(
            "apply_patch",
            &json!({
                "patch": "*** Begin Patch\n*** Update File: crates/miao-tui/src/app.rs\n@@\n-a\n+b\n*** End Patch"
            }),
        );
        assert_eq!(one["filePath"], json!("crates/miao-tui/src/app.rs"));

        let many = product_input(
            "apply_patch",
            &json!({
                "patch": "*** Begin Patch\n*** Add File: a.rs\n*** Delete File: b.rs\n*** Update File: c.rs\n*** End Patch"
            }),
        );
        assert_eq!(many["filePath"], json!("a.rs +2 more"));

        // A payload with no target marker leaves the row as it was rather than
        // printing an empty subject.
        let none = product_input("apply_patch", &json!({ "patch": "not a patch" }));
        assert!(none.get("filePath").is_none());
    }
}

fn quote_argument(argument: &str) -> String {
    if !argument.is_empty()
        && argument.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | ':' | '=' | '+' | '-')
        })
    {
        return argument.to_string();
    }
    format!("'{}'", argument.replace('\'', "'\\''"))
}

fn parse_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

fn header(tool: &ToolPart, label: &str, detail: &str, theme: &Theme) -> Line<'static> {
    header_with(tool, label, detail, None, theme)
}

fn header_with(
    tool: &ToolPart,
    label: &str,
    detail: &str,
    summary: Option<&str>,
    theme: &Theme,
) -> Line<'static> {
    let palette = theme.palette;
    // Each state gets its own glyph as well as its own colour: a finished call
    // reads as a tick and a failed one as a cross even in a monochrome theme,
    // where both dots were the same shape and only the colour told them apart.
    let (glyph, glyph_style) = if tool.failed {
        ("✗", Style::new().fg(palette.error))
    } else if tool.output.is_some() {
        ("✓", Style::new().fg(palette.success))
    } else if tool.unreturned {
        // The turn ended without this call's result: nothing is in flight, so
        // the row must not keep the running glyph.
        ("–", Style::new().fg(palette.text_muted))
    } else {
        ("·", Style::new().fg(palette.warning))
    };
    // The label stays plain lowercase text on the page ground. A filled badge
    // turned every call into a coloured block that competed with the answer
    // around it, so the call is one line again: the name in the secondary
    // colour, its argument muted after two spaces.
    let mut spans = vec![
        Span::styled(format!("{glyph} "), glyph_style),
        Span::styled(label.to_string(), Style::new().fg(palette.secondary)),
    ];
    if !detail.is_empty() {
        spans.push(Span::styled(
            format!("  {detail}"),
            Style::new().fg(palette.text_muted),
        ));
    }
    if let Some(summary) = summary.filter(|summary| !summary.is_empty()) {
        spans.push(Span::styled(
            format!(" {summary}"),
            Style::new().fg(palette.text_muted),
        ));
    }
    Line::from(spans)
}

fn result_line(text: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(format!("  ⎿ {text}"), Style::new().fg(color)))
}

fn preview(
    out: &mut Vec<Line<'static>>,
    text: &str,
    color: Color,
    limit: usize,
    toggle: Option<&str>,
) {
    let text = scrub_output(text);
    let text = text.trim_end_matches('\n');
    if text.is_empty() {
        return;
    }
    let lines = text.lines().collect::<Vec<_>>();
    // `Fold::limit` answers `usize::MAX` when the block is open, so the budget
    // also says whether this preview is folded. An open block keeps every row
    // whole; a folded one clips each kept row, because a single 20 KB line is
    // one line to the budget and a screenful to the terminal once it wraps.
    let folded = limit != usize::MAX;
    let kept = lines.len().min(limit);
    let mut clipped = false;
    for line in &lines[..kept] {
        let (row, cut) = match folded {
            true => clip_entry(line),
            false => ((*line).to_string(), false),
        };
        clipped |= cut;
        out.push(Line::from(Span::styled(
            format!("  │ {row}"),
            Style::new().fg(color),
        )));
    }
    let hidden = lines.len() - kept;
    if hidden == 0 && !clipped {
        return;
    }
    // Say how to see the rest: a folded tool block that cannot be opened
    // reads like lost output. The chord is the one the keymap actually
    // binds, so a remap or an unbound action cannot leave the line
    // promising a key that does nothing.
    let mut notes = Vec::new();
    if hidden > 0 {
        notes.push(format!("+{hidden} lines"));
    }
    if clipped {
        notes.push("long lines clipped".to_string());
    }
    let detail = notes.join(" · ");
    let line = match toggle {
        Some(chord) => format!("  │ … {detail} [{chord} to expand]"),
        None => format!("  │ … {detail}"),
    };
    out.push(Line::from(Span::styled(line, Style::new().fg(color))));
}

/// One folded row, cut to [`FOLDED_ROW_CELLS`] display cells.
fn clip_entry(line: &str) -> (String, bool) {
    clip_cells(line, FOLDED_ROW_CELLS)
}

/// Cut `text` to at most `cells` display cells, ending in `…` when it was too
/// wide.
///
/// The cut is by display width, not characters, so a CJK or emoji blob is not
/// clipped early by counting bytes, and it ends in `…` so the row reads as
/// continuing rather than as a truncated value.
fn clip_cells(text: &str, cells: usize) -> (String, bool) {
    if text.width() <= cells {
        return (text.to_string(), false);
    }
    let mut used = 0;
    let mut cut = String::new();
    for character in text.chars() {
        let width = character.width().unwrap_or(0);
        if used + width > cells.saturating_sub(1) {
            break;
        }
        used += width;
        cut.push(character);
    }
    cut.push('…');
    (cut, true)
}

/// A coordination message reads as its receipt: who it went to and whether that
/// peer was asked to act. The engine answers with exactly that, so the row shows
/// the answer instead of raw JSON, and the reason stays visible under it.
fn receipt(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    input: &Value,
    raw: Option<&str>,
    theme: &Theme,
) {
    let palette = theme.palette;
    let to = text(input, "to");
    let state = match raw
        .and_then(parse_json)
        .as_ref()
        .and_then(|answer| answer.get("wake"))
        .and_then(Value::as_str)
    {
        Some("requested") => " · wake requested",
        Some(_) => " · left for later",
        None => "",
    };
    out.push(header(
        tool,
        "message",
        &compact(&format!("→ {to}{state}")),
        theme,
    ));
    if let Some(reason) = input.get("reason").and_then(Value::as_str) {
        out.push(result_line(reason, palette.text_muted));
    }
}

fn inline(out: &mut Vec<Line<'static>>, tool: &ToolPart, label: &str, detail: &str, theme: &Theme) {
    out.push(header(tool, label, &compact(detail), theme));
}

/// Peer discovery as one line per Session. The raw result is a catalog of
/// every live Session in the Location — printing it verbatim buried the
/// transcript under a screen of JSON on sessions that coordinate with peers.
fn sessions(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    result: Option<&Value>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let palette = theme.palette;
    let rows = result
        .and_then(|value| value.get("sessions"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    // The engine explains an empty answer (`note`): peers are limited to this
    // workspace, so "none here" must not read as "your history is gone".
    let note = result
        .and_then(|value| value.get("note"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let summary = match rows {
        [] if !note.is_empty() => compact(note),
        [] => "no peer sessions".to_string(),
        [only] => peer_line(only),
        rows => format!("{} sessions: {}", rows.len(), peer_line(&rows[0])),
    };
    out.push(header(tool, "sessions", &summary, theme));
    let limit = fold.limit(TOOL_PREVIEW_LINES);
    let body = rows.iter().map(peer_line).collect::<Vec<_>>().join("\n");
    if !body.is_empty() {
        preview(out, &body, palette.text_muted, limit, fold.toggle);
    }
}

fn peer_line(row: &Value) -> String {
    let title = row
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .unwrap_or("untitled");
    let task = row.get("task").and_then(Value::as_str).unwrap_or("unknown");
    let objective = row
        .get("objective")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if objective.is_empty() {
        format!("{title} · {task}")
    } else {
        format!("{title} · {task} · {objective}")
    }
}

/// Session-owned interactive terminals read as their id and state. The read
/// payload carries raw PTY text with escape sequences, so the row paints the
/// decoded output instead of the JSON envelope that delivered it — that
/// envelope dumped verbatim buried the transcript under a screen of escapes.
fn terminal_started(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    input: &Value,
    result: Option<&Value>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let limit = fold.limit(TOOL_PREVIEW_LINES);
    let palette = theme.palette;
    let argv = input
        .get("argv")
        .and_then(Value::as_array)
        .map(|argv| {
            argv.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    out.push(header(tool, "terminal", &compact(&argv), theme));
    if tool.failed {
        if let Some(output) = tool.output.as_deref() {
            preview(out, output, palette.error, limit, fold.toggle);
        }
        return;
    }
    let Some(record) = result else {
        return;
    };
    out.push(result_line(
        &format!(
            "{} · {}×{}",
            text(record, "id"),
            record.get("rows").and_then(Value::as_u64).unwrap_or(0),
            record.get("cols").and_then(Value::as_u64).unwrap_or(0)
        ),
        palette.text_muted,
    ));
}

fn terminal_control(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    name: &str,
    input: &Value,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let limit = fold.limit(TOOL_PREVIEW_LINES);
    let palette = theme.palette;
    let id = compact(&text(input, "id"));
    let detail = match name {
        "terminal_resize" => format!(
            "{id} · {}×{}",
            input.get("rows").and_then(Value::as_u64).unwrap_or(0),
            input.get("cols").and_then(Value::as_u64).unwrap_or(0)
        ),
        _ => id,
    };
    out.push(header(
        tool,
        &format!("terminal {}", name.trim_start_matches("terminal_")),
        &detail,
        theme,
    ));
    if tool.failed {
        if let Some(output) = tool.output.as_deref() {
            preview(out, output, palette.error, limit, fold.toggle);
        }
        return;
    }
    out.push(result_line("accepted", palette.text_muted));
}

fn terminal_read(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    input: &Value,
    result: Option<&Value>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let limit = fold.limit(TOOL_PREVIEW_LINES);
    let palette = theme.palette;
    out.push(header(
        tool,
        "terminal read",
        &compact(&text(input, "id")),
        theme,
    ));
    if tool.failed {
        if let Some(output) = tool.output.as_deref() {
            preview(out, output, palette.error, limit, fold.toggle);
        }
        return;
    }
    let Some(record) = result else {
        return;
    };
    let body = record
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut summary = byte_summary(body.len());
    let dropped = record.get("dropped").and_then(Value::as_u64).unwrap_or(0);
    if dropped > 0 {
        summary.push_str(&format!(" · {} dropped", byte_summary(dropped as usize)));
    }
    if record
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        summary.push_str(" · more retained");
    }
    summary.push_str(&format!(" · {}", exit_summary(record.get("exit"))));
    out.push(result_line(&summary, palette.text_muted));
    if !body.is_empty() {
        preview(
            out,
            &paint_rows(&scrub_output(body)),
            palette.text_muted,
            limit,
            fold.toggle,
        );
    }
}

/// A PTY window is laid out as rows before the fold counts them. A full-screen
/// program positions cells with escape sequences and emits no newline at all, so
/// one read arrives as a single line of thousands of characters: a row budget
/// cannot fold what is not a row, and the pane laid the whole window out
/// instead. Scrubbing comes first so a row break cannot land inside an escape
/// sequence and leave its tail as text.
fn paint_rows(text: &str) -> String {
    text.split('\n')
        .flat_map(|line| {
            let chars = line.chars().collect::<Vec<_>>();
            match chars.len() <= PAINT_ROW_CHARS {
                true => vec![line.to_string()],
                false => chars
                    .chunks(PAINT_ROW_CHARS)
                    .map(|chunk| chunk.iter().collect())
                    .collect(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn terminal_stopped(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    input: &Value,
    result: Option<&Value>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let limit = fold.limit(TOOL_PREVIEW_LINES);
    let palette = theme.palette;
    out.push(header(
        tool,
        "terminal stop",
        &compact(&text(input, "id")),
        theme,
    ));
    if tool.failed {
        if let Some(output) = tool.output.as_deref() {
            preview(out, output, palette.error, limit, fold.toggle);
        }
        return;
    }
    let Some(record) = result else {
        return;
    };
    out.push(result_line(&exit_summary(Some(record)), palette.text_muted));
}

fn terminal_list(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    result: Option<&Value>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let limit = fold.limit(TOOL_PREVIEW_LINES);
    let palette = theme.palette;
    out.push(header(tool, "terminal list", "", theme));
    if tool.failed {
        if let Some(output) = tool.output.as_deref() {
            preview(out, output, palette.error, limit, fold.toggle);
        }
        return;
    }
    let body = result
        .and_then(|value| value.get("terminals"))
        .and_then(Value::as_array)
        .map(|terminals| {
            terminals
                .iter()
                .map(|terminal| {
                    format!(
                        "{} · {}",
                        text(terminal, "id"),
                        exit_summary(terminal.get("exit"))
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if body.is_empty() {
        out.push(result_line("no terminals", palette.text_muted));
        return;
    }
    preview(out, &body, palette.text_muted, limit, fold.toggle);
}

fn exit_summary(exit: Option<&Value>) -> String {
    // A live terminal answers `"exit": null` — the key exists but carries no
    // exit yet, and that reads as running, not as a finished call.
    let Some(exit) = exit.filter(|value| !value.is_null()) else {
        return "running".to_string();
    };
    let reason = exit
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("exited");
    if let Some(code) = exit.get("exit_code").and_then(Value::as_i64) {
        return format!("{reason} · exit {code}");
    }
    if let Some(signal) = exit.get("signal").and_then(Value::as_i64) {
        return format!("{reason} · signal {signal}");
    }
    reason.to_string()
}

fn byte_summary(len: usize) -> String {
    if len < 1024 {
        return format!("{len} B");
    }
    if len < 1024 * 1024 {
        return format!("{:.1} KiB", len as f64 / 1024.0);
    }
    format!("{:.1} MiB", len as f64 / (1024.0 * 1024.0))
}

fn bash(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    input: &Value,
    result: Option<&Value>,
    raw: Option<&str>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let limit = fold.limit(TOOL_PREVIEW_LINES);
    let palette = theme.palette;
    let command = text(input, "command");
    let first = command.lines().next().unwrap_or_default().trim();
    out.push(header(tool, "bash", &compact(first), theme));
    if tool.failed {
        let text = result
            .and_then(|value| value.get("error"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| raw.map(str::to_string))
            .unwrap_or_default();
        preview(out, &text, palette.error, limit, fold.toggle);
        return;
    }
    let Some(text) = shell_text(result) else {
        return;
    };
    preview(
        out,
        &text,
        if tool.failed {
            palette.error
        } else {
            palette.text_muted
        },
        limit,
        fold.toggle,
    );
}

/// The readable body of a shell result, with the same tail marker the bridge
/// appends for the TS renderer.
fn shell_text(result: Option<&Value>) -> Option<String> {
    let Some(Value::Object(record)) = result else {
        return None;
    };
    if let Some(error) = record.get("error").and_then(Value::as_str) {
        return Some(error.to_string());
    }
    let stdout = record
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_end();
    let stderr = record
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_end();
    let body = [stdout, stderr]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let exit = record.get("exit_code").and_then(Value::as_i64);
    let signal = record.get("signal").and_then(Value::as_i64);
    let timed_out = record.get("reason").and_then(Value::as_str) == Some("timed_out");
    let tail = if timed_out {
        "[command timed out]".to_string()
    } else if let Some(exit) = exit {
        format!("[exit code {exit}]")
    } else if let Some(signal) = signal {
        format!("[killed by signal {signal}]")
    } else {
        "[command not executed]".to_string()
    };
    Some(if body.is_empty() {
        tail
    } else {
        format!("{body}\n{tail}")
    })
}

fn read(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    input: &Value,
    result: Option<&Value>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let limit = fold.limit(TOOL_PREVIEW_LINES);
    let palette = theme.palette;
    let path = text(input, "filePath");
    let summary = read_summary(&tool.name, result).filter(|_| !tool.failed);
    out.push(header_with(
        tool,
        "read",
        &compact(&path),
        summary.as_deref(),
        theme,
    ));
    if !tool.failed {
        return;
    }
    if let Some(output) = tool.output.as_deref() {
        preview(out, output, palette.error, limit, fold.toggle);
    }
}

fn read_summary(engine: &str, result: Option<&Value>) -> Option<String> {
    let Some(Value::Object(record)) = result else {
        return None;
    };
    if engine == "read_image" {
        return Some("image".to_string());
    }
    if engine == "list_files" {
        let count = record
            .get("entries")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let truncated = record
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return Some(format!(
            "{count} {}{}",
            if count == 1 { "entry" } else { "entries" },
            if truncated { " (truncated)" } else { "" }
        ));
    }
    let body = record.get("text").and_then(Value::as_str)?;
    let count = if body.is_empty() {
        0
    } else {
        body.trim_end_matches('\n').lines().count()
    };
    let truncated = record
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(format!(
        "{count} {}{}",
        if count == 1 { "line" } else { "lines" },
        if truncated { " (truncated)" } else { "" }
    ))
}

fn search(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    label: &str,
    input: &Value,
    result: Option<&Value>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let limit = fold.limit(TOOL_PREVIEW_LINES);
    let palette = theme.palette;
    let pattern = text(input, "pattern");
    let path = text(input, "path");
    let detail = if path.is_empty() || path == "." {
        compact(&pattern)
    } else {
        compact(&format!("{pattern} in {path}"))
    };
    let summary = search_summary(result).filter(|_| !tool.failed);
    out.push(header_with(tool, label, &detail, summary.as_deref(), theme));
    if !tool.failed {
        return;
    }
    if let Some(output) = tool.output.as_deref() {
        preview(out, output, palette.error, limit, fold.toggle);
    }
}

fn search_summary(result: Option<&Value>) -> Option<String> {
    let Value::Object(record) = result? else {
        return None;
    };
    let count = record
        .get("matches")
        .and_then(Value::as_array)
        .map(Vec::len)?;
    let truncated = record
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(format!(
        "{count} {}{}",
        if count == 1 { "match" } else { "matches" },
        if truncated { " (truncated)" } else { "" }
    ))
}

// The render context is flat on purpose: every helper reads the same pieces
// and a context struct would only rename them at each call site.
#[allow(clippy::too_many_arguments)]
fn file(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    name: &str,
    input: &Value,
    files: Option<&[DisplayFile]>,
    raw: Option<&str>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let diff_limit = fold.limit(DIFF_PREVIEW_LINES);
    let limit = fold.limit(TOOL_PREVIEW_LINES);
    let palette = theme.palette;
    let path = text(input, "filePath");
    let label = match name {
        "write" => "write",
        "edit" => "edit",
        _ => "apply_patch",
    };
    out.push(header(tool, label, &compact(&path), theme));
    if tool.failed {
        let message = raw.unwrap_or_default();
        preview(out, message, palette.error, limit, fold.toggle);
        return;
    }
    let Some(files) = files.filter(|files| !files.is_empty()) else {
        // A running call has no recorded diff yet. Painting the input now would
        // show a different shape from the committed patch (no line numbers, no
        // context, no summary) and then flash into it, so wait for the result.
        // An unreturned call has no result coming, and an engine that committed
        // no display block leaves the input as the only record of the change.
        if tool.output.is_none() && !tool.unreturned {
            return;
        }
        let (rows, hidden) = input_diff(name, input, diff_limit);
        paint_diff(out, &rows, hidden, &path, theme, fold);
        return;
    };
    let added: u64 = files.iter().map(|file| file.additions).sum();
    let removed: u64 = files.iter().map(|file| file.deletions).sum();
    out.push(result_line(
        &file_summary(added, removed),
        palette.text_muted,
    ));
    for file in files {
        out.push(Line::from(Span::styled(
            format!(
                "  ◆ {} {} +{} −{}",
                file.action, file.path, file.additions, file.deletions
            ),
            Style::new().fg(palette.accent),
        )));
        let (rows, hidden) = patch_diff(&file.patch, diff_limit);
        paint_diff(out, &rows, hidden, &file.path, theme, fold);
    }
}

fn file_summary(added: u64, removed: u64) -> String {
    let mut parts = Vec::new();
    if added > 0 {
        parts.push(format!(
            "Added {added} {}",
            if added == 1 { "line" } else { "lines" }
        ));
    }
    if removed > 0 {
        parts.push(format!(
            "{} {removed} {}",
            if added > 0 { "removed" } else { "Removed" },
            if removed == 1 { "line" } else { "lines" }
        ));
    }
    if parts.is_empty() {
        "No line changes".to_string()
    } else {
        parts.join(", ")
    }
}

/// One painted diff row: the source line number when the patch states one, the
/// `+`/`-` marker, and the code the line holds.
enum DiffRow {
    /// The seam between two hunks, where the numbers restart.
    Hunk,
    Line {
        number: Option<u64>,
        marker: char,
        text: String,
    },
}

/// Split a unified patch into paintable rows, stopping after `budget` lines.
///
/// `hidden` counts the lines the budget left out so the fold can say how much of
/// the change is off screen. A hunk header that states its range
/// (`@@ -12,7 +12,9 @@`) restarts both counters; the V4A shape an `apply_patch`
/// tool takes as input states no range, so its rows carry no numbers.
fn patch_diff(patch: &str, budget: usize) -> (Vec<DiffRow>, usize) {
    let mut rows = Vec::new();
    let mut hidden = 0;
    let mut drawn = 0;
    let mut numbers: Option<(u64, u64)> = None;
    let mut started = false;
    for line in patch.lines() {
        if line.starts_with("@@") {
            if drawn > 0 && drawn < budget {
                rows.push(DiffRow::Hunk);
            }
            started = true;
            numbers = hunk_start(line);
            continue;
        }
        // Everything above the first hunk names the file, not a change to it.
        if !started {
            continue;
        }
        let Some((marker, text)) = marker_line(line) else {
            continue;
        };
        let current = numbers;
        let number = current.map(|(old, new)| {
            numbers = Some(match marker {
                '+' => (old, new + 1),
                '-' => (old + 1, new),
                _ => (old + 1, new + 1),
            });
            if marker == '+' {
                new
            } else {
                old
            }
        });
        if drawn >= budget {
            hidden += 1;
            continue;
        }
        drawn += 1;
        rows.push(DiffRow::Line {
            number,
            marker,
            text: text.to_string(),
        });
    }
    (rows, hidden)
}

/// The change a call asked for, for the call no diff was recorded for.
///
/// A streamed call has no display diff yet, and an engine that committed none
/// leaves the input as the only record of what changed.
fn input_diff(name: &str, input: &Value, budget: usize) -> (Vec<DiffRow>, usize) {
    if name == "apply_patch" {
        return patch_diff(&text(input, "patch"), budget);
    }
    let (removed, added) = match name {
        "edit" => (text(input, "old_string"), text(input, "new_string")),
        _ => (String::new(), text(input, "content")),
    };
    // A replacement spends the budget on both halves: giving it all to one side
    // would hide the very text the model asked to write.
    let per_side = if removed.is_empty() || added.is_empty() {
        budget
    } else {
        budget.div_ceil(2)
    };
    let mut rows = Vec::new();
    let mut hidden = 0;
    for (marker, side) in [('-', removed.as_str()), ('+', added.as_str())] {
        for (index, text) in side.lines().enumerate() {
            if index >= per_side {
                hidden += 1;
                continue;
            }
            rows.push(DiffRow::Line {
                number: None,
                marker,
                text: text.to_string(),
            });
        }
    }
    (rows, hidden)
}

fn paint_diff(
    out: &mut Vec<Line<'static>>,
    rows: &[DiffRow],
    hidden: usize,
    path: &str,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let palette = theme.palette;
    // One gutter width for the whole block: a digit count that varies per row
    // makes the markers jag down the block and the code jump sideways.
    let width = rows
        .iter()
        .filter_map(|row| match row {
            DiffRow::Line {
                number: Some(number),
                ..
            } => Some(number.to_string().len()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    // The grammar carries block-comment state across lines, so the file's own
    // language decides the spans; an extension no grammar claims stays plain.
    let mut highlighter = Highlighter::new(extension(path), palette);
    let mut clipped_rows = false;
    for row in rows {
        match row {
            DiffRow::Hunk => out.push(Line::from(Span::styled(
                format!("  {:width$}  …", ""),
                Style::new().fg(palette.diff_hunk_header),
            ))),
            DiffRow::Line {
                number,
                marker,
                text,
            } => {
                let gutter = match number {
                    Some(number) => format!("  {number:>width$}  "),
                    None => format!("  {:width$}  ", ""),
                };
                // A changed row is one band from the gutter to the pane's last
                // column, in the theme's own diff colour; the code inside keeps
                // its syntax hues, the way a fenced snippet does. Unchanged rows
                // stay on the canvas.
                let tint = match marker {
                    '+' | '-' => Some(palette.diff_block(*marker == '+'))
                        .filter(|tint| *tint != ratatui::style::Color::Reset),
                    _ => None,
                };
                let block_style = match tint {
                    Some(tint) => Style::new().bg(tint),
                    None => Style::new(),
                };
                let mut spans = vec![
                    Span::styled(
                        gutter,
                        Style::new().fg(palette.diff_line_number).patch(block_style),
                    ),
                    Span::styled(
                        marker.to_string(),
                        Style::new()
                            .fg(diff_color(*marker, theme))
                            .patch(block_style),
                    ),
                ];
                // The gutter already spends cells, so a long line is measured
                // against what is left. A minified or generated file has rows of
                // thousands of columns: left whole, one of them wrapped across
                // the viewport and buried the change it was meant to show.
                let (text, clipped) = match fold.expanded {
                    true => ((*text).to_string(), false),
                    false => clip_cells(text, FOLDED_ROW_CELLS.saturating_sub(width + 6)),
                };
                clipped_rows |= clipped;
                spans.extend(
                    highlighter
                        .line(&text)
                        .into_iter()
                        .map(|span| span.patch_style(block_style)),
                );
                let mut line = Line::from(spans);
                // The row-level tint is what survives the transcript pipeline:
                // trailing pad spans can be trimmed downstream, and the pane
                // re-pads any row whose line style carries a background. A
                // span-only tint reads as a ragged streak; the line style is
                // the block.
                if let Some(tint) = tint {
                    line.style = Style::new().bg(tint);
                }
                out.push(line);
            }
        }
    }
    if hidden > 0 || clipped_rows {
        let mut notes = Vec::new();
        if hidden > 0 {
            notes.push(format!("{hidden} more diff lines"));
        }
        if clipped_rows {
            notes.push("long lines clipped".to_string());
        }
        let detail = notes.join(" · ");
        let line = match fold.toggle {
            Some(chord) => format!("  … {detail} · {chord} expand"),
            None => format!("  … {detail}"),
        };
        out.push(Line::from(Span::styled(
            line,
            Style::new().fg(palette.text_muted),
        )));
    }
}

/// The `-a,b +c,d` range a unified hunk header states, if it states one.
fn hunk_start(line: &str) -> Option<(u64, u64)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, new) = rest.split_once(" +")?;
    Some((
        old.split(',').next()?.parse().ok()?,
        new.split([',', ' ']).next()?.parse().ok()?,
    ))
}

/// The marker a diff body line opens with, and the text after it. A line that is
/// not a change at all (`\ No newline at end of file`) has no marker.
fn marker_line(line: &str) -> Option<(char, &str)> {
    match line.chars().next() {
        Some(marker @ ('+' | '-' | ' ')) => Some((marker, &line[marker.len_utf8()..])),
        None => Some((' ', "")),
        Some(_) => None,
    }
}

/// The extension whose scanner colours a file's code.
fn extension(path: &str) -> &str {
    path.rsplit_once('.')
        .map(|(_, extension)| extension)
        .unwrap_or_default()
}

fn diff_color(marker: char, theme: &Theme) -> Color {
    let palette = theme.palette;
    match marker {
        '+' => palette.diff_added,
        '-' => palette.diff_removed,
        _ => palette.text_muted,
    }
}

fn todos(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    result: Option<&Value>,
    raw: Option<&str>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let limit = fold.limit(TOOL_PREVIEW_LINES);
    let palette = theme.palette;
    out.push(header(tool, "todowrite", "", theme));
    if tool.failed {
        preview(
            out,
            raw.unwrap_or_default(),
            palette.error,
            limit,
            fold.toggle,
        );
        return;
    }
    // The list the engine recorded, never the one the model asked for. The
    // result carries the applied list, while the input is what the call meant
    // to write — and while the call is still in flight that input is a partial
    // stream that would paint as the Session's plan before the engine has one.
    // The sidebar paints the recorded list, so a row painted from the input
    // disagreed with it every time the plan moved: same six items, different
    // statuses, in the two places an operator reads the plan.
    let Some(list) = result
        .and_then(|value| value.get("value"))
        .and_then(|value| value.get("todos"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for todo in list {
        let status = todo
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let glyph = match status {
            "completed" => "✓",
            "in_progress" => "•",
            "cancelled" => "×",
            "waiting" => "…",
            "blocked" => "!",
            _ => " ",
        };
        let color = if status == "in_progress" {
            palette.warning
        } else {
            palette.text_muted
        };
        let id = todo
            .get("id")
            .and_then(Value::as_str)
            .map(|id| format!("{id}: "))
            .unwrap_or_default();
        let content = todo
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let depends = todo
            .get("depends_on")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|deps| !deps.is_empty())
            .map(|deps| format!(" ← {deps}"))
            .unwrap_or_default();
        out.push(Line::from(Span::styled(
            format!("  [{glyph}] {id}{content}{depends}"),
            Style::new().fg(color),
        )));
    }
}

fn question(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    input: &Value,
    result: Option<&Value>,
    theme: &Theme,
) {
    let palette = theme.palette;
    out.push(header(tool, "question", "", theme));
    let answers = result
        .and_then(|value| value.get("answers"))
        .and_then(Value::as_array);
    let questions = input.get("questions").and_then(Value::as_array);
    for (index, item) in questions.into_iter().flatten().enumerate() {
        let text = item
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or_default();
        out.push(Line::from(Span::styled(
            format!("  {text}"),
            Style::new().fg(palette.text_muted),
        )));
        let answer = answers
            .and_then(|answers| answers.get(index))
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|answer| !answer.is_empty())
            .unwrap_or_else(|| "(no answer)".to_string());
        out.push(result_line(&answer, palette.text));
    }
}

fn task(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    input: &Value,
    raw: Option<&str>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let palette = theme.palette;
    let description = text(input, "description");
    out.push(header(tool, "task", &compact(&description), theme));
    // The child Session's transcript is not part of this Session's wire; the
    // row carries the task's final text so the delegating turn stays readable.
    if let Some(output) = raw {
        let value = parse_json(output);
        let first = value
            .as_ref()
            .and_then(|value| value.get("result").or(Some(value)))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                output
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let first = first.trim();
        if !first.is_empty() {
            preview(
                out,
                first,
                if tool.failed {
                    palette.error
                } else {
                    palette.text_muted
                },
                3,
                fold.toggle,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn generic(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    label: &str,
    input: &Value,
    result: Option<&Value>,
    raw: Option<&str>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let limit = fold.limit(TOOL_PREVIEW_LINES);
    let palette = theme.palette;
    out.push(header(tool, label, &input_summary(input), theme));
    let body = if tool.failed {
        result
            .and_then(|value| value.get("error"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| raw.map(str::to_string))
    } else {
        raw.map(str::to_string)
    };
    let Some(body) = body else {
        return;
    };
    preview(
        out,
        &body,
        if tool.failed {
            palette.error
        } else {
            palette.text_muted
        },
        limit,
        fold.toggle,
    );
}

fn input_summary(input: &Value) -> String {
    let candidate = [
        "command",
        "filePath",
        "pattern",
        "query",
        "url",
        "description",
    ]
    .iter()
    .find_map(|key| input.get(key).and_then(Value::as_str));
    match candidate {
        Some(text) => compact(text),
        None => compact(&input.to_string()),
    }
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn compact(text: &str) -> String {
    let single = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() <= 80 {
        return single;
    }
    let mut cut = single.chars().take(77).collect::<String>();
    cut.push('…');
    cut
}

/// Terminal escape sequences and stray control characters never paint: PTY
/// text and echoed model output would otherwise corrupt the transcript's own
/// rendering. Newlines and tabs survive.
pub(crate) fn scrub_output(text: &str) -> String {
    strip_ansi(text)
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

/// Remove terminal escape sequences from PTY output before it is laid out.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameter/intermediate bytes end at the first final byte.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: terminated by BEL or ST.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // DCS, SOS, PM, and APC: terminated by ST, like OSC without BEL.
            // The tmux passthrough miao itself emits is a DCS, so tool output
            // echoed from inside tmux carries these too.
            Some('P' | 'X' | '^' | '_') => {
                while let Some(c) = chars.next() {
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Other escapes end at the introducer byte the match consumed.
            _ => {}
        }
    }
    out
}

/// One readable row per background subagent: the operator tracks progress by
/// description and state, never by the child's full report, which the model
/// already received and which used to paint as a wall of JSON.
fn tasks(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    result: Option<&Value>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let palette = theme.palette;
    let rows = result
        .and_then(|value| value.get("tasks"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let running = rows
        .iter()
        .filter(|task| task["status"] == "running")
        .count();
    let summary = if rows.is_empty() {
        "no background tasks".to_string()
    } else {
        format!("{} tasks · {} running", rows.len(), running)
    };
    out.push(header(tool, "tasks", &summary, theme));
    let body = rows.iter().map(task_line).collect::<Vec<_>>().join("\n");
    if !body.is_empty() {
        preview(
            out,
            &body,
            palette.text_muted,
            fold.limit(TOOL_PREVIEW_LINES),
            fold.toggle,
        );
    }
}

fn task_line(task: &Value) -> String {
    let status = task["status"].as_str().unwrap_or("unknown");
    let glyph = match status {
        "running" => "•",
        "completed" => "✓",
        "cancelled" | "failed" => "×",
        _ => " ",
    };
    let description = task["description"].as_str().unwrap_or("subagent");
    format!("  [{glyph}] {description} · {status}")
}

/// A child's report: the row names the task and its state, and the body is the
/// report text — the part an operator actually reads.
fn task_report(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    result: Option<&Value>,
    raw: Option<&str>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let palette = theme.palette;
    let task = result.and_then(|value| value.get("task"));
    let description = task
        .and_then(|task| task["description"].as_str())
        .unwrap_or("subagent");
    let status = task
        .and_then(|task| task["status"].as_str())
        .unwrap_or("unknown");
    out.push(header(
        tool,
        "task",
        &format!("{description} · {status}"),
        theme,
    ));
    let limit = fold.limit(TOOL_PREVIEW_LINES);
    if tool.failed {
        preview(
            out,
            raw.unwrap_or_default(),
            palette.error,
            limit,
            fold.toggle,
        );
        return;
    }
    let body = result
        .and_then(|value| value["text"].as_str())
        .unwrap_or_default();
    if body.is_empty() {
        if status == "running" {
            out.push(result_line("still running", palette.text_muted));
        }
        return;
    }
    preview(out, body, palette.text, limit, fold.toggle);
}

/// The durable Session state: the checklist and the goal, in the shapes the
/// projection carries (`todos`/`goal` each holding the last mutation's value).
fn session_state(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    result: Option<&Value>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let palette = theme.palette;
    let todos = result
        .and_then(|value| value.get("todos"))
        .and_then(|value| value.get("value"))
        .and_then(|value| value.get("todos"))
        .and_then(Value::as_array);
    let goal = result
        .and_then(|value| value.get("goal"))
        .and_then(|value| value.get("value"));
    let summary = match (todos, goal) {
        // This tool reads only the calling Session's own state; another
        // Session's checklist lives in `session_state` of that Session.
        (None, None) => "no durable state (this session only)".to_string(),
        (Some(todos), _) => format!("{} todos", todos.len()),
        (_, Some(_)) => "goal active".to_string(),
    };
    out.push(header(tool, "session state", &summary, theme));
    let mut body = String::new();
    if let Some(goal) = goal {
        let objective = goal["objective"].as_str().unwrap_or_default();
        let status = goal["status"].as_str().unwrap_or_default();
        if !objective.is_empty() {
            body.push_str(&format!("  ◆ {objective} · {status}\n"));
        }
    }
    if let Some(todos) = todos {
        for todo in todos {
            let status = todo["status"].as_str().unwrap_or("pending");
            let glyph = match status {
                "completed" => "✓",
                "in_progress" => "•",
                "cancelled" => "×",
                "waiting" => "…",
                "blocked" => "!",
                _ => " ",
            };
            let content = todo["content"].as_str().unwrap_or_default();
            body.push_str(&format!("  [{glyph}] {content}\n"));
        }
    }
    if !body.is_empty() {
        preview(
            out,
            body.trim_end(),
            palette.text_muted,
            fold.limit(TOOL_PREVIEW_LINES),
            fold.toggle,
        );
    }
}

/// Scheduled wakes and crons: one row per timer with its pattern and fire
/// counters, instead of the ledger event serialized whole. A bare
/// `schedule_wakeup` answer still names the timer it armed.
fn schedules(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    result: Option<&Value>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let palette = theme.palette;
    let crons = result
        .and_then(|value| value.get("crons"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let timer = result
        .and_then(|value| value.get("timer_id"))
        .and_then(Value::as_str);
    let summary = if crons.is_empty() {
        timer
            .map(|id| {
                let tail = id.len().saturating_sub(12);
                format!("wake {}", &id[tail..])
            })
            .unwrap_or_else(|| "no schedules".to_string())
    } else {
        format!("{} schedules", crons.len())
    };
    out.push(header(tool, "schedules", &summary, theme));
    let mut body = String::new();
    for cron in crons {
        let pattern = cron["pattern"].as_str().unwrap_or("once");
        let fired = cron["fired_count"].as_u64().unwrap_or(0);
        let skipped = cron["skipped_count"].as_u64().unwrap_or(0);
        let state = cron["state"].as_str().unwrap_or("active");
        body.push_str(&format!(
            "  · {pattern} · {state} · fired {fired} · skipped {skipped}\n"
        ));
    }
    if !body.is_empty() {
        preview(
            out,
            body.trim_end(),
            palette.text_muted,
            fold.limit(TOOL_PREVIEW_LINES),
            fold.toggle,
        );
    }
}

/// Background jobs: one row per job with its state and the command it runs.
fn jobs(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    result: Option<&Value>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let palette = theme.palette;
    let listed = result
        .and_then(|value| value.get("jobs"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let single = result.and_then(|value| value.get("job"));
    let summary = if listed.is_empty() {
        single
            .and_then(|job| job["state"].as_str())
            .map(|state| format!("job {state}"))
            .unwrap_or_else(|| "no jobs".to_string())
    } else {
        format!("{} jobs", listed.len())
    };
    out.push(header(tool, "jobs", &summary, theme));
    let mut body = String::new();
    for job in listed.iter().chain(single) {
        let state = job["state"].as_str().unwrap_or("unknown");
        let command = job["command"].as_str().unwrap_or_default();
        body.push_str(&format!("  · {command} · {state}\n"));
    }
    if !body.is_empty() {
        preview(
            out,
            body.trim_end(),
            palette.text_muted,
            fold.limit(TOOL_PREVIEW_LINES),
            fold.toggle,
        );
    }
}

/// Worktrees: one row per tree with its branch and path.
fn worktree_rows(
    out: &mut Vec<Line<'static>>,
    tool: &ToolPart,
    result: Option<&Value>,
    theme: &Theme,
    fold: Fold<'_>,
) {
    let palette = theme.palette;
    let trees = result
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    out.push(header(
        tool,
        "worktrees",
        &format!("{} trees", trees.len()),
        theme,
    ));
    let mut body = String::new();
    for tree in trees {
        let branch = tree["branch"].as_str().unwrap_or("(detached)");
        let path = tree["path"].as_str().unwrap_or_default();
        body.push_str(&format!("  · {branch} · {path}\n"));
    }
    if !body.is_empty() {
        preview(
            out,
            body.trim_end(),
            palette.text_muted,
            fold.limit(TOOL_PREVIEW_LINES),
            fold.toggle,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use serde_json::json;
    use unicode_width::UnicodeWidthStr;

    fn theme() -> Theme {
        Theme {
            name: "test".to_string(),
            palette: theme::builtin("miao", crate::theme::ThemeMode::Dark).expect("builtin theme"),
        }
    }

    /// A call whose turn ended without a result is not running any more. The
    /// row's state used to be inferred from the result alone, so such a row
    /// kept the running glyph for the rest of the Session.
    #[test]
    fn an_unreturned_call_reads_as_settled_not_running() {
        let theme = Theme {
            name: crate::theme::DEFAULT_THEME.to_string(),
            palette: crate::theme::builtin(
                crate::theme::DEFAULT_THEME,
                crate::theme::ThemeMode::Dark,
            )
            .expect("theme"),
        };
        let mut call = tool("apply_patch", serde_json::json!({}), None, false);
        call.unreturned = true;
        let text = format!("{:?}", lines(&call, None, &theme, false, None));
        assert!(
            text.contains('–'),
            "an unanswered call after a finished turn shows the settled glyph: {text}"
        );
        assert!(
            !text.contains('·'),
            "it must not keep the running glyph: {text}"
        );
        // Before the turn ends the same row still reads as in flight.
        call.unreturned = false;
        let running = format!("{:?}", lines(&call, None, &theme, false, None));
        assert!(running.contains('·'), "a live call still shows running");
    }

    fn tool(name: &str, input: Value, output: Option<Value>, failed: bool) -> ToolPart {
        ToolPart {
            id: "call_1".into(),
            name: name.into(),
            input,
            output: output.map(|value| value.to_string()),
            failed,
            unreturned: false,
        }
    }

    /// A completed `read_file` reports its line count on the header row, so a
    /// summary never costs a second row of transcript.
    #[test]
    fn a_read_summary_rides_on_the_header() {
        let lines = lines(
            &tool(
                "read_file",
                json!({"path": "src/lib.rs"}),
                Some(json!({"text": "one\ntwo", "sha256": "0"})),
                false,
            ),
            None,
            &theme(),
            false,
            None,
        );
        let rendered = rendered(&lines);
        assert!(rendered[0].contains(" 2 lines"), "{rendered:?}");
        assert_eq!(rendered.len(), 1, "{rendered:?}");
    }

    fn rendered(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(row_text).collect()
    }

    /// A diff reads by shape before it reads by text: the whole changed row
    /// carries the change's tint, not only the `+`/`-` glyph, and every changed
    /// row ends on the same column so the block's edge is straight. An unchanged
    /// row stays on the canvas, where the tint would claim a change that is not
    /// there.
    #[test]
    fn a_changed_diff_row_is_a_tinted_block() {
        let theme = theme();
        let palette = theme.palette;
        let call = tool(
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Update File: src/a.rs\n@@ -1,2 +1,2 @@\n kept\n-gone\n+kept better and longer\n*** End Patch"}),
            Some(json!({})),
            false,
        );
        let rows = lines(&call, None, &theme, false, None);
        let find = |needle: &str| {
            rows.iter()
                .find(|row| row_text(row).contains(needle))
                .unwrap_or_else(|| panic!("no row holds {needle}: {:?}", rendered(&rows)))
        };
        let added = find("kept better and longer");
        let removed = find("gone");
        let context = find(" kept");
        let added_bg = palette.diff_block(true);
        let removed_bg = palette.diff_block(false);
        assert!(
            added
                .spans
                .iter()
                .all(|span| span.style.bg == Some(added_bg)),
            "every cell of an added row carries the added block: {:?}",
            added
        );
        assert!(
            removed
                .spans
                .iter()
                .all(|span| span.style.bg == Some(removed_bg)),
            "every cell of a removed row carries the removed block: {:?}",
            removed
        );
        assert!(
            context.spans.iter().all(|span| span.style.bg.is_none()),
            "an unchanged row stays on the canvas: {:?}",
            context
        );
        // The tint is the pane's job, not a pad to the block's longest line: a
        // row keeps the width of its own text and the pane fills the rest, so a
        // short change and a long one are both bands of the same colour.
        let cells = |row: &Line<'_>| {
            row.spans
                .iter()
                .map(|span| span.content.width())
                .sum::<usize>()
        };
        assert!(
            cells(removed) < cells(added),
            "a row is not padded out to the block's longest line: {:?}",
            rendered(&rows)
        );
    }

    /// A colourless theme has no canvas to mix into, so the block drops away and
    /// the glyphs and words keep carrying the change on their own.
    /// The row-level style is what survives the transcript pipeline: trailing
    /// pad spans can be trimmed downstream, and the pane re-pads any row whose
    /// line style carries a background. A span-only tint reads as a ragged
    /// streak; the line style is the block.
    #[test]
    fn a_diff_row_carries_its_tint_on_the_line_style() {
        let theme = theme();
        let palette = theme.palette;
        let call = tool(
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Update File: src/a.rs\n@@ -1,2 +1,2 @@\n kept\n-gone\n+kept better and longer\n*** End Patch"}),
            Some(json!({})),
            false,
        );
        let rows = lines(&call, None, &theme, false, None);
        let find = |needle: &str| {
            rows.iter()
                .find(|row| row_text(row).contains(needle))
                .unwrap_or_else(|| panic!("no row holds {needle}: {:?}", rendered(&rows)))
        };
        assert_eq!(
            find("kept better and longer").style.bg,
            Some(palette.diff_block(true))
        );
        assert_eq!(find("gone").style.bg, Some(palette.diff_block(false)));
        assert_eq!(
            find(" kept").style.bg,
            None,
            "an unchanged row stays on the canvas"
        );
    }

    #[test]
    fn a_colourless_theme_paints_no_diff_block() {
        let theme = theme().monochrome();
        let call = tool(
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Update File: src/a.rs\n@@ -1,1 +1,1 @@\n-gone\n+kept\n*** End Patch"}),
            Some(json!({})),
            false,
        );
        let rows = lines(&call, None, &theme, false, None);
        assert!(
            rows.iter().flat_map(|row| row.spans.iter()).all(|span| span
                .style
                .bg
                .is_none_or(|bg| bg == ratatui::style::Color::Reset)),
            "no cell claims a block colour: {:?}",
            rendered(&rows)
        );
    }

    fn row_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn display_file() -> DisplayFile {
        DisplayFile {
            path: "src/lib.rs".into(),
            action: "update".into(),
            patch: "Index: src/lib.rs\n====\n--- src/lib.rs\n+++ src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n"
                .into(),
            additions: 1,
            deletions: 1,
        }
    }

    #[test]
    fn engine_tools_map_onto_renderer_names() {
        assert_eq!(product_name("read_file"), "read");
        assert_eq!(product_name("run_command"), "bash");
        assert_eq!(product_name("cancel_wakeup"), "schedule");
        assert_eq!(product_name("cron_create"), "schedule");
        assert_eq!(product_name("lsp_hover"), "lsp");
        assert_eq!(product_name("mcp_server_tool"), "mcp_server_tool");
    }

    #[test]
    fn file_inputs_gain_the_fields_the_renderers_read() {
        let input = product_input(
            "write_file",
            &json!({"path": "a.txt", "text": "body", "expected_sha256": null}),
        );
        assert_eq!(input["filePath"], "a.txt");
        assert_eq!(input["content"], "body");
        let command = product_input(
            "run_command",
            &json!({"argv": ["echo", "two words"], "cwd": "."}),
        );
        assert_eq!(command["command"], "echo 'two words'");
        let task = product_input(
            "task",
            &json!({"agent": "build", "prompt": "first line\nrest"}),
        );
        assert_eq!(task["subagent_type"], "build");
        assert_eq!(task["description"], "first line");
    }

    #[test]
    fn a_shell_result_becomes_stdout_stderr_and_a_tail() {
        let part = tool(
            "run_command",
            json!({"argv": ["echo", "hi"]}),
            Some(json!({"stdout": "hi\nthere\nand\nmore", "stderr": "", "exit_code": 0})),
            false,
        );
        let lines = lines(&part, None, &theme(), false, Some("ctrl+x u"));
        let rendered = rendered(&lines);
        assert_eq!(rendered[0], "✓ bash  echo hi");
        assert!(rendered.contains(&"  │ hi".to_string()));
        // Folded output keeps the row budget; the tail and the hint stay
        // reachable through the elision marker instead of crowding the
        // transcript.
        assert!(rendered.iter().any(|line| line.contains("  │ … +")));
        // The hint names the key the operator presses, not the config's
        // `<leader>` placeholder.
        assert!(rendered
            .iter()
            .any(|line| line.contains("[ctrl+x u to expand]")));
    }

    /// The fold budget is a preview, not a transcript: three rows is enough for
    /// a command's answer to read without opening anything.
    #[test]
    fn a_folded_preview_keeps_three_rows() {
        let output = (0..10)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let part = tool(
            "run_command",
            json!({"argv": ["seq", "10"]}),
            Some(json!({"stdout": output, "stderr": "", "exit_code": 0})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        let kept = rendered
            .iter()
            .filter(|line| line.starts_with("  │ line"))
            .count();
        assert_eq!(kept, TOOL_PREVIEW_LINES, "{rendered:?}");
    }

    /// A minified or generated file has rows of thousands of columns. Left
    /// whole, one of them wrapped across the viewport and buried the change the
    /// diff was meant to show, so a folded diff clips it and says so.
    #[test]
    fn a_diff_row_too_wide_to_read_is_clipped_while_folded() {
        let long = "x".repeat(4000);
        let part = tool(
            "edit_file",
            json!({"filePath": "bundle.min.js"}),
            None,
            false,
        );
        let files = [DisplayFile {
            path: "bundle.min.js".into(),
            action: "update".into(),
            patch: format!(
                "Index: bundle.min.js\n====\n--- bundle.min.js\n+++ bundle.min.js\n@@ -1 +1 @@\n-{long}\n+{long}y\n"
            ),
            additions: 1,
            deletions: 1,
        }];
        let folded = rendered(&lines(
            &part,
            Some(&files),
            &theme(),
            false,
            Some("ctrl+x u"),
        ));
        let body = folded
            .iter()
            .find(|line| line.trim_start().starts_with('+') || line.contains("xxx"))
            .expect("the diff paints its rows");
        assert!(
            body.width() <= FOLDED_ROW_CELLS + 8,
            "a folded diff row stays readable: {} cells",
            body.width()
        );
        assert!(
            folded
                .iter()
                .any(|line| line.contains("long lines clipped")),
            "the block says it clipped: {folded:?}"
        );
        // Unfolding is the escape hatch, so the full row comes back.
        let open = rendered(&lines(
            &part,
            Some(&files),
            &theme(),
            true,
            Some("ctrl+x u"),
        ));
        assert!(open.iter().any(|line| line.len() > 4000));
        assert!(!open.iter().any(|line| line.contains("clipped")));
    }

    /// A result that is one enormous line has no line to fold away: the line
    /// budget keeps it whole and the terminal wrapped it across the viewport.
    /// A folded preview clips it to a readable width and says so; unfolding
    /// still hands the operator the entire value.
    #[test]
    fn a_single_enormous_line_is_clipped_while_folded() {
        let blob = json!({
            "content": (0..400)
                .map(|n| format!("INSERT INTO context(session_id,seq) VALUES('s',{n});"))
                .collect::<Vec<_>>()
                .join(" "),
        })
        .to_string();
        let part = tool(
            "run_command",
            json!({"argv": ["sqlite3", "storage.db"]}),
            Some(json!({"stdout": blob, "stderr": "", "exit_code": 0})),
            false,
        );
        let folded = rendered(&lines(&part, None, &theme(), false, Some("ctrl+x u")));
        let body = folded
            .iter()
            .find(|line| line.starts_with("  │ "))
            .expect("the row paints its output");
        assert!(
            body.width() <= FOLDED_ROW_CELLS + 4,
            "a folded row stays a couple of normal rows wide: {} cells",
            body.width()
        );
        assert!(body.ends_with('…'), "the clip reads as continuing: {body}");
        assert!(
            folded
                .iter()
                .any(|line| line.contains("long lines clipped")),
            "the row says the value was clipped: {folded:?}"
        );
        // Unfolding is the escape hatch, so the whole value must survive it.
        let open = rendered(&lines(&part, None, &theme(), true, Some("ctrl+x u")));
        assert!(
            open.iter().any(|line| line.len() >= blob.len()),
            "expanded output keeps the entire line"
        );
        assert!(!open.iter().any(|line| line.contains("clipped")));
    }

    #[test]
    fn a_tool_label_stays_plain_text_on_the_page_ground() {
        let theme = theme();
        let palette = theme.palette;
        let part = tool("run_command", json!({"argv": ["echo", "hi"]}), None, false);
        let rows = lines(&part, None, &theme, false, None);
        let label = rows[0]
            .spans
            .iter()
            .find(|span| span.content == "bash")
            .expect("the label is painted as its lowercase name");
        assert_eq!(label.style.fg, Some(palette.secondary));
        assert_eq!(label.style.bg, None);
        // The row keeps the page ground, so a column of calls cannot stack into
        // grey slabs down the transcript.
        assert_eq!(rows[0].style.bg, None);
    }

    #[test]
    fn expanded_tool_output_shows_every_line_and_ignores_the_fold_budget() {
        let output = (1..=12)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let part = tool(
            "run_command",
            json!({"argv": ["seq", "12"]}),
            Some(json!({"stdout": output, "stderr": "", "exit_code": 0})),
            false,
        );
        let folded = rendered(&lines(&part, None, &theme(), false, None));
        assert!(folded.len() < 12);
        assert!(folded.iter().any(|line| line.contains("  │ … +")));
        let expanded = rendered(&lines(&part, None, &theme(), true, None));
        assert!(expanded.iter().any(|line| line.contains("line 12")));
        assert!(!expanded.iter().any(|line| line.contains("  │ … +")));
    }

    /// The receipt is the engine's own answer, so the row repeats it rather
    /// than dumping JSON: who received the message and whether it was asked to
    /// act are the two facts a caller needs.
    #[test]
    fn a_coordination_message_reads_as_a_receipt() {
        let part = tool(
            "send_message",
            json!({"to": "ses_b", "message": "verify the migration", "reason": "the operator asked"}),
            Some(
                json!({"sessionID": "ses_b", "wake": "requested", "reason": "the operator asked"}),
            ),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert!(
            rendered[0].contains("message") && rendered[0].contains("→ ses_b"),
            "{rendered:?}"
        );
        assert!(rendered[0].contains("wake requested"), "{rendered:?}");
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("the operator asked")),
            "the reason stays visible: {rendered:?}"
        );
        // The raw answer never reaches the transcript.
        assert!(!rendered.iter().any(|line| line.contains("sessionID")));
    }

    #[test]
    fn a_failed_shell_result_shows_the_reason() {
        let part = tool(
            "bash",
            json!({"command": "ls"}),
            Some(json!({"error": "permission denied"})),
            true,
        );
        let lines = lines(&part, None, &theme(), false, None);
        let rendered = rendered(&lines);
        assert_eq!(rendered[0], "✗ bash  ls");
        assert!(rendered.contains(&"  │ permission denied".to_string()));
    }

    #[test]
    fn long_shell_output_is_collapsed() {
        let output = (0..10)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let part = tool(
            "bash",
            json!({"command": "seq"}),
            Some(json!({"stdout": output, "exit_code": 0})),
            false,
        );
        // An unbound toggle keeps the count and promises no key at all.
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert!(rendered.iter().any(|line| line.contains("  │ … +")));
        assert!(!rendered.iter().any(|line| line.contains("expand")));
    }

    #[test]
    fn terminal_escape_sequences_are_stripped() {
        let part = tool(
            "bash",
            json!({"command": "ls"}),
            Some(json!({"stdout": "\u{1b}[31mred\u{1b}[0m\n", "exit_code": 0})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert!(rendered.contains(&"  │ red".to_string()));
    }

    /// DCS, SOS, PM, and APC carry their own payload up to ST. Tool output
    /// echoed from inside tmux carries the passthrough form miao itself emits,
    /// and its payload used to paint as literal transcript text.
    #[test]
    fn dcs_and_apc_payloads_never_reach_the_render() {
        let part = tool(
            "bash",
            json!({"command": "ls"}),
            Some(
                json!({"stdout": "\u{1b}Ptmux;hidden\u{1b}\\ \u{1b}_apcpayload\u{1b}\\visible\n", "exit_code": 0}),
            ),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert!(rendered.iter().any(|line| line.contains("visible")));
        assert!(!rendered.iter().any(|line| line.contains("hidden")));
        assert!(!rendered.iter().any(|line| line.contains("tmux")));
        assert!(!rendered.iter().any(|line| line.contains("apcpayload")));
    }

    #[test]
    fn a_terminal_read_row_reports_its_state_not_the_envelope() {
        let part = tool(
            "terminal_read",
            json!({"id": "term_1"}),
            Some(json!({
                "id": "term_1",
                "output": "\u{1b}[38;2;255;0;0mhello\u{1b}[0m world\nsecond line\nthird line\nfourth line",
                "cursor": 65,
                "retained_from": 0,
                "dropped": 0,
                "truncated": true,
                "exit": null
            })),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, Some("ctrl+x r")));
        assert!(rendered.iter().any(|line| line.contains("terminal read")));
        assert!(rendered.iter().any(|line| line.contains("term_1")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("65 B · more retained · running")));
        assert!(rendered.iter().any(|line| line.contains("hello world")));
        assert!(rendered.iter().any(|line| line.contains("  │ … +")));
        assert!(!rendered.iter().any(|line| line.contains("retained_from")));
        assert!(!rendered.iter().any(|line| line.contains('\u{1b}')));
    }

    #[test]
    fn an_expanded_terminal_read_shows_every_decoded_line() {
        let part = tool(
            "terminal_read",
            json!({"id": "term_1"}),
            Some(json!({
                "id": "term_1",
                "output": "one\n2 \u{1b}[31mred\u{1b}[0m\nthree",
                "cursor": 20,
                "truncated": false,
                "exit": null
            })),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), true, Some("ctrl+x r")));
        assert!(rendered.iter().any(|line| line.contains("2 red")));
        assert!(rendered.iter().any(|line| line.contains("three")));
        assert!(!rendered.iter().any(|line| line.contains("  │ … +")));
    }

    /// A full-screen program repaints cells with escapes and never emits a
    /// newline, so a screen of paint is one endless logical line: the row budget
    /// folded nothing and the pane laid the whole window out.
    #[test]
    fn a_terminal_read_window_without_newlines_folds_at_the_row_budget() {
        let paint = format!(
            "{}end",
            "\u{1b}[38;2;252;232;108;48;2;46;52;64m█".repeat(4000)
        );
        let part = tool(
            "terminal_read",
            json!({"id": "term_1"}),
            Some(json!({
                "id": "term_1",
                "output": paint,
                "cursor": 9,
                "dropped": 0,
                "truncated": false,
                "exit": null
            })),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, Some("ctrl+x r")));
        assert!(
            rendered.iter().all(|line| line.chars().count() <= 140),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains("  │ … +")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("38;2;")),
            "a row break must not leave an escape tail as text: {rendered:?}"
        );
    }

    #[test]
    fn a_terminal_start_row_names_its_command_and_size() {
        let part = tool(
            "terminal_start",
            json!({"argv": ["bash"], "rows": 24, "cols": 80}),
            Some(json!({
                "id": "term_9",
                "rows": 24,
                "cols": 80,
                "sandbox_profile": "workspace_write",
                "network_allowed": true,
                "max_lifetime_ms": 86400000
            })),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert!(rendered.iter().any(|line| line.contains("bash")));
        assert!(rendered.iter().any(|line| line.contains("term_9 · 24×80")));
    }

    #[test]
    fn a_terminal_list_row_reads_one_line_per_terminal() {
        let part = tool(
            "terminal_list",
            json!({}),
            Some(json!({"terminals": [
                {"id": "term_1", "exit": null},
                {"id": "term_2", "exit": {"reason": "stopped", "exit_code": 0, "signal": null, "error": null}}
            ]})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), true, None));
        assert!(rendered
            .iter()
            .any(|line| line.contains("term_1 · running")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("term_2 · stopped · exit 0")));
    }

    #[test]
    fn a_terminal_stop_row_reports_how_it_ended() {
        let part = tool(
            "terminal_stop",
            json!({"id": "term_2"}),
            Some(json!({"reason": "stopped", "exit_code": 0, "signal": null, "error": null})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert!(rendered
            .iter()
            .any(|line| line.contains("stopped · exit 0")));
    }

    #[test]
    fn a_read_reports_its_line_count() {
        let part = tool(
            "read_file",
            json!({"path": "a.rs"}),
            Some(json!({"text": "one\ntwo\n", "sha256": "0"})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert_eq!(rendered[0], "✓ read  a.rs 2 lines");
    }

    #[test]
    fn a_search_reports_its_match_count() {
        let part = tool(
            "grep",
            json!({"pattern": "fn main", "path": "src"}),
            Some(json!({"matches": [{"path": "@a"}], "truncated": true})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert_eq!(rendered[0], "✓ grep  fn main in src 1 match (truncated)");
    }

    #[test]
    fn an_edit_renders_the_committed_diff_not_its_json() {
        let files = vec![display_file()];
        let part = tool(
            "edit_file",
            json!({"path": "src/lib.rs", "expected_sha256": "0"}),
            Some(json!({"path": "src/lib.rs", "sha256": "1"})),
            false,
        );
        let rendered = rendered(&lines(&part, Some(&files), &theme(), false, None));
        assert_eq!(rendered[0], "✓ edit  src/lib.rs");
        assert!(rendered.contains(&"  ⎿ Added 1 line, removed 1 line".to_string()));
        assert!(rendered.contains(&"  ◆ update src/lib.rs +1 −1".to_string()));
        assert!(rendered.contains(&"  1  -a".to_string()), "{rendered:?}");
        assert!(rendered.contains(&"  1  +b".to_string()), "{rendered:?}");
        assert!(!rendered.iter().any(|line| line.contains("Index:")));
    }

    /// A streamed edit paints no provisional diff: the input only has the
    /// replacement text, while the committed patch also carries line numbers and
    /// context. Showing both in sequence made the row flash and change.
    #[test]
    fn a_streaming_edit_waits_for_the_committed_diff() {
        let part = tool(
            "edit_file",
            json!({
                "path": "docs/guide.zh.md",
                "old_string": "旧的一行",
                "new_string": "新的一行",
                "expected_sha256": "0"
            }),
            None,
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert_eq!(rendered, ["· edit  docs/guide.zh.md"]);
    }

    /// A completed call can still carry no recorded diff, and a row that is a
    /// bare filename says nothing about the edit. The input is the only record
    /// of the change then, so the row has to paint it.
    #[test]
    fn an_edit_without_a_recorded_diff_shows_what_it_asked_for() {
        let part = tool(
            "edit_file",
            json!({
                "path": "docs/guide.zh.md",
                "old_string": "旧的一行",
                "new_string": "新的一行",
                "expected_sha256": "0"
            }),
            Some(json!({"path": "docs/guide.zh.md", "sha256": "1"})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert_eq!(rendered[0], "✓ edit  docs/guide.zh.md");
        assert!(
            rendered.contains(&"    -旧的一行".to_string()),
            "{rendered:?}"
        );
        assert!(
            rendered.contains(&"    +新的一行".to_string()),
            "{rendered:?}"
        );
    }

    #[test]
    fn a_write_without_a_recorded_diff_shows_the_lines_it_wrote() {
        let part = tool(
            "write_file",
            json!({"path": "notes.md", "text": "first\nsecond", "expected_sha256": null}),
            Some(json!({"path": "notes.md", "sha256": "1"})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert_eq!(rendered[0], "✓ write  notes.md");
        // A changed row is padded to the block's width, so the tint ends on one
        // column; the padding carries no text.
        assert!(
            rendered.iter().any(|row| row.trim_end() == "    +first"),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|row| row.trim_end() == "    +second"),
            "{rendered:?}"
        );
    }

    /// A hunk header states where its lines sit, so the row can print the number
    /// the operator would see in an editor. Two hunks are one change to read:
    /// the numbers restart at the second and a seam marks the skipped lines.
    #[test]
    fn a_second_hunk_restarts_its_numbers_under_a_seam() {
        let files = vec![DisplayFile {
            path: "src/lib.rs".into(),
            action: "update".into(),
            patch: "@@ -2,2 +2,2 @@\n-let old = 1;\n+let new = 1;\n@@ -40,2 +40,2 @@\n-let tail = 1;\n+let tail = 2;\n"
                .into(),
            additions: 2,
            deletions: 2,
        }];
        let part = tool(
            "edit_file",
            json!({"path": "src/lib.rs"}),
            Some(json!({"applied": true})),
            false,
        );
        let rendered = rendered(&lines(&part, Some(&files), &theme(), false, None));
        assert!(
            rendered
                .iter()
                .any(|row| row.trim_end() == "   2  -let old = 1;"),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|row| row.trim_end() == "  40  -let tail = 1;"),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.trim() == "…"),
            "{rendered:?}"
        );
    }

    /// The code keeps its colours: the marker column says what changed, and the
    /// scanner says what the line is.
    #[test]
    fn a_diff_row_is_highlighted_by_the_files_language() {
        let files = vec![DisplayFile {
            path: "src/lib.rs".into(),
            action: "update".into(),
            patch: "@@ -1,3 +1,3 @@\n let header = 0;\n-let old = 1;\n+let new = 1;\n".into(),
            additions: 1,
            deletions: 1,
        }];
        let part = tool(
            "edit_file",
            json!({"path": "src/lib.rs"}),
            Some(json!({"applied": true})),
            false,
        );
        let theme = theme();
        let rendered_lines = lines(&part, Some(&files), &theme, false, None);
        // A changed row keeps the file's own scanner colours — keywords stay
        // keyword-coloured — while every cell carries the product's diff block.
        let changed = rendered_lines
            .iter()
            .find(|line| row_text(line) == "  2  -let old = 1;")
            .expect("removed row");
        let keyword = changed
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "let")
            .expect("the keyword is its own span");
        assert_eq!(keyword.style.fg, Some(theme.palette.syntax.keyword));
        assert_eq!(keyword.style.bg, Some(theme.palette.diff_block(false)));
        let row = rendered_lines
            .iter()
            .find(|line| row_text(line) == "  1   let header = 0;")
            .expect("context row");
        let keyword = row
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "let")
            .expect("the keyword is its own span");
        assert_eq!(keyword.style.fg, Some(theme.palette.syntax.keyword));
    }

    #[test]
    fn a_long_diff_folds_and_says_how_much_it_hides() {
        let body = (1..=30)
            .map(|number| format!("+line {number}"))
            .collect::<Vec<_>>()
            .join("\n");
        let files = vec![DisplayFile {
            path: "notes.txt".into(),
            action: "create".into(),
            patch: format!("@@ -0,0 +1,30 @@\n{body}\n"),
            additions: 30,
            deletions: 0,
        }];
        let part = tool(
            "write_file",
            json!({"path": "notes.txt"}),
            Some(json!({"applied": true})),
            false,
        );
        let folded = rendered(&lines(&part, Some(&files), &theme(), false, None));
        assert!(
            folded
                .iter()
                .any(|line| line.contains("18 more diff lines")),
            "{folded:?}"
        );
        let expanded = rendered(&lines(&part, Some(&files), &theme(), true, None));
        assert!(
            expanded.iter().any(|line| line.contains("line 30")),
            "{expanded:?}"
        );
    }

    #[test]
    fn todos_render_the_records_the_engine_wrote_back() {
        // The row shows the list the engine recorded, so it can never disagree
        // with the sidebar's copy: the call's input is what the model meant to
        // write, and by the time the row settles the engine has the applied
        // list.
        let part = tool(
            "todowrite",
            json!({"todos": [
                {"id": "1", "content": "write code", "status": "pending"},
                {"id": "2", "content": "test it", "status": "pending"}
            ]}),
            Some(json!({"kind": "todos", "revision": 9, "value": {"todos": [
                {"id": "1", "content": "write code", "status": "completed"},
                {"id": "2", "content": "test it", "status": "in_progress", "depends_on": ["1"]}
            ]}})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert_eq!(rendered[0], "✓ todowrite");
        assert!(rendered.contains(&"  [✓] 1: write code".to_string()));
        assert!(rendered.contains(&"  [•] 2: test it ← 1".to_string()));
        // The input said both were pending; the painted row is the applied list.
        assert!(!rendered.iter().any(|line| line.contains("  [ ] 1")));
    }

    #[test]
    fn a_todo_write_still_in_flight_paints_only_its_header() {
        // A partial stream is not the Session's plan: until the engine answers,
        // the sidebar keeps painting the recorded list, so the row must not
        // paint a half-written one beside it.
        let part = tool(
            "todowrite",
            json!({"todos": [{"id": "1", "content": "write code", "status": "completed"}]}),
            None,
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert_eq!(rendered, vec!["· todowrite".to_string()]);
    }

    #[test]
    fn a_web_fetch_keeps_only_its_url() {
        let part = tool(
            "webfetch",
            json!({"url": "https://example.com/docs"}),
            Some(json!({"content": "an entire page"})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert_eq!(rendered, ["✓ WebFetch  https://example.com/docs"]);
    }

    #[test]
    fn a_question_shows_each_answer() {
        let part = tool(
            "question",
            json!({"questions": [
                {"question": "Which files?", "options": [{"label": "src"}]},
                {"question": "Proceed?", "options": [{"label": "yes"}]}
            ]}),
            Some(json!({"state": "answered", "answers": [["src"], ["yes", "now"]]})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert!(rendered.contains(&"  Which files?".to_string()));
        assert!(rendered.contains(&"  ⎿ src".to_string()));
        assert!(rendered.contains(&"  ⎿ yes, now".to_string()));
    }

    #[test]
    fn an_unknown_tool_keeps_its_input_and_output() {
        let part = tool(
            "mcp_server_tool",
            json!({"query": "hello"}),
            Some(json!({"result": "world"})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert_eq!(rendered[0], "✓ mcp_server_tool  hello");
        assert!(rendered.contains(&"  │ {\"result\":\"world\"}".to_string()));
    }
    /// A task list reads as one row per child — description and state — never
    /// as the serialized ledger: a completed child's report alone ran for
    /// pages, and the operator reads none of it.
    #[test]
    fn a_task_list_reads_as_rows_not_json() {
        let part = tool(
            "task_list",
            json!({}),
            Some(json!({"tasks": [
                {"id": "ses_1/sub-a", "session_id": "ses_1/sub-a", "description": "close #1974", "status": "completed", "text": "a very long report"},
                {"id": "ses_1/sub-b", "session_id": "ses_1/sub-b", "description": "close #1953", "status": "running", "text": null}
            ]})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), true, None));
        assert!(
            rendered
                .iter()
                .any(|row| row.contains("[✓] close #1974 · completed")),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|row| row.contains("[•] close #1953 · running")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|row| row.contains("very long report")),
            "the child's report stays out of the list: {rendered:?}"
        );
        assert!(
            !rendered.iter().any(|row| row.contains("\"tasks\"")),
            "no raw JSON reaches the row: {rendered:?}"
        );
    }

    /// A child's report is the body of a `task_result` row, under a header
    /// naming the task — not the envelope with the task serialized beside it.
    #[test]
    fn a_task_result_names_the_task_and_shows_the_report() {
        let part = tool(
            "task_result",
            json!({"id": "ses_1/sub-a"}),
            Some(
                json!({"task": {"id": "ses_1/sub-a", "description": "close #1974", "status": "completed"}, "text": "PR #2726 opened"}),
            ),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), false, None));
        assert!(
            rendered
                .iter()
                .any(|row| row.contains("close #1974 · completed")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|row| row.contains("PR #2726 opened")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|row| row.contains("\"text\"")),
            "no envelope JSON reaches the row: {rendered:?}"
        );
    }

    /// A terminal without image support is the common case, so an image
    /// result names the media instead of painting its bytes: one placeholder
    /// row, and the record's payload stays out even when the row is expanded.
    #[test]
    fn an_image_read_names_the_media_and_never_paints_its_bytes() {
        let payload = "iVBORw0KGgoAAAANSUhEUg==".repeat(8);
        let part = tool(
            "read_image",
            json!({"filePath": "assets/logo.png"}),
            Some(json!({"text": payload})),
            false,
        );
        let rendered = rendered(&lines(&part, None, &theme(), true, None));
        assert_eq!(rendered[0], "✓ read  assets/logo.png image");
        assert!(
            !rendered.iter().any(|line| line.contains("iVBOR")),
            "the image bytes stay out of the row: {rendered:?}"
        );
        assert_eq!(rendered.len(), 1, "nothing further to expand: {rendered:?}");
    }
}
