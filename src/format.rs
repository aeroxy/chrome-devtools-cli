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

#[cfg(test)]
mod tests {
    use super::*;
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
