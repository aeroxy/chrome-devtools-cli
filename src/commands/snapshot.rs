use anyhow::Result;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use crate::cdp::CdpClient;
use crate::format::{format_structured, OutputFormat};
use crate::result::CommandResult;

/// Take an accessibility tree snapshot of the current page.
pub async fn take_snapshot(
    client: &mut CdpClient,
    session_id: &str,
    format: OutputFormat,
    output: Option<&str>,
) -> Result<CommandResult> {
    let result = client
        .send_to_target(session_id, "Accessibility.getFullAXTree", json!({}))
        .await?;

    let content = if format.is_text() {
        if let Some(nodes) = result["nodes"].as_array() {
            format_ax_tree(nodes)
        } else {
            serde_json::to_string_pretty(&result)?
        }
    } else {
        format_structured(&result, format)?
    };

    Ok(CommandResult::output(content).save_output(output).await?)
}

/// Render the AX tree as an indented outline in document order.
///
/// `getFullAXTree` returns a flat list in no particular order, with the
/// structure only in `parentId`/`childIds`, so the tree is walked from its
/// roots. What an agent can't use is left out: wrapper nodes with no role of
/// their own (their children move up a level), `InlineTextBox` line
/// fragments, which repeat their parent's text, and text children that only
/// repeat their parent's name (`link "Home"` → `"Home"`). Text nodes print
/// without their `StaticText` role, since the quoted text says what they are.
fn format_ax_tree(nodes: &[Value]) -> String {
    let by_id: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|n| Some((n["nodeId"].as_str()?, n)))
        .collect();
    let tree = AxTree { by_id };
    let mut out = String::new();
    let mut seen = HashSet::new();
    for node in nodes {
        let parent_known = node["parentId"]
            .as_str()
            .is_some_and(|p| tree.by_id.contains_key(p));
        if !parent_known {
            tree.emit(node, 0, &mut out, &mut seen);
        }
    }
    out
}

struct AxTree<'a> {
    by_id: HashMap<&'a str, &'a Value>,
}

impl<'a> AxTree<'a> {
    fn emit(&self, node: &'a Value, depth: usize, out: &mut String, seen: &mut HashSet<&'a str>) {
        let id = node["nodeId"].as_str().unwrap_or("");
        // Guards against a malformed tree that links a node twice.
        if !seen.insert(id) || role(node) == "InlineTextBox" {
            return;
        }
        if is_pass_through(node) {
            for child in self.children(node) {
                self.emit(child, depth, out, seen);
            }
            return;
        }

        let name = name(node);
        let indent = "  ".repeat(depth);
        match (role(node), name.is_empty()) {
            ("StaticText", _) => writeln!(out, "{indent}\"{name}\" #{id}").unwrap(),
            (role, true) => writeln!(out, "{indent}[{role}] #{id}").unwrap(),
            (role, false) => writeln!(out, "{indent}[{role}] \"{name}\" #{id}").unwrap(),
        }

        let kids = self.visible_children(node);
        if !name.is_empty() && self.only_repeat(&kids, name) {
            return;
        }
        for kid in kids {
            self.emit(kid, depth + 1, out, seen);
        }
    }

    fn children(&self, node: &Value) -> Vec<&'a Value> {
        node["childIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| self.by_id.get(c.as_str()?).copied())
            .collect()
    }

    /// The children that will be printed, with pass-through wrappers replaced
    /// by their own children and line fragments dropped.
    fn visible_children(&self, node: &Value) -> Vec<&'a Value> {
        let mut out = Vec::new();
        let mut stack: Vec<&'a Value> = self.children(node).into_iter().rev().collect();
        let mut guard = 0usize;
        while let Some(child) = stack.pop() {
            guard += 1;
            if guard > self.by_id.len() {
                break;
            }
            if role(child) == "InlineTextBox" {
                continue;
            }
            if is_pass_through(child) {
                stack.extend(self.children(child).into_iter().rev());
            } else {
                out.push(child);
            }
        }
        out
    }

    /// Whether the children are all plain text that adds up to the parent's
    /// name, so printing them would only say the same thing again.
    fn only_repeat(&self, kids: &[&'a Value], parent_name: &str) -> bool {
        if kids.is_empty() {
            return false;
        }
        let all_leaf_text = kids
            .iter()
            .all(|k| role(k) == "StaticText" && self.visible_children(k).is_empty());
        if !all_leaf_text {
            return false;
        }
        let combined: String = kids.iter().map(|k| name(k)).collect();
        strip_whitespace(&combined) == strip_whitespace(parent_name)
    }
}

fn role(node: &Value) -> &str {
    node["role"]["value"].as_str().unwrap_or("")
}

fn name(node: &Value) -> &str {
    node["name"]["value"].as_str().unwrap_or("")
}

/// Nodes that only group others: printing them adds a line and an indent
/// level but no information.
fn is_pass_through(node: &Value) -> bool {
    let role = role(node);
    node["ignored"].as_bool().unwrap_or(false)
        || matches!(role, "none" | "generic" | "Ignored" | "")
        || (role == "StaticText" && name(node).is_empty())
}

fn strip_whitespace(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, role: &str, name: &str, parent: Option<&str>, children: &[&str]) -> Value {
        let mut n = json!({
            "nodeId": id,
            "ignored": false,
            "role": {"type": "role", "value": role},
            "name": {"type": "computedString", "value": name},
            "childIds": children,
        });
        if let Some(p) = parent {
            n["parentId"] = json!(p);
        }
        n
    }

    /// The nodes arrive out of document order; the outline must follow the
    /// tree, not the list.
    #[test]
    fn outline_follows_the_tree_and_drops_noise() {
        let nodes = vec![
            // Listed before its parents, as getFullAXTree can.
            node("6", "StaticText", "Home", Some("5"), &["7"]),
            node("7", "InlineTextBox", "Home", Some("6"), &[]),
            node("1", "RootWebArea", "Shop", None, &["2"]),
            node("2", "generic", "", Some("1"), &["3", "5", "8"]),
            node("3", "heading", "Welcome", Some("2"), &["4"]),
            node("4", "StaticText", "Welcome", Some("3"), &[]),
            node("5", "link", "Home", Some("2"), &["6"]),
            node("8", "paragraph", "", Some("2"), &["9", "10"]),
            node("9", "StaticText", "Price: ", Some("8"), &[]),
            node("10", "StaticText", "$5", Some("8"), &[]),
        ];
        assert_eq!(
            format_ax_tree(&nodes),
            "[RootWebArea] \"Shop\" #1\n\
             \x20 [heading] \"Welcome\" #3\n\
             \x20 [link] \"Home\" #5\n\
             \x20 [paragraph] #8\n\
             \x20   \"Price: \" #9\n\
             \x20   \"$5\" #10\n"
        );
    }

    /// Text is dropped only when it says nothing new: a button whose label
    /// differs from its text keeps the text.
    #[test]
    fn text_that_differs_from_the_name_is_kept() {
        let nodes = vec![
            node("1", "RootWebArea", "", None, &["2"]),
            node("2", "button", "Close dialog", Some("1"), &["3"]),
            node("3", "StaticText", "×", Some("2"), &[]),
        ];
        assert_eq!(
            format_ax_tree(&nodes),
            "[RootWebArea] #1\n  [button] \"Close dialog\" #2\n    \"×\" #3\n"
        );
    }

    #[test]
    fn ignored_nodes_pass_their_children_up() {
        let mut ignored = node("2", "none", "", Some("1"), &["3"]);
        ignored["ignored"] = json!(true);
        let nodes = vec![
            node("1", "RootWebArea", "", None, &["2"]),
            ignored,
            node("3", "textbox", "Search", Some("2"), &[]),
        ];
        assert_eq!(
            format_ax_tree(&nodes),
            "[RootWebArea] #1\n  [textbox] \"Search\" #3\n"
        );
    }

    #[test]
    fn a_node_linked_twice_prints_once() {
        let nodes = vec![
            node("1", "RootWebArea", "", None, &["2", "2"]),
            node("2", "button", "Go", Some("1"), &[]),
        ];
        assert_eq!(
            format_ax_tree(&nodes),
            "[RootWebArea] #1\n  [button] \"Go\" #2\n"
        );
    }
}
