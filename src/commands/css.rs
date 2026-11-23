use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cdp::CdpClient;
use crate::format::{format_structured, OutputFormat};
use crate::result::CommandResult;

/// Options of `css-styles`.
#[derive(Debug, Default)]
pub struct CssOptions {
    /// Extra properties to show computed values for, beyond the declared ones.
    pub computed: Vec<String>,
    /// Also show rules from the browser's user-agent stylesheet.
    pub ua_rules: bool,
}

/// Properties that inherit by default. CDP has no call that says which do, so
/// the list follows the CSS specs (and Blink's `css_properties.json5`). A
/// property missing here only means its ancestor declarations are not shown.
const INHERITED: &[&str] = &[
    "-webkit-border-horizontal-spacing",
    "-webkit-border-vertical-spacing",
    "-webkit-font-smoothing",
    "-webkit-locale",
    "-webkit-tap-highlight-color",
    "-webkit-text-fill-color",
    "-webkit-text-security",
    "-webkit-text-size-adjust",
    "-webkit-text-stroke",
    "-webkit-text-stroke-color",
    "-webkit-text-stroke-width",
    "-webkit-user-modify",
    "accent-color",
    "border-collapse",
    "border-spacing",
    "caption-side",
    "caret-color",
    "clip-rule",
    "color",
    "color-interpolation",
    "color-interpolation-filters",
    "color-rendering",
    "color-scheme",
    "cursor",
    "direction",
    "dominant-baseline",
    "dynamic-range-limit",
    "empty-cells",
    "fill",
    "fill-opacity",
    "fill-rule",
    "font",
    "font-family",
    "font-feature-settings",
    "font-kerning",
    "font-language-override",
    "font-optical-sizing",
    "font-palette",
    "font-size",
    "font-size-adjust",
    "font-stretch",
    "font-style",
    "font-synthesis",
    "font-synthesis-position",
    "font-synthesis-small-caps",
    "font-synthesis-style",
    "font-synthesis-weight",
    "font-variant",
    "font-variant-alternates",
    "font-variant-caps",
    "font-variant-east-asian",
    "font-variant-emoji",
    "font-variant-ligatures",
    "font-variant-numeric",
    "font-variant-position",
    "font-variation-settings",
    "font-weight",
    "font-width",
    "forced-color-adjust",
    "hanging-punctuation",
    "hyphenate-character",
    "hyphenate-limit-chars",
    "hyphens",
    "image-orientation",
    "image-rendering",
    "interactivity",
    "letter-spacing",
    "line-break",
    "line-height",
    "list-style",
    "list-style-image",
    "list-style-position",
    "list-style-type",
    "marker",
    "marker-end",
    "marker-mid",
    "marker-start",
    "math-depth",
    "math-shift",
    "math-style",
    "orphans",
    "overflow-wrap",
    "paint-order",
    "pointer-events",
    "print-color-adjust",
    "quotes",
    "ruby-align",
    "ruby-position",
    "shape-rendering",
    "speak",
    "stroke",
    "stroke-dasharray",
    "stroke-dashoffset",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-opacity",
    "stroke-width",
    "tab-size",
    "text-align",
    "text-align-last",
    "text-anchor",
    "text-autospace",
    "text-decoration-skip-ink",
    "text-emphasis",
    "text-emphasis-color",
    "text-emphasis-position",
    "text-emphasis-style",
    "text-indent",
    "text-justify",
    "text-orientation",
    "text-rendering",
    "text-shadow",
    "text-size-adjust",
    "text-spacing-trim",
    "text-transform",
    "text-underline-offset",
    "text-underline-position",
    "text-wrap",
    "text-wrap-mode",
    "text-wrap-style",
    "visibility",
    "white-space",
    "white-space-collapse",
    "widows",
    "word-break",
    "word-spacing",
    "word-wrap",
    "writing-mode",
];

/// Computed values always shown, as DevTools' Styles pane does.
const ALWAYS_COMPUTED: &[&str] = &["display", "width", "height"];

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Active,
    Overridden,
    Invalid,
    Disabled,
}

/// One declaration, as the cascade sees it and the output shows it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Decl {
    name: String,
    /// The value without `!important`.
    value: String,
    /// The value with `var()` references replaced, when that differs.
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    important: bool,
    /// `Invalid` or `Disabled` when the declaration can't apply at all,
    /// otherwise `Overridden` until the cascade finds it wins a longhand.
    status: Status,
    /// The longhands this declaration sets (just its name for a longhand).
    #[serde(skip)]
    longhands: Vec<String>,
    /// A shorthand CDP lists without its longhands (one using `var()`).
    #[serde(skip)]
    unexpanded: bool,
}

impl Decl {
    fn is_shorthand(&self) -> bool {
        self.longhands.len() != 1 || self.longhands[0] != self.name
    }

    fn usable(&self) -> bool {
        !matches!(self.status, Status::Invalid | Status::Disabled)
    }
}

/// What the shown declarations set, for picking computed values: shorthands
/// are kept whole so their computed value can stand in for their longhands.
#[derive(Debug, Default)]
struct Declared {
    longhands: HashSet<String>,
    shorthands: Vec<(String, Vec<String>)>,
}

impl Declared {
    fn add(&mut self, d: &Decl) {
        if !d.is_shorthand() {
            self.longhands.insert(d.name.clone());
        } else if !self.shorthands.iter().any(|(n, _)| *n == d.name) {
            self.shorthands.push((d.name.clone(), d.longhands.clone()));
        }
    }
}

/// A style that applies to a node: its inline style, a matched rule, or the
/// style its presentational attributes produce.
#[derive(Debug, Serialize)]
struct Block {
    selector: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    /// `inline`, `attributes`, or the rule's CDP origin (`regular`,
    /// `user-agent`, `injected`, `inspector`).
    origin: String,
    /// Enclosing at-rules and nesting parents, outermost first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    conditions: Vec<String>,
    /// The cascade layer the rule is in (see `layer_key`), empty for none.
    #[serde(skip)]
    layer: String,
    #[serde(rename = "properties")]
    decls: Vec<Decl>,
}

#[derive(Debug, Serialize)]
struct InheritedView {
    from: String,
    rules: Vec<Block>,
}

/// What the page says about the element and its ancestors, which CDP's style
/// calls don't: a readable label for each, the custom property values an
/// ancestor's `var()` references resolve to there, the element's computed
/// shorthands (CDP's computed style lists longhands only), and the longhands
/// of the shorthands CDP didn't expand.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PageInfo {
    element: String,
    ancestors: Vec<AncestorInfo>,
    shorthands: HashMap<String, String>,
    longhands: HashMap<String, Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AncestorInfo {
    label: String,
    /// The referenced custom properties the ancestor has, empty ones
    /// included; one it doesn't have is left out.
    vars: HashMap<String, String>,
}

#[derive(Debug)]
struct Report {
    element: String,
    computed: Vec<(String, String)>,
    rules: Vec<Block>,
    inherited: Vec<InheritedView>,
    hidden_ua_rules: usize,
}

/// A JSON array's items, or none.
fn items(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// The styles CDP matched for the element or one inherited level: the inline
/// style, then each matched rule's.
fn level_styles(level: &Value) -> impl Iterator<Item = &Value> {
    std::iter::once(&level["inlineStyle"]).chain(
        items(&level["matchedCSSRules"])
            .iter()
            .map(|r| &r["rule"]["style"]),
    )
}

/// Custom properties registered not to inherit. CDP lists the registration in
/// effect for each name: a stylesheet `@property` rule in `cssPropertyRules`,
/// or a `CSS.registerProperty` call in `cssPropertyRegistrations`.
fn non_inheriting(matched: &Value) -> HashSet<String> {
    let rules = items(&matched["cssPropertyRules"]).iter().filter_map(|r| {
        let inherits = items(&r["style"]["cssProperties"])
            .iter()
            .find(|p| p["name"] == "inherits")?;
        // The authored text keeps its case (`inherits: FALSE`).
        let off = inherits["value"]
            .as_str()?
            .trim()
            .eq_ignore_ascii_case("false");
        r["propertyName"]["text"].as_str().filter(|_| off)
    });
    let scripts = items(&matched["cssPropertyRegistrations"])
        .iter()
        .filter(|r| r["inherits"] == false)
        .filter_map(|r| r["propertyName"].as_str());
    rules.chain(scripts).map(String::from).collect()
}

fn is_inherited(name: &str, non_inheriting: &HashSet<String>) -> bool {
    if name.starts_with("--") {
        // Custom properties inherit unless registered otherwise.
        return !non_inheriting.contains(name);
    }
    INHERITED.contains(&name)
}

fn strip_important(value: &str) -> &str {
    let trimmed = value.trim_end();
    if let Some(pos) = trimmed.rfind('!') {
        if trimmed[pos + 1..].trim().eq_ignore_ascii_case("important") {
            return trimmed[..pos].trim_end();
        }
    }
    trimmed
}

/// The declarations of a CDP `CSSStyle`, each with the longhands it sets.
///
/// An authored style lists its declarations (those with a source range) and
/// then every longhand they expand to. A shorthand names its longhands in
/// `longhandProperties`, except when it uses `var()`: Chrome can't expand it
/// before substitution. Then the page's expansion in `expanded` says what it
/// sets, since the longhands needn't be named after it (`border-color` sets
/// `border-top-color`, `font` sets `line-height`). Without one, it takes the
/// unclaimed trailing longhands named after it (`padding` takes
/// `padding-top`, ...). User-agent and attribute styles have no ranges and
/// list longhands only.
fn declarations(style: &Value, expanded: &HashMap<String, Vec<String>>) -> Vec<Decl> {
    let props: Vec<&Value> = style["cssProperties"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let has_ranges = props.iter().any(|p| p.get("range").is_some());
    let (authored, trailing): (Vec<&Value>, Vec<&Value>) = if has_ranges {
        props.iter().partition(|p| p.get("range").is_some())
    } else {
        (props.clone(), props.clone())
    };
    let trailing_names: Vec<&str> = trailing.iter().filter_map(|p| p["name"].as_str()).collect();
    let name_of = |p: &Value| p["name"].as_str().unwrap_or("").to_string();

    let mut longhands: Vec<Option<Vec<String>>> = authored
        .iter()
        .map(|p| {
            let listed: Vec<String> = p["longhandProperties"]
                .as_array()
                .map(|a| a.iter().map(name_of).collect())
                .unwrap_or_default();
            if !listed.is_empty() {
                Some(listed)
            } else if trailing_names.contains(&p["name"].as_str().unwrap_or("")) {
                Some(vec![name_of(p)])
            } else {
                None
            }
        })
        .collect();
    let unexpanded: Vec<bool> = longhands.iter().map(Option::is_none).collect();
    let claimed: HashSet<String> = longhands.iter().flatten().flatten().cloned().collect();
    for (p, slot) in authored.iter().zip(longhands.iter_mut()) {
        if slot.is_none() {
            let name = name_of(p);
            if let Some(parts) = expanded.get(&name).filter(|parts| !parts.is_empty()) {
                *slot = Some(parts.clone());
                continue;
            }
            let prefix = format!("{name}-");
            let found: Vec<String> = trailing_names
                .iter()
                .filter(|n| n.starts_with(&prefix) && !claimed.contains(**n))
                .map(|n| n.to_string())
                .collect();
            *slot = Some(if found.is_empty() { vec![name] } else { found });
        }
    }

    authored
        .iter()
        .zip(longhands)
        .zip(unexpanded)
        .map(|((p, longhands), unexpanded)| {
            let important = p["important"].as_bool().unwrap_or(false);
            let raw = p["value"].as_str().unwrap_or("");
            let status = if p["parsedOk"].as_bool() == Some(false) {
                Status::Invalid
            } else if p["disabled"].as_bool() == Some(true) {
                Status::Disabled
            } else {
                Status::Overridden
            };
            Decl {
                name: name_of(p),
                value: if important { strip_important(raw) } else { raw }.to_string(),
                resolved: None,
                important,
                status,
                longhands: longhands.unwrap_or_default(),
                unexpanded,
            }
        })
        .collect()
}

/// Last path component of a stylesheet URL, as DevTools names sources.
fn file_name(url: &str) -> String {
    if url.is_empty() {
        return "<style>".to_string();
    }
    if url.starts_with("data:") {
        return "data-uri".to_string();
    }
    if url.starts_with("blob:") {
        return "blob".to_string();
    }
    let url = url.split(['?', '#']).next().unwrap_or(url);
    // The host is not a file name, so `http://host` is the index too.
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("", |i| &rest[i..]),
        None => url,
    };
    match path.rsplit('/').next() {
        Some(last) if !last.is_empty() => last.to_string(),
        _ => "(index)".to_string(),
    }
}

/// Where a rule comes from: `file:line` (1-based, in the file the sheet lives
/// in, so an inline `<style>` gives the document's line), or the origin when
/// there is no file. `None` when CDP never announced the sheet.
fn rule_source(rule: &Value, sheets: &HashMap<String, Value>) -> Option<String> {
    match rule["origin"].as_str() {
        Some("user-agent") => return Some("user agent stylesheet".into()),
        Some("injected") => return Some("injected stylesheet".into()),
        Some("inspector") => return Some("via inspector".into()),
        _ => {}
    }
    let header = sheets.get(rule["styleSheetId"].as_str()?)?;
    let url = header["sourceURL"].as_str().unwrap_or("");
    if url.is_empty() && header["isConstructed"].as_bool() == Some(true) {
        return Some("constructed stylesheet".into());
    }
    let line = rule["selectorList"]["selectors"][0]["range"]["startLine"]
        .as_u64()
        .or_else(|| rule["style"]["range"]["startLine"].as_u64())?;
    let start = header["startLine"].as_u64().unwrap_or(0);
    Some(format!("{}:{}", file_name(url), start + line + 1))
}

/// Enclosing at-rules and nesting parents of a rule, outermost first. CDP
/// lists each kind innermost first, and `ruleTypes` says how they interleave.
fn conditions(rule: &Value) -> Vec<String> {
    let text = |v: &Value| v["text"].as_str().unwrap_or("").trim().to_string();
    let at = |keyword: &str, v: &Value| match text(v) {
        t if t.is_empty() => keyword.to_string(),
        t => format!("{keyword} {t}"),
    };
    let fmt_media = |m: &Value| format!("@media {}", text(m));
    let fmt_container = |c: &Value| {
        let query = text(c);
        match c["name"]
            .as_str()
            .filter(|n| !n.is_empty() && !query.starts_with(n))
        {
            Some(name) => format!("@container {name} {query}"),
            None => format!("@container {query}"),
        }
    };
    let (rule_media, sheet_media): (Vec<&Value>, Vec<&Value>) = items(&rule["media"])
        .iter()
        .partition(|m| m["source"].as_str() == Some("mediaRule"));
    let mut media = rule_media.into_iter().map(fmt_media);
    let mut supports = items(&rule["supports"])
        .iter()
        .map(|s| format!("@supports {}", text(s)));
    let mut containers = items(&rule["containerQueries"]).iter().map(fmt_container);
    let mut layers = items(&rule["layers"]).iter().map(|l| at("@layer", l));
    let mut scopes = items(&rule["scopes"]).iter().map(|s| at("@scope", s));
    let mut nesting = items(&rule["nestingSelectors"])
        .iter()
        .filter_map(|s| s.as_str().map(String::from));

    let mut inner_first = Vec::new();
    match items(&rule["ruleTypes"]) {
        // Older Chrome: no interleaving info, so group by kind.
        [] => inner_first.extend(
            scopes
                .chain(containers)
                .chain(supports)
                .chain(media)
                .chain(layers),
        ),
        rule_types => {
            for t in rule_types {
                inner_first.extend(match t.as_str().unwrap_or("") {
                    "MediaRule" => media.next(),
                    "SupportsRule" => supports.next(),
                    "ContainerRule" => containers.next(),
                    "LayerRule" => layers.next(),
                    "ScopeRule" => scopes.next(),
                    "StyleRule" => nesting.next(),
                    "StartingStyleRule" => Some("@starting-style".to_string()),
                    _ => None,
                });
            }
        }
    }
    // A <link media> or @import condition encloses everything in the sheet.
    inner_first.extend(
        sheet_media
            .into_iter()
            .filter(|m| !matches!(text(m).as_str(), "" | "all"))
            .map(fmt_media),
    );
    inner_first.reverse();
    inner_first
}

/// A key for a rule's cascade layer, the same for every rule in it: the
/// dotted layer path (`@layer a.b` is `@layer a { @layer b }`), with an
/// anonymous layer, which can't be reopened, named by where it starts. Empty
/// for an unlayered rule.
fn layer_key(rule: &Value) -> String {
    items(&rule["layers"])
        .iter()
        .map(|l| match l["text"].as_str() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => format!(
                "{}:{}:{}",
                l["styleSheetId"], l["range"]["startLine"], l["range"]["startColumn"]
            ),
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn rule_block(
    rule_match: &Value,
    sheets: &HashMap<String, Value>,
    expanded: &HashMap<String, Vec<String>>,
) -> Block {
    let rule = &rule_match["rule"];
    let selectors = rule["selectorList"]["selectors"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    // A selector list names the selectors that matched, so `h1, .btn` shows
    // as `.btn` when only that one did.
    let matching: Vec<&str> = rule_match["matchingSelectors"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|i| selectors.get(i.as_u64()? as usize)?["text"].as_str())
                .collect()
        })
        .unwrap_or_default();
    let selector = if selectors.len() > 1 && !matching.is_empty() {
        matching.join(", ")
    } else {
        rule["selectorList"]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    Block {
        selector,
        source: rule_source(rule, sheets),
        origin: rule["origin"].as_str().unwrap_or("regular").to_string(),
        conditions: conditions(rule),
        layer: layer_key(rule),
        decls: declarations(&rule["style"], expanded),
    }
}

/// A node's styles, highest precedence first: inline style, matched rules
/// (CDP lists them lowest first), with presentational attributes below the
/// author rules but above the user-agent ones, as the author origin's
/// lowest-precedence rules.
fn node_blocks(
    inline: &Value,
    rules: &Value,
    attributes: &Value,
    sheets: &HashMap<String, Value>,
    expanded: &HashMap<String, Vec<String>>,
) -> Vec<Block> {
    let mut blocks = Vec::new();
    if inline.is_object() {
        blocks.push(Block {
            selector: "element.style".into(),
            source: None,
            origin: "inline".into(),
            conditions: vec![],
            layer: String::new(),
            decls: declarations(inline, expanded),
        });
    }
    if let Some(rules) = rules.as_array() {
        blocks.extend(rules.iter().rev().map(|r| rule_block(r, sheets, expanded)));
    }
    if attributes.is_object() {
        let at = blocks
            .iter()
            .position(|b| b.origin == "user-agent")
            .unwrap_or(blocks.len());
        blocks.insert(
            at,
            Block {
                selector: "[attributes style]".into(),
                source: None,
                origin: "attributes".into(),
                conditions: vec![],
                layer: String::new(),
                decls: declarations(attributes, expanded),
            },
        );
    }
    blocks.retain(|b| !b.decls.is_empty());
    blocks
}

/// Run the cascade over one node's blocks, marking each declaration that wins
/// at least one of its longhands active. `claimed` longhands are already
/// decided (set on the element itself, or on a nearer ancestor) and nothing
/// here wins them. Returns the longhands decided here. A winning `inherit`
/// or `unset` decides none: the value comes from an ancestor's declaration,
/// so that one still applies.
///
/// `!important` beats normal, and an important declaration is ranked by
/// `important_order`.
fn cascade(blocks: &mut [Block], claimed: &HashSet<String>) -> HashSet<String> {
    let mut won = HashSet::new();
    let mut decided = HashSet::new();
    let ranked = important_order(blocks);
    let in_order: Vec<usize> = (0..blocks.len()).collect();
    for (important, order) in [(true, &ranked), (false, &in_order)] {
        for &bi in order {
            // Within a block the last declaration wins.
            for d in blocks[bi].decls.iter_mut().rev() {
                if d.important != important || !d.usable() {
                    continue;
                }
                let value = d.value.trim();
                let from_parent =
                    value.eq_ignore_ascii_case("inherit") || value.eq_ignore_ascii_case("unset");
                for longhand in &d.longhands {
                    if !claimed.contains(longhand) && won.insert(longhand.clone()) {
                        d.status = Status::Active;
                        if !from_parent {
                            decided.insert(longhand.clone());
                        }
                    }
                }
            }
        }
    }
    decided
}

/// The blocks in the order their `!important` declarations take precedence.
/// Importance reverses origins and cascade layers: a user-agent declaration
/// beats an author one, a layered one beats an unlayered one, and an earlier
/// layer beats a later one. The style attribute beats every rule either way.
/// The blocks come in normal order, which CDP sorts by layer first, so each
/// layer's rules are one run of them; reversing the runs keeps the order
/// within a layer.
fn important_order(blocks: &[Block]) -> Vec<usize> {
    let of = |keep: fn(&str) -> bool| (0..blocks.len()).filter(move |&i| keep(&blocks[i].origin));
    let rules: Vec<usize> = of(|o| o != "user-agent" && o != "inline").collect();
    of(|o| o == "user-agent")
        .chain(of(|o| o == "inline"))
        .chain(
            rules
                .chunk_by(|&a, &b| blocks[a].layer == blocks[b].layer)
                .rev()
                .flatten()
                .copied(),
        )
        .collect()
}

/// Byte offset of the next `var(` that is a function call: not the end of a
/// longer name such as `somevar(`, and not text in a string such as
/// `content: "var(--x)"`, which the browser shows as written.
fn find_var(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut quote = None;
    let mut i = 0;
    while i < bytes.len() {
        let prev = i.checked_sub(1).map(|p| bytes[p]);
        let call = bytes
            .get(i..i + 4)
            .is_some_and(|w| w.eq_ignore_ascii_case(b"var("))
            && !prev.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_');
        match (quote, bytes[i]) {
            // An escaped character is never a quote or the start of `var(`.
            (_, b'\\') => i += 1,
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, c @ (b'"' | b'\'')) => quote = Some(c),
            (None, _) if call => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Offset of the `)` closing a function whose arguments start at `s[0]`, and
/// of the first top-level comma before it, skipping nested parentheses and
/// strings.
fn scan_args(s: &str) -> Option<(usize, Option<usize>)> {
    let (mut depth, mut quote, mut comma) = (0usize, None::<char>, None);
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') if depth == 0 => return Some((i, comma)),
            (None, ')') => depth -= 1,
            (None, ',') if depth == 0 && comma.is_none() => comma = Some(i),
            _ => {}
        }
    }
    None
}

/// A `var()` call whose arguments start at `args[0]`: the trimmed property
/// name, the fallback, and the offset of the closing `)`.
fn split_var(args: &str) -> Option<(&str, Option<&str>, usize)> {
    let (close, comma) = scan_args(args)?;
    Some(match comma {
        Some(c) => (args[..c].trim(), Some(&args[c + 1..close]), close),
        None => (args[..close].trim(), None, close),
    })
}

/// Replace every `var(--x, fallback)` in `value`. `lookup` is `None` for a
/// custom property that isn't defined; an empty one (`--x: ;`) is defined,
/// and its empty value replaces the reference rather than the fallback.
/// `None` when a reference has no value and no fallback, which makes the
/// declaration invalid at computed-value time, so there is nothing to show.
fn substitute(
    value: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    depth: usize,
) -> Option<String> {
    if depth > 16 {
        return None;
    }
    let mut out = String::new();
    let mut rest = value;
    while let Some(pos) = find_var(rest) {
        out.push_str(&rest[..pos]);
        let args = &rest[pos + 4..];
        let (name, fallback, close) = split_var(args)?;
        match lookup(name) {
            Some(v) => out.push_str(&v),
            None => out.push_str(&substitute(fallback?.trim(), lookup, depth + 1)?),
        }
        rest = &args[close + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// `value` with its `var()` references resolved, when it has any and the
/// result says something the authored value doesn't.
fn resolve_vars(value: &str, vars: &HashMap<String, String>) -> Option<String> {
    find_var(value)?;
    let lookup = |name: &str| vars.get(name).map(|v| v.trim().to_string());
    let resolved = substitute(value, &lookup, 0)?;
    (resolved.trim() != value.trim()).then(|| resolved.trim().to_string())
}

/// Custom property names a value references, fallbacks included.
fn var_names(value: &str, out: &mut Vec<String>) {
    let mut rest = value;
    while let Some(pos) = find_var(rest) {
        let args = &rest[pos + 4..];
        let Some((name, fallback, close)) = split_var(args) else {
            return;
        };
        if name.starts_with("--") && !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
        if let Some(fallback) = fallback {
            var_names(fallback, out);
        }
        rest = &args[close + 1..];
    }
}

/// The custom properties each ancestor's inheritable declarations reference,
/// so the page can be asked for their values on that ancestor.
fn ancestor_var_names(matched: &Value) -> Vec<Vec<String>> {
    let non_inheriting = non_inheriting(matched);
    let none = HashMap::new();
    items(&matched["inherited"])
        .iter()
        .map(|level| {
            let mut names = Vec::new();
            for d in level_styles(level).flat_map(|s| declarations(s, &none)) {
                if is_inherited(&d.name, &non_inheriting) {
                    var_names(&d.value, &mut names);
                }
            }
            names
        })
        .collect()
}

/// The shorthands the element's and its ancestors' styles declare that CDP
/// didn't expand, so the page can say what they set.
fn unexpanded_shorthands(matched: &Value) -> Vec<String> {
    let none = HashMap::new();
    let mut names: Vec<String> = Vec::new();
    let levels = std::iter::once(matched).chain(items(&matched["inherited"]));
    for d in levels
        .flat_map(level_styles)
        .flat_map(|s| declarations(s, &none))
    {
        if d.unexpanded && d.usable() && !names.contains(&d.name) {
            names.push(d.name);
        }
    }
    names
}

/// The blocks worth showing, with `var()` references resolved, noting what
/// they declare. User-agent blocks are only counted unless asked for. `vars`
/// is `None` when the node's custom properties are unknown, and then nothing
/// is resolved rather than guessed.
fn shown_blocks(
    blocks: Vec<Block>,
    vars: Option<&HashMap<String, String>>,
    ua_rules: bool,
    hidden_ua: &mut usize,
    declared: &mut Declared,
) -> Vec<Block> {
    let mut out = Vec::new();
    for mut block in blocks {
        if block.decls.is_empty() {
            continue;
        }
        if block.origin == "user-agent" && !ua_rules {
            *hidden_ua += 1;
            continue;
        }
        for d in &mut block.decls {
            if d.usable() {
                declared.add(d);
            }
            d.resolved = vars.and_then(|vars| resolve_vars(&d.value, vars));
        }
        out.push(block);
    }
    out
}

/// The computed values worth showing: display, width and height, what the
/// shown declarations set, and the properties asked for. A shorthand shows as
/// one value when Chrome can serialize it (`padding: 16px 8px`) and as its
/// longhands when it can't; custom properties show only when asked for.
fn pick_computed(
    computed: &[(String, String)],
    declared: &Declared,
    shorthand_values: &HashMap<String, String>,
    extra: &[String],
) -> Vec<(String, String)> {
    let values: HashMap<&str, &str> = computed
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    let shorthand = |name: &str| shorthand_values.get(name).filter(|v| !v.is_empty());
    let mut picked: Vec<(String, String)> = Vec::new();
    let mut push = |name: &str, value: &str| {
        if !ALWAYS_COMPUTED.contains(&name) && !picked.iter().any(|(n, _)| n == name) {
            picked.push((name.to_string(), value.to_string()));
        }
    };
    let mut longhands: Vec<String> = declared
        .longhands
        .iter()
        .filter(|n| !n.starts_with("--"))
        .cloned()
        .collect();
    for (name, parts) in &declared.shorthands {
        if let Some(value) = shorthand(name) {
            push(name, value);
            continue;
        }
        // Chrome can't write one value for some shorthands (`border` with
        // sides that differ), but often can for the parts in between
        // (`border-top`), which beats listing every longhand.
        for part in parts {
            match parent_shorthand(part)
                .filter(|p| p != name)
                .and_then(|p| shorthand(p).map(|v| (p, v)))
            {
                Some((p, value)) => push(p, value),
                None => longhands.push(part.clone()),
            }
        }
    }
    for name in extra {
        if values.contains_key(name.as_str()) {
            longhands.push(name.clone());
        } else if let Some(value) = shorthand(name) {
            push(name, value);
        } else if !name.starts_with("--") {
            // A custom property has no longhands: `--theme` is not the
            // shorthand of `--theme-accent`.
            let prefix = format!("{name}-");
            longhands.extend(
                computed
                    .iter()
                    .filter(|(n, _)| n.starts_with(&prefix))
                    .map(|(n, _)| n.clone()),
            );
        }
    }
    for name in &longhands {
        // A var() shorthand Chrome couldn't expand is filed under its own name.
        if let Some(value) = values
            .get(name.as_str())
            .copied()
            .or_else(|| shorthand(name).map(String::as_str))
        {
            push(name, value);
        }
    }
    picked.sort();
    ALWAYS_COMPUTED
        .iter()
        .filter_map(|n| values.get(n).map(|v| (n.to_string(), v.to_string())))
        .chain(picked)
        .collect()
}

/// The shorthand a longhand's name suggests one level up: `border-top-width`
/// gives `border-top`. Only a guess; a name that isn't a property reads as
/// empty in the page and is passed over.
fn parent_shorthand(longhand: &str) -> Option<&str> {
    longhand
        .rsplit_once('-')
        .map(|(head, _)| head)
        .filter(|head| !head.is_empty() && *head != "-webkit" && head.contains(char::is_alphabetic))
}

/// Properties to read from the element's computed style in the page, because
/// CDP's list has longhands only: the shorthands its styles declare (and
/// declared names CDP doesn't list, such as a var() shorthand it couldn't
/// expand), the ones between shorthands and their longhands, and the extra
/// names asked for.
fn element_props_to_read(
    matched: &Value,
    computed: &[(String, String)],
    extra: &[String],
) -> Vec<String> {
    let listed: HashSet<&str> = computed.iter().map(|(n, _)| n.as_str()).collect();
    let non_inheriting = non_inheriting(matched);
    let own = level_styles(matched).map(|style| (style, false));
    let inherited = items(&matched["inherited"])
        .iter()
        .flat_map(|level| level_styles(level).map(|style| (style, true)));
    let mut names: Vec<String> = Vec::new();
    let mut add = |name: &str| {
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    };
    for (style, ancestor) in own.chain(inherited) {
        for d in declarations(style, &HashMap::new()) {
            if !d.usable()
                || d.name.starts_with("--")
                || (ancestor && !is_inherited(&d.name, &non_inheriting))
            {
                continue;
            }
            if d.is_shorthand() {
                add(&d.name);
                for part in &d.longhands {
                    if let Some(p) = parent_shorthand(part).filter(|p| *p != d.name) {
                        add(p);
                    }
                }
            } else if !listed.contains(d.name.as_str()) {
                add(&d.name);
            }
        }
    }
    for e in extra {
        if !e.starts_with("--") {
            add(e);
        }
    }
    names
}

/// Turn CDP's matched and computed styles into what `css-styles` prints.
fn build_report(
    matched: &Value,
    computed: &[(String, String)],
    sheets: &HashMap<String, Value>,
    page: &PageInfo,
    opts: &CssOptions,
) -> Report {
    let non_inheriting = non_inheriting(matched);
    let mut hidden_ua = 0;
    let mut declared = Declared::default();

    // The element's own styles. Custom properties resolve against the
    // element's computed values, which list each defined one, empty or not.
    let element_vars: HashMap<String, String> = computed
        .iter()
        .filter(|(n, _)| n.starts_with("--"))
        .cloned()
        .collect();
    let mut blocks = node_blocks(
        &matched["inlineStyle"],
        &matched["matchedCSSRules"],
        &matched["attributesStyle"],
        sheets,
        &page.longhands,
    );
    let mut claimed = cascade(&mut blocks, &HashSet::new());
    let rules = shown_blocks(
        blocks,
        Some(&element_vars),
        opts.ua_rules,
        &mut hidden_ua,
        &mut declared,
    );

    // Ancestors, nearest first. Only inheritable declarations reach the
    // element, and only where neither it nor a nearer ancestor sets them.
    let mut inherited = Vec::new();
    for (i, level) in items(&matched["inherited"]).iter().enumerate() {
        let info = page.ancestors.get(i);
        let mut blocks = node_blocks(
            &level["inlineStyle"],
            &level["matchedCSSRules"],
            &Value::Null,
            sheets,
            &page.longhands,
        );
        for block in &mut blocks {
            block
                .decls
                .retain(|d| is_inherited(&d.name, &non_inheriting));
        }
        let set_here = cascade(&mut blocks, &claimed);
        claimed.extend(set_here);
        let rules = shown_blocks(
            blocks,
            info.map(|a| &a.vars),
            opts.ua_rules,
            &mut hidden_ua,
            &mut declared,
        );
        if !rules.is_empty() {
            inherited.push(InheritedView {
                from: info.map_or_else(|| format!("ancestor {}", i + 1), |a| a.label.clone()),
                rules,
            });
        }
    }

    Report {
        element: page.element.clone(),
        computed: pick_computed(computed, &declared, &page.shorthands, &opts.computed),
        rules,
        inherited,
        hidden_ua_rules: hidden_ua,
    }
}

fn write_rule(out: &mut String, rule: &Block, indent: usize) {
    let pad = |n: usize| "  ".repeat(n);
    let mut depth = indent;
    for c in &rule.conditions {
        writeln!(out, "{}{c} {{", pad(depth)).unwrap();
        depth += 1;
    }
    match &rule.source {
        Some(src) => writeln!(out, "{}{} ({src}) {{", pad(depth), rule.selector).unwrap(),
        None => writeln!(out, "{}{} {{", pad(depth), rule.selector).unwrap(),
    }
    for p in &rule.decls {
        let tag = match p.status {
            Status::Active => "",
            Status::Overridden => "[overridden] ",
            Status::Invalid => "[invalid] ",
            Status::Disabled => "[disabled] ",
        };
        let important = if p.important { " !important" } else { "" };
        let resolved = p
            .resolved
            .as_ref()
            .map(|r| format!(" /* {} */", r.replace("*/", "*\\/")))
            .unwrap_or_default();
        writeln!(
            out,
            "{}{tag}{}: {}{important};{resolved}",
            pad(depth + 1),
            p.name,
            p.value
        )
        .unwrap();
    }
    writeln!(out, "{}}}", pad(depth)).unwrap();
    while depth > indent {
        depth -= 1;
        writeln!(out, "{}}}", pad(depth)).unwrap();
    }
}

fn format_text(report: &Report) -> String {
    let mut out = String::new();
    writeln!(out, "Element: {}", report.element).unwrap();
    if !report.computed.is_empty() {
        out.push_str("Computed:\n");
        for (name, value) in &report.computed {
            writeln!(out, "  {name}: {value};").unwrap();
        }
    }
    if !report.rules.is_empty() {
        out.push_str("Rules (highest precedence first):\n");
        for rule in &report.rules {
            write_rule(&mut out, rule, 1);
        }
    }
    for group in &report.inherited {
        writeln!(out, "Inherited from {}:", group.from).unwrap();
        for rule in &group.rules {
            write_rule(&mut out, rule, 1);
        }
    }
    if report.hidden_ua_rules > 0 {
        writeln!(
            out,
            "({} user-agent rule(s) hidden; --ua-rules shows them)",
            report.hidden_ua_rules
        )
        .unwrap();
    }
    out.trim_end().to_string()
}

fn report_json(report: &Report) -> Result<Value> {
    let computed: serde_json::Map<String, Value> = report
        .computed
        .iter()
        .map(|(n, v)| (n.clone(), json!(v)))
        .collect();
    Ok(json!({
        "element": report.element,
        "computed": computed,
        "rules": serde_json::to_value(&report.rules)?,
        "inherited": serde_json::to_value(&report.inherited)?,
        "hiddenUserAgentRules": report.hidden_ua_rules,
    }))
}

/// Labels the element and its ancestors (flat-tree parents, as CDP walks them
/// for inherited styles), reads on each ancestor the custom properties its
/// declarations reference, reads the element's computed `props`, and expands
/// the shorthands in `expand` into their longhands. Runs with `this` bound to
/// the element.
const PAGE_INFO_JS: &str = r#"function (varNames, props, expand) {
  const label = (el) => {
    let s = el.localName;
    if (el.id) s += '#' + el.id;
    const classes = [...el.classList];
    if (classes.length) s += '.' + classes.slice(0, 3).join('.') + (classes.length > 3 ? '…' : '');
    return s;
  };
  const parent = (el) => el.assignedSlot || el.parentElement || (el.parentNode && el.parentNode.host) || null;
  const own = getComputedStyle(this);
  const shorthands = {};
  for (const p of props) shorthands[p] = own.getPropertyValue(p).trim();
  // A shorthand set on a scratch style lists the longhands it sets. The
  // HTML namespace gives the element a style in any document.
  const scratch = document.createElementNS('http://www.w3.org/1999/xhtml', 'div').style;
  const longhands = {};
  for (const p of expand) {
    scratch.cssText = '';
    scratch.setProperty(p, 'initial');
    longhands[p] = [...scratch];
  }
  const out = { element: label(this), ancestors: [], shorthands, longhands };
  let el = this;
  for (const names of varNames) {
    el = parent(el);
    if (!el) break;
    const cs = getComputedStyle(el);
    const vars = {};
    let defined;
    for (const n of names) {
      const v = cs.getPropertyValue(n).trim();
      // '' is both an empty value and no value. Only a defined custom
      // property is among the computed style's names.
      if (v || (defined = defined || new Set(cs)).has(n)) vars[n] = v;
    }
    out.ancestors.push({ label: label(el), vars });
  }
  return out;
}"#;

async fn page_info(
    client: &mut CdpClient,
    session_id: &str,
    node_id: i64,
    var_names: &[Vec<String>],
    props: &[String],
    expand: &[String],
) -> Result<PageInfo> {
    let resolved = client
        .send_to_target(
            session_id,
            "DOM.resolveNode",
            json!({"nodeId": node_id, "objectGroup": "css-styles"}),
        )
        .await?;
    let object_id = resolved["object"]["objectId"]
        .as_str()
        .context("DOM.resolveNode returned no objectId")?
        .to_string();
    let result = client
        .send_to_target(
            session_id,
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": PAGE_INFO_JS,
                "arguments": [{"value": var_names}, {"value": props}, {"value": expand}],
                "returnByValue": true,
            }),
        )
        .await;
    // Nothing to wait for: the reply only says it was done.
    let _ = client
        .send_raw_no_wait(
            Some(session_id),
            "Runtime.releaseObjectGroup",
            json!({"objectGroup": "css-styles"}),
        )
        .await;
    let result = result?;
    if let Some(e) = result.get("exceptionDetails") {
        bail!(
            "labelling the element failed: {}",
            crate::remote_object::describe_exception(e)
        );
    }
    Ok(PageInfo::deserialize(&result["result"]["value"])?)
}

async fn collect(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
    opts: &CssOptions,
) -> Result<Report> {
    // The CSS agent refuses to start unless the DOM agent is on. No need to
    // wait for it: CDP runs a session's commands in order.
    client
        .send_raw_no_wait(Some(session_id), "DOM.enable", json!({}))
        .await?;
    let doc = client
        .send_to_target(session_id, "DOM.getDocument", json!({"depth": 0}))
        .await?;
    let root = doc["root"]["nodeId"]
        .as_i64()
        .context("DOM.getDocument returned no root node")?;
    let found = client
        .send_to_target(
            session_id,
            "DOM.querySelector",
            json!({"nodeId": root, "selector": selector}),
        )
        .await
        .with_context(|| {
            format!("querySelector({selector:?}) failed; is it a valid CSS selector?")
        })?;
    let node_id = found["nodeId"].as_i64().unwrap_or(0);
    if node_id == 0 {
        bail!("No element matches selector {selector:?}");
    }

    // CSS.enable announces every stylesheet with CSS.styleSheetAdded before
    // it returns. Those headers are the only place a rule's file and line
    // offset come from, so they are taken out of the event buffer here.
    client
        .send_to_target(session_id, "CSS.enable", json!({}))
        .await?;
    let (added, others): (Vec<Value>, Vec<Value>) = std::mem::take(&mut client.events)
        .into_iter()
        .partition(|e| {
            e["method"].as_str() == Some("CSS.styleSheetAdded")
                && e["sessionId"].as_str() == Some(session_id)
        });
    client.events = others.into();
    let sheets: HashMap<String, Value> = added
        .into_iter()
        .filter_map(|mut e| {
            let header = e["params"]["header"].take();
            Some((header["styleSheetId"].as_str()?.to_string(), header))
        })
        .collect();

    let matched = client
        .send_to_target(
            session_id,
            "CSS.getMatchedStylesForNode",
            json!({"nodeId": node_id}),
        )
        .await?;
    let computed = client
        .send_to_target(
            session_id,
            "CSS.getComputedStyleForNode",
            json!({"nodeId": node_id}),
        )
        .await?;
    let computed: Vec<(String, String)> = computed["computedStyle"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|p| {
                    (
                        p["name"].as_str().unwrap_or("").to_string(),
                        p["value"].as_str().unwrap_or("").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    // What the page adds is a nicety: without it the output says
    // "ancestor N", leaves ancestor var() references unresolved, lists
    // shorthands' computed longhands one by one, and guesses what a var()
    // shorthand sets from its longhands' names.
    let names = ancestor_var_names(&matched);
    let props = element_props_to_read(&matched, &computed, &opts.computed);
    let expand = unexpanded_shorthands(&matched);
    let page = page_info(client, session_id, node_id, &names, &props, &expand)
        .await
        .unwrap_or_else(|_| PageInfo {
            element: selector.to_string(),
            ..Default::default()
        });

    Ok(build_report(&matched, &computed, &sheets, &page, opts))
}

/// Property names are case-insensitive; custom property names are not.
fn normalize_property_name(name: &str) -> String {
    let name = name.trim();
    if name.starts_with("--") {
        name.to_string()
    } else {
        name.to_ascii_lowercase()
    }
}

/// Show the styles that apply to the first element matching `selector`:
/// computed values, matched rules with their cascade status, inline style,
/// and what it inherits from its ancestors.
pub async fn css_styles(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
    mut opts: CssOptions,
    format: OutputFormat,
) -> Result<CommandResult> {
    opts.computed = opts
        .computed
        .iter()
        .map(|n| normalize_property_name(n))
        .collect();
    let report = collect(client, session_id, selector, &opts).await;
    // Turn the agents off again whatever happened. Left on, the CSS agent
    // keeps tracking stylesheets for the life of the tab's session, and the
    // next css-styles would get no CSS.styleSheetAdded events to name files.
    // CDP runs them in order, so waiting for the last one covers both.
    let _ = client
        .send_raw_no_wait(Some(session_id), "CSS.disable", json!({}))
        .await;
    let _ = client
        .send_to_target(session_id, "DOM.disable", json!({}))
        .await;
    let report = report?;

    if format.is_text() {
        Ok(CommandResult::output(format_text(&report)))
    } else {
        Ok(CommandResult::output(format_structured(
            &report_json(&report)?,
            format,
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(line: u64) -> Value {
        json!({"startLine": line, "startColumn": 0, "endLine": line, "endColumn": 1})
    }

    /// An authored CDP style: declarations with ranges, then their longhands.
    fn authored(decls: &[(&str, &str)], longhands: &[(&str, &str)]) -> Value {
        let mut props: Vec<Value> = decls
            .iter()
            .map(|(n, v)| {
                let mut p = json!({"name": n, "value": v, "range": range(0)});
                if v.ends_with("!important") {
                    p["important"] = json!(true);
                }
                p
            })
            .collect();
        props.extend(
            longhands
                .iter()
                .map(|(n, v)| json!({"name": n, "value": v})),
        );
        json!({"cssProperties": props})
    }

    fn rule(selector: &str, line: u64, style: Value) -> Value {
        json!({
            "rule": {
                "styleSheetId": "s1",
                "origin": "regular",
                "selectorList": {"text": selector, "selectors": [{"text": selector, "range": range(line)}]},
                "style": style,
            },
            "matchingSelectors": [0],
        })
    }

    fn sheets() -> HashMap<String, Value> {
        HashMap::from([(
            "s1".to_string(),
            json!({"styleSheetId": "s1", "sourceURL": "http://localhost:8000/app/page.html?x=1", "startLine": 6, "isInline": true}),
        )])
    }

    fn page() -> PageInfo {
        PageInfo {
            element: "button#go.btn".into(),
            ancestors: vec![AncestorInfo {
                label: "body".into(),
                vars: HashMap::from([("--fg".to_string(), "#222".to_string())]),
            }],
            shorthands: HashMap::from([("padding".to_string(), "8px".to_string())]),
            longhands: HashMap::new(),
        }
    }

    fn computed() -> Vec<(String, String)> {
        [
            ("display", "inline-block"),
            ("width", "40px"),
            ("height", "20px"),
            ("color", "rgb(255, 255, 255)"),
            ("padding-top", "8px"),
            ("padding-right", "8px"),
            ("padding-bottom", "8px"),
            ("padding-left", "8px"),
            ("z-index", "auto"),
            ("cursor", "default"),
            ("--gap", "8px"),
        ]
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect()
    }

    /// The fixture behind most tests: inline style, two author rules, a
    /// user-agent rule, and a body ancestor with inheritable and
    /// non-inheritable declarations.
    fn matched() -> Value {
        json!({
            "inlineStyle": authored(&[("color", "yellow")], &[("color", "yellow")]),
            "matchedCSSRules": [
                {
                    "rule": {
                        "origin": "user-agent",
                        "selectorList": {"text": "input, button", "selectors": [{"text": "input"}, {"text": "button"}]},
                        "style": {"cssProperties": [{"name": "color", "value": "buttontext"}, {"name": "cursor", "value": "default"}]},
                    },
                    "matchingSelectors": [1],
                },
                rule(".btn", 8, authored(
                    &[("color", "black"), ("padding", "var(--gap)")],
                    &[("color", "black"), ("padding-top", ""), ("padding-right", ""), ("padding-bottom", ""), ("padding-left", "")],
                )),
                rule(".btn.primary", 9, authored(
                    &[("color", "white !important")],
                    &[("color", "white !important")],
                )),
            ],
            "inherited": [{
                "matchedCSSRules": [rule("body", 2, authored(
                    &[("color", "var(--fg)"), ("font-family", "Georgia"), ("margin", "0")],
                    &[("color", "var(--fg)"), ("font-family", "Georgia"), ("margin-top", "0px")],
                ))],
            }],
            "cssPropertyRegistrations": [],
        })
    }

    fn report_for(m: &Value, opts: CssOptions) -> Report {
        build_report(m, &computed(), &sheets(), &page(), &opts)
    }

    fn block(style: &Value) -> Block {
        Block {
            selector: "x".into(),
            source: None,
            origin: "regular".into(),
            conditions: vec![],
            layer: String::new(),
            decls: declarations(style, &HashMap::new()),
        }
    }

    /// A rule in the cascade layer `layer` (`""` for an anonymous one, at
    /// `line`).
    fn layered(selector: &str, line: u64, layer: &str, style: Value) -> Value {
        let mut r = rule(selector, line, style);
        r["rule"]["layers"] = json!([{"text": layer, "styleSheetId": "s1", "range": range(line)}]);
        r
    }

    fn statuses(block: &Block) -> Vec<Status> {
        block.decls.iter().map(|d| d.status).collect()
    }

    #[test]
    fn report_marks_the_cascade_and_hides_user_agent_rules() {
        let report = report_for(&matched(), CssOptions::default());
        let text = format_text(&report);
        assert_eq!(
            text,
            "Element: button#go.btn
Computed:
  display: inline-block;
  width: 40px;
  height: 20px;
  color: rgb(255, 255, 255);
  padding: 8px;
Rules (highest precedence first):
  element.style {
    [overridden] color: yellow;
  }
  .btn.primary (page.html:16) {
    color: white !important;
  }
  .btn (page.html:15) {
    [overridden] color: black;
    padding: var(--gap); /* 8px */
  }
Inherited from body:
  body (page.html:9) {
    [overridden] color: var(--fg); /* #222 */
    font-family: Georgia;
  }
(1 user-agent rule(s) hidden; --ua-rules shows them)"
        );
    }

    /// A user-agent `color` on the element stops the ancestor's `color` from
    /// inheriting, even when the user-agent rule itself isn't shown.
    #[test]
    fn hidden_user_agent_rules_still_take_part_in_the_cascade() {
        let mut m = matched();
        m["matchedCSSRules"].as_array_mut().unwrap().truncate(1);
        m["inlineStyle"] = Value::Null;
        let report = report_for(&m, CssOptions::default());
        assert!(report.rules.is_empty());
        let body = &report.inherited[0].rules[0];
        assert_eq!(body.decls[0].name, "color");
        assert_eq!(body.decls[0].status, Status::Overridden);
        assert_eq!(report.hidden_ua_rules, 1);

        let shown = report_for(
            &m,
            CssOptions {
                ua_rules: true,
                ..Default::default()
            },
        );
        assert_eq!(shown.rules[0].selector, "button");
        assert_eq!(
            shown.rules[0].source.as_deref(),
            Some("user agent stylesheet")
        );
        assert_eq!(shown.hidden_ua_rules, 0);
    }

    #[test]
    fn json_report_shape() {
        let v = report_json(&report_for(&matched(), CssOptions::default())).unwrap();
        assert_eq!(v["element"], "button#go.btn");
        assert_eq!(v["computed"]["display"], "inline-block");
        assert_eq!(v["rules"][0]["origin"], "inline");
        assert_eq!(v["rules"][1]["properties"][0]["important"], true);
        assert_eq!(v["rules"][2]["properties"][1]["resolved"], "8px");
        assert_eq!(v["rules"][2]["properties"][0]["status"], "overridden");
        assert!(v["rules"][2]["properties"][0].get("important").is_none());
        assert_eq!(v["inherited"][0]["from"], "body");
        assert_eq!(v["hiddenUserAgentRules"], 1);
    }

    #[test]
    fn later_declaration_in_a_block_wins() {
        let style = authored(&[("color", "red"), ("color", "blue")], &[("color", "blue")]);
        let mut blocks = vec![block(&style)];
        cascade(&mut blocks, &HashSet::new());
        let status: Vec<Status> = blocks[0].decls.iter().map(|d| d.status).collect();
        assert_eq!(status, [Status::Overridden, Status::Active]);
    }

    /// A shorthand overrides a longhand from a lower-precedence rule, and
    /// stays active while any of its longhands still wins.
    #[test]
    fn shorthands_cascade_by_longhand() {
        let high = authored(&[("padding-top", "0")], &[("padding-top", "0")]);
        let mut low = authored(&[("padding", "4px")], &[]);
        low["cssProperties"][0]["longhandProperties"] =
            json!([{"name": "padding-top"}, {"name": "padding-bottom"}]);
        let lower = authored(&[("padding-bottom", "9px")], &[("padding-bottom", "9px")]);
        let mut blocks = vec![block(&high), block(&low), block(&lower)];
        cascade(&mut blocks, &HashSet::new());
        assert_eq!(blocks[1].decls[0].status, Status::Active);
        assert_eq!(blocks[2].decls[0].status, Status::Overridden);
    }

    #[test]
    fn invalid_and_disabled_declarations_are_tagged() {
        let mut style = authored(&[("colr", "red"), ("color", "red")], &[("color", "red")]);
        style["cssProperties"][0]["parsedOk"] = json!(false);
        style["cssProperties"][1]["disabled"] = json!(true);
        let decls = declarations(&style, &HashMap::new());
        assert_eq!(decls[0].status, Status::Invalid);
        assert_eq!(decls[1].status, Status::Disabled);
    }

    #[test]
    fn var_substitution() {
        let vars = HashMap::from([
            ("--gap".to_string(), "8px".to_string()),
            ("--brand".to_string(), " #0a84ff".to_string()),
        ]);
        assert_eq!(resolve_vars("var(--gap)", &vars).as_deref(), Some("8px"));
        assert_eq!(
            resolve_vars("calc(var(--gap) * 2) var(--gap)", &vars).as_deref(),
            Some("calc(8px * 2) 8px")
        );
        assert_eq!(
            resolve_vars("1px solid VAR(--brand)", &vars).as_deref(),
            Some("1px solid #0a84ff")
        );
        // Fallbacks, nested ones too, and a fallback with a comma in it.
        assert_eq!(
            resolve_vars("var(--missing, 3px)", &vars).as_deref(),
            Some("3px")
        );
        assert_eq!(
            resolve_vars("var(--missing, var(--gap))", &vars).as_deref(),
            Some("8px")
        );
        assert_eq!(
            resolve_vars("var(--font, Georgia, serif)", &vars).as_deref(),
            Some("Georgia, serif")
        );
        // No value and no fallback: invalid at computed-value time.
        assert_eq!(resolve_vars("var(--missing)", &vars), None);
        // Nothing to resolve.
        assert_eq!(resolve_vars("8px", &vars), None);
        assert_eq!(resolve_vars("somevar(--gap)", &vars), None);
    }

    #[test]
    fn var_names_include_fallbacks() {
        let mut names = Vec::new();
        var_names("var(--a, var(--b, 1px)) var( --c )", &mut names);
        assert_eq!(names, ["--a", "--b", "--c"]);
    }

    #[test]
    fn ancestor_var_names_skip_non_inheritable_declarations() {
        let mut m = matched();
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] =
            authored(&[("color", "var(--fg)"), ("margin", "var(--m)")], &[]);
        assert_eq!(ancestor_var_names(&m), vec![vec!["--fg".to_string()]]);
    }

    /// A stylesheet `@property` rule and a `CSS.registerProperty` call can
    /// each stop a custom property inheriting; an unregistered one inherits.
    #[test]
    fn registered_custom_property_can_opt_out_of_inheritance() {
        let inherits = |v: &str| json!({"cssProperties": [{"name": "inherits", "value": v}]});
        let regs = json!({
            "cssPropertyRules": [
                {"propertyName": {"text": "--rule"}, "style": inherits("FALSE")},
                {"propertyName": {"text": "--on"}, "style": inherits("true")},
            ],
            "cssPropertyRegistrations": [{"propertyName": "--script", "inherits": false}],
        });
        let off = non_inheriting(&regs);
        assert!(!is_inherited("--rule", &off));
        assert!(!is_inherited("--script", &off));
        assert!(is_inherited("--on", &off));
        assert!(is_inherited("--y", &off));
        assert!(is_inherited("color", &off));
        assert!(!is_inherited("margin", &off));

        // The body's `--rule` stays on the body: the report leaves it out,
        // and the page isn't asked about what it references.
        let mut m = matched();
        m["cssPropertyRules"] = regs["cssPropertyRules"].clone();
        let decls = [("--rule", "var(--a)"), ("--free", "var(--b)")];
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] = authored(&decls, &decls);
        assert_eq!(ancestor_var_names(&m), vec![vec!["--b".to_string()]]);
        let report = report_for(&m, CssOptions::default());
        let body: Vec<&str> = report.inherited[0].rules[0]
            .decls
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(body, ["--free"]);
    }

    #[test]
    fn sources_name_the_file_and_document_line() {
        let s = sheets();
        let r = rule(".a", 3, json!({}));
        assert_eq!(rule_source(&r["rule"], &s).as_deref(), Some("page.html:10"));
        assert_eq!(file_name("http://localhost:8000/"), "(index)");
        assert_eq!(file_name("http://localhost:8000"), "(index)");
        assert_eq!(file_name("https://x.test/css/site.css#a"), "site.css");
        assert_eq!(file_name("data:text/html,<p>"), "data-uri");
        let unknown = json!({"styleSheetId": "nope", "origin": "regular"});
        assert_eq!(rule_source(&unknown, &s), None);
    }

    #[test]
    fn conditions_follow_rule_types_outermost_first() {
        let r = json!({
            "media": [
                {"text": "(prefers-color-scheme: dark)", "source": "mediaRule"},
                {"text": "screen", "source": "linkedSheet"},
            ],
            "layers": [{"text": "base"}],
            "supports": [{"text": "(display: grid)"}],
            "nestingSelectors": [".card"],
            "ruleTypes": ["StyleRule", "MediaRule", "SupportsRule", "LayerRule"],
        });
        assert_eq!(
            conditions(&r),
            [
                "@media screen",
                "@layer base",
                "@supports (display: grid)",
                "@media (prefers-color-scheme: dark)",
                ".card",
            ]
        );
    }

    #[test]
    fn matching_selectors_trim_selector_lists() {
        let m = json!({
            "rule": {
                "origin": "regular",
                "selectorList": {"text": "h1, .btn, p", "selectors": [{"text": "h1"}, {"text": ".btn"}, {"text": "p"}]},
                "style": authored(&[("color", "red")], &[("color", "red")]),
            },
            "matchingSelectors": [1],
        });
        assert_eq!(
            rule_block(&m, &HashMap::new(), &HashMap::new()).selector,
            ".btn"
        );
    }

    #[test]
    fn computed_extra_names_and_shorthands() {
        let declared = Declared {
            longhands: HashSet::from(["--gap".to_string(), "color".to_string()]),
            shorthands: vec![],
        };
        assert_eq!(normalize_property_name(" Z-Index "), "z-index");
        assert_eq!(normalize_property_name("--Gap"), "--Gap");
        let extra = ["z-index".to_string(), "padding".to_string()];
        // Without a serialized shorthand, its longhands stand in.
        let picked = pick_computed(&computed(), &declared, &HashMap::new(), &extra);
        let names: Vec<&str> = picked.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "display",
                "width",
                "height",
                "color",
                "padding-bottom",
                "padding-left",
                "padding-right",
                "padding-top",
                "z-index"
            ]
        );
        let shorthands = HashMap::from([("padding".to_string(), "8px".to_string())]);
        let picked = pick_computed(&computed(), &declared, &shorthands, &extra);
        assert_eq!(picked[4], ("padding".to_string(), "8px".to_string()));
        assert_eq!(picked.len(), 6);
    }

    /// Chrome serializes `border` as "" when its sides differ; the longhands
    /// are shown then.
    #[test]
    fn declared_shorthand_without_a_value_falls_back_to_longhands() {
        let declared = Declared {
            longhands: HashSet::new(),
            shorthands: vec![(
                "padding".to_string(),
                vec!["padding-top".to_string(), "padding-left".to_string()],
            )],
        };
        let shorthands = HashMap::from([("padding".to_string(), String::new())]);
        let picked = pick_computed(&computed(), &declared, &shorthands, &[]);
        let names: Vec<&str> = picked.iter().skip(3).map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["padding-left", "padding-top"]);
    }

    /// `border` with differing sides has no single value, so the shorthands
    /// one level down stand in for it, and longhands only where those fail.
    #[test]
    fn unserializable_shorthand_shows_the_parts_in_between() {
        assert_eq!(parent_shorthand("border-top-width"), Some("border-top"));
        assert_eq!(
            parent_shorthand("-webkit-text-stroke-width"),
            Some("-webkit-text-stroke")
        );
        assert_eq!(parent_shorthand("-webkit-locale"), None);
        assert_eq!(parent_shorthand("color"), None);

        let parts = [
            "border-top-width",
            "border-top-style",
            "border-left-width",
            "border-image-source",
        ];
        let declared = Declared {
            longhands: HashSet::new(),
            shorthands: vec![(
                "border".into(),
                parts.iter().map(|p| p.to_string()).collect(),
            )],
        };
        let mut list = computed();
        list.push(("border-left-width".into(), "1px".into()));
        list.push(("border-image-source".into(), "none".into()));
        let values = HashMap::from([
            ("border".to_string(), String::new()),
            ("border-top".to_string(), "3px solid teal".to_string()),
            ("border-left".to_string(), String::new()),
            ("border-image".to_string(), "none".to_string()),
        ]);
        let picked = pick_computed(&list, &declared, &values, &[]);
        assert_eq!(
            picked[3..].to_vec(),
            [
                ("border-image".to_string(), "none".to_string()),
                ("border-left-width".to_string(), "1px".to_string()),
                ("border-top".to_string(), "3px solid teal".to_string()),
            ]
        );

        let mut m = matched();
        m["matchedCSSRules"][1]["rule"]["style"] = authored(
            &[("border", "1px solid")],
            &[("border-top-width", "1px"), ("border-image-source", "none")],
        );
        // `font-family` (on the body) is read too: the fixture's computed
        // list doesn't have it.
        let props = element_props_to_read(&m, &computed(), &[]);
        assert_eq!(
            props,
            ["border", "border-top", "border-image", "font-family"]
        );
    }

    #[test]
    fn props_to_read_are_declared_shorthands_and_extras() {
        let mut m = matched();
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] = authored(
            &[("font", "12px serif"), ("margin", "0")],
            &[
                ("font-size", "12px"),
                ("font-family", "serif"),
                ("margin-top", "0px"),
            ],
        );
        // `padding` from the element, `font` from the ancestor (inheritable,
        // `margin` there is not), then the extra name.
        assert_eq!(
            element_props_to_read(&m, &computed(), &["inset".into(), "--x".into()]),
            ["padding", "font", "inset"]
        );
    }

    /// Chrome can't expand `border-color: var(--c)` before substitution, and
    /// its longhands don't start with `border-color-`, so the declaration is
    /// filed under its own name, which CDP's computed list doesn't have.
    #[test]
    fn unexpanded_var_shorthand_reads_its_own_name() {
        let mut m = matched();
        m["matchedCSSRules"][1]["rule"]["style"] = authored(
            &[("border-color", "var(--c)")],
            &[("border-top-color", ""), ("border-left-color", "")],
        );
        m["inherited"] = json!([]);
        assert_eq!(
            element_props_to_read(&m, &computed(), &[]),
            ["border-color"]
        );

        let declared = Declared {
            longhands: HashSet::from(["border-color".to_string()]),
            shorthands: vec![],
        };
        let values = HashMap::from([("border-color".to_string(), "rgb(0, 0, 255)".to_string())]);
        let picked = pick_computed(&computed(), &declared, &values, &[]);
        assert_eq!(
            picked[3],
            ("border-color".to_string(), "rgb(0, 0, 255)".to_string())
        );
    }

    #[test]
    fn nested_conditions_render_as_blocks() {
        let rule = Block {
            selector: "body".into(),
            source: Some("page.html:12".into()),
            origin: "regular".into(),
            conditions: vec!["@media (prefers-color-scheme: dark)".into()],
            layer: String::new(),
            decls: vec![Decl {
                name: "background".into(),
                value: "rgb(17, 17, 17)".into(),
                resolved: Some("a */ b".into()),
                important: false,
                status: Status::Active,
                longhands: vec!["background".into()],
                unexpanded: false,
            }],
        };
        let mut out = String::new();
        write_rule(&mut out, &rule, 1);
        assert_eq!(
            out,
            "  @media (prefers-color-scheme: dark) {\n    body (page.html:12) {\n      background: rgb(17, 17, 17); /* a *\\/ b */\n    }\n  }\n"
        );
    }

    /// CDP can't say that `border-color: var(--c)` sets `border-top-color`,
    /// so the page expands it, and it then overrides a lower rule's
    /// `border-top-color`. The ancestors' var() shorthands are expanded too.
    #[test]
    fn page_expansion_gives_var_shorthands_their_longhands() {
        let low = [("border-top-color", "red")];
        let high = [("border-color", "var(--c)")];
        let high_longhands = [("border-top-color", ""), ("border-left-color", "")];
        let mut m = matched();
        m["matchedCSSRules"] = json!([
            rule(".low", 3, authored(&low, &low)),
            rule(".high", 4, authored(&high, &high_longhands)),
        ]);
        let font = [("font", "var(--f)")];
        let font_longhands = [("font-size", ""), ("line-height", "")];
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] = authored(&font, &font_longhands);
        assert_eq!(unexpanded_shorthands(&m), ["border-color", "font"]);

        // Without the page, `border-color` is only its own name.
        let report = report_for(&m, CssOptions::default());
        assert_eq!(report.rules[2].selector, ".low");
        assert_eq!(report.rules[2].decls[0].status, Status::Active);

        let mut info = page();
        let sides = ["top", "right", "bottom", "left"].map(|s| format!("border-{s}-color"));
        info.longhands = HashMap::from([("border-color".to_string(), sides.to_vec())]);
        let report = build_report(&m, &computed(), &sheets(), &info, &CssOptions::default());
        assert_eq!(report.rules[1].decls[0].status, Status::Active);
        assert_eq!(report.rules[2].decls[0].status, Status::Overridden);
    }

    /// Importance reverses cascade layers: layer `one`'s important color
    /// beats layer `two`'s and the unlayered one, while the unlayered rule
    /// still wins the normal background. The style attribute stays on top.
    #[test]
    fn important_declarations_reverse_the_layer_order() {
        use Status::{Active, Overridden};
        let style = |color: &str| {
            let important = format!("{color} !important");
            let decls = [("color", important.as_str()), ("background-color", color)];
            authored(&decls, &decls)
        };
        let mut m = matched();
        m["inlineStyle"] = Value::Null;
        // CDP lists rules lowest precedence first, layer by layer.
        m["matchedCSSRules"] = json!([
            layered(".a", 3, "one", style("red")),
            layered(".a", 4, "two", style("green")),
            rule(".a", 5, style("blue")),
        ]);
        let report = report_for(&m, CssOptions::default());
        let all: Vec<Vec<Status>> = report.rules.iter().map(statuses).collect();
        assert_eq!(
            all,
            [
                vec![Overridden, Active],
                vec![Overridden, Overridden],
                vec![Active, Overridden],
            ]
        );

        // Anonymous layers are told apart by where they start.
        m["matchedCSSRules"] = json!([
            layered(".a", 3, "", style("red")),
            layered(".a", 4, "", style("green")),
        ]);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(statuses(&report.rules[1]), [Active, Overridden]);

        m["inlineStyle"] = authored(
            &[("color", "purple !important")],
            &[("color", "purple !important")],
        );
        let report = report_for(&m, CssOptions::default());
        assert_eq!(statuses(&report.rules[0]), [Active]);
        assert_eq!(report.rules[2].decls[0].status, Overridden);

        let path = |layers: Value| layer_key(&json!({ "layers": layers }));
        assert_eq!(
            path(json!([{"text": "a"}, {"text": "b"}])),
            path(json!([{"text": "a.b"}]))
        );
    }

    /// A winning `color: inherit` takes the parent's color, so the parent's
    /// declaration applies, and through `unset` the grandparent's; a
    /// concrete value on the element still blocks them.
    #[test]
    fn inherit_leaves_the_property_to_the_ancestors() {
        let mut m = matched();
        // The user-agent rule and `.btn`, without `.btn.primary`'s
        // important white.
        m["matchedCSSRules"].as_array_mut().unwrap().truncate(2);
        m["inlineStyle"] = authored(&[("color", "inherit")], &[("color", "inherit")]);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(report.rules[0].decls[0].status, Status::Active);
        assert_eq!(report.rules[1].decls[0].status, Status::Overridden);
        assert_eq!(report.inherited[0].rules[0].decls[0].status, Status::Active);

        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] =
            authored(&[("color", "unset")], &[("color", "unset")]);
        let html = rule(
            "html",
            1,
            authored(&[("color", "red")], &[("color", "red")]),
        );
        m["inherited"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "matchedCSSRules": [html] }));
        let report = report_for(&m, CssOptions::default());
        assert_eq!(report.inherited[0].rules[0].decls[0].status, Status::Active);
        assert_eq!(report.inherited[1].rules[0].decls[0].status, Status::Active);

        m["inlineStyle"] = authored(&[("color", "blue")], &[("color", "blue")]);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(
            report.inherited[0].rules[0].decls[0].status,
            Status::Overridden
        );
        assert_eq!(
            report.inherited[1].rules[0].decls[0].status,
            Status::Overridden
        );
    }

    /// `<table cellspacing>` beats the user-agent stylesheet's spacing, and
    /// an author rule beats it.
    #[test]
    fn attribute_style_ranks_between_author_and_user_agent_rules() {
        let spacing = |h: &str, v: &str| {
            json!({"cssProperties": [
                {"name": "-webkit-border-horizontal-spacing", "value": h},
                {"name": "-webkit-border-vertical-spacing", "value": v},
            ]})
        };
        let ua = json!({
            "rule": {
                "origin": "user-agent",
                "selectorList": {"text": "table", "selectors": [{"text": "table"}]},
                "style": spacing("2px", "2px"),
            },
            "matchingSelectors": [0],
        });
        let author = rule(
            "table",
            3,
            authored(
                &[("-webkit-border-horizontal-spacing", "4px")],
                &[("-webkit-border-horizontal-spacing", "4px")],
            ),
        );
        let mut blocks = node_blocks(
            &Value::Null,
            &json!([ua, author]),
            &spacing("8px", "8px"),
            &sheets(),
            &HashMap::new(),
        );
        let origins: Vec<&str> = blocks.iter().map(|b| b.origin.as_str()).collect();
        assert_eq!(origins, ["regular", "attributes", "user-agent"]);
        cascade(&mut blocks, &HashSet::new());
        assert_eq!(statuses(&blocks[0]), [Status::Active]);
        assert_eq!(statuses(&blocks[1]), [Status::Overridden, Status::Active]);
        assert_eq!(
            statuses(&blocks[2]),
            [Status::Overridden, Status::Overridden]
        );
    }

    /// `var(` in a string is text: `content: "var(--word)"` shows as written.
    #[test]
    fn var_in_a_string_is_not_a_reference() {
        let vars = HashMap::from([("--word".to_string(), "\"hello\"".to_string())]);
        assert_eq!(resolve_vars("\"var(--word)\"", &vars), None);
        // An escaped quote doesn't end the string.
        assert_eq!(resolve_vars(r"'it\'s var(--word)'", &vars), None);
        assert_eq!(
            resolve_vars("\"(\" var(--word)", &vars).as_deref(),
            Some("\"(\" \"hello\"")
        );
        let mut names = Vec::new();
        var_names("'var(--a)' var(--b)", &mut names);
        assert_eq!(names, ["--b"]);
    }

    /// `--computed=--theme` names one custom property, not every one that
    /// starts with it, while `padding` still stands for its longhands.
    #[test]
    fn computed_custom_property_is_matched_exactly() {
        let mut list = computed();
        list.push(("--theme-accent".into(), "teal".into()));
        let extra = ["--theme".to_string(), "padding".to_string()];
        let picked = pick_computed(&list, &Declared::default(), &HashMap::new(), &extra);
        let names: Vec<&str> = picked.iter().skip(3).map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "padding-bottom",
                "padding-left",
                "padding-right",
                "padding-top"
            ]
        );
    }

    /// An empty custom property (`--p: ;`) is a value, so `var(--p, x)` is
    /// empty, not `x`. An ancestor the page said nothing about could have
    /// either, so nothing is resolved there.
    #[test]
    fn empty_custom_property_is_a_value() {
        let vars = HashMap::from([("--p".to_string(), String::new())]);
        assert_eq!(
            resolve_vars("var(--p, Georgia,) serif", &vars).as_deref(),
            Some("serif")
        );
        assert_eq!(
            resolve_vars("var(--q, Georgia,) serif", &vars).as_deref(),
            Some("Georgia, serif")
        );

        let mut m = matched();
        let decls = [("font-family", "var(--p, Georgia,) serif")];
        m["inlineStyle"] = authored(&decls, &decls);
        let html = rule(
            "html",
            1,
            authored(&[("color", "var(--x, red)")], &[("color", "var(--x, red)")]),
        );
        m["inherited"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "matchedCSSRules": [html] }));
        // CDP's computed list has every defined custom property.
        let mut list = computed();
        list.push(("--p".into(), String::new()));
        let report = build_report(&m, &list, &sheets(), &page(), &CssOptions::default());
        assert_eq!(report.rules[0].decls[0].resolved.as_deref(), Some("serif"));
        assert_eq!(
            report.inherited[0].rules[0].decls[0].resolved.as_deref(),
            Some("#222")
        );
        assert_eq!(report.inherited[1].from, "ancestor 2");
        assert_eq!(report.inherited[1].rules[0].decls[0].resolved, None);
    }
}
