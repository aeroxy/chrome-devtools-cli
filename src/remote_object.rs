//! Text for CDP `Runtime.RemoteObject`s and `ExceptionDetails`.
//!
//! Console arguments are drained long after they were logged, when their
//! object handles may be gone, so objects are described from the `preview`
//! Chrome attaches to each argument rather than by asking the page again. The
//! layout follows the DevTools console (`{id: 3}`, `(3) [1, 2, 3]`,
//! `Map(1) {'a' => 1}`), simplified from devtools-frontend's
//! RemoteObjectPreviewFormatter: no gap markers in sparse arrays and no nested
//! previews.

use serde_json::Value;

/// Long descriptions (function sources, huge strings) would flood an agent's
/// context; DevTools trims previews at the same length.
const MAX_DESCRIPTION_LENGTH: usize = 400;

/// Subtypes whose description says more than a list of their properties.
const SUBTYPES_WITHOUT_VALUE_PREVIEW: &[&str] = &[
    "arraybuffer",
    "dataview",
    "date",
    "error",
    "internal#entry",
    "null",
    "node",
    "regexp",
    "trustedtype",
    "webassemblymemory",
];

/// Describe one RemoteObject the way the console would print it.
pub fn describe(obj: &Value) -> String {
    let ty = obj["type"].as_str().unwrap_or("");
    let subtype = obj["subtype"].as_str();
    if ty == "object" && !matches!(subtype, Some("node") | Some("trustedtype")) {
        if let Some(preview) = obj.get("preview").filter(|p| p.is_object()) {
            return format_object_preview(preview);
        }
    }
    if ty == "undefined" {
        return "undefined".to_string();
    }
    if let Some(u) = obj["unserializableValue"].as_str() {
        return u.to_string();
    }
    match obj.get("value") {
        // Strings print raw, as the console prints a string argument.
        Some(Value::String(s)) => return s.clone(),
        // `returnByValue` objects, numbers, booleans and null.
        Some(v) => return v.to_string(),
        None => {}
    }
    match obj["description"].as_str() {
        Some(d) => crate::format::truncate_chars(d, MAX_DESCRIPTION_LENGTH).into_owned(),
        None => "undefined".to_string(),
    }
}

/// Describe what a script threw or rejected with.
///
/// An Error's description already carries its message and stack. Anything
/// else (`throw 'boom'`, `Promise.reject({code: 42})`) has no useful
/// description, and `text` alone is just "Uncaught", so both are combined:
/// `Uncaught (in promise) {"code":42}`.
pub fn describe_exception(details: &Value) -> String {
    let text = details["text"].as_str().unwrap_or("");
    let exception = &details["exception"];
    let has_exception = exception.as_object().is_some_and(|o| !o.is_empty());
    if !has_exception {
        return if text.is_empty() {
            "Unknown error".to_string()
        } else {
            text.to_string()
        };
    }
    // Without a type the object can't be described any better than by its
    // description, which is what an Error's is.
    let is_error = exception["subtype"] == "error" || exception.get("type").is_none();
    if is_error {
        if let Some(d) = exception["description"].as_str() {
            return d.to_string();
        }
    }
    let reason = describe(exception);
    if text.is_empty() || text == reason {
        reason
    } else {
        format!("{text} {reason}")
    }
}

fn format_object_preview(preview: &Value) -> String {
    let ty = preview["type"].as_str().unwrap_or("");
    let subtype = preview["subtype"].as_str();
    let description = preview["description"].as_str().unwrap_or("");
    if ty != "object" || subtype.is_some_and(|s| SUBTYPES_WITHOUT_VALUE_PREVIEW.contains(&s)) {
        return format_property_value(ty, subtype, description);
    }

    let is_array = matches!(subtype, Some("array") | Some("typedarray"));
    let prefix = if is_array {
        let (name, length) = split_array_description(description);
        let length_text = if length > 1 {
            format!("({length})")
        } else {
            String::new()
        };
        if name == "Array" {
            length_text
        } else {
            format!("{name}{length_text}")
        }
    } else if description != "Object" {
        description.to_string()
    } else {
        String::new()
    };

    let empty = Vec::new();
    let properties = preview["properties"].as_array().unwrap_or(&empty);
    let mut items: Vec<String> = if let Some(entries) = preview["entries"].as_array() {
        entries.iter().map(format_entry).collect()
    } else if is_array {
        format_array_items(properties)
    } else {
        format_object_items(subtype, properties)
    };
    if preview["overflow"].as_bool().unwrap_or(false) {
        items.push("…".to_string());
    }
    let body = if is_array {
        format!("[{}]", items.join(", "))
    } else {
        format!("{{{}}}", items.join(", "))
    };
    if prefix.is_empty() {
        body
    } else {
        format!("{prefix} {body}")
    }
}

/// `"Array(3)"` → `("Array", 3)`, `"Uint8Array(2)"` → `("Uint8Array", 2)`.
fn split_array_description(description: &str) -> (&str, usize) {
    let Some(open) = description.rfind(['(', '[']) else {
        return (description, 0);
    };
    let digits = &description[open + 1..description.len().saturating_sub(1)];
    match digits.parse() {
        Ok(n) if description.ends_with([')', ']']) => (&description[..open], n),
        _ => (description, 0),
    }
}

fn format_array_items(properties: &[Value]) -> Vec<String> {
    let index_of = |p: &&Value| p["name"].as_str().and_then(|n| n.parse::<usize>().ok());
    let mut indexed: Vec<&Value> = properties
        .iter()
        .filter(|p| index_of(p).is_some())
        .collect();
    indexed.sort_by_key(|p| index_of(p));
    let others = properties.iter().filter(|p| index_of(p).is_none());
    indexed
        .into_iter()
        .map(property_value)
        .chain(others.map(|p| format!("{}: {}", format_name(p), property_value(p))))
        .collect()
}

fn format_object_items(subtype: Option<&str>, properties: &[Value]) -> Vec<String> {
    let properties: Vec<&Value> = properties
        .iter()
        .filter(|p| p["type"] != "accessor")
        .collect();
    let mut items = Vec::new();
    let mut i = 0;
    while i < properties.len() {
        let p = properties[i];
        let name = p["name"].as_str().unwrap_or("");
        let value = p["value"].as_str().unwrap_or("");
        if subtype == Some("promise") && name == "[[PromiseState]]" {
            let result = properties
                .get(i + 1)
                .filter(|r| r["name"] == "[[PromiseResult]]");
            match result {
                Some(r) if value != "pending" => {
                    items.push(format!("<{value}>: {}", property_value(r)));
                }
                _ => items.push(format!("<{value}>")),
            }
            if result.is_some() {
                i += 1;
            }
        } else if name == "[[PrimitiveValue]]" {
            items.push(property_value(p));
        } else {
            items.push(format!("{}: {}", format_name(p), property_value(p)));
        }
        i += 1;
    }
    items
}

fn format_entry(entry: &Value) -> String {
    let side = |v: &Value| {
        format_property_value(
            v["type"].as_str().unwrap_or(""),
            v["subtype"].as_str(),
            v["description"].as_str().unwrap_or(""),
        )
    };
    match entry.get("key").filter(|k| k.is_object()) {
        Some(key) => format!("{} => {}", side(key), side(&entry["value"])),
        None => side(&entry["value"]),
    }
}

fn property_value(p: &Value) -> String {
    format_property_value(
        p["type"].as_str().unwrap_or(""),
        p["subtype"].as_str(),
        p["value"].as_str().unwrap_or(""),
    )
}

fn format_property_value(ty: &str, subtype: Option<&str>, description: &str) -> String {
    match ty {
        "accessor" => "(...)".to_string(),
        "function" => "ƒ".to_string(),
        "string" => format!(
            "'{}'",
            description
                .replace('\\', "\\\\")
                .replace('\'', "\\'")
                .replace('\n', "\\n")
        ),
        "object" if subtype.is_none() && description == "Object" => "{…}".to_string(),
        _ => description.to_string(),
    }
}

fn format_name(p: &Value) -> String {
    let name = p["name"].as_str().unwrap_or("");
    if name.is_empty()
        || name.starts_with(char::is_whitespace)
        || name.ends_with(char::is_whitespace)
        || name.contains('\n')
    {
        format!("\"{}\"", name.replace('\n', "↵"))
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn primitives_print_like_the_console() {
        assert_eq!(describe(&json!({"type": "string", "value": "hi"})), "hi");
        assert_eq!(describe(&json!({"type": "number", "value": 3})), "3");
        assert_eq!(describe(&json!({"type": "undefined"})), "undefined");
        assert_eq!(
            describe(&json!({"type": "object", "subtype": "null", "value": null})),
            "null"
        );
        assert_eq!(
            describe(
                &json!({"type": "number", "unserializableValue": "NaN", "description": "NaN"})
            ),
            "NaN"
        );
        assert_eq!(
            describe(&json!({"type": "bigint", "unserializableValue": "1n", "description": "1n"})),
            "1n"
        );
    }

    /// Without the preview, `console.log({id: 3})` printed `Object`.
    #[test]
    fn objects_print_from_their_preview() {
        let obj = json!({"type": "object", "className": "Object", "description": "Object",
        "preview": {"type": "object", "description": "Object", "overflow": false, "properties": [
            {"name": "id", "type": "number", "value": "3"},
            {"name": "name", "type": "string", "value": "it's"},
            {"name": "nested", "type": "object", "value": "Object"},
            {"name": "fn", "type": "function", "value": ""},
        ]}});
        assert_eq!(
            describe(&obj),
            "{id: 3, name: 'it\\'s', nested: {…}, fn: ƒ}"
        );

        let arr = json!({"type": "object", "subtype": "array", "description": "Array(3)",
        "preview": {"type": "object", "subtype": "array", "description": "Array(3)", "overflow": false, "properties": [
            {"name": "1", "type": "number", "value": "2"},
            {"name": "0", "type": "number", "value": "1"},
            {"name": "2", "type": "string", "value": "x"},
        ]}});
        assert_eq!(describe(&arr), "(3) [1, 2, 'x']");

        let map = json!({"type": "object", "subtype": "map", "description": "Map(2)",
            "preview": {"type": "object", "subtype": "map", "description": "Map(2)", "overflow": true, "properties": [],
                "entries": [{"key": {"type": "string", "description": "a"}, "value": {"type": "number", "description": "1"}}]}});
        assert_eq!(describe(&map), "Map(2) {'a' => 1, …}");

        let set = json!({"type": "object", "subtype": "set", "description": "Set(1)",
            "preview": {"type": "object", "subtype": "set", "description": "Set(1)", "overflow": false, "properties": [],
                "entries": [{"value": {"type": "number", "description": "7"}}]}});
        assert_eq!(describe(&set), "Set(1) {7}");

        let promise = json!({"type": "object", "subtype": "promise", "description": "Promise",
        "preview": {"type": "object", "subtype": "promise", "description": "Promise", "overflow": false, "properties": [
            {"name": "[[PromiseState]]", "type": "string", "value": "fulfilled"},
            {"name": "[[PromiseResult]]", "type": "number", "value": "5"},
        ]}});
        assert_eq!(describe(&promise), "Promise {<fulfilled>: 5}");
    }

    #[test]
    fn nodes_and_errors_print_their_description() {
        let node = json!({"type": "object", "subtype": "node", "description": "div#app",
            "preview": {"type": "object", "subtype": "node", "description": "div#app", "overflow": false, "properties": []}});
        assert_eq!(describe(&node), "div#app");
        let err = json!({"type": "object", "subtype": "error", "description": "Error: x\n    at f",
            "preview": {"type": "object", "subtype": "error", "description": "Error: x\n    at f", "overflow": false, "properties": []}});
        assert_eq!(describe(&err), "Error: x\n    at f");
    }

    #[test]
    fn long_function_sources_are_trimmed_on_a_char_boundary() {
        let src = format!("function f() {{ return '{}'; }}", "é".repeat(500));
        let out = describe(&json!({"type": "function", "description": src}));
        assert_eq!(out.chars().count(), MAX_DESCRIPTION_LENGTH + 1);
        assert!(out.ends_with('…'));
    }

    /// `throw 'boom'` used to read as a bare "Uncaught".
    #[test]
    fn exceptions_name_what_was_thrown() {
        let thrown_string =
            json!({"text": "Uncaught", "exception": {"type": "string", "value": "boom"}});
        assert_eq!(describe_exception(&thrown_string), "Uncaught boom");

        let rejected_object = json!({"text": "Uncaught (in promise)",
            "exception": {"type": "object", "value": {"code": 42}, "description": "Object"}});
        assert_eq!(
            describe_exception(&rejected_object),
            "Uncaught (in promise) {\"code\":42}"
        );

        let rejected_undefined =
            json!({"text": "Uncaught (in promise)", "exception": {"type": "undefined"}});
        assert_eq!(
            describe_exception(&rejected_undefined),
            "Uncaught (in promise) undefined"
        );

        let error = json!({"text": "Uncaught", "exception": {"type": "object", "subtype": "error",
            "description": "TypeError: x is not a function\n    at <anonymous>:1:1"}});
        assert_eq!(
            describe_exception(&error),
            "TypeError: x is not a function\n    at <anonymous>:1:1"
        );

        assert_eq!(
            describe_exception(&json!({"text": "Script error."})),
            "Script error."
        );
        assert_eq!(
            describe_exception(&json!({"text": "oops", "exception": {}})),
            "oops"
        );
        assert_eq!(describe_exception(&json!({})), "Unknown error");
    }
}
