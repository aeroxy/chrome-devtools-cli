//! `cookies`: the cookies Chrome would send to a page, read over the daemon's
//! connection.
//!
//! Page JavaScript can't see HttpOnly cookies, and a second DevTools client
//! would make Chrome ask the user to approve another connection, so this asks
//! the page's own session (`Network.getCookies`). Cookie values are
//! credentials: the table leaves them out, and only `--header`, `--json` and
//! `--toon` print them, to stdout and nowhere else.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::fmt::Write;

use crate::cdp::CdpClient;
use crate::format::{format_structured, OutputFormat};
use crate::result::CommandResult;

pub async fn cookies(
    client: &mut CdpClient,
    session_id: &str,
    urls: &[String],
    names: &[String],
    header: bool,
    format: OutputFormat,
) -> Result<CommandResult> {
    if header && !format.is_text() {
        bail!("--header prints a Cookie header, so it can't be combined with --json or --toon");
    }
    let urls = if urls.is_empty() {
        vec![client.current_url(session_id).await?]
    } else {
        urls.to_vec()
    };
    let result = client
        .send_to_target(session_id, "Network.getCookies", json!({ "urls": urls }))
        .await?;
    let cookies: Vec<&Value> = result["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| names.is_empty() || names.iter().any(|n| c["name"] == n.as_str()))
        .collect();

    if header {
        let out = cookie_header(&cookies);
        if out.is_empty() {
            return Ok(CommandResult::output("")
                .with_note(format!("[note] No cookies for {}", urls.join(", "))));
        }
        return Ok(CommandResult::output(out));
    }
    if !format.is_text() {
        return Ok(CommandResult::output(format_structured(
            &json!(cookies),
            format,
        )?));
    }
    Ok(CommandResult::output(cookie_table(&cookies, &urls)))
}

/// `name=value; name2=value2`, ready to send as a `Cookie` header. Longer
/// paths first, as browsers order them (RFC 6265, 5.4).
fn cookie_header(cookies: &[&Value]) -> String {
    let mut sorted = cookies.to_vec();
    sorted.sort_by_key(|c| std::cmp::Reverse(c["path"].as_str().unwrap_or("").len()));
    sorted
        .iter()
        .map(|c| {
            format!(
                "{}={}",
                c["name"].as_str().unwrap_or(""),
                c["value"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// A table of the cookies with their value lengths but not their values.
fn cookie_table(cookies: &[&Value], urls: &[String]) -> String {
    if cookies.is_empty() {
        return format!("No cookies for {}.", urls.join(", "));
    }
    let rows: Vec<[String; 6]> = cookies
        .iter()
        .map(|c| {
            [
                c["name"].as_str().unwrap_or("").to_string(),
                c["domain"].as_str().unwrap_or("").to_string(),
                c["path"].as_str().unwrap_or("").to_string(),
                expiry(c),
                flags(c),
                c["value"]
                    .as_str()
                    .map_or(0, |v| v.chars().count())
                    .to_string(),
            ]
        })
        .collect();
    let header = ["NAME", "DOMAIN", "PATH", "EXPIRES", "FLAGS", "LENGTH"];
    let widths: Vec<usize> = (0..header.len())
        .map(|i| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .chain([header[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: &[&str]| {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                out.push_str(cell);
            } else {
                let pad = widths[i] - cell.chars().count();
                write!(out, "{cell}{}  ", " ".repeat(pad)).unwrap();
            }
        }
        out
    };
    let mut out = line(&header);
    out.push('\n');
    for row in &rows {
        out.push_str(&line(&row.each_ref().map(String::as_str)));
        out.push('\n');
    }
    out.push_str("Values are hidden; --header or --json prints them.");
    out
}

fn expiry(cookie: &Value) -> String {
    if cookie["session"].as_bool() == Some(true) {
        return "session".to_string();
    }
    cookie["expires"]
        .as_f64()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs as i64, 0))
        .map_or_else(
            || "session".to_string(),
            |t| t.format("%Y-%m-%d %H:%M UTC").to_string(),
        )
}

fn flags(cookie: &Value) -> String {
    let mut flags = Vec::new();
    if cookie["httpOnly"].as_bool() == Some(true) {
        flags.push("HttpOnly".to_string());
    }
    if cookie["secure"].as_bool() == Some(true) {
        flags.push("Secure".to_string());
    }
    if let Some(same_site) = cookie["sameSite"].as_str() {
        flags.push(format!("SameSite={same_site}"));
    }
    if cookie.get("partitionKey").is_some_and(|k| !k.is_null()) {
        flags.push("Partitioned".to_string());
    }
    flags.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, value: &str, path: &str) -> Value {
        json!({
            "name": name, "value": value, "domain": ".a.test", "path": path,
            "expires": 1_798_761_600.0, "session": false,
            "httpOnly": true, "secure": true, "sameSite": "Lax",
        })
    }

    /// The table is for a terminal and transcripts, so no value appears in it.
    #[test]
    fn the_table_shows_lengths_not_values() {
        let session = json!({
            "name": "sid", "value": "s3cr3t-token", "domain": "a.test", "path": "/",
            "expires": -1, "session": true, "httpOnly": true, "secure": true,
            "partitionKey": {"topLevelSite": "https://a.test", "hasCrossSiteAncestor": false},
        });
        let theme = cookie("theme", "dark", "/app");
        let table = cookie_table(&[&session, &theme], &["https://a.test/".into()]);
        assert!(
            !table.contains("s3cr3t") && !table.contains("dark"),
            "{table}"
        );
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(
            lines[0],
            "NAME   DOMAIN   PATH  EXPIRES               FLAGS                         LENGTH"
        );
        assert_eq!(
            lines[1],
            "sid    a.test   /     session               HttpOnly Secure Partitioned   12"
        );
        assert_eq!(
            lines[2],
            "theme  .a.test  /app  2027-01-01 00:00 UTC  HttpOnly Secure SameSite=Lax  4"
        );
        assert_eq!(
            cookie_table(&[], &["https://a.test/".into()]),
            "No cookies for https://a.test/."
        );
    }

    #[test]
    fn the_header_lists_longer_paths_first() {
        let root = cookie("a", "1", "/");
        let deep = cookie("b", "2", "/app/x");
        assert_eq!(cookie_header(&[&root, &deep]), "b=2; a=1");
        assert_eq!(cookie_header(&[]), "");
    }
}
