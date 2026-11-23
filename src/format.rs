use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Toon,
}

impl OutputFormat {
    pub fn from_flags(json: bool, toon: bool) -> Self {
        if toon {
            OutputFormat::Toon
        } else if json {
            OutputFormat::Json
        } else {
            OutputFormat::Text
        }
    }

    pub fn is_text(self) -> bool {
        self == OutputFormat::Text
    }

    pub fn is_json(self) -> bool {
        self == OutputFormat::Json
    }

    pub fn is_toon(self) -> bool {
        self == OutputFormat::Toon
    }
}

/// Encode a serde_json::Value as TOON or pretty-printed JSON.
pub fn format_structured(value: &Value, format: OutputFormat) -> Result<String> {
    match format {
        OutputFormat::Json => Ok(serde_json::to_string_pretty(value)?),
        OutputFormat::Toon => {
            let options = toon_format::EncodeOptions::default();
            Ok(toon_format::encode(value, &options)?)
        }
        OutputFormat::Text => Ok(serde_json::to_string_pretty(value)?),
    }
}

/// Cut `s` to at most `max_chars` characters, ending a cut one with "…".
/// Counts and cuts by `char`, so a multi-byte character is never split.
pub fn truncate_chars(s: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    match s.char_indices().nth(max_chars) {
        Some((cut, _)) => std::borrow::Cow::Owned(format!("{}…", &s[..cut])),
        None => std::borrow::Cow::Borrowed(s),
    }
}

/// Remove terminal control sequences from text before it reaches a terminal.
///
/// Console messages, page titles, URLs and error text all come from the page,
/// and a page can log `ESC ] 0 ; title BEL` to rename the terminal, OSC 52 to
/// write the clipboard, or CSI codes to recolour or hide output. CSI and OSC
/// sequences go with their payloads; other C0 and C1 controls go too, except
/// newline, tab and carriage return. An unterminated OSC loses only its ESC,
/// so the rest of its line stays readable.
pub fn strip_terminal_escapes(s: &str) -> std::borrow::Cow<'_, str> {
    let stripped =
        |c: char| matches!(c as u32, 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f..=0x9f);
    if !s.chars().any(stripped) {
        return std::borrow::Cow::Borrowed(s);
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if b[i] == 0x1b {
            i = csi_end(b, i).or_else(|| osc_end(b, i)).unwrap_or(i + 1);
            continue;
        }
        let c = s[i..].chars().next().unwrap_or_default();
        if !stripped(c) {
            out.push(c);
        }
        i += c.len_utf8().max(1);
    }
    std::borrow::Cow::Owned(out)
}

/// End of `ESC [ params intermediates final`, if `b[i]` starts one.
fn csi_end(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i + 1) != Some(&b'[') {
        return None;
    }
    let mut j = i + 2;
    while b.get(j).is_some_and(|c| (0x20..=0x3f).contains(c)) {
        j += 1;
    }
    b.get(j)
        .filter(|c| (0x40..=0x7e).contains(*c))
        .map(|_| j + 1)
}

/// End of `ESC ] payload (BEL | ESC \)` on one line, if `b[i]` starts one.
fn osc_end(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i + 1) != Some(&b']') {
        return None;
    }
    let mut j = i + 2;
    while let Some(&c) = b.get(j) {
        match c {
            0x07 => return Some(j + 1),
            0x1b => return (b.get(j + 1) == Some(&b'\\')).then_some(j + 2),
            b'\r' | b'\n' => return None,
            _ => j += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_cuts_at_char_boundaries() {
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("exactly10!", 10), "exactly10!");
        assert_eq!(truncate_chars("abcdefghijk", 10), "abcdefghij…");
        // Multi-byte characters count once and are never split.
        assert_eq!(truncate_chars("héllo wörld", 2), "hé…");
        assert_eq!(truncate_chars("日本語のテキスト", 3), "日本語…");
        assert_eq!(truncate_chars("🦀🦀🦀", 1), "🦀…");
        assert_eq!(truncate_chars("🦀🦀🦀", 3), "🦀🦀🦀");
    }

    #[test]
    fn terminal_escapes_are_stripped_with_their_payloads() {
        let page_log = "\x1b]0;PWNED-TITLE\x07\x1b[31mESCAPE-INJECTION\x1b[0m";
        assert_eq!(strip_terminal_escapes(page_log), "ESCAPE-INJECTION");
        // OSC 52 clipboard write terminated by ST.
        assert_eq!(strip_terminal_escapes("a\x1b]52;c;aGk=\x1b\\b"), "ab");
        // Unterminated OSC: only the ESC goes, the line stays readable.
        assert_eq!(
            strip_terminal_escapes("\x1b]0;title\nnext"),
            "]0;title\nnext"
        );
        // 8-bit CSI and other controls; newline, tab and CR survive.
        assert_eq!(
            strip_terminal_escapes("x\u{9b}31my\x08\t\r\n"),
            "x31my\t\r\n"
        );
        assert_eq!(strip_terminal_escapes("héllo ✓"), "héllo ✓");
        assert!(matches!(
            strip_terminal_escapes("plain"),
            std::borrow::Cow::Borrowed(_)
        ));
    }
    use serde_json::json;

    #[test]
    fn from_flags_text_default() {
        assert_eq!(OutputFormat::from_flags(false, false), OutputFormat::Text);
    }

    #[test]
    fn from_flags_json() {
        assert_eq!(OutputFormat::from_flags(true, false), OutputFormat::Json);
    }

    #[test]
    fn from_flags_toon_wins_over_json() {
        // toon takes precedence when both are set (the CLI enforces mutual
        // exclusivity, but from_flags should have a defined precedence)
        assert_eq!(OutputFormat::from_flags(true, true), OutputFormat::Toon);
    }

    #[test]
    fn is_flags() {
        assert!(OutputFormat::Text.is_text());
        assert!(OutputFormat::Json.is_json());
        assert!(OutputFormat::Toon.is_toon());
        assert!(!OutputFormat::Text.is_json());
        assert!(!OutputFormat::Json.is_text());
    }

    #[test]
    fn format_structured_json_produces_pretty_json() {
        let v = json!({"a": 1, "b": [2, 3]});
        let out = format_structured(&v, OutputFormat::Json).unwrap();
        // Contains indentation (pretty-printed)
        assert!(out.contains("  \"a\": 1"));
        // Round-trips back to the same Value
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, v);
    }

    #[test]
    fn format_structured_toon_is_compact() {
        let v = json!([{"name": "a", "value": 1}, {"name": "b", "value": 2}]);
        let toon_out = format_structured(&v, OutputFormat::Toon).unwrap();
        let json_out = format_structured(&v, OutputFormat::Json).unwrap();
        // TOON should be shorter than pretty-printed JSON
        assert!(
            toon_out.len() < json_out.len(),
            "TOON ({}) should be shorter than JSON ({})",
            toon_out.len(),
            json_out.len()
        );
    }

    #[test]
    fn format_structured_text_default_matches_json() {
        let v = json!({"x": "hello"});
        let text = format_structured(&v, OutputFormat::Text).unwrap();
        let json = format_structured(&v, OutputFormat::Json).unwrap();
        assert_eq!(text, json);
    }
}

#[cfg(test)]
mod daemon_request_format_tests {
    use crate::format::OutputFormat;
    use crate::protocol::DaemonRequest;
    use serde_json::json;

    fn make_req(json_output: bool, output_format: Option<OutputFormat>) -> DaemonRequest {
        DaemonRequest {
            command: "list-pages".to_string(),
            args: json!({}),
            page: None,
            target: None,
            json_output,
            output_format,
            block_url: vec![],
            unblock_url: vec![],
            cwd: None,
        }
    }

    #[test]
    fn format_default_is_text() {
        let req = make_req(false, None);
        assert_eq!(req.format(), OutputFormat::Text);
    }

    #[test]
    fn format_legacy_json_output_bool() {
        let req = make_req(true, None);
        assert_eq!(req.format(), OutputFormat::Json);
    }

    #[test]
    fn format_output_format_wins_over_legacy() {
        let req = make_req(true, Some(OutputFormat::Toon));
        assert_eq!(req.format(), OutputFormat::Toon);
    }

    #[test]
    fn format_output_format_none_uses_default() {
        let req = make_req(false, Some(OutputFormat::Text));
        assert_eq!(req.format(), OutputFormat::Text);
    }
}
