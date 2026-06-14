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
