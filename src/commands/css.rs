use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cdp::{
    is_connection_or_page_failure, CdpClient, ConnectionLost, DialogOpen, SETUP_CALL_TIMEOUT,
};
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
    /// The winning static declaration is overridden by a running animation or
    /// transition, so this report cannot say it is the value in use.
    Indeterminate,
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
    /// The name as the cascade and the computed style know it. CDP keeps the
    /// authored spelling, and `COLOR` is `color`, but `--Tone` is not `--tone`.
    fn key(&self) -> String {
        normalize_property_name(&self.name)
    }

    fn is_shorthand(&self) -> bool {
        self.longhands.len() != 1 || self.longhands[0] != self.key()
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
        let key = d.key();
        if !d.is_shorthand() {
            self.longhands.insert(key);
        } else if !self.shorthands.iter().any(|(n, _)| *n == key) {
            self.shorthands.push((key, d.longhands.clone()));
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
/// shorthands (CDP's computed style lists longhands only), the longhands
/// of the shorthands CDP didn't expand, and which values after `var()`
/// substitution the element's properties reject.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PageInfo {
    element: String,
    ancestors: Vec<AncestorInfo>,
    /// How many ancestors the page's walk reaches. It can't enter a closed
    /// shadow root (`assignedSlot` hides the slot), so when this differs
    /// from CDP's flat-tree count the two lists don't line up.
    depth: usize,
    shorthands: HashMap<String, String>,
    longhands: HashMap<String, Vec<String>>,
    /// The `substituted_values` that `CSS.supports` turns down.
    rejected: Vec<(String, String)>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AncestorInfo {
    label: String,
    /// The referenced custom properties the ancestor has, empty ones
    /// included; one it doesn't have is left out.
    vars: HashMap<String, String>,
    /// The ancestor's own `writing-mode` and `direction`, so a logical
    /// longhand it declares can be matched to the physical side a child
    /// explicitly inherits.
    flow: Option<(String, String)>,
}

#[derive(Debug)]
struct Report {
    element: String,
    computed: Vec<(String, String)>,
    rules: Vec<Block>,
    inherited: Vec<InheritedView>,
    hidden_ua_rules: usize,
}

/// Matched rules, computed values and the page's own details are read in
/// separate CDP requests. Page script can change the element's classes or
/// styles between them, so a rule marked active and a computed value can come
/// from different states. Say so rather than presenting them as one snapshot.
const STATE_NOTE: &str =
    "Matched rules, computed values and page details are read in separate requests; if the page \
     changes styles during the inspection they may describe different states.";

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

/// Custom properties registered to inherit. A registered one that fails `var()`
/// substitution takes the parent's computed value, unlike an unregistered one,
/// which gets the guaranteed-invalid value and blocks the ancestor. The
/// registration is document-wide, wherever CDP reports it.
fn registered_inheriting(matched: &Value) -> HashSet<String> {
    let rules = items(&matched["cssPropertyRules"]).iter().filter_map(|r| {
        let inherits = items(&r["style"]["cssProperties"])
            .iter()
            .find(|p| p["name"] == "inherits")?;
        let on = inherits["value"]
            .as_str()?
            .trim()
            .eq_ignore_ascii_case("true");
        r["propertyName"]["text"].as_str().filter(|_| on)
    });
    let scripts = items(&matched["cssPropertyRegistrations"])
        .iter()
        .filter(|r| r["inherits"] == true)
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
    // The longhands CDP lists are lowercase whatever the authored case.
    let key_of = |p: &Value| normalize_property_name(p["name"].as_str().unwrap_or(""));

    let mut longhands: Vec<Option<Vec<String>>> = authored
        .iter()
        .map(|p| {
            let listed: Vec<String> = p["longhandProperties"]
                .as_array()
                .map(|a| a.iter().map(name_of).collect())
                .unwrap_or_default();
            if !listed.is_empty() {
                Some(listed)
            } else if trailing_names.contains(&key_of(p).as_str()) {
                Some(vec![key_of(p)])
            } else {
                None
            }
        })
        .collect();
    let unexpanded: Vec<bool> = longhands.iter().map(Option::is_none).collect();
    let claimed: HashSet<String> = longhands.iter().flatten().flatten().cloned().collect();
    for (p, slot) in authored.iter().zip(longhands.iter_mut()) {
        if slot.is_none() {
            let name = key_of(p);
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
/// lists each kind innermost first, except layers, which it lists outermost
/// first, and `ruleTypes` says how they interleave, innermost first.
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
    let mut layers = items(&rule["layers"]).iter().rev().map(|l| at("@layer", l));
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

/// A node's styles in normal precedence order, highest first: inline style,
/// matched rules (CDP lists them lowest first), with presentational
/// attributes below the author rules but above the user-agent ones, as the
/// author origin's lowest-precedence rules.
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

/// What the cascade knows about the node whose blocks it runs over.
#[derive(Debug, Default)]
struct NodeFacts<'a> {
    /// The node's custom property values. `None` when they're unknown, and
    /// then every `var()` reference counts as having one.
    vars: Option<&'a HashMap<String, String>>,
    /// The names the page was asked about on this node. A reference to one it
    /// wasn't asked about is unknown, not missing: its fallback cannot be used
    /// as if the page had confirmed the name undefined. `None` when every
    /// reference is known, as for the element's own computed custom properties.
    queried: Option<&'a HashSet<String>>,
    /// The node's `writing-mode` and `direction`, which say what physical
    /// side a flow-relative longhand sets. `None` keeps the two apart.
    flow: Option<(&'a str, &'a str)>,
    /// Properties with the value `var()` substitution gives them that the
    /// page says they reject (see `substituted_values`).
    rejected: &'a [(String, String)],
    /// Registered custom properties that inherit: a failed substitution in
    /// one takes the parent's value instead of claiming the declaration.
    inheriting: Option<&'a HashSet<String>>,
}

impl NodeFacts<'_> {
    /// The key a longhand cascades under: a flow-relative longhand and the
    /// physical one it maps to are one property for the cascade.
    fn key(&self, longhand: &str) -> String {
        mapped_key(longhand, self.flow)
    }

    /// The value the cascade acts on: the one after `var()` substitution
    /// where the node's custom properties are known, so that, as in Chrome,
    /// `var(--x, inherit)` is `inherit`. `None` when the declaration is
    /// invalid at computed-value time, which makes it act as `unset`: a
    /// reference with no value and no fallback, or a value the property
    /// rejects (`color: var(--size)` with `--size: 20px`). Custom properties
    /// are left out of that: an unregistered one gets the guaranteed-invalid
    /// value instead, which doesn't inherit. A registered inheriting one
    /// instead takes the ancestor's computed value, like `unset`.
    fn value(&self, d: &Decl) -> Option<String> {
        let Some(vars) = self.vars.filter(|_| find_var(&d.value).is_some()) else {
            return Some(d.value.clone());
        };
        // A name the page was never asked about may still be defined there, so
        // a fallback is not known to apply. Keep the authored value instead:
        // the declaration stays local rather than being passed to a parent as
        // `inherit` on an unconfirmed fallback.
        if let Some(queried) = self.queried {
            let mut names = Vec::new();
            var_names(&d.value, &mut names);
            if names.iter().any(|name| !queried.contains(name)) {
                return Some(d.value.clone());
            }
        }
        match substituted(&d.value, vars) {
            Some(v) if self.rejected.iter().any(|(p, r)| *p == d.key() && *r == v) => None,
            Some(v) => Some(v),
            None if d.name.starts_with("--") => {
                if self
                    .inheriting
                    .is_some_and(|names| names.contains(&d.key()))
                {
                    None
                } else {
                    Some(d.value.clone())
                }
            }
            None => None,
        }
    }
}

/// The property keys the `all` shorthand resets on this node. CDP reports it
/// as one declaration and leaves the expansion to the cascade. The computed
/// style names every longhand the element has; the declared ones also cover
/// names CDP couldn't expand (`var()` shorthands, for instance). CSS excludes
/// `direction`, `unicode-bidi` and custom properties from `all`.
fn all_properties(computed: &[(String, String)], blocks: &[Block]) -> HashSet<String> {
    let mut keys: HashSet<String> = computed
        .iter()
        .map(|(name, _)| name.clone())
        .chain(
            blocks
                .iter()
                .flat_map(|b| b.decls.iter())
                .flat_map(|d| d.longhands.iter().cloned()),
        )
        .collect();
    keys.retain(|k| k != "direction" && k != "unicode-bidi" && !k.starts_with("--"));
    keys
}

/// Run the cascade over one node's blocks, marking each declaration that wins
/// at least one of its longhands active. `claimed` longhands are already
/// decided (set on the element itself, or on a nearer ancestor) and nothing
/// here wins them. `all` holds the keys an `all` declaration resets. `inherited`
/// holds the longhands a nearer element takes from its parent that do not
/// inherit by default: an ancestor's `unset` on one of those is the initial
/// value, which stops a more distant ancestor from supplying it (on one that
/// does inherit, `unset` is `inherit` and keeps looking further up). Returns
/// the longhands decided here and the ones whose winning value comes from a
/// parent. `non_inheriting` names custom properties registered not to inherit,
/// used to decide which longhands take a parent's value on `unset`.
/// `animated` longhands are marked indeterminate rather than active:
/// an animation or transition can override the winning declaration. A winning
/// `inherit` always, and `unset` only on a property that inherits by default,
/// decides none on the element: the value comes from an ancestor's declaration,
/// so that one still applies. Nor does a declaration that is invalid at
/// computed-value time, which acts as `unset`, on such a property. These
/// keywords count after
/// `var()` substitution too (see `NodeFacts::value`).
///
/// A winning `revert-layer` rolls the longhand back to the next lower
/// cascade layer, and `revert` to the user-agent origin: the first
/// declaration there wins it, while the rest of the reverting layer stays
/// overridden. With no such declaration it acts as `unset`.
///
/// `!important` beats normal, and an important declaration is ranked by
/// `important_order`.
fn cascade(
    blocks: &mut [Block],
    claimed: &HashSet<String>,
    node: &NodeFacts,
    non_inheriting: &HashSet<String>,
    all: &HashSet<String>,
    stop_on_unset: &HashSet<String>,
    animated: &AnimatedProperties,
) -> (HashSet<String>, HashSet<String>) {
    let mut won = HashSet::new();
    let mut decided = HashSet::new();
    let mut from_parent = HashSet::new();
    // Longhands a winning `revert-layer` or `revert` rolled back, with the
    // first block that may still win them. That is a position in normal
    // order even for an important one: Chrome rolls an important
    // `revert-layer` back to the layers below it in normal order.
    let mut rolled_back: HashMap<String, usize> = HashMap::new();
    let ranked = important_order(blocks);
    let in_order: Vec<usize> = (0..blocks.len()).collect();
    let layer_ends = layer_ends(blocks);
    let nowhere = blocks.len();
    let user_agent = blocks
        .iter()
        .position(|b| b.origin == "user-agent")
        .unwrap_or(nowhere);
    // An injected sheet may be in the user origin, which `revert` from an
    // author one stops at; with one present, where it lands is unknown.
    let revert_known = !blocks.iter().any(|b| b.origin == "injected");
    for (important, order) in [(true, &ranked), (false, &in_order)] {
        for &bi in order {
            // The user-agent origin has nothing lower to revert to.
            let revert_to = if blocks[bi].origin == "user-agent" {
                nowhere
            } else {
                user_agent
            };
            // Within a block the last declaration wins.
            for d in blocks[bi].decls.iter_mut().rev() {
                if d.important != important || !d.usable() {
                    continue;
                }
                let value = node.value(d);
                let is = |keyword: &str| {
                    value
                        .as_deref()
                        .is_some_and(|v| v.trim().eq_ignore_ascii_case(keyword))
                };
                let rollback = if is("revert-layer") {
                    Some(layer_ends[bi])
                } else if is("revert") && revert_known {
                    Some(revert_to)
                } else {
                    None
                };
                // The one `all` row CDP gives us resets every property at
                // once; a later declaration in this block was seen first and
                // has already claimed its own longhands.
                let longhands: Vec<String> = if d.key() == "all" {
                    all.iter().cloned().collect()
                } else {
                    d.longhands.clone()
                };
                for longhand in &longhands {
                    let key = node.key(longhand);
                    if claimed.contains(&key)
                        || won.contains(&key)
                        || rolled_back.get(&key).is_some_and(|&from| bi < from)
                    {
                        continue;
                    }
                    // `inherit` always takes the parent's value, and
                    // `color: currentColor` is defined to behave as `inherit`
                    // (for other properties like `border-color`, `currentColor`
                    // uses this element's own `color`). `unset` and a value
                    // invalid at computed-value time inherit only for
                    // properties that inherit by default.
                    let explicit_inherit = is("inherit") || (key == "color" && is("currentcolor"));
                    let needs_parent = if explicit_inherit {
                        true
                    } else if value.is_none() || is("unset") {
                        is_inherited(&key, non_inheriting)
                    } else {
                        false
                    };
                    // A running animation or transition sets the property at a
                    // higher level than the static cascade. The declaration is
                    // still the winner among rules, but whether its value is in
                    // use is not something this report can claim. An animation
                    // does not outrank `!important`; a transition does.
                    if animated.transitions.contains(&key)
                        || (animated.animations.contains(&key) && !d.important)
                    {
                        d.status = Status::Indeterminate;
                    } else if d.status != Status::Indeterminate {
                        d.status = Status::Active;
                    }
                    if let Some(from) = rollback {
                        rolled_back.insert(key, from);
                    } else {
                        if needs_parent {
                            from_parent.insert(key.clone());
                            // On an ancestor, `unset` (or a value that is
                            // invalid at computed-value time) is the initial
                            // value for a longhand a nearer element inherits:
                            // no more distant ancestor supplies it. `inherit`
                            // keeps looking further up instead.
                            if stop_on_unset.contains(&key) && !explicit_inherit {
                                decided.insert(key.clone());
                            }
                        } else {
                            decided.insert(key.clone());
                        }
                        won.insert(key);
                    }
                }
            }
        }
    }
    (decided, from_parent)
}

/// For each block, the first block (in normal order) of the next lower
/// cascade layer, which a `revert-layer` in it rolls back to. As in Chrome,
/// the style attribute, the presentational attributes and the user-agent
/// origin are layers of their own, so the lowest author layer rolls back to
/// the presentational attributes. CDP sorts rules by layer, so a layer's
/// blocks are one run.
fn layer_ends(blocks: &[Block]) -> Vec<usize> {
    fn layer(b: &Block) -> (&str, &str) {
        match b.origin.as_str() {
            origin @ ("inline" | "attributes" | "user-agent") => (origin, ""),
            _ => ("", b.layer.as_str()),
        }
    }
    let mut ends = Vec::with_capacity(blocks.len());
    for run in blocks.chunk_by(|a, b| layer(a) == layer(b)) {
        let end = ends.len() + run.len();
        ends.resize(end, end);
    }
    ends
}

/// The key a longhand cascades under. A flow-relative name maps to the
/// physical side this node's `writing-mode` and `direction` give it; without
/// that information the two stay apart. Ancestors need their own flow here:
/// an explicit `inherit` reads the parent's physical side, not its authored
/// logical name (see `physical`).
fn mapped_key(longhand: &str, flow: Option<(&str, &str)>) -> String {
    flow.and_then(|(writing_mode, direction)| physical(longhand, writing_mode, direction))
        .unwrap_or_else(|| longhand.to_string())
}

/// The physical longhand a flow-relative one sets on a node with this
/// `writing-mode` and `direction`: `margin-inline-start` is `margin-left` in
/// horizontal left-to-right text and `margin-right` in right-to-left text.
/// Covers margins, paddings, insets, border sides and corner radii, scroll
/// margins and paddings, and sizes, and gives `None` for anything else.
fn physical(longhand: &str, writing_mode: &str, direction: &str) -> Option<String> {
    let opposite = |side: &'static str| match side {
        "top" => "bottom",
        "bottom" => "top",
        "left" => "right",
        _ => "left",
    };
    let (block_start, ltr_inline_start) = match writing_mode {
        "horizontal-tb" => ("top", "left"),
        "vertical-rl" | "sideways-rl" => ("right", "top"),
        "vertical-lr" => ("left", "top"),
        "sideways-lr" => ("left", "bottom"),
        _ => return None,
    };
    let inline_start = match direction {
        "ltr" => ltr_inline_start,
        "rtl" => opposite(ltr_inline_start),
        _ => return None,
    };
    let horizontal = writing_mode == "horizontal-tb";
    let (prefix, size) = ["min-", "max-", ""]
        .iter()
        .find_map(|p| Some((*p, longhand.strip_prefix(p)?)))?;
    let width = match size {
        "inline-size" => Some(horizontal),
        "block-size" => Some(!horizontal),
        _ => None,
    };
    if let Some(width) = width {
        let dimension = if width { "width" } else { "height" };
        return Some(format!("{prefix}{dimension}"));
    }
    // `border-start-end-radius` rounds the corner between the block-start
    // and inline-end sides, which a physical name gives top or bottom first.
    if let Some(corner) = longhand
        .strip_prefix("border-")
        .and_then(|r| r.strip_suffix("-radius"))
    {
        let edge = |end: &str, start: &'static str| match end {
            "start" => Some(start),
            "end" => Some(opposite(start)),
            _ => None,
        };
        let (block, inline) = corner.split_once('-')?;
        let (block, inline) = (edge(block, block_start)?, edge(inline, inline_start)?);
        let (y, x) = if horizontal {
            (block, inline)
        } else {
            (inline, block)
        };
        return Some(format!("border-{y}-{x}-radius"));
    }
    let (base, rest) = [
        "margin",
        "padding",
        "inset",
        "border",
        "scroll-margin",
        "scroll-padding",
    ]
    .iter()
    .find_map(|b| Some((*b, longhand.strip_prefix(b)?.strip_prefix('-')?)))?;
    let (start, rest) = match rest.split_once('-')? {
        ("block", rest) => (block_start, rest),
        ("inline", rest) => (inline_start, rest),
        _ => return None,
    };
    let (side, suffix) = match rest.strip_prefix("start") {
        Some(suffix) => (start, suffix),
        None => (opposite(start), rest.strip_prefix("end")?),
    };
    match (base, suffix) {
        ("inset", "") => Some(side.to_string()),
        (_, "") | ("border", "-width" | "-style" | "-color") => {
            Some(format!("{base}-{side}{suffix}"))
        }
        _ => None,
    }
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

/// Offset of the `(` after the CSS identifier at `start`, when that identifier
/// decodes (CSS escapes included) to `var`. A hex escape consumes the one
/// whitespace that ends it, so `v\\61 r(` reads as `var(`. The authored spelling
/// is never changed; only the lookup uses the decoded name.
fn var_call_open(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = start;
    let mut decoded = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'(' => return decoded.eq_ignore_ascii_case("var").then_some(i),
            b'\\' => {
                i += 1;
                let c = *bytes.get(i)?;
                if c.is_ascii_hexdigit() {
                    let mut value = 0u32;
                    let mut digits = 0;
                    while i < bytes.len() && digits < 6 {
                        let Some(d) = (bytes[i] as char).to_digit(16) else {
                            break;
                        };
                        value = value * 16 + d;
                        i += 1;
                        digits += 1;
                    }
                    if i < bytes.len() && bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    decoded.push(char::from_u32(value).unwrap_or('\u{FFFD}'));
                } else {
                    let ch = s[i..].chars().next()?;
                    // A backslash before a line break continues the line; it
                    // contributes no character to the identifier.
                    if !matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
                        decoded.push(ch);
                    }
                    i += ch.len_utf8();
                }
            }
            c if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_') => {
                decoded.push(c as char);
                i += 1;
            }
            c if !c.is_ascii() => {
                let ch = s[i..].chars().next()?;
                decoded.push(ch);
                i += ch.len_utf8();
            }
            _ => return None,
        }
    }
    None
}

/// Start of the next `var()` function call and the offset just past its `(`:
/// not the end of a longer name such as `somevar(`, not text in a string such
/// as `content: "var(--x)"`, which the browser shows as written, and not text
/// in a comment, which Blink keeps but does not tokenize. Escaped function
/// names are decoded, so `v\\61 r(` is found just like `var(`.
fn find_var(s: &str) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    let mut quote = None;
    let mut i = 0;
    while i < bytes.len() {
        if quote.is_none() {
            let prev = i.checked_sub(1).map(|p| bytes[p]);
            let boundary = !prev
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'\\'));
            if boundary && matches!(bytes[i], b'v' | b'V' | b'\\') {
                if let Some(open) = var_call_open(s, i) {
                    return Some((i, open + 1));
                }
            }
        }
        match (quote, bytes[i]) {
            // An escaped character is never a quote or the start of `var(`.
            (_, b'\\') => i += 1,
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, b'/') if bytes.get(i + 1) == Some(&b'*') => match s[i + 2..].find("*/") {
                Some(end) => i += 3 + end,
                None => return None,
            },
            (None, c @ (b'"' | b'\'')) => quote = Some(c),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Offset of the `)` closing a function whose arguments start at `s[0]`, and
/// of the first top-level comma before it, skipping nested parentheses,
/// strings and comments.
fn scan_args(s: &str) -> Option<(usize, Option<usize>)> {
    let bytes = s.as_bytes();
    let (mut depth, mut quote, mut comma) = (0usize, None::<u8>, None);
    let mut i = 0;
    while i < bytes.len() {
        match (quote, bytes[i]) {
            // An escaped character (`"a\"b"`) never ends a string or a call.
            (_, b'\\') => i += 2,
            (Some(q), c) if c == q => {
                quote = None;
                i += 1;
            }
            (Some(_), _) => i += 1,
            (None, b'/') if bytes.get(i + 1) == Some(&b'*') => {
                // A `)` or `,` in an authored comment is not a delimiter.
                i = match s[i + 2..].find("*/") {
                    Some(end) => i + 4 + end,
                    None => return None,
                };
            }
            (None, b'"' | b'\'') => {
                quote = Some(bytes[i]);
                i += 1;
            }
            (None, b'(') => {
                depth += 1;
                i += 1;
            }
            (None, b')') if depth == 0 => return Some((i, comma)),
            (None, b')') => {
                depth -= 1;
                i += 1;
            }
            (None, b',') if depth == 0 && comma.is_none() => {
                comma = Some(i);
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// The first CSS identifier in `text`, skipping whitespace and comments, with
/// identifier escapes decoded. Blink keeps a comment in the authored `var()`
/// value but ignores it when resolving the property name, and reads `\74 `
/// as `t`: the whitespace ending a hex escape belongs to it, not to what
/// follows. The lookup key must do the same; the authored value is untouched.
fn var_name(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    loop {
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if bytes.get(i..i + 2) == Some(b"/*") {
            match text[i + 2..].find("*/") {
                Some(end) => {
                    i += 4 + end;
                    continue;
                }
                None => return Cow::Borrowed(&text[i..]),
            }
        }
        break;
    }
    let start = i;
    if bytes.get(i).is_some_and(|c| matches!(c, b'-' | b'+')) {
        i += 1;
    }
    let mut decoded: Option<String> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' {
            let out = decoded.get_or_insert_with(|| text[start..i].to_string());
            i += 1;
            if i >= bytes.len() {
                break;
            }
            if bytes[i].is_ascii_hexdigit() {
                let mut value = 0u32;
                let mut digits = 0;
                while i < bytes.len() && digits < 6 {
                    let Some(digit) = (bytes[i] as char).to_digit(16) else {
                        break;
                    };
                    value = value * 16 + digit;
                    i += 1;
                    digits += 1;
                }
                if i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                out.push(char::from_u32(value).unwrap_or('\u{FFFD}'));
            } else {
                let ch = text[i..].chars().next().unwrap_or('\u{FFFD}');
                out.push(ch);
                i += ch.len_utf8();
            }
            continue;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_') || !c.is_ascii()) {
            break;
        }
        if let Some(out) = decoded.as_mut() {
            let ch = text[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        } else {
            i += 1;
        }
    }
    match decoded {
        Some(out) => Cow::Owned(out),
        None => Cow::Borrowed(&text[start..i]),
    }
}

/// A `var()` call whose arguments start at `args[0]`: the property name as
/// looked up, the fallback, and the offset of the closing `)`.
fn split_var(args: &str) -> Option<(Cow<'_, str>, Option<&str>, usize)> {
    let (close, comma) = scan_args(args)?;
    Some(match comma {
        Some(c) => (var_name(&args[..c]), Some(&args[c + 1..close]), close),
        None => (var_name(&args[..close]), None, close),
    })
}

/// Whether CSS text ending in `before` and text starting with `after` would
/// read as one token if written together: `20` and `px` make the length
/// `20px`, and `/` and `*` open a comment. Covers every pair css-syntax
/// needs a comment between, and a few more, where one changes nothing.
fn runs_together(before: &str, after: &str) -> bool {
    let name =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '\\') || !c.is_ascii();
    // What a name, a number or a dimension can start with.
    let starts_word = |c: char| name(c) || matches!(c, '.' | '+');
    match (before.chars().next_back(), after.chars().next()) {
        (Some('/'), Some(b)) => b == '*',
        (Some('#' | '@' | '.' | '+'), Some(b)) => starts_word(b),
        // A name and `(` make a function, a number and `%` a percentage.
        (Some(a), Some(b)) if name(a) => {
            starts_word(b) || b == '(' || (b == '%' && a.is_ascii_digit())
        }
        _ => false,
    }
}

/// Append `piece` to `out` as tokens of its own. `var()` substitutes tokens,
/// not text, so `var(--size)px` with `--size: 20` is a number and then an
/// identifier, which no property takes as a length. Chrome writes such a
/// result with an empty comment between the two (`20/**/px`), and so does
/// this, so that `CSS.supports` judges what Chrome does.
fn push_tokens(out: &mut String, piece: &str) {
    if runs_together(out, piece) {
        out.push_str("/**/");
    }
    out.push_str(piece);
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
    while let Some((start, args_at)) = find_var(rest) {
        push_tokens(&mut out, &rest[..start]);
        let args = &rest[args_at..];
        let (name, fallback, close) = split_var(args)?;
        let replacement = match lookup(&name) {
            Some(v) => v,
            None => substitute(fallback?.trim(), lookup, depth + 1)?,
        };
        push_tokens(&mut out, &replacement);
        rest = &args[close + 1..];
    }
    push_tokens(&mut out, rest);
    Some(out)
}

/// `value` after `var()` substitution with a node's custom property values,
/// as the cascade judges it: `None` as for `substitute`.
fn substituted(value: &str, vars: &HashMap<String, String>) -> Option<String> {
    substitute(value, &|name| vars.get(name).cloned(), 0)
}

/// The element's custom property values, from its computed style, which
/// lists each defined one, empty or not.
fn custom_properties(computed: &[(String, String)]) -> HashMap<String, String> {
    computed
        .iter()
        .filter(|(n, _)| n.starts_with("--"))
        .cloned()
        .collect()
}

/// The element's `var()` declarations as the page should check them: the
/// property and its value after substitution, which the property can still
/// reject. Only the element's: an ancestor's custom property values come
/// from the same page call, so they aren't known in time.
fn substituted_values(matched: &Value, vars: &HashMap<String, String>) -> Vec<(String, String)> {
    let none = HashMap::new();
    let mut checks: Vec<(String, String)> = Vec::new();
    for d in level_styles(matched).flat_map(|s| declarations(s, &none)) {
        let key = d.key();
        if !d.usable() || key.starts_with("--") || find_var(&d.value).is_none() {
            continue;
        }
        if let Some(check) = substituted(&d.value, vars).map(|v| (key, v)) {
            if !checks.contains(&check) {
                checks.push(check);
            }
        }
    }
    checks
}

/// `value` with its `var()` references resolved, when it has any and the
/// result says something the authored value doesn't.
fn resolve_vars(value: &str, vars: &HashMap<String, String>) -> Option<String> {
    find_var(value)?;
    let lookup = |name: &str| vars.get(name).map(|v| v.trim().to_string());
    let resolved = substitute(value, &lookup, 0)?;
    (resolved.trim() != value.trim()).then(|| resolved.trim().to_string())
}

/// As `resolve_vars`, except that a name the page was never asked about stays
/// unknown. A queried name missing from the page still takes its fallback:
/// that is what the page reported; an unqueried one would be a guess.
fn resolve_queried_vars(
    value: &str,
    vars: &HashMap<String, String>,
    queried: &HashSet<String>,
) -> Option<String> {
    let mut names = Vec::new();
    var_names(value, &mut names);
    if names.iter().any(|name| !queried.contains(name)) {
        return None;
    }
    resolve_vars(value, vars)
}

/// Custom property names a value references, fallbacks included.
fn var_names(value: &str, out: &mut Vec<String>) {
    var_names_within(value, out, 0);
}

/// [`var_names`] down to the fallback depth [`substitute`] follows: a page's
/// CSS sets how deep the nesting goes, and a name deeper than that can't be
/// resolved anyway.
fn var_names_within(value: &str, out: &mut Vec<String>, depth: usize) {
    if depth > 16 {
        return;
    }
    let mut rest = value;
    while let Some((_, args_at)) = find_var(rest) {
        let args = &rest[args_at..];
        let Some((name, fallback, close)) = split_var(args) else {
            return;
        };
        if name.starts_with("--") && !out.iter().any(|n| n == name.as_ref()) {
            out.push(name.into_owned());
        }
        if let Some(fallback) = fallback {
            var_names_within(fallback, out, depth + 1);
        }
        rest = &args[close + 1..];
    }
}

/// Custom properties whose declarations refer back to themselves through
/// `var()`, directly or through another property. A cycle makes every property
/// in it invalid at computed-value time, so no resolved value can be shown for
/// their declarations, even though the lookup has no value for a reference in
/// the cycle and would otherwise substitute its fallback.
fn cyclic_custom_properties(blocks: &[Block]) -> HashSet<String> {
    let mut refs: HashMap<String, Vec<String>> = HashMap::new();
    for d in blocks.iter().flat_map(|b| b.decls.iter()) {
        if d.name.starts_with("--") {
            let mut names = Vec::new();
            var_names(&d.value, &mut names);
            refs.entry(d.key()).or_default().extend(names);
        }
    }
    let mut cyclic = HashSet::new();
    for start in refs.keys() {
        let mut stack: Vec<&String> = refs.get(start).into_iter().flatten().collect();
        let mut seen: HashSet<&String> = HashSet::new();
        while let Some(name) = stack.pop() {
            if name == start {
                cyclic.insert(start.clone());
                break;
            }
            if seen.insert(name) {
                stack.extend(refs.get(name).into_iter().flatten());
            }
        }
    }
    cyclic
}

/// The writing-mode and direction pairs `physical` recognises. The ancestor's
/// own flow is not known when the page is asked for variables, so a logical
/// longhand is matched against the physical key it would take under any flow;
/// the final pass still filters by the ancestor's real flow.
const FLOW_COMBINATIONS: &[(&str, &str)] = &[
    ("horizontal-tb", "ltr"),
    ("horizontal-tb", "rtl"),
    ("vertical-rl", "ltr"),
    ("vertical-rl", "rtl"),
    ("vertical-lr", "ltr"),
    ("vertical-lr", "rtl"),
    ("sideways-rl", "ltr"),
    ("sideways-rl", "rtl"),
    ("sideways-lr", "ltr"),
    ("sideways-lr", "rtl"),
];

/// Whether a longhand name can supply one of the physical keys the element
/// takes from its parent. A logical longhand such as `margin-inline-start`
/// must match an inherited physical `margin-left` even though the authored
/// names differ: the final cascade maps it by the ancestor's flow.
fn may_reach_inherited(longhand: &str, inherited_keys: &HashSet<String>) -> bool {
    inherited_keys.contains(longhand)
        || FLOW_COMBINATIONS.iter().any(|&(writing_mode, direction)| {
            physical(longhand, writing_mode, direction)
                .is_some_and(|key| inherited_keys.contains(&key))
        })
}

/// The custom properties each ancestor's declarations that can reach the
/// element reference, so the page can be asked for their values on that
/// ancestor. Besides properties that inherit by default, this covers rows the
/// report keeps because the element explicitly inherits them: a page that was
/// never asked for such a name cannot resolve it.
fn ancestor_var_names(matched: &Value, inherited_keys: &HashSet<String>) -> Vec<Vec<String>> {
    let non_inheriting = non_inheriting(matched);
    let none = HashMap::new();
    items(&matched["inherited"])
        .iter()
        .map(|level| {
            let mut names = Vec::new();
            for d in level_styles(level).flat_map(|s| declarations(s, &none)) {
                let key = d.key();
                if key == "all"
                    || is_inherited(&key, &non_inheriting)
                    || may_reach_inherited(&key, inherited_keys)
                    || d.longhands
                        .iter()
                        .any(|l| may_reach_inherited(l, inherited_keys))
                {
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
        let key = d.key();
        if d.unexpanded && d.usable() && !names.contains(&key) {
            names.push(key);
        }
    }
    names
}

/// The shorthands the page should expand: the unexpanded ones above, and
/// the extra names asked for that CDP's computed list doesn't have, whose
/// longhands stand in for them when Chrome can't write one value.
fn shorthands_to_expand(
    matched: &Value,
    computed: &[(String, String)],
    extra: &[String],
) -> Vec<String> {
    let mut names = unexpanded_shorthands(matched);
    for e in extra {
        if !e.starts_with("--") && !computed.iter().any(|(n, _)| n == e) && !names.contains(e) {
            names.push(e.clone());
        }
    }
    names
}

/// The blocks worth showing, with `var()` references resolved, noting what
/// they declare. User-agent blocks are only counted unless asked for. `vars`
/// is `None` when the node's custom properties are unknown, and then nothing
/// is resolved rather than guessed. `queried`, when given, names what the page
/// was asked for on this node; a reference outside it stays unresolved too.
fn shown_blocks(
    blocks: Vec<Block>,
    vars: Option<&HashMap<String, String>>,
    queried: Option<&HashSet<String>>,
    ua_rules: bool,
    hidden_ua: &mut usize,
    declared: &mut Declared,
) -> Vec<Block> {
    let cyclic = cyclic_custom_properties(&blocks);
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
            d.resolved = if d.name.starts_with("--") && cyclic.contains(&d.key()) {
                None
            } else {
                vars.and_then(|vars| match queried {
                    Some(queried) => resolve_queried_vars(&d.value, vars, queried),
                    None => resolve_vars(&d.value, vars),
                })
            };
        }
        out.push(block);
    }
    out
}

/// The computed values worth showing: display, width and height, what the
/// shown declarations set, and the properties asked for. A shorthand shows as
/// one value when Chrome can serialize it (`padding: 16px 8px`) and as its
/// longhands (`members`, as the page expanded it) when it can't; custom
/// properties show only when asked for.
fn pick_computed(
    computed: &[(String, String)],
    declared: &Declared,
    shorthand_values: &HashMap<String, String>,
    members: &HashMap<String, Vec<String>>,
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
        } else if let Some(parts) = members.get(name).filter(|parts| !parts.is_empty()) {
            // Not every name that starts with it: `border` doesn't set
            // `border-collapse` or the corner radii.
            longhands.extend(parts.iter().cloned());
        } else if !name.starts_with("--") {
            // Without the page's expansion, a guess from the name. A custom
            // property has no longhands: `--theme` is not the shorthand of
            // `--theme-accent`.
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
            let key = d.key();
            if !d.usable()
                || key.starts_with("--")
                || (ancestor && !is_inherited(&key, &non_inheriting))
            {
                continue;
            }
            if d.is_shorthand() {
                add(&key);
                for part in &d.longhands {
                    if let Some(p) = parent_shorthand(part).filter(|p| *p != key) {
                        add(p);
                    }
                }
            } else if !listed.contains(key.as_str()) {
                add(&key);
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

/// Properties a running animation or transition sets on one node. Animations
/// override normal author declarations but not `!important` ones; transitions
/// outrank even `!important` author declarations, so the two are kept apart.
#[derive(Debug, Default)]
struct AnimatedProperties {
    animations: HashSet<String>,
    transitions: HashSet<String>,
}

/// The properties whose value this node takes from a running effect rather
/// than from its static declarations. A transition outranks even `!important`;
/// an animation does not, so where an important declaration won here it is that
/// declaration which supplies the inherited value.
fn supplying_effects(
    blocks: &[Block],
    animated: &AnimatedProperties,
    flow: Option<(&str, &str)>,
    all: &HashSet<String>,
) -> HashSet<String> {
    let mut important_in_use: HashSet<String> = HashSet::new();
    for d in blocks
        .iter()
        .flat_map(|b| b.decls.iter())
        .filter(|d| d.important && matches!(d.status, Status::Active | Status::Indeterminate))
    {
        if d.key() == "all" {
            important_in_use.extend(all.iter().cloned());
        } else {
            important_in_use.extend(d.longhands.iter().map(|l| mapped_key(l, flow)));
        }
    }
    animated
        .transitions
        .iter()
        .chain(
            animated
                .animations
                .iter()
                .filter(|key| !important_in_use.contains(*key)),
        )
        .cloned()
        .collect()
}

/// Animated properties CDP reports for the inspected node and, separately,
/// for each ancestor in its inheritance chain. An animation on a parent can
/// supply the value a child inherits without setting that property on the
/// child, so the two sets must not be merged.
#[derive(Debug, Default)]
struct AnimatedStyles {
    element: AnimatedProperties,
    ancestors: Vec<AnimatedProperties>,
}

/// Cascade the element's own styles once. The inherited keys are needed before
/// the page is asked which custom properties its ancestors reference; the
/// animation data only changes declaration statuses, so an empty set suffices
/// for that early pass.
fn element_cascade(
    matched: &Value,
    computed: &[(String, String)],
    sheets: &HashMap<String, Value>,
    page: &PageInfo,
    animated: &AnimatedProperties,
) -> (Vec<Block>, HashSet<String>, HashSet<String>) {
    let non_inheriting = non_inheriting(matched);
    let mut blocks = node_blocks(
        &matched["inlineStyle"],
        &matched["matchedCSSRules"],
        &matched["attributesStyle"],
        sheets,
        &page.longhands,
    );
    let element_vars = custom_properties(computed);
    let computed_value = |name: &str| {
        computed
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    let element = NodeFacts {
        vars: Some(&element_vars),
        queried: None,
        flow: computed_value("writing-mode").zip(computed_value("direction")),
        rejected: &page.rejected,
        inheriting: Some(&registered_inheriting(matched)),
    };
    let all = all_properties(computed, &blocks);
    let (mut claimed, mut inherited_keys) = cascade(
        &mut blocks,
        &HashSet::new(),
        &element,
        &non_inheriting,
        &all,
        &HashSet::new(),
        animated,
    );
    // A running transition or animation supplies these properties on the
    // element even when it has no static declaration for them. Claim only the
    // ones the effect actually supplies: an `!important` declaration on the
    // element (such as `color: inherit !important`) defeats an animation, while
    // transitions outrank `!important`.
    let supplied = supplying_effects(&blocks, animated, element.flow, &all);
    claimed.extend(supplied.iter().cloned());
    inherited_keys.retain(|k| !supplied.contains(k));
    (blocks, claimed, inherited_keys)
}

/// Turn CDP's matched and computed styles into what `css-styles` prints.
#[cfg(test)]
fn build_report(
    matched: &Value,
    computed: &[(String, String)],
    sheets: &HashMap<String, Value>,
    page: &PageInfo,
    opts: &CssOptions,
) -> Report {
    build_report_with_animated(
        matched,
        computed,
        sheets,
        page,
        opts,
        &AnimatedStyles::default(),
        None,
    )
}

fn build_report_with_animated(
    matched: &Value,
    computed: &[(String, String)],
    sheets: &HashMap<String, Value>,
    page: &PageInfo,
    opts: &CssOptions,
    animated: &AnimatedStyles,
    queried: Option<&[Vec<String>]>,
) -> Report {
    let non_inheriting = non_inheriting(matched);
    let inheriting = registered_inheriting(matched);
    let mut hidden_ua = 0;
    let mut declared = Declared::default();

    // The element's own styles. Custom properties resolve against the
    // element's computed values.
    let element_vars = custom_properties(computed);
    let (blocks, mut claimed, mut inherited_keys) =
        element_cascade(matched, computed, sheets, page, &animated.element);
    let all = all_properties(computed, &blocks);
    let rules = shown_blocks(
        blocks,
        Some(&element_vars),
        None,
        opts.ua_rules,
        &mut hidden_ua,
        &mut declared,
    );

    // Ancestors, nearest first. Only inheritable declarations reach the
    // element, and only where neither it nor a nearer ancestor sets them.
    // The page's labels and values pair with CDP's ancestors by position,
    // which is only right when both walks took the same path.
    let levels = items(&matched["inherited"]);
    let ancestors: &[AncestorInfo] = if page.depth == levels.len() {
        &page.ancestors
    } else {
        &[]
    };
    let mut inherited = Vec::new();
    let no_animated = AnimatedProperties::default();
    for (i, level) in levels.iter().enumerate() {
        let info = ancestors.get(i);
        // A logical longhand maps to the physical side this ancestor sets,
        // which is also the side a child's explicit `inherit` reads. The
        // child's flow can be the opposite one (RTL under an LTR parent).
        let flow = info
            .and_then(|a| a.flow.as_ref())
            .map(|(writing_mode, direction)| (writing_mode.as_str(), direction.as_str()));
        let key = move |name: &str| mapped_key(name, flow);
        let mut blocks = node_blocks(
            &level["inlineStyle"],
            &level["matchedCSSRules"],
            &Value::Null,
            sheets,
            &page.longhands,
        );
        for block in &mut blocks {
            // `all` is not itself an inherited property, but its reset of
            // inherited ones stops a more distant ancestor's declarations.
            // Keep it for the cascade, which expands it over the properties
            // that can reach the element. A shorthand can also reach it
            // through one longhand the element explicitly inherits; the
            // others must not be marked active.
            block.decls.retain(|d| {
                let k = key(&d.key());
                k == "all"
                    || is_inherited(&k, &non_inheriting)
                    || inherited_keys.contains(&k)
                    || d.longhands.iter().any(|l| inherited_keys.contains(&key(l)))
            });
            for d in &mut block.decls {
                let k = key(&d.key());
                if k != "all" && !is_inherited(&k, &non_inheriting) && !inherited_keys.contains(&k)
                {
                    d.longhands.retain(|l| {
                        let lk = key(l);
                        is_inherited(&lk, &non_inheriting) || inherited_keys.contains(&lk)
                    });
                }
            }
        }
        // Only properties that can reach the element are an inherited `all`'s
        // reset: those that inherit by default, and those the element (or a
        // nearer ancestor) explicitly takes from its parent.
        let inherited_all: HashSet<String> = all
            .iter()
            .filter(|name| is_inherited(name, &non_inheriting) || inherited_keys.contains(*name))
            .cloned()
            .collect();
        // CDP reports an ancestor's animations on its own entry, not on the
        // child's: use that entry's properties when judging this ancestor's
        // declarations, and never fold them into the child's set.
        let ancestor_animated = animated.ancestors.get(i).unwrap_or(&no_animated);
        let queried_set: Option<HashSet<String>> = queried
            .and_then(|names| names.get(i))
            .map(|names| names.iter().cloned().collect());
        let ancestor = NodeFacts {
            vars: info.map(|a| &a.vars),
            queried: queried_set.as_ref(),
            flow,
            rejected: &[],
            inheriting: Some(&inheriting),
        };
        let stop_on_unset: HashSet<String> = inherited_keys
            .iter()
            .filter(|key| !is_inherited(key, &non_inheriting))
            .cloned()
            .collect();
        let (set_here, more_parent) = cascade(
            &mut blocks,
            &claimed,
            &ancestor,
            &non_inheriting,
            &inherited_all,
            &stop_on_unset,
            ancestor_animated,
        );
        claimed.extend(set_here.iter().cloned());
        // An effect on this ancestor supplies the value the element inherits,
        // so a declaration further up is not in use. Keep the inheritance path
        // when an animation was displaced by an important declaration here.
        let supplied = supplying_effects(&blocks, ancestor_animated, flow, &inherited_all);
        claimed.extend(supplied.iter().cloned());
        // A longhand decided here no longer reaches from a further ancestor.
        // A non-inherited one stays eligible only while the parent takes it
        // from its own parent; otherwise the parent's initial value is what
        // the element inherits.
        inherited_keys.retain(|k| {
            !set_here.contains(k)
                && !supplied.contains(k)
                && (is_inherited(k, &non_inheriting) || more_parent.contains(k))
        });
        inherited_keys.extend(more_parent);
        let rules = shown_blocks(
            blocks,
            info.map(|a| &a.vars),
            queried_set.as_ref(),
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
        computed: pick_computed(
            computed,
            &declared,
            &page.shorthands,
            &page.longhands,
            &opts.computed,
        ),
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
            Status::Indeterminate => "[animation] ",
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
        // Normal precedence: an `!important` declaration can beat one in a
        // rule above it, which only the declarations' tags show.
        out.push_str("Rules (normal precedence order, highest first):\n");
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
    writeln!(out, "Note: {STATE_NOTE}").unwrap();
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
        "note": STATE_NOTE,
    }))
}

/// Labels the element and its ancestors (flat-tree parents, as CDP walks them
/// for inherited styles), reads on each ancestor the custom properties its
/// declarations reference, counts the ancestors, reads the element's computed
/// `props`, expands the shorthands in `expand` into their longhands, and says
/// which `[property, value]` pairs in `checks` the property rejects. Runs
/// with `this` bound to the element.
const PAGE_INFO_JS: &str = r#"function (varNames, props, expand, checks) {
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
  // CSS.supports parses as a standards-mode document does, while in quirks
  // mode a substituted `20` is still a length, so nothing is rejected there.
  const rejected = document.compatMode === 'BackCompat' ? [] : checks.filter(([p, v]) => !CSS.supports(p, v));
  const out = { element: label(this), ancestors: [], depth: 0, shorthands, longhands, rejected };
  // On to the root: a slot in a closed shadow root is no element's
  // assignedSlot, so this walk can skip ancestors CDP lists, and a count
  // that differs from CDP's is how the caller can tell.
  for (let el = parent(this); el; el = parent(el)) {
    const names = varNames[out.depth++];
    if (!names) continue;
    const cs = getComputedStyle(el);
    const vars = {};
    let defined;
    for (const n of names) {
      const v = cs.getPropertyValue(n).trim();
      // '' is both an empty value and no value. Only a defined custom
      // property is among the computed style's names.
      if (v || (defined = defined || new Set(cs)).has(n)) vars[n] = v;
    }
    out.ancestors.push({
      label: label(el),
      vars,
      flow: [
        cs.getPropertyValue('writing-mode').trim(),
        cs.getPropertyValue('direction').trim(),
      ],
    });
  }
  return out;
}"#;

/// A connection lost while writing `Runtime.releaseObjectGroup` leaves the
/// WebSocket unusable, so the command must stop rather than send another
/// request on it. Any other release failure does not change the result.
fn release_outcome(result: Result<u64>) -> Result<()> {
    match result {
        Err(e) if e.downcast_ref::<ConnectionLost>().is_some() => Err(e),
        _ => Ok(()),
    }
}

async fn page_info(
    client: &mut CdpClient,
    session_id: &str,
    node_id: i64,
    var_names: &[Vec<String>],
    props: &[String],
    expand: &[String],
    checks: &[(String, String)],
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
                "arguments": [
                    {"value": var_names},
                    {"value": props},
                    {"value": expand},
                    {"value": checks},
                ],
                "returnByValue": true,
            }),
        )
        .await;
    // A lost connection stops here: a release on a dead socket could wait out
    // the whole write budget before the caller hears about the loss.
    let result = match result {
        Err(e) if e.downcast_ref::<ConnectionLost>().is_some() => return Err(e),
        result => result,
    };
    // Nothing to wait for: the reply only says it was done. A failed write
    // leaves the stream unusable, so a loss here stops too; other release
    // errors do not change the result.
    release_outcome(
        client
            .send_raw_no_wait(
                Some(session_id),
                "Runtime.releaseObjectGroup",
                json!({"objectGroup": "css-styles"}),
            )
            .await,
    )?;
    let result = result?;
    if let Some(e) = result.get("exceptionDetails") {
        bail!(
            "labelling the element failed: {}",
            crate::remote_object::describe_exception(e)
        );
    }
    Ok(PageInfo::deserialize(&result["result"]["value"])?)
}

/// Take the `styleSheetAdded` headers out of the event buffer, leaving every
/// other event where the next command can see it. A sheet can load after
/// `CSS.enable` replies, so this is drained again after the matched-style
/// request, whose reply may carry the rule that uses the late header.
fn take_sheet_headers(client: &mut CdpClient, session_id: &str) -> HashMap<String, Value> {
    let (added, others): (Vec<Value>, Vec<Value>) = std::mem::take(&mut client.events)
        .into_iter()
        .partition(|e| {
            e["method"].as_str() == Some("CSS.styleSheetAdded")
                && e["sessionId"].as_str() == Some(session_id)
        });
    client.events = others.into();
    added
        .into_iter()
        .filter_map(|mut e| {
            let header = e["params"]["header"].take();
            Some((header["styleSheetId"].as_str()?.to_string(), header))
        })
        .collect()
}

/// The properties one `CSS.getAnimatedStylesForNode` entry sets, normalised
/// to the keys the cascade uses. An entry describes the node itself or one
/// ancestor in the inheritance chain: CDP puts each animation's style under
/// `animationStyles[].style` and the transition's under `transitionsStyle`,
/// both `CSSStyle`s with properties in `cssProperties`.
fn animated_entry_names(entry: &Value) -> AnimatedProperties {
    let animations = items(&entry["animationStyles"])
        .iter()
        .map(|animation| &animation["style"])
        .flat_map(|style| items(&style["cssProperties"]))
        .filter_map(|p| p["name"].as_str())
        .map(normalize_property_name)
        .collect();
    let transitions = items(&entry["transitionsStyle"]["cssProperties"])
        .iter()
        .filter_map(|p| p["name"].as_str())
        .map(normalize_property_name)
        .collect();
    AnimatedProperties {
        animations,
        transitions,
    }
}

/// Longhands a running animation or transition sets on the node and on each
/// of its ancestors. CDP reports these apart from the matched rules, so the
/// cascade can avoid calling a static declaration active when something else
/// currently sets its value. A browser without the call is treated as having
/// no animated properties anywhere in the chain.
async fn animated_properties(
    client: &mut CdpClient,
    session_id: &str,
    node_id: i64,
) -> Result<AnimatedStyles> {
    let result = client
        .send_to_target(
            session_id,
            "CSS.getAnimatedStylesForNode",
            json!({"nodeId": node_id}),
        )
        .await?;
    Ok(AnimatedStyles {
        element: animated_entry_names(&result),
        ancestors: items(&result["inherited"])
            .iter()
            .map(animated_entry_names)
            .collect(),
    })
}

/// The page's labels and the animation list are enrichments: an ordinary
/// refusal or an unsupported method leaves them out of the report. A page
/// that stopped answering or a lost connection is not optional — it must
/// reach the caller before another full-budget page call is started.
fn page_info_fallback(result: Result<PageInfo>, selector: &str) -> Result<PageInfo> {
    match result {
        Ok(page) => Ok(page),
        Err(e) if is_connection_or_page_failure(&e) => Err(e),
        Err(_) => Ok(PageInfo {
            element: selector.to_string(),
            ..Default::default()
        }),
    }
}

fn animated_fallback(result: Result<AnimatedStyles>) -> Result<AnimatedStyles> {
    match result {
        Ok(animated) => Ok(animated),
        Err(e) if is_connection_or_page_failure(&e) => Err(e),
        Err(_) => Ok(AnimatedStyles::default()),
    }
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
    let mut sheets = take_sheet_headers(client, session_id);

    let matched = client
        .send_to_target(
            session_id,
            "CSS.getMatchedStylesForNode",
            json!({"nodeId": node_id}),
        )
        .await?;
    // A sheet that loaded between the two replies is announced with the
    // matched styles, too late for the first drain but in time for the report.
    sheets.extend(take_sheet_headers(client, session_id));
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
    // shorthands' computed longhands one by one, guesses what a var()
    // shorthand sets from its longhands' names, and counts every value a
    // var() reference resolves to as one its property accepts. A page that
    // stops answering or a lost connection is no nicety: it must reach the
    // caller instead of starting another page call on the same renderer.
    // The element's cascade says which properties it takes from a parent, so
    // an ancestor declaration that reaches it through an explicit `inherit`
    // has the custom properties it references queried on that ancestor too.
    // This pass precedes the page call that supplies labels and longhand
    // expansions; its own defaults are enough for the key test, and anything
    // it misses stays unknown rather than resolving to a guessed fallback.
    let (_, _, inherited_keys) = element_cascade(
        &matched,
        &computed,
        &sheets,
        &PageInfo::default(),
        &AnimatedProperties::default(),
    );
    let names = ancestor_var_names(&matched, &inherited_keys);
    let props = element_props_to_read(&matched, &computed, &opts.computed);
    let expand = shorthands_to_expand(&matched, &computed, &opts.computed);
    let checks = substituted_values(&matched, &custom_properties(&computed));
    let page = page_info_fallback(
        page_info(
            client, session_id, node_id, &names, &props, &expand, &checks,
        )
        .await,
        selector,
    )?;
    // An animation or transition setting a property is not observable from the
    // matched rules or the computed value alone. A browser that does not know
    // the CDP call is treated as having none.
    let animated = animated_fallback(animated_properties(client, session_id, node_id).await)?;

    Ok(build_report_with_animated(
        &matched,
        &computed,
        &sheets,
        &page,
        opts,
        &animated,
        Some(&names),
    ))
}

/// Turn the CSS and DOM agents off again. Left on, the CSS agent keeps
/// tracking stylesheets for the life of the tab's session, and the next
/// css-styles would get no CSS.styleSheetAdded events to name files. CDP
/// runs them in order, so waiting for the last one covers both, but only
/// up to `limit`: a page that didn't answer the inspection won't answer
/// this either, and the daemon serves nobody meanwhile. Chrome still runs
/// them once the page is free. The two writes keep the client's own bound:
/// they are small, and after a lost connection, where one could stall,
/// there is no cleanup (see `css_styles`).
///
/// A dialog recorded during the inspection makes the guarded send refuse to
/// write at all, so with one open on this session both commands are queued
/// without that guard. The renderer runs them once the dialog is settled.
async fn disable_agents(
    client: &mut CdpClient,
    session_id: &str,
    limit: std::time::Duration,
) -> Result<()> {
    client
        .send_raw_no_wait(Some(session_id), "CSS.disable", json!({}))
        .await?;
    // Let the guarded send decide whether this session has a dialog. The
    // public `open_dialog` only mirrors the newest one, which can belong to a
    // different session; reading it would send DOM.disable through a guard
    // that then refuses it and leaves the agent enabled. On that refusal,
    // queue the write without the guard, as for CSS.disable.
    match client
        .send_to_target_within(session_id, "DOM.disable", json!({}), limit)
        .await
    {
        Ok(_) => Ok(()),
        Err(e) if e.downcast_ref::<DialogOpen>().is_some() => client
            .send_raw_no_wait(Some(session_id), "DOM.disable", json!({}))
            .await
            .map(|_| ()),
        Err(e) => Err(e),
    }
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
    // A lost connection leaves no agents to turn off: Chrome drops a
    // session's agents with its connection, and the daemon exits on the
    // error. Writing to it anyway could wait out the whole call timeout,
    // behind a write the inspection never finished.
    let report = match collect(client, session_id, selector, &opts).await {
        Err(e) if e.downcast_ref::<ConnectionLost>().is_some() => return Err(e),
        report => report,
    };
    // Otherwise turn the agents off whatever happened. A lost connection on
    // cleanup still takes precedence over a selector miss: the daemon has to
    // retire the dead socket rather than report an ordinary error on it.
    let disabled = disable_agents(client, session_id, SETUP_CALL_TIMEOUT).await;
    let report = match (report, disabled) {
        (_, Err(e)) if e.downcast_ref::<ConnectionLost>().is_some() => return Err(e),
        (Err(e), _) => return Err(e),
        (Ok(report), _) => report,
    };

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
    use crate::cdp::{DialogOpen, OpenDialog, PageUnresponsive};

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
                flow: None,
            }],
            depth: 1,
            shorthands: HashMap::from([("padding".to_string(), "8px".to_string())]),
            longhands: HashMap::new(),
            rejected: vec![],
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

    /// The rules come in normal precedence order, which the heading says:
    /// `.btn.primary`'s important color beats the inline style above it.
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
Rules (normal precedence order, highest first):
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
(1 user-agent rule(s) hidden; --ua-rules shows them)
Note: Matched rules, computed values and page details are read in separate requests; if the page changes styles during the inspection they may describe different states."
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
        assert_eq!(v["note"], STATE_NOTE);
    }

    #[test]
    fn later_declaration_in_a_block_wins() {
        let style = authored(&[("color", "red"), ("color", "blue")], &[("color", "blue")]);
        let mut blocks = vec![block(&style)];
        cascade(
            &mut blocks,
            &HashSet::new(),
            &NodeFacts::default(),
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            &AnimatedProperties::default(),
        );
        let status: Vec<Status> = blocks[0].decls.iter().map(|d| d.status).collect();
        assert_eq!(status, [Status::Overridden, Status::Active]);
    }

    /// `all` is one CDP declaration, but it resets the properties lower rules
    /// set. One `all` row stays in the output.
    #[test]
    fn all_resets_lower_priority_longhands() {
        let mut m = matched();
        m["inlineStyle"] = Value::Null;
        m["matchedCSSRules"] = json!([
            rule("p", 7, authored(&[("color", "red")], &[("color", "red")])),
            rule("p.reset", 10, authored(&[("all", "initial")], &[])),
        ]);
        let report = build_report(&m, &computed(), &sheets(), &page(), &CssOptions::default());
        assert_eq!(report.rules.len(), 2);
        assert_eq!(report.rules[0].decls.len(), 1);
        assert_eq!(report.rules[0].decls[0].name, "all");
        assert_eq!(report.rules[0].decls[0].status, Status::Active);
        assert_eq!(report.rules[1].decls[0].status, Status::Overridden);
    }

    /// An ancestor's `all` is kept in the inherited pass: it resets the
    /// grandparent's inheritable declaration, but only over properties that
    /// can reach the element. Without the parent's reset the grandparent wins.
    #[test]
    fn ancestor_all_resets_a_grandparent_declaration() {
        let grandparent = rule(
            "html",
            1,
            authored(&[("color", "red")], &[("color", "red")]),
        );
        let parent = rule("body", 2, authored(&[("all", "initial")], &[]));
        let m = json!({
            "inherited": [
                {"matchedCSSRules": [parent]},
                {"matchedCSSRules": [grandparent]},
            ],
            "cssPropertyRegistrations": [],
        });
        let info = PageInfo { depth: 2, ..page() };
        let report = build_report(&m, &computed(), &sheets(), &info, &CssOptions::default());
        assert_eq!(report.inherited.len(), 2);
        assert_eq!(report.inherited[0].rules[0].decls[0].name, "all");
        assert_eq!(report.inherited[0].rules[0].decls[0].status, Status::Active);
        assert_eq!(
            report.inherited[1].rules[0].decls[0].status,
            Status::Overridden
        );

        let mut m = m;
        m["inherited"][0] = json!({"matchedCSSRules": [rule("body", 2, authored(
            &[("margin", "0")],
            &[("margin-top", "0px")],
        ))]});
        let report = build_report(&m, &computed(), &sheets(), &info, &CssOptions::default());
        // The parent's non-inheritable margin is not shown, so the
        // grandparent is the only inherited view, and its color wins.
        assert_eq!(report.inherited.len(), 1);
        assert_eq!(report.inherited[0].rules[0].decls[0].status, Status::Active);
    }

    /// A later declaration in the block still wins its own longhand, and the
    /// keys CSS excludes from `all` are never part of its expansion.
    #[test]
    fn all_leaves_excluded_and_later_declarations_alone() {
        let style = authored(
            &[
                ("all", "initial"),
                ("color", "blue"),
                ("direction", "rtl"),
                ("unicode-bidi", "isolate"),
                ("--tone", "red"),
            ],
            &[
                ("color", "blue"),
                ("direction", "rtl"),
                ("unicode-bidi", "isolate"),
                ("--tone", "red"),
            ],
        );
        let mut blocks = vec![block(&style)];
        let all = all_properties(&computed(), &blocks);
        assert!(!all.contains("direction"));
        assert!(!all.contains("unicode-bidi"));
        assert!(!all.contains("--tone"));
        let (decided, _) = cascade(
            &mut blocks,
            &HashSet::new(),
            &NodeFacts::default(),
            &HashSet::new(),
            &all,
            &HashSet::new(),
            &AnimatedProperties::default(),
        );
        assert_eq!(
            blocks[0].decls.iter().map(|d| d.status).collect::<Vec<_>>(),
            [Status::Active; 5]
        );
        assert!(decided.contains("color"));
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
        cascade(
            &mut blocks,
            &HashSet::new(),
            &NodeFacts::default(),
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            &AnimatedProperties::default(),
        );
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

    /// A cyclic custom property is invalid at computed-value time: its
    /// declaration has no resolved value, while a noncyclic fallback and a
    /// reference that uses the fallback keep theirs.
    #[test]
    fn cyclic_custom_property_has_no_resolved_value() {
        let block = block(&authored(
            &[
                ("--tone", "var(--tone, red)"),
                ("--absent-tone", "var(--absent, red)"),
                ("color", "var(--tone, blue)"),
                ("--a", "var(--b, red)"),
                ("--b", "var(--a, blue)"),
            ],
            &[],
        ));
        // Chrome leaves a cyclic property out of the computed custom values.
        let vars = HashMap::new();
        let mut declared = Declared::default();
        let shown = shown_blocks(vec![block], Some(&vars), None, false, &mut 0, &mut declared);
        let resolved: Vec<Option<&str>> = shown[0]
            .decls
            .iter()
            .map(|d| d.resolved.as_deref())
            .collect();
        assert_eq!(resolved, [None, Some("red"), Some("blue"), None, None]);
    }

    #[test]
    fn var_names_include_fallbacks() {
        let mut names = Vec::new();
        var_names("var(--a, var(--b, 1px)) var( --c )", &mut names);
        assert_eq!(names, ["--a", "--b", "--c"]);

        // A page can nest fallbacks as deep as it likes; the walk stops where
        // substitution does, instead of recursing until the stack runs out.
        let depth = 100_000;
        let nested = format!(
            "{}1px{}",
            (0..depth)
                .map(|i| format!("var(--v{i}, "))
                .collect::<String>(),
            ")".repeat(depth)
        );
        let mut names = Vec::new();
        var_names(&nested, &mut names);
        assert_eq!(names.len(), 17);
        assert_eq!(names[16], "--v16");
    }

    #[test]
    fn ancestor_var_names_skip_non_inheritable_declarations() {
        let mut m = matched();
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] =
            authored(&[("color", "var(--fg)"), ("margin", "var(--m)")], &[]);
        assert_eq!(
            ancestor_var_names(&m, &HashSet::new()),
            vec![vec!["--fg".to_string()]]
        );
    }

    /// An explicitly inherited declaration brings its ancestor's `var()`
    /// references along. A name that was not queried stays unknown rather
    /// than showing its fallback as if it were resolved.
    #[test]
    fn explicit_inherit_queries_the_ancestors_variables() {
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = authored(
            &[("background-color", "inherit")],
            &[("background-color", "inherit")],
        );
        let value = "var(--tone, blue)";
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] = authored(
            &[("background-color", value)],
            &[("background-color", value)],
        );

        let (_, _, inherited_keys) = element_cascade(
            &m,
            &computed(),
            &sheets(),
            &page(),
            &AnimatedProperties::default(),
        );
        assert!(inherited_keys.contains("background-color"));
        assert_eq!(
            ancestor_var_names(&m, &inherited_keys),
            vec![vec!["--tone".to_string()]]
        );
        // Without that key test the ancestor's `var(--tone, blue)` is never
        // asked about, even though its row is shown.
        let without = ancestor_var_names(&m, &HashSet::new());
        assert!(!without[0].iter().any(|n| n == "--tone"));

        // Queried names cover the fixture's inherited `color` but not the
        // ancestor's `--tone`, so its fallback must stay unclaimed.
        let unqueried = vec![vec!["--fg".to_string()]];
        let report = build_report_with_animated(
            &m,
            &computed(),
            &sheets(),
            &page(),
            &CssOptions::default(),
            &AnimatedStyles::default(),
            Some(&unqueried),
        );
        assert_eq!(report.inherited[0].rules[0].decls[0].resolved, None);

        let mut info = page();
        info.ancestors[0].vars.insert("--tone".into(), "red".into());
        let queried = vec![vec!["--tone".to_string()]];
        let report = build_report_with_animated(
            &m,
            &computed(),
            &sheets(),
            &info,
            &CssOptions::default(),
            &AnimatedStyles::default(),
            Some(&queried),
        );
        assert_eq!(
            report.inherited[0].rules[0].decls[0].resolved.as_deref(),
            Some("red")
        );
    }

    /// A parent's logical longhand maps to the physical side the child
    /// explicitly inherits. Its `var()` names must be queried on that parent,
    /// and a name never queried must not take its fallback as if the page had
    /// confirmed it missing.
    #[test]
    fn logical_ancestor_longhand_queries_its_variables() {
        let m = json!({
            "inlineStyle": authored(
                &[("margin-left", "inherit")],
                &[("margin-left", "inherit")]
            ),
            "matchedCSSRules": json!([]),
            "inherited": [
                {"matchedCSSRules": [rule("parent", 2, authored(
                    &[("margin-inline-start", "var(--gap, inherit)")],
                    &[("margin-inline-start", "var(--gap, inherit)")],
                ))]},
                {"matchedCSSRules": [rule("grandparent", 1, authored(
                    &[("margin-left", "40px")],
                    &[("margin-left", "40px")],
                ))]},
            ],
            "cssPropertyRegistrations": [],
        });
        let (_, _, inherited_keys) = element_cascade(
            &m,
            &computed(),
            &sheets(),
            &PageInfo::default(),
            &AnimatedProperties::default(),
        );
        assert!(inherited_keys.contains("margin-left"));
        // A logical name must match the inherited physical key so `--gap` is
        // asked about on the parent.
        assert_eq!(
            ancestor_var_names(&m, &inherited_keys)[0],
            vec!["--gap".to_string()]
        );

        let info = |gap: Option<&str>| {
            let mut parent = AncestorInfo {
                label: "parent".into(),
                vars: HashMap::new(),
                flow: Some(("horizontal-tb".into(), "ltr".into())),
            };
            if let Some(gap) = gap {
                parent.vars.insert("--gap".into(), gap.into());
            }
            PageInfo {
                element: "child".into(),
                ancestors: vec![
                    parent,
                    AncestorInfo {
                        label: "grandparent".into(),
                        vars: HashMap::new(),
                        flow: Some(("horizontal-tb".into(), "ltr".into())),
                    },
                ],
                depth: 2,
                ..Default::default()
            }
        };
        let report = |info: &PageInfo, queried: Vec<Vec<String>>| {
            build_report_with_animated(
                &m,
                &computed(),
                &sheets(),
                info,
                &CssOptions::default(),
                &AnimatedStyles::default(),
                Some(&queried),
            )
        };
        let status = |r: &Report, label: &str| {
            r.inherited
                .iter()
                .find(|view| view.from == label)
                .and_then(|view| view.rules.first())
                .and_then(|rule| rule.decls.first())
                .map(|d| d.status)
        };

        // The page reported `--gap`: the parent's declaration supplies the
        // value, resolves to it, and the grandparent's is not active.
        let with_gap = report(&info(Some("20px")), vec![vec!["--gap".into()], vec![]]);
        assert_eq!(status(&with_gap, "parent"), Some(Status::Active));
        assert_eq!(
            with_gap.inherited[0].rules[0].decls[0].resolved.as_deref(),
            Some("20px")
        );
        assert_ne!(status(&with_gap, "grandparent"), Some(Status::Active));

        // Never queried: `--gap` is unknown, so its `inherit` fallback cannot
        // send the declaration up to the grandparent.
        let unqueried = report(&info(None), vec![vec![], vec![]]);
        assert_eq!(status(&unqueried, "parent"), Some(Status::Active));
        assert_eq!(unqueried.inherited[0].rules[0].decls[0].resolved, None);
        assert_ne!(status(&unqueried, "grandparent"), Some(Status::Active));

        // Queried but missing: the page confirmed the fallback applies, so
        // the value really is inherited from the grandparent.
        let missing = report(&info(None), vec![vec!["--gap".into()], vec![]]);
        assert_eq!(status(&missing, "grandparent"), Some(Status::Active));
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
        assert_eq!(
            ancestor_var_names(&m, &HashSet::new()),
            vec![vec!["--b".to_string()]]
        );
        let report = report_for(&m, CssOptions::default());
        let body: Vec<&str> = report.inherited[0].rules[0]
            .decls
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(body, ["--free"]);
    }

    /// A registered custom property that inherits takes the parent's computed
    /// value when its own substitution fails; an unregistered one keeps the
    /// guaranteed-invalid value and blocks the ancestor.
    #[test]
    fn registered_inheriting_property_takes_the_parents_value() {
        let decls = [("--tone", "var(--missing)")];
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = authored(&decls, &decls);
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] =
            authored(&[("--tone", "red")], &[("--tone", "red")]);
        m["cssPropertyRules"] = json!([{
            "propertyName": {"text": "--tone"},
            "style": {"cssProperties": [{"name": "inherits", "value": "true"}]},
        }]);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(report.rules[0].decls[0].status, Status::Active);
        assert_eq!(report.inherited[0].rules[0].decls[0].status, Status::Active);

        m["cssPropertyRules"] = json!([]);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(
            report.inherited[0].rules[0].decls[0].status,
            Status::Overridden
        );
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
        let picked = pick_computed(
            &computed(),
            &declared,
            &HashMap::new(),
            &HashMap::new(),
            &extra,
        );
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
        let picked = pick_computed(&computed(), &declared, &shorthands, &HashMap::new(), &extra);
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
        let picked = pick_computed(&computed(), &declared, &shorthands, &HashMap::new(), &[]);
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
        let picked = pick_computed(&list, &declared, &values, &HashMap::new(), &[]);
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
        let picked = pick_computed(&computed(), &declared, &values, &HashMap::new(), &[]);
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

    /// `background-color` does not inherit by default, but an element that
    /// takes it explicitly with `inherit` still needs the parent declaration
    /// that supplies the value. An ancestor's `unset` is the initial value for
    /// that longhand and stops a more distant ancestor from supplying one.
    #[test]
    fn explicit_inherit_keeps_the_parent_declaration() {
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = authored(
            &[("background-color", "inherit")],
            &[("background-color", "inherit")],
        );
        let red = [("background-color", "red")];
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] = authored(&red, &red);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(report.rules[0].decls[0].status, Status::Active);
        let parent = &report.inherited[0].rules[0].decls[0];
        assert_eq!(parent.name, "background-color");
        assert_eq!(parent.status, Status::Active);

        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] = authored(
            &[("background-color", "unset")],
            &[("background-color", "unset")],
        );
        let html = rule("html", 1, authored(&red, &red));
        m["inherited"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "matchedCSSRules": [html] }));
        let report = report_for(&m, CssOptions::default());
        assert_eq!(report.inherited[0].rules[0].decls[0].status, Status::Active);
        // The parent's `unset` is the initial value for this non-inherited
        // property, so the grandparent's red cannot reach the element and is
        // no longer an inherited declaration to show.
        assert_eq!(report.inherited.len(), 1);
    }

    /// A parent shorthand supplies a longhand the element explicitly
    /// inherits, even though the shorthand itself is not an inherited
    /// property. Only the inherited member is propagated.
    #[test]
    fn an_explicitly_inherited_longhand_matches_an_ancestor_shorthand() {
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = authored(
            &[("background-color", "inherit")],
            &[("background-color", "inherit")],
        );
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] = authored(
            &[("background", "red")],
            &[("background-color", "red"), ("background-image", "none")],
        );
        // The non-inherited member is not propagated, so it does not become
        // a computed value worth showing either.
        let mut computed = computed();
        computed.push(("background-image".into(), "none".into()));
        let report = build_report(&m, &computed, &sheets(), &page(), &CssOptions::default());
        let parent = &report.inherited[0].rules[0].decls[0];
        assert_eq!(parent.name, "background");
        assert_eq!(parent.status, Status::Active);
        assert!(!report.computed.iter().any(|(n, _)| n == "background-image"));
    }

    /// The element inherits `background-color`, so the parent's `all` reset
    /// includes it and beats that parent's own red declaration. Without the
    /// reset red still supplies the value.
    #[test]
    fn an_ancestors_all_includes_explicitly_inherited_keys() {
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = authored(
            &[("background-color", "inherit")],
            &[("background-color", "inherit")],
        );
        let red = [("background-color", "red")];
        let parent = |decls: &[(&str, &str)], longhands: &[(&str, &str)]| {
            let mut m = m.clone();
            m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] = authored(decls, longhands);
            report_for(&m, CssOptions::default())
        };
        let report = parent(&red, &red);
        assert_eq!(report.inherited[0].rules[0].decls[0].status, Status::Active);

        let report = parent(&[("background-color", "red"), ("all", "initial")], &red);
        let decls = &report.inherited[0].rules[0].decls;
        assert_eq!(decls[0].name, "background-color");
        assert_eq!(decls[0].status, Status::Overridden);
        assert_eq!(decls[1].name, "all");
        assert_eq!(decls[1].status, Status::Active);
    }

    /// A non-inherited key only reaches further ancestors while the nearer
    /// one explicitly inherits it; otherwise its initial value is what the
    /// element gets.
    #[test]
    fn a_non_inherited_key_needs_the_parent_to_inherit_it() {
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = authored(
            &[("background-color", "inherit")],
            &[("background-color", "inherit")],
        );
        let html = rule(
            "html",
            1,
            authored(
                &[("background-color", "red")],
                &[("background-color", "red")],
            ),
        );
        m["inherited"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "matchedCSSRules": [html] }));
        let report = report_for(&m, CssOptions::default());
        assert_eq!(report.inherited.len(), 1);

        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] = authored(
            &[("background-color", "inherit")],
            &[("background-color", "inherit")],
        );
        let report = report_for(&m, CssOptions::default());
        assert_eq!(report.inherited[1].rules[0].decls[0].status, Status::Active);
    }

    /// `unset` on a non-inherited property uses the initial value, but on one
    /// that inherits by default it still takes the parent's.
    #[test]
    fn unset_only_inherits_properties_that_inherit_by_default() {
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = authored(
            &[("background-color", "unset"), ("color", "unset")],
            &[("background-color", "unset"), ("color", "unset")],
        );
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] = authored(
            &[("background-color", "red"), ("color", "blue")],
            &[("background-color", "red"), ("color", "blue")],
        );
        let report = report_for(&m, CssOptions::default());
        let decls = &report.inherited[0].rules[0].decls;
        // `background-color: unset` uses the initial value, so the parent's
        // red is not an inherited declaration. `color: unset` still takes
        // the parent's blue.
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0].name, "color");
        assert_eq!(decls[0].status, Status::Active);
    }

    /// An animation or transition sets a property above the static cascade, so
    /// a rule that wins it is shown as indeterminate, not active. Properties
    /// without one keep their definite status.
    #[test]
    fn animated_properties_are_not_marked_active() {
        let reply = json!({
            "animationStyles": [
                {"name": "fade", "style": {"cssProperties": [{"name": "Opacity", "value": "0"}]}}
            ],
            "transitionsStyle": {"cssProperties": [{"name": "width", "value": "10px"}]},
        });
        let animated = animated_entry_names(&reply);
        assert_eq!(animated.animations, HashSet::from(["opacity".into()]));
        assert_eq!(animated.transitions, HashSet::from(["width".into()]));
        // A node with no animated styles has nothing to mark indeterminate.
        let none = animated_entry_names(&json!({}));
        assert!(none.animations.is_empty() && none.transitions.is_empty());

        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = authored(
            &[("opacity", "1"), ("color", "red")],
            &[("opacity", "1"), ("color", "red")],
        );
        let report = build_report_with_animated(
            &m,
            &computed(),
            &sheets(),
            &page(),
            &CssOptions::default(),
            &AnimatedStyles {
                element: animated,
                ..Default::default()
            },
            None,
        );
        let decls = &report.rules[0].decls;
        assert_eq!(decls[0].status, Status::Indeterminate);
        assert_eq!(decls[1].status, Status::Active);
        let text = format_text(&report);
        assert!(text.contains("[animation] opacity: 1;"), "{text}");
        assert!(text.contains("  color: red;"), "{text}");
        let json = report_json(&report).unwrap();
        assert_eq!(json["rules"][0]["properties"][0]["status"], "indeterminate");
        assert_eq!(json["rules"][0]["properties"][1]["status"], "active");
    }

    /// CDP reports an ancestor's animations on that ancestor's own entry. A
    /// child's running animation supplies the property even with no static
    /// declaration of its own, so the ancestor's declaration is not active;
    /// the ancestor's own animation only makes its declaration indeterminate.
    #[test]
    fn an_ancestors_animation_is_used_only_for_that_ancestor() {
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = Value::Null;
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] =
            authored(&[("color", "green")], &[("color", "green")]);
        let animations = |items: &[&str]| AnimatedProperties {
            animations: items.iter().map(|n| (*n).to_string()).collect(),
            ..Default::default()
        };
        let none = AnimatedProperties::default();
        let status = |element: AnimatedProperties, ancestors: Vec<AnimatedProperties>| {
            let report = build_report_with_animated(
                &m,
                &computed(),
                &sheets(),
                &page(),
                &CssOptions::default(),
                &AnimatedStyles { element, ancestors },
                None,
            );
            report.inherited[0].rules[0].decls[0].status
        };
        // The child's animation supplies the value: the parent's static
        // declaration is no longer the active one.
        assert_eq!(
            status(animations(&["color"]), vec![none]),
            Status::Overridden
        );
        // With no effect on the child the parent still supplies the value;
        // an animation on the parent itself leaves its status indeterminate.
        assert_eq!(
            status(
                AnimatedProperties::default(),
                vec![AnimatedProperties::default()]
            ),
            Status::Active
        );
        assert_eq!(
            status(AnimatedProperties::default(), vec![animations(&["color"])]),
            Status::Indeterminate
        );
    }

    /// An effect on an ancestor supplies the value the element inherits, so a
    /// static declaration further up is not in use. An animation displaced by
    /// an important declaration on that ancestor does not supply the value; a
    /// transition does.
    #[test]
    fn ancestor_effect_stops_more_distant_declarations() {
        let mut m = matched();
        m["inlineStyle"] = Value::Null;
        m["matchedCSSRules"] = json!([]);
        let far = rule(
            "html",
            1,
            authored(&[("color", "green")], &[("color", "green")]),
        );
        let effect = |animations: &[&str], transitions: &[&str]| AnimatedProperties {
            animations: animations.iter().map(|n| (*n).to_string()).collect(),
            transitions: transitions.iter().map(|n| (*n).to_string()).collect(),
        };
        let far_status = |m: &Value, ancestors: Vec<AnimatedProperties>| {
            let report = build_report_with_animated(
                m,
                &computed(),
                &sheets(),
                &page(),
                &CssOptions::default(),
                &AnimatedStyles {
                    ancestors,
                    ..Default::default()
                },
                None,
            );
            report
                .inherited
                .iter()
                .flat_map(|g| &g.rules)
                .flat_map(|r| &r.decls)
                .find(|d| d.value == "green")
                .unwrap()
                .status
        };
        m["inherited"] = json!([{"matchedCSSRules": []}, {"matchedCSSRules": [far]}]);
        let none = || effect(&[], &[]);
        assert_eq!(far_status(&m, vec![none(), none()]), Status::Active);
        assert_eq!(
            far_status(&m, vec![effect(&["color"], &[]), none()]),
            Status::Overridden
        );
        assert_eq!(
            far_status(&m, vec![effect(&[], &["color"]), none()]),
            Status::Overridden
        );

        // An important declaration on the nearer ancestor wins over an
        // animation, so the more distant declaration keeps its status.
        m["inherited"][0]["matchedCSSRules"] = json!([rule(
            "body",
            0,
            authored(
                &[("color", "inherit !important")],
                &[("color", "inherit !important")]
            )
        )]);
        assert_eq!(
            far_status(&m, vec![effect(&["color"], &[]), none()]),
            Status::Active
        );
        // A transition outranks the important declaration.
        assert_eq!(
            far_status(&m, vec![effect(&[], &["color"]), none()]),
            Status::Overridden
        );
    }

    /// An animation sits below `!important` author declarations in the
    /// cascade; a transition sits above them. Only a declaration whose value
    /// is actually in use stays active.
    #[test]
    fn animation_does_not_displace_an_important_declaration() {
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = authored(
            &[("color", "red !important")],
            &[("color", "red !important")],
        );
        let status = |effect: AnimatedProperties| {
            let report = build_report_with_animated(
                &m,
                &computed(),
                &sheets(),
                &page(),
                &CssOptions::default(),
                &AnimatedStyles {
                    element: effect,
                    ..Default::default()
                },
                None,
            );
            report.rules[0].decls[0].status
        };
        assert_eq!(
            status(AnimatedProperties {
                animations: HashSet::from(["color".into()]),
                ..Default::default()
            }),
            Status::Active
        );
        assert_eq!(
            status(AnimatedProperties {
                transitions: HashSet::from(["color".into()]),
                ..Default::default()
            }),
            Status::Indeterminate
        );
    }

    /// `color: inherit !important` defeats an animation on `color`, so the
    /// parent's color stays active; a transition still outranks `!important`.
    #[test]
    fn important_inherit_keeps_parent_active_against_animation() {
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = authored(
            &[("color", "inherit !important")],
            &[("color", "inherit !important")],
        );
        let parent_status = |effect: AnimatedProperties| {
            let report = build_report_with_animated(
                &m,
                &computed(),
                &sheets(),
                &page(),
                &CssOptions::default(),
                &AnimatedStyles {
                    element: effect,
                    ..Default::default()
                },
                None,
            );
            report.inherited[0].rules[0]
                .decls
                .iter()
                .find(|d| d.name == "color")
                .unwrap()
                .status
        };
        assert_eq!(
            parent_status(AnimatedProperties {
                animations: HashSet::from(["color".into()]),
                ..Default::default()
            }),
            Status::Active
        );
        assert_eq!(
            parent_status(AnimatedProperties {
                transitions: HashSet::from(["color".into()]),
                ..Default::default()
            }),
            Status::Overridden
        );
    }

    /// `color: currentColor` inherits the parent's color, whereas
    /// `border-color: currentColor` uses the element's own color.
    #[test]
    fn current_color_inherits_only_for_color() {
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = authored(
            &[("color", "currentColor"), ("border-color", "currentColor")],
            &[("color", "currentColor"), ("border-color", "currentColor")],
        );
        let report = build_report(&m, &computed(), &sheets(), &page(), &CssOptions::default());
        let parent_color = report.inherited[0].rules[0]
            .decls
            .iter()
            .find(|d| d.name == "color")
            .unwrap()
            .status;
        assert_eq!(parent_color, Status::Active);
    }

    /// An ancestor's logical longhand reaches an explicitly inheriting child
    /// under the physical side that ancestor's own flow gives it, not the
    /// child's or the authored name.
    #[test]
    fn ancestor_logical_longhands_map_with_the_ancestors_flow() {
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = authored(&[("margin-left", "inherit")], &[("margin-left", "inherit")]);
        let value = "20px";
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] = authored(
            &[("margin-inline-start", value)],
            &[("margin-inline-start", value)],
        );
        let info = |direction: &str| {
            let mut info = page();
            info.ancestors[0].flow = Some(("horizontal-tb".into(), direction.into()));
            info
        };
        let report = build_report(
            &m,
            &computed(),
            &sheets(),
            &info("ltr"),
            &CssOptions::default(),
        );
        let decl = &report.inherited[0].rules[0].decls[0];
        assert_eq!(decl.name, "margin-inline-start");
        assert_eq!(decl.status, Status::Active);

        // The same LTR parent has no margin-right for a margin-left request,
        // but an RTL parent maps the logical start to that side instead.
        let report = build_report(
            &m,
            &computed(),
            &sheets(),
            &info("rtl"),
            &CssOptions::default(),
        );
        assert!(report.inherited.is_empty());

        m["inlineStyle"] = authored(
            &[("margin-right", "inherit")],
            &[("margin-right", "inherit")],
        );
        let report = build_report(
            &m,
            &computed(),
            &sheets(),
            &info("rtl"),
            &CssOptions::default(),
        );
        assert_eq!(
            report.inherited[0].rules[0].decls[0].name,
            "margin-inline-start"
        );
    }

    /// A stalled page or a lost connection during either optional enrichment
    /// is the command's error; only an ordinary refusal gets the reduced
    /// report.
    #[test]
    fn a_page_failure_stops_both_enrichments() {
        for kind in ["page", "connection"] {
            let error = || match kind {
                "page" => anyhow::Error::new(PageUnresponsive("no answer".into())),
                _ => anyhow::Error::new(ConnectionLost("closed".into())),
            };
            let info = page_info_fallback(Err(error()), "p");
            assert!(is_connection_or_page_failure(&info.unwrap_err()), "{kind}");
            let animated = animated_fallback(Err(error()));
            assert!(
                is_connection_or_page_failure(&animated.unwrap_err()),
                "{kind}"
            );
        }
        let info = page_info_fallback(Err(anyhow::anyhow!("Method not found")), "p").unwrap();
        assert_eq!(info.element, "p");
        let animated = animated_fallback(Err(anyhow::anyhow!("Method not found"))).unwrap();
        assert!(
            animated.element.animations.is_empty()
                && animated.element.transitions.is_empty()
                && animated.ancestors.is_empty()
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
        cascade(
            &mut blocks,
            &HashSet::new(),
            &NodeFacts::default(),
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            &AnimatedProperties::default(),
        );
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
        let picked = pick_computed(
            &list,
            &Declared::default(),
            &HashMap::new(),
            &HashMap::new(),
            &extra,
        );
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
        let info = PageInfo { depth: 2, ..page() };
        let report = build_report(&m, &list, &sheets(), &info, &CssOptions::default());
        assert_eq!(report.rules[0].decls[0].resolved.as_deref(), Some("serif"));
        assert_eq!(
            report.inherited[0].rules[0].decls[0].resolved.as_deref(),
            Some("#222")
        );
        assert_eq!(report.inherited[1].from, "ancestor 2");
        assert_eq!(report.inherited[1].rules[0].decls[0].resolved, None);
    }

    /// `COLOR` is `color` to the cascade, to inheritance and to the computed
    /// values, and still shows as written; `--Tone` and `--tone` are two
    /// properties.
    #[test]
    fn property_names_ignore_case_except_custom_ones() {
        use Status::{Active, Overridden};
        let mut m = matched();
        m["inlineStyle"] = Value::Null;
        let decls = [
            ("color", "red"),
            ("COLOR", "blue"),
            ("--Tone", "1"),
            ("--tone", "2"),
        ];
        let longhands = [("color", "blue"), ("--Tone", "1"), ("--tone", "2")];
        m["matchedCSSRules"] = json!([rule(".a", 3, authored(&decls, &longhands))]);
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] =
            authored(&[("Font-Family", "serif")], &[("font-family", "serif")]);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(report.rules[0].decls[1].name, "COLOR");
        assert_eq!(
            statuses(&report.rules[0]),
            [Overridden, Active, Active, Active]
        );
        let body = &report.inherited[0].rules[0].decls[0];
        assert_eq!((body.name.as_str(), body.status), ("Font-Family", Active));
        let names: Vec<&str> = report.computed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["display", "width", "height", "color"]);
    }

    /// `revert-layer` hands a property to the next lower layer, where the
    /// highest declaration wins it while the rest of the reverting layer
    /// stays overridden; below the lowest author layer come the
    /// presentational attributes. `revert` goes straight to the user-agent
    /// origin. With nothing there, the ancestors' value applies. An
    /// important `revert-layer` rolls back to the layers below it in normal
    /// order, as Chrome does, so nothing on the element applies here.
    #[test]
    fn revert_layer_and_revert_roll_back() {
        use Status::{Active, Overridden};
        let color = |v: &str| authored(&[("color", v)], &[("color", v)]);
        let firsts =
            |r: &Report| -> Vec<Status> { r.rules.iter().map(|b| b.decls[0].status).collect() };
        let body_color = |r: &Report| r.inherited[0].rules[0].decls[0].status;
        let all = || CssOptions {
            ua_rules: true,
            ..Default::default()
        };
        let mut m = matched();
        m["inlineStyle"] = Value::Null;
        let ua = m["matchedCSSRules"][0].clone();
        // CDP lists rules lowest precedence first.
        m["matchedCSSRules"] = json!([
            ua,
            layered(".a", 3, "base", color("red")),
            layered(".b", 4, "theme", color("blue")),
            layered("#a", 5, "theme", color("revert-layer")),
        ]);
        let report = report_for(&m, all());
        // `#a`, `.b`, `.a`, then the user-agent rule.
        assert_eq!(firsts(&report), [Active, Overridden, Active, Overridden]);

        m["matchedCSSRules"] = json!([
            ua,
            layered(".a", 3, "base", color("red")),
            rule("#a", 5, color("revert")),
        ]);
        let report = report_for(&m, all());
        // `#a`, `.a`, then the user-agent rule.
        assert_eq!(firsts(&report), [Active, Overridden, Active]);

        m["attributesStyle"] = json!({"cssProperties": [{"name": "color", "value": "red"}]});
        m["matchedCSSRules"] = json!([rule("#a", 5, color("revert-layer"))]);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(firsts(&report), [Active, Active]);
        assert_eq!(body_color(&report), Overridden);
        m["matchedCSSRules"] = json!([rule("#a", 5, color("revert"))]);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(firsts(&report), [Active, Overridden]);
        assert_eq!(body_color(&report), Active);

        m["attributesStyle"] = Value::Null;
        m["matchedCSSRules"] = json!([
            layered("#a", 3, "base", color("revert-layer !important")),
            layered(".b", 4, "theme", color("red !important")),
            rule(".c", 5, color("blue")),
        ]);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(firsts(&report), [Overridden, Overridden, Active]);
        assert_eq!(body_color(&report), Active);

        // An injected sheet may be a user one, which `revert` would stop
        // at, so `revert` is left deciding the color as any value would.
        let mut injected = rule(".i", 2, color("green"));
        injected["rule"]["origin"] = json!("injected");
        m["matchedCSSRules"] = json!([injected, rule("#a", 5, color("revert"))]);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(firsts(&report), [Active, Overridden]);
        assert_eq!(body_color(&report), Overridden);
    }

    /// A winning `var()` reference with no value and no fallback acts as
    /// `unset`, so the ancestor's color applies and the element's earlier
    /// one stays overridden. Not when the reference has a value, nor for a
    /// custom property, nor on an ancestor whose values the page didn't give.
    #[test]
    fn unresolvable_var_acts_as_unset() {
        use Status::{Active, Overridden};
        let body_color = |r: &Report| r.inherited[0].rules[0].decls[0].status;
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        let decls = [("color", "blue"), ("color", "var(--missing)")];
        m["inlineStyle"] = authored(&decls, &[("color", "var(--missing)")]);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(statuses(&report.rules[0]), [Overridden, Active]);
        assert_eq!(body_color(&report), Active);

        let mut list = computed();
        list.push(("--missing".into(), "red".into()));
        let report = build_report(&m, &list, &sheets(), &page(), &CssOptions::default());
        assert_eq!(body_color(&report), Overridden);

        // An unregistered custom property gets no value at all instead.
        let custom = [("--fg", "var(--missing)")];
        m["inlineStyle"] = authored(&custom, &custom);
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] =
            authored(&[("--fg", "red")], &[("--fg", "red")]);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(body_color(&report), Overridden);

        // The page says the body has no `--nope`, so the html element's
        // color applies; with the page's values unknown, nothing changes.
        m["inlineStyle"] = Value::Null;
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] =
            authored(&[("color", "var(--nope)")], &[("color", "var(--nope)")]);
        let html = rule(
            "html",
            1,
            authored(&[("color", "purple")], &[("color", "purple")]),
        );
        m["inherited"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "matchedCSSRules": [html] }));
        let html_color = |r: &Report| r.inherited[1].rules[0].decls[0].status;
        let info = PageInfo { depth: 2, ..page() };
        let report = build_report(&m, &computed(), &sheets(), &info, &CssOptions::default());
        assert_eq!(html_color(&report), Active);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(html_color(&report), Overridden);
    }

    /// In horizontal left-to-right text `margin-inline-start` is
    /// `margin-left`, so the later of the two wins; right to left, they set
    /// different sides.
    #[test]
    fn flow_relative_longhands_cascade_with_their_physical_side() {
        use Status::{Active, Overridden};
        let mut m = matched();
        let decls = [("margin-left", "1px"), ("margin-inline-start", "2px")];
        m["inlineStyle"] = authored(&decls, &decls);
        let flow = |direction: &str| {
            let mut list = computed();
            list.push(("writing-mode".into(), "horizontal-tb".into()));
            list.push(("direction".into(), direction.into()));
            build_report(&m, &list, &sheets(), &page(), &CssOptions::default())
        };
        assert_eq!(statuses(&flow("ltr").rules[0]), [Overridden, Active]);
        assert_eq!(statuses(&flow("rtl").rules[0]), [Active, Active]);

        let side = |longhand: &str, writing_mode: &str, direction: &str| {
            physical(longhand, writing_mode, direction)
        };
        let ltr = |longhand: &str, writing_mode: &str| side(longhand, writing_mode, "ltr");
        assert_eq!(
            ltr("margin-inline-start", "vertical-rl").as_deref(),
            Some("margin-top")
        );
        assert_eq!(
            ltr("padding-block-start", "vertical-rl").as_deref(),
            Some("padding-right")
        );
        assert_eq!(
            ltr("padding-inline-start", "sideways-lr").as_deref(),
            Some("padding-bottom")
        );
        assert_eq!(
            ltr("inset-block-end", "vertical-lr").as_deref(),
            Some("right")
        );
        assert_eq!(
            side("border-inline-end-color", "horizontal-tb", "rtl").as_deref(),
            Some("border-left-color")
        );
        assert_eq!(
            ltr("scroll-padding-block-start", "horizontal-tb").as_deref(),
            Some("scroll-padding-top")
        );
        assert_eq!(
            ltr("max-inline-size", "vertical-rl").as_deref(),
            Some("max-height")
        );
        assert_eq!(
            ltr("block-size", "horizontal-tb").as_deref(),
            Some("height")
        );
        // Physical longhands and shorthands stay as they are.
        assert_eq!(ltr("margin-left", "horizontal-tb"), None);
        assert_eq!(ltr("margin-inline", "horizontal-tb"), None);
        assert_eq!(ltr("border-top-left-radius", "horizontal-tb"), None);
        assert_eq!(ltr("border-radius", "horizontal-tb"), None);
    }

    /// A corner radius is named block side first, a physical one top or
    /// bottom first. As in Chrome 158: left to right, `border-start-start-
    /// radius` and `border-top-left-radius` are one corner and the later
    /// wins; right to left it's the top-right corner; vertical-rl turns
    /// start-end into bottom-right and end-start into top-left.
    #[test]
    fn flow_relative_corners_cascade_with_their_physical_corner() {
        use Status::{Active, Overridden};
        let corners = [
            ("start-start", "horizontal-tb", "ltr", "top-left"),
            ("start-start", "horizontal-tb", "rtl", "top-right"),
            ("end-end", "horizontal-tb", "ltr", "bottom-right"),
            ("start-end", "vertical-rl", "ltr", "bottom-right"),
            ("end-start", "vertical-rl", "ltr", "top-left"),
            ("start-start", "vertical-lr", "rtl", "bottom-left"),
            ("start-start", "sideways-lr", "ltr", "bottom-left"),
        ];
        for (logical, writing_mode, direction, corner) in corners {
            assert_eq!(
                physical(&format!("border-{logical}-radius"), writing_mode, direction),
                Some(format!("border-{corner}-radius")),
                "{logical} in {writing_mode} {direction}"
            );
        }

        let mut m = matched();
        let decls = [
            ("border-top-left-radius", "2px"),
            ("border-start-start-radius", "8px"),
            ("border-end-end-radius", "6px"),
        ];
        m["inlineStyle"] = authored(&decls, &decls);
        let mut list = computed();
        list.push(("writing-mode".into(), "horizontal-tb".into()));
        list.push(("direction".into(), "ltr".into()));
        let report = build_report(&m, &list, &sheets(), &page(), &CssOptions::default());
        assert_eq!(statuses(&report.rules[0]), [Overridden, Active, Active]);
    }

    /// CDP's ancestors follow the flat tree into a closed shadow root, where
    /// the page's walk can't go. When the walk reaches a different number of
    /// ancestors, its labels and values would belong to other elements.
    #[test]
    fn a_walk_that_differs_from_cdps_labels_nothing() {
        let mut m = matched();
        let html = rule(
            "html",
            1,
            authored(&[("color", "var(--fg)")], &[("color", "var(--fg)")]),
        );
        m["inherited"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "matchedCSSRules": [html] }));
        let report = report_for(&m, CssOptions::default());
        let from: Vec<&str> = report.inherited.iter().map(|g| g.from.as_str()).collect();
        assert_eq!(from, ["ancestor 1", "ancestor 2"]);
        assert_eq!(report.inherited[0].rules[0].decls[0].resolved, None);

        let info = PageInfo { depth: 2, ..page() };
        let report = build_report(&m, &computed(), &sheets(), &info, &CssOptions::default());
        assert_eq!(report.inherited[0].from, "body");
        assert_eq!(
            report.inherited[0].rules[0].decls[0].resolved.as_deref(),
            Some("#222")
        );
    }

    /// Chrome 158 lists `@layer outer { @media (min-width: 1px) { @layer
    /// inner { … } } }` with its layers outermost first and its `ruleTypes`
    /// innermost first.
    #[test]
    fn nested_layers_keep_their_order() {
        let mut r = json!({
            "media": [{"text": "(min-width: 1px)", "source": "mediaRule"}],
            "layers": [{"text": "outer"}, {"text": "inner"}],
            "ruleTypes": ["LayerRule", "MediaRule", "LayerRule"],
        });
        assert_eq!(
            conditions(&r),
            ["@layer outer", "@media (min-width: 1px)", "@layer inner"]
        );
        assert_eq!(layer_key(&r), "outer.inner");
        r["ruleTypes"] = json!([]);
        assert_eq!(
            conditions(&r),
            ["@layer outer", "@layer inner", "@media (min-width: 1px)"]
        );
    }

    /// `color: var(--size)` with `--size: 20px` substitutes fine, but 20px is
    /// no color, so Chrome treats the declaration as `unset` and the body's
    /// color applies, not the element's earlier blue. The page is asked
    /// about the element's substituted values only, and only says no to
    /// some; with nothing rejected the reference decides the color.
    #[test]
    fn a_value_the_property_rejects_acts_as_unset() {
        use Status::{Active, Overridden};
        let body_color = |r: &Report| r.inherited[0].rules[0].decls[0].status;
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        let decls = [
            ("color", "blue"),
            ("color", "var(--size)"),
            ("--x", "var(--size)"),
            ("padding", "var(--size)"),
        ];
        m["inlineStyle"] = authored(&decls, &decls);
        let mut list = computed();
        list.push(("--size".into(), "20px".into()));
        let vars = custom_properties(&list);
        assert_eq!(
            substituted_values(&m, &vars),
            [
                ("color".to_string(), "20px".to_string()),
                ("padding".to_string(), "20px".to_string()),
            ]
        );

        let info = PageInfo {
            rejected: vec![("color".into(), "20px".into())],
            ..page()
        };
        let report = build_report(&m, &list, &sheets(), &info, &CssOptions::default());
        assert_eq!(
            statuses(&report.rules[0]),
            [Overridden, Active, Active, Active]
        );
        assert_eq!(body_color(&report), Active);

        let report = build_report(&m, &list, &sheets(), &page(), &CssOptions::default());
        assert_eq!(body_color(&report), Overridden);
    }

    /// A comment in the authored value is not part of the property name:
    /// `var(/* theme */ --tone, blue)` resolves to `--tone`, as in Blink.
    #[test]
    fn a_comment_does_not_become_part_of_the_var_name() {
        assert_eq!(split_var("/* theme */ --tone, blue)").unwrap().0, "--tone");
        assert_eq!(split_var("  --tone, blue)").unwrap().0, "--tone");
        assert_eq!(split_var("--tone)").unwrap().0, "--tone");
        let vars = HashMap::from([("--tone".to_string(), "red".to_string())]);
        assert_eq!(
            substituted("var(/* theme */ --tone, blue)", &vars).as_deref(),
            Some("red")
        );
    }

    /// An escaped identifier resolves to the same custom property as its
    /// plain spelling: the whitespace ending a hex escape continues the name,
    /// and neither the lookup nor the ancestor value request sees the escape.
    #[test]
    fn identifier_escapes_decode_before_custom_property_lookup() {
        let value = "color: var(--\\74 one, blue)";
        assert_eq!(var_name(" --\\74 one"), "--tone");
        let mut names = Vec::new();
        var_names(value, &mut names);
        assert_eq!(names, ["--tone"]);
        let vars = HashMap::from([("--tone".to_string(), "red".to_string())]);
        assert_eq!(substituted(value, &vars).as_deref(), Some("color: red"));
        // The authored spelling is still what a declaration displays.
        let decls = [("--tone", "red"), ("color", "var(--\\74 one, blue)")];
        let report = report_for(
            &json!({"inlineStyle": authored(&decls, &decls)}),
            CssOptions::default(),
        );
        assert_eq!(report.rules[0].decls[1].value, "var(--\\74 one, blue)");
    }

    /// Delimiters in an authored comment are not delimiters to Blink: the
    /// `)` and `,` below must not end the call or start its fallback.
    #[test]
    fn comments_do_not_end_var_arguments() {
        let vars = HashMap::from([("--tone".to_string(), "red".to_string())]);
        assert_eq!(
            resolve_vars("var(/* ) */ --tone, blue)", &vars).as_deref(),
            Some("red")
        );
        assert_eq!(
            resolve_vars("var(--tone/* , */, blue)", &vars).as_deref(),
            Some("red")
        );
        assert_eq!(
            resolve_vars("var(--tone, calc(1px/* ) */ + 2px))", &vars).as_deref(),
            Some("red")
        );
        // A call named in a comment is text, not a reference.
        assert_eq!(find_var("/* var(--tone) */ color"), None);
        assert_eq!(resolve_vars("/* var(--tone) */ color", &vars), None);
    }

    /// CSS decodes escapes in a function name, so `v\61 r(` is a `var()` call
    /// and uses the parent's value. Blink keeps the authored spelling in the
    /// declaration, which the report shows; only the lookup uses the decoded
    /// name. Strings and longer names are still not calls.
    #[test]
    fn escaped_function_names_are_var_calls() {
        let vars = HashMap::from([("--tone".to_string(), "red".to_string())]);
        for value in [r"v\61 r(--tone)", r"\76 ar(--tone)"] {
            let args_at = value.find('(').unwrap() + 1;
            assert_eq!(find_var(value), Some((0, args_at)), "{value}");
            assert_eq!(
                resolve_vars(value, &vars).as_deref(),
                Some("red"),
                "{value}"
            );
        }
        assert_eq!(find_var(r#""v\61 r(--tone)""#), None);
        assert_eq!(find_var(r"somev\61 r(--tone)"), None);
        let mut names = Vec::new();
        var_names(r"v\61 r(--tone, var(--other))", &mut names);
        assert_eq!(names, ["--tone", "--other"]);
    }

    /// `var()` substitutes tokens, not text. In Chrome 158, `var(--size)px`
    /// with `--size: 20` is a number and an identifier, no font size, so the
    /// body's applies; `var(--a)var(--b)` is two lengths, which a margin
    /// takes. The substituted value keeps them apart as Chrome writes them,
    /// with an empty comment, so `CSS.supports` sees what Chrome does.
    #[test]
    fn substitution_keeps_tokens_apart() {
        let vars = HashMap::from([
            ("--size".to_string(), "20".to_string()),
            ("--len".to_string(), "20px".to_string()),
            ("--a".to_string(), "1px".to_string()),
            ("--b".to_string(), "2px".to_string()),
            ("--dot".to_string(), "1.".to_string()),
            ("--none".to_string(), String::new()),
        ]);
        let resolve = |v: &str| resolve_vars(v, &vars);
        assert_eq!(resolve("var(--size)px").as_deref(), Some("20/**/px"));
        assert_eq!(resolve("var(--missing, 20)%").as_deref(), Some("20/**/%"));
        assert_eq!(resolve("var(--a)var(--b)").as_deref(), Some("1px/**/2px"));
        assert_eq!(resolve("var(--dot)5px").as_deref(), Some("1./**/5px"));
        // Nor does an empty value join what is around it: `/*` is no comment.
        assert_eq!(resolve("1/var(--none)*2").as_deref(), Some("1//**/*2"));
        // Tokens that stay apart anyway are left as they are.
        assert_eq!(resolve("var(--len)").as_deref(), Some("20px"));
        assert_eq!(
            resolve("calc(var(--size)*var(--len)/2)").as_deref(),
            Some("calc(20*20px/2)")
        );
        assert_eq!(
            resolve("rgb(var(--size),var(--size),0)").as_deref(),
            Some("rgb(20,20,0)")
        );

        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        let decls = [("font-size", "var(--size)px")];
        m["inlineStyle"] = authored(&decls, &decls);
        let parent = [("font-size", "30px")];
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] = authored(&parent, &parent);
        let mut list = computed();
        list.push(("--size".into(), "20".into()));
        // Chrome's `CSS.supports` turns down what the page is asked about.
        let checks = substituted_values(&m, &custom_properties(&list));
        assert_eq!(checks, [("font-size".to_string(), "20/**/px".to_string())]);
        let info = PageInfo {
            rejected: checks,
            ..page()
        };
        let report = build_report(&m, &list, &sheets(), &info, &CssOptions::default());
        assert_eq!(report.inherited[0].rules[0].decls[0].status, Status::Active);
    }

    /// Chrome 158 takes a CSS-wide keyword that `var()` substitution leaves
    /// as that keyword: `var(--missing, inherit)` inherits, also for a
    /// custom property, `revert-layer` rolls back, and `initial` is a value
    /// like any other. The authored value is still what shows.
    #[test]
    fn keywords_count_after_substitution() {
        use Status::{Active, Overridden};
        let color = |v: &str| authored(&[("color", v)], &[("color", v)]);
        let body = |r: &Report| r.inherited[0].rules[0].decls[0].status;
        let mut m = matched();
        m["matchedCSSRules"] = json!([]);
        m["inlineStyle"] = color("var(--missing, inherit)");
        let report = report_for(&m, CssOptions::default());
        assert_eq!(body(&report), Active);
        assert_eq!(report.rules[0].decls[0].value, "var(--missing, inherit)");
        assert_eq!(
            report.rules[0].decls[0].resolved.as_deref(),
            Some("inherit")
        );

        m["inlineStyle"] = color("var(--missing, initial)");
        let report = report_for(&m, CssOptions::default());
        assert_eq!(body(&report), Overridden);

        m["inlineStyle"] = Value::Null;
        m["matchedCSSRules"] = json!([
            layered(".a", 3, "base", color("red")),
            rule("#a", 5, color("var(--missing, revert-layer)")),
        ]);
        let report = report_for(&m, CssOptions::default());
        let firsts: Vec<Status> = report.rules.iter().map(|b| b.decls[0].status).collect();
        assert_eq!(firsts, [Active, Active]);
        assert_eq!(body(&report), Overridden);

        let tone = [("--tone", "var(--missing, inherit)")];
        m["matchedCSSRules"] = json!([rule("#a", 5, authored(&tone, &tone))]);
        let parent = [("--tone", "green")];
        m["inherited"][0]["matchedCSSRules"][0]["rule"]["style"] = authored(&parent, &parent);
        let report = report_for(&m, CssOptions::default());
        assert_eq!(body(&report), Active);
    }

    /// `--computed border` when the sides differ lists what `border` sets,
    /// as the page expanded it, not every computed name that starts with
    /// `border-`; the page is asked to expand it because CDP's computed
    /// list doesn't have it.
    #[test]
    fn computed_shorthand_lists_its_own_longhands() {
        let mut list = computed();
        for (n, v) in [
            ("border-top-width", "3px"),
            ("border-left-width", "1px"),
            ("border-collapse", "collapse"),
            ("border-top-left-radius", "0px"),
        ] {
            list.push((n.into(), v.into()));
        }
        let extra = ["border".to_string(), "z-index".to_string()];
        assert_eq!(shorthands_to_expand(&json!({}), &list, &extra), ["border"]);
        let values = HashMap::from([("border".to_string(), String::new())]);
        let members = HashMap::from([(
            "border".to_string(),
            vec![
                "border-top-width".to_string(),
                "border-left-width".to_string(),
            ],
        )]);
        let picked = pick_computed(&list, &Declared::default(), &values, &members, &extra);
        let names: Vec<&str> = picked.iter().skip(3).map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["border-left-width", "border-top-width", "z-index"]);
    }

    /// An escaped quote doesn't end a fallback string, so the `)` after it
    /// still closes the call.
    #[test]
    fn escaped_quote_in_a_fallback_string() {
        let vars = HashMap::from([("--gap".to_string(), "8px".to_string())]);
        assert_eq!(
            resolve_vars(r#"var(--missing, "a\"b")"#, &vars).as_deref(),
            Some(r#""a\"b""#)
        );
        assert_eq!(
            resolve_vars(r"var(--missing, 'a\'b') var(--gap)", &vars).as_deref(),
            Some(r"'a\'b' 8px")
        );
    }

    /// A browser that answers css-styles' calls about an element with no
    /// styles (`.nope` matches nothing), hangs up when `close_on` arrives,
    /// and never answers `ignore`, though it still answers liveness pings.
    /// It answers `garble` with a frame the client can't read, which ends
    /// the client's reads but not its writes, and goes on reading.
    /// The task returns the methods it was sent.
    async fn fake_browser(
        close_on: &'static str,
        ignore: &'static str,
        garble: &'static str,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use futures_util::{SinkExt, StreamExt};
        use tokio::io::AsyncWriteExt;
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut seen = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let req: Value = serde_json::from_str(&text).unwrap();
                let method = req["method"].as_str().unwrap_or("").to_string();
                seen.push(method.clone());
                if method == close_on {
                    break;
                }
                if method == ignore {
                    continue;
                }
                if method == garble {
                    // A one-byte text frame with a reserved bit set.
                    let _ = ws.get_mut().write_all(&[0xC1, 0x01, b'x']).await;
                    continue;
                }
                let result = match method.as_str() {
                    "DOM.getDocument" => json!({"root": {"nodeId": 1}}),
                    "DOM.querySelector" if req["params"]["selector"] == ".nope" => {
                        json!({"nodeId": 0})
                    }
                    "DOM.querySelector" => json!({"nodeId": 2}),
                    "CSS.getComputedStyleForNode" => json!({"computedStyle": []}),
                    "DOM.resolveNode" => json!({"object": {"objectId": "o"}}),
                    "Runtime.callFunctionOn" => json!({"result": {"value": {"element": "p"}}}),
                    _ => json!({}),
                };
                let reply = json!({"id": req["id"], "result": result});
                if ws.send(Message::Text(reply.to_string())).await.is_err() {
                    break;
                }
            }
            seen
        });
        (url, task)
    }

    /// A stylesheet announced with the matched-style reply still gives the
    /// rule its file and line. Unrelated events in the buffer survive.
    #[tokio::test]
    async fn a_late_stylesheet_header_is_collected() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let browser = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let req: Value = serde_json::from_str(&text).unwrap();
                let result = match req["method"].as_str().unwrap_or("") {
                    "DOM.getDocument" => json!({"root": {"nodeId": 1}}),
                    "DOM.querySelector" => json!({"nodeId": 2}),
                    "CSS.getMatchedStylesForNode" => {
                        ws.send(Message::Text(json!({
                            "method": "CSS.styleSheetAdded",
                            "sessionId": "S",
                            "params": {"header": {"styleSheetId": "late", "sourceURL": "http://host/late.css", "startLine": 0}},
                        }).to_string()))
                        .await
                        .unwrap();
                        json!({"matchedCSSRules": [{
                            "rule": {
                                "origin": "regular",
                                "styleSheetId": "late",
                                "selectorList": {"text": "p", "selectors": [{"text": "p", "range": range(2)}]},
                                "style": {"cssProperties": [{"name": "color", "value": "red", "range": range(2)}]},
                            },
                            "matchingSelectors": [0],
                        }]})
                    }
                    "DOM.resolveNode" => json!({"object": {"objectId": "o"}}),
                    "Runtime.callFunctionOn" => json!({"result": {"value": {"element": "p"}}}),
                    _ => json!({}),
                };
                ws.send(Message::Text(
                    json!({"id": req["id"], "result": result}).to_string(),
                ))
                .await
                .unwrap();
            }
        });
        let mut client = CdpClient::connect(&url, "Chrome").await.unwrap();
        client.events.push_back(json!({
            "method": "Runtime.consoleAPICalled",
            "sessionId": "S",
        }));
        let report = collect(&mut client, "S", "p", &CssOptions::default())
            .await
            .unwrap();
        assert_eq!(report.rules[0].source.as_deref(), Some("late.css:3"));
        assert!(client
            .events
            .iter()
            .any(|e| e["method"] == "Runtime.consoleAPICalled"));
        drop(client);
        browser.await.unwrap();
    }

    /// A connection lost while the page labels the element, or while the
    /// agents are turned off, is the command's error, so the daemon exits
    /// on it instead of reporting success over a dead connection.
    #[tokio::test]
    async fn a_lost_connection_is_not_a_report() {
        for close_on in ["Runtime.callFunctionOn", "DOM.disable"] {
            let (url, browser) = fake_browser(close_on, "", "").await;
            let mut client = CdpClient::connect(&url, "Chrome").await.unwrap();
            let result = css_styles(
                &mut client,
                "S",
                "p",
                CssOptions::default(),
                OutputFormat::Text,
            )
            .await;
            let error = result
                .err()
                .unwrap_or_else(|| panic!("{close_on}: a report"));
            assert!(
                error.downcast_ref::<ConnectionLost>().is_some(),
                "{close_on}: {error:#}"
            );
            drop(client);
            let seen = browser.await.unwrap();
            if close_on == "Runtime.callFunctionOn" {
                assert!(
                    !seen.iter().any(|m| m == "Runtime.releaseObjectGroup"),
                    "{seen:?}"
                );
            } else {
                assert!(seen.iter().any(|m| m == "Runtime.releaseObjectGroup"));
            }
        }
    }

    /// A release write that loses the connection must stop the command; other
    /// release failures do not change the result.
    #[test]
    fn a_lost_release_write_stops_the_command() {
        let lost = release_outcome(Err(anyhow::Error::new(ConnectionLost("closed".into()))));
        assert!(lost.unwrap_err().downcast_ref::<ConnectionLost>().is_some());
        release_outcome(Err(anyhow::anyhow!("ordinary release failure"))).unwrap();
        release_outcome(Ok(1)).unwrap();
    }

    /// A page that doesn't answer gets the cleanup's own short wait, not a
    /// full call timeout, and both agents are still asked to turn off.
    #[tokio::test]
    async fn cleanup_waits_only_as_long_as_it_is_given() {
        let (url, browser) = fake_browser("", "DOM.disable", "").await;
        let mut client = CdpClient::connect(&url, "Chrome").await.unwrap();
        let limit = std::time::Duration::from_millis(300);
        let disabled = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            disable_agents(&mut client, "S", limit),
        )
        .await
        .expect("the cleanup waited past its limit");
        let error = disabled.expect_err("DOM.disable was never answered");
        assert!(
            error.downcast_ref::<ConnectionLost>().is_none(),
            "{error:#}"
        );
        drop(client);
        let seen = browser.await.unwrap();
        assert!(seen.iter().any(|m| m == "CSS.disable"), "{seen:?}");
        assert!(seen.iter().any(|m| m == "DOM.disable"), "{seen:?}");
    }

    /// A dialog left open by the inspection must not cost `DOM.disable` its
    /// write: the guarded send would refuse before putting it on the wire.
    #[tokio::test]
    async fn a_recorded_dialog_still_gets_both_disable_writes() {
        let (url, browser) = fake_browser("", "", "").await;
        let mut client = CdpClient::connect(&url, "Chrome").await.unwrap();
        client.open_dialog = Some(OpenDialog {
            session_id: Some("S".into()),
            dialog: DialogOpen {
                dialog_type: "alert".into(),
                message: "hi".into(),
            },
        });
        disable_agents(&mut client, "S", std::time::Duration::from_millis(300))
            .await
            .expect("cleanup writes both commands even with a dialog open");
        drop(client);
        let seen = browser.await.unwrap();
        assert!(seen.iter().any(|m| m == "CSS.disable"), "{seen:?}");
        assert!(seen.iter().any(|m| m == "DOM.disable"), "{seen:?}");
    }

    /// The agents are turned off after a failed inspection too. A selector
    /// miss remains the reported error when cleanup only fails ordinarily;
    /// a lost connection to the browser takes precedence, because the daemon
    /// has to retire the dead socket.
    #[tokio::test]
    async fn cleanup_keeps_the_inspection_error_unless_the_connection_is_lost() {
        for (close_on, lost) in [("", false), ("DOM.disable", true)] {
            let (url, browser) = fake_browser(close_on, "", "").await;
            let mut client = CdpClient::connect(&url, "Chrome").await.unwrap();
            let result = css_styles(
                &mut client,
                "S",
                ".nope",
                CssOptions::default(),
                OutputFormat::Text,
            )
            .await;
            let error = result.err().expect("no element matches");
            if lost {
                assert!(
                    error.downcast_ref::<ConnectionLost>().is_some(),
                    "{error:#}"
                );
            } else {
                assert!(
                    error.to_string().contains("No element matches"),
                    "{error:#}"
                );
            }
            drop(client);
            let seen = browser.await.unwrap();
            assert_eq!(seen.last().map(String::as_str), Some("DOM.disable"));
        }
    }

    /// Once the inspection has lost the connection, nothing more is sent to
    /// turn the agents off. This browser still reads after the client's
    /// reads failed, so it would see the cleanup's calls.
    #[tokio::test]
    async fn no_cleanup_after_a_lost_connection() {
        let (url, browser) = fake_browser("", "", "Runtime.callFunctionOn").await;
        let mut client = CdpClient::connect(&url, "Chrome").await.unwrap();
        let result = css_styles(
            &mut client,
            "S",
            "p",
            CssOptions::default(),
            OutputFormat::Text,
        )
        .await;
        let error = result.err().expect("a lost connection");
        assert!(
            error.downcast_ref::<ConnectionLost>().is_some(),
            "{error:#}"
        );
        drop(client);
        let seen = browser.await.unwrap();
        assert!(
            seen.iter().any(|m| m == "Runtime.callFunctionOn"),
            "{seen:?}"
        );
        assert!(!seen.iter().any(|m| m.ends_with(".disable")), "{seen:?}");
    }
}
