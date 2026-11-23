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
/// Names are quoted as JSON strings, so a line break inside one (a `<br>`, a
/// `<pre>` line) can't start a row of its own.
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
            tree.emit(node, &mut out, &mut seen);
        }
    }
    out
}

struct AxTree<'a> {
    by_id: HashMap<&'a str, &'a Value>,
}

impl<'a> AxTree<'a> {
    /// Print `root` and everything under it. The walk keeps its own stack
    /// instead of recursing: a page can nest elements deeper than the call
    /// stack goes, and on Windows the main thread has 1 MiB by default, which
    /// a few thousand nested groups used up.
    fn emit(&self, root: &'a Value, out: &mut String, seen: &mut HashSet<&'a str>) {
        // Children go on in reverse, so they come off in document order.
        let mut stack = vec![(root, 0)];
        while let Some((node, depth)) = stack.pop() {
            let id = node["nodeId"].as_str().unwrap_or("");
            // Guards against a malformed tree that links a node twice.
            if !seen.insert(id) || role(node) == "InlineTextBox" {
                continue;
            }
            if is_pass_through(node) {
                stack.extend(self.children(node).into_iter().rev().map(|c| (c, depth)));
                continue;
            }

            let name = name(node);
            let indent = "  ".repeat(depth);
            let quoted = Value::from(name);
            match (role(node), name.is_empty()) {
                ("StaticText", _) => writeln!(out, "{indent}{quoted} #{id}").unwrap(),
                (role, true) => writeln!(out, "{indent}[{role}] #{id}").unwrap(),
                (role, false) => writeln!(out, "{indent}[{role}] {quoted} #{id}").unwrap(),
            }

            let kids = self.visible_children(node);
            if !name.is_empty() && self.only_repeat(&kids, name) {
                continue;
            }
            stack.extend(kids.into_iter().rev().map(|k| (k, depth + 1)));
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
        // A malformed tree can link a node twice, or a wrapper into itself:
        // each node counts once, where it first turns up, and the walk ends.
        let mut visited = HashSet::new();
        while let Some(child) = stack.pop() {
            if !visited.insert(child["nodeId"].as_str().unwrap_or(""))
                || role(child) == "InlineTextBox"
            {
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
        let texts: Vec<&str> = kids.iter().map(|k| name(k)).collect();
        spells(&texts, parent_name)
    }
}

/// Whether `texts`, read in order, say `name`. A run of whitespace reads as
/// one space, and a space may also stand between two texts, since Chrome puts
/// one between block-level children when it builds a name from them. Anywhere
/// else a space is a word boundary: `Now here` does not say `Nowhere`.
fn spells(texts: &[&str], name: &str) -> bool {
    let name = squash(name);
    let mut rest = name.as_str();
    let mut first = true;
    // Whitespace since the last word, which the name has to have as well.
    let mut gap = false;
    for text in texts {
        gap |= text.starts_with(char::is_whitespace);
        let words = squash(text);
        if words.is_empty() {
            continue;
        }
        if !first {
            match rest.strip_prefix(' ') {
                Some(after) => rest = after,
                None if gap => return false,
                None => {}
            }
        }
        let Some(after) = rest.strip_prefix(words.as_str()) else {
            return false;
        };
        rest = after;
        gap = text.ends_with(char::is_whitespace);
        first = false;
    }
    rest.is_empty()
}

/// `s` with each run of whitespace as one space, and none at either end.
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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

    /// Repeated links, and a wrapper linked into itself, must not use up the
    /// walk before the siblings that follow them.
    #[test]
    fn repeated_links_do_not_hide_later_children() {
        let nodes = vec![
            node("1", "RootWebArea", "", None, &["2", "2", "2", "4", "3"]),
            node("2", "button", "Go", Some("1"), &[]),
            node("3", "button", "Stop", Some("1"), &[]),
            node("4", "generic", "", Some("1"), &["4", "2"]),
        ];
        assert_eq!(
            format_ax_tree(&nodes),
            "[RootWebArea] #1\n  [button] \"Go\" #2\n  [button] \"Stop\" #3\n"
        );
    }

    /// Text that only repeats the name is dropped, wherever Chrome joined the
    /// pieces with a space; text that puts its word breaks elsewhere is not.
    #[test]
    fn redundant_text_keeps_its_word_breaks() {
        assert!(spells(&["Home"], "Home"));
        // Block children: Chrome joins them with a space.
        assert!(spells(&["Save", "changes"], "Save changes"));
        // Inline children: no space between them.
        assert!(spells(&["Sub", "title"], "Subtitle"));
        assert!(spells(&["Hello", " ", "world"], "Hello world"));
        assert!(spells(&["Price: ", "$5"], "Price: $5"));
        assert!(spells(&["Buy", "\u{a0}now"], "Buy\u{a0}now"));
        assert!(spells(&["Read  more\nabout it"], "Read more about it"));
        assert!(!spells(&["Nowhere"], "Now here"));
        assert!(!spells(&["Now here"], "Nowhere"));
        assert!(!spells(&["Now ", "here"], "Nowhere"));
        assert!(!spells(&["Home"], "Go home"));

        let nodes = vec![
            node("1", "RootWebArea", "", None, &["2"]),
            node("2", "button", "Now here", Some("1"), &["3"]),
            node("3", "StaticText", "Nowhere", Some("2"), &[]),
        ];
        assert_eq!(
            format_ax_tree(&nodes),
            "[RootWebArea] #1\n  [button] \"Now here\" #2\n    \"Nowhere\" #3\n"
        );
    }

    /// A line break, quote or backslash in a name stays inside its quotes,
    /// so every row of the outline is one node.
    #[test]
    fn names_are_quoted_on_their_own_row() {
        let nodes = vec![
            node("1", "RootWebArea", "Say \"hi\"", None, &["2", "3", "4"]),
            node("2", "StaticText", "line one\nline \\two", Some("1"), &[]),
            node("3", "LineBreak", "\n", Some("1"), &[]),
            node("4", "button", "Go", Some("1"), &[]),
        ];
        assert_eq!(
            format_ax_tree(&nodes),
            "[RootWebArea] \"Say \\\"hi\\\"\" #1\n\
             \x20 \"line one\\nline \\\\two\" #2\n\
             \x20 [LineBreak] \"\\n\" #3\n\
             \x20 [button] \"Go\" #4\n"
        );
    }

    /// Nesting deeper than the call stack allows still prints, on a stack as
    /// small as the 1 MiB Windows gives the main thread, or smaller.
    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        const DEPTH: usize = 2000;
        let ids: Vec<String> = (1..=DEPTH + 1).map(|i| i.to_string()).collect();
        let mut nodes = vec![node("1", "RootWebArea", "", None, &["2"])];
        for i in 2..=DEPTH + 1 {
            let kids: Vec<&str> = ids.get(i).map(|k| k.as_str()).into_iter().collect();
            nodes.push(node(&ids[i - 1], "group", "g", Some(&ids[i - 2]), &kids));
        }
        let out = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || format_ax_tree(&nodes))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(out.lines().count(), DEPTH + 1);
        let last = out.lines().next_back().unwrap();
        assert_eq!(
            last,
            format!("{}[group] \"g\" #{}", "  ".repeat(DEPTH), DEPTH + 1)
        );
    }
}
