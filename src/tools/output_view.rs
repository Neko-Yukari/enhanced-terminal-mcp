//! Stateless slices of the stored raw UTF-8 transcript. No screen reconstruction.
use super::terminal_executor::{preview_output, preview_tail_output};
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadDirection { #[default] Head, Tail }
impl ReadDirection {
    pub fn name(self) -> &'static str { match self { Self::Head => "head", Self::Tail => "tail" } }
}

/// LF delimits stored lines; CRLF is one break. A terminal repaint is not a new line.
pub fn line_starts(text: &str) -> Vec<usize> {
    if text.is_empty() { return vec![]; }
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(at, _)| at + 1).filter(|at| *at < text.len()));
    starts
}
pub fn floor_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) { at -= 1; }
    at
}
fn ceil_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) { at += 1; }
    at
}

pub struct ReadSpec {
    pub direction: Option<ReadDirection>,
    pub offset_bytes: Option<usize>,
    pub limit_bytes: Option<usize>,
    pub before_byte: Option<usize>,
    pub start_line: Option<usize>,
    pub line_count: Option<usize>,
    pub cursor: usize,
    pub preview_tokens: usize,
}
pub struct OutputPage {
    pub text: String,
    pub start: usize,
    pub end: usize,
    pub selection_start: usize,
    pub selection_end: usize,
    pub first_line: Option<usize>,
    pub last_line: Option<usize>,
    pub total_lines: usize,
    pub partial_start: bool,
    pub partial_end: bool,
    pub direction: ReadDirection,
    pub explicit: bool,
    pub selection_has_more: bool,
    pub tokens: Option<usize>,
}

pub fn select_page(text: &str, spec: &ReadSpec) -> Result<OutputPage> {
    let direction = spec.direction.unwrap_or_default();
    let tail = direction == ReadDirection::Tail;
    let by_line = spec.start_line.is_some() || spec.line_count.is_some();
    let by_byte = spec.offset_bytes.is_some() || spec.limit_bytes.is_some();
    let explicit = spec.direction.is_some() || by_line || by_byte || spec.before_byte.is_some();
    if spec.start_line == Some(0) || spec.line_count == Some(0) { bail!("start_line and line_count are 1-based positive values"); }
    if by_line && by_byte { bail!("Choose line positioning or byte positioning, not both"); }
    if !tail && spec.before_byte.is_some() { bail!("before_byte requires direction=tail"); }
    if tail && (spec.start_line.is_some() || spec.offset_bytes.is_some()) { bail!("Tail uses line_count or limit_bytes and optional exclusive before_byte; omit start_line/offset_bytes"); }
    let lines = line_starts(text);
    let (start, end) = if tail {
        let end = floor_boundary(text, spec.before_byte.unwrap_or(text.len()));
        let start = if let Some(count) = spec.line_count {
            let last = lines.partition_point(|at| *at < end);
            lines.get(last.saturating_sub(count)).copied().unwrap_or(end)
        } else if let Some(limit) = spec.limit_bytes.filter(|n| *n > 0) {
            let start = ceil_boundary(text, end.saturating_sub(limit));
            if start == end && end > 0 { text[..end].char_indices().next_back().unwrap().0 } else { start }
        } else { 0 };
        (start, end)
    } else if by_line {
        let first = spec.start_line.unwrap_or(1) - 1;
        let start = lines.get(first).copied().unwrap_or(text.len());
        let end = spec.line_count.and_then(|n| lines.get(first.saturating_add(n)).copied()).unwrap_or(text.len());
        (start, end)
    } else {
        let start = floor_boundary(text, if explicit { spec.offset_bytes.unwrap_or(0) } else { spec.cursor });
        let mut end = spec.limit_bytes.filter(|n| *n > 0).map(|n| floor_boundary(text, start.saturating_add(n))).unwrap_or(text.len());
        if end == start && start < text.len() && spec.limit_bytes != Some(0) {
            end += text[start..].chars().next().unwrap().len_utf8();
        }
        (start, end)
    };
    let selected = &text[start..end];
    let preview = if tail { preview_tail_output(selected, spec.preview_tokens) } else { preview_output(selected, spec.preview_tokens) };
    if !selected.is_empty() && preview.text.is_empty() {
        bail!("preview_tokens cannot fit one complete UTF-8 code point; increase it or use a byte-limited page with preview_tokens=0");
    }
    let returned_start = if tail { end - preview.text.len() } else { start };
    let returned_end = if tail { end } else { start + preview.text.len() };
    let first_line = (returned_start < returned_end).then(|| lines.partition_point(|at| *at <= returned_start));
    let last_line = (returned_start < returned_end).then(|| lines.partition_point(|at| *at < returned_end));
    let partial_start = first_line.is_some_and(|line| returned_start != lines[line - 1]);
    let partial_end = returned_end > returned_start && returned_end < text.len() && text.as_bytes()[returned_end - 1] != b'\n';
    Ok(OutputPage { text: preview.text, start: returned_start, end: returned_end,
        selection_start: start, selection_end: end, first_line, last_line, total_lines: lines.len(),
        partial_start, partial_end, direction, explicit,
        selection_has_more: if tail { returned_start > start } else { returned_end < end }, tokens: preview.tokens })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> ReadSpec { ReadSpec { direction: None, offset_bytes: None, limit_bytes: None, before_byte: None, start_line: None, line_count: None, cursor: 0, preview_tokens: 4096 } }
    #[test]
    fn lines_crlf_and_tail_stay_in_raw_byte_space() {
        let text = "甲\r\n乙🙂\r\n尾";
        let page = select_page(text, &ReadSpec { start_line: Some(2), line_count: Some(1), ..spec() }).unwrap();
        assert_eq!(page.text, "乙🙂\r\n"); assert_eq!((page.start, page.end), (5, 14));
        assert_eq!((page.first_line, page.last_line), (Some(2), Some(2)));
        let tail = select_page(text, &ReadSpec { direction: Some(ReadDirection::Tail), line_count: Some(2), ..spec() }).unwrap();
        assert_eq!(tail.text, "乙🙂\r\n尾"); assert!(!tail.partial_start);
    }
    #[test]
    fn giant_single_line_roundtrips_in_both_directions() {
        let text = "中文🙂0123456789".repeat(400);
        let mut at = 0; let mut forward = String::new();
        while at < text.len() {
            let page = select_page(&text, &ReadSpec { offset_bytes: Some(at), preview_tokens: 32, ..spec() }).unwrap();
            assert!(page.end > at); assert_eq!(page.first_line, Some(1)); forward.push_str(&page.text); at = page.end;
        }
        assert_eq!(forward, text);
        let mut before = text.len(); let mut backward = Vec::new();
        while before > 0 {
            let page = select_page(&text, &ReadSpec { direction: Some(ReadDirection::Tail), before_byte: Some(before), preview_tokens: 32, ..spec() }).unwrap();
            assert!(page.start < before); before = page.start; backward.push(page.text);
        }
        backward.reverse(); assert_eq!(backward.concat(), text);
        let tiny = select_page("中文", &ReadSpec { offset_bytes: Some(0), limit_bytes: Some(1), preview_tokens: 0, ..spec() }).unwrap();
        assert_eq!(tiny.text, "中"); assert_eq!(tiny.end, 3);
    }
}
