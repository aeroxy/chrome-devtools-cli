use anyhow::{anyhow, bail, Result};
use serde_json::json;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

async fn get_element_center(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
) -> Result<(f64, f64)> {
    let escaped = serde_json::to_string(selector)?;
    let expr = format!(
        r#"(() => {{
            const el = document.querySelector({escaped});
            if (!el) return JSON.stringify({{error: "Element not found: " + {escaped}}});
            const rect = el.getBoundingClientRect();
            return JSON.stringify({{x: rect.x + rect.width/2, y: rect.y + rect.height/2}});
        }})()"#
    );

    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": expr, "returnByValue": true}),
        )
        .await?;

    if let Some(exception) = result.get("exceptionDetails") {
        let text = exception["text"].as_str().unwrap_or("Unknown error");
        let desc = exception["exception"]["description"]
            .as_str()
            .unwrap_or(text);
        bail!("JavaScript error evaluating element position: {desc}");
    }

    let val_str = result["result"]["value"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Failed to evaluate element position"))?;

    let val: serde_json::Value = serde_json::from_str(val_str)?;
    if let Some(err) = val.get("error").and_then(|v| v.as_str()) {
        bail!("{err}");
    }

    let x = val["x"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("Missing x coordinate"))?;
    let y = val["y"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("Missing y coordinate"))?;
    Ok((x, y))
}

async fn dispatch_mouse(
    client: &mut CdpClient,
    session_id: &str,
    event_type: &str,
    x: f64,
    y: f64,
    button: &str,
    click_count: u32,
) -> Result<()> {
    client
        .send_to_target(
            session_id,
            "Input.dispatchMouseEvent",
            json!({
                "type": event_type,
                "x": x,
                "y": y,
                "button": button,
                "clickCount": click_count,
            }),
        )
        .await?;
    Ok(())
}

/// Click an element identified by CSS selector.
pub async fn click(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
) -> Result<CommandResult> {
    let initial_url = client.current_url(session_id).await?;
    // Special handling for native <option> elements which don't always respond to mouse clicks
    let escaped = serde_json::to_string(selector)?;
    let check_expr = format!(
        r#"(() => {{
            const el = document.querySelector({escaped});
            if (!el) return 'not_found';
            if (el.tagName.toLowerCase() === 'option') {{
                if (el.disabled) return 'option_disabled';
                const select = el.closest('select');
                if (!select) return 'option_no_select';
                if (select.disabled) return 'select_disabled';
                if (select.multiple) return 'select_multiple';

                select.value = el.value;
                select.dispatchEvent(new Event('input', {{bubbles: true}}));
                select.dispatchEvent(new Event('change', {{bubbles: true}}));
                return 'selected';
            }}
            return 'not_option';
        }})()"#
    );

    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": check_expr, "returnByValue": true}),
        )
        .await?;

    if let Some(exception) = result.get("exceptionDetails") {
        let text = exception["text"].as_str().unwrap_or("Unknown error");
        let desc = exception["exception"]["description"]
            .as_str()
            .unwrap_or(text);
        bail!("JavaScript error during click handling: {desc}");
    }

    let res_val = result["result"]["value"].as_str().unwrap_or("error");
    match res_val {
        "not_found" => bail!("Element not found: {selector}"),
        "selected" => {
            let new_url = client.current_url(session_id).await?;
            Ok(CommandResult::output(format!("Selected option: {selector}"))
                .with_navigated_to_if_changed(new_url, initial_url))
        }
        "option_disabled" => bail!("Cannot click disabled option: {selector}"),
        "option_no_select" => bail!("Option element is not inside a select: {selector}"),
        "select_disabled" => bail!("Cannot click option in disabled select: {selector}"),
        "select_multiple" => bail!("Manual clicking on <option> is not supported for <select multiple>. Use `evaluate` to update selection instead: {selector}"),
        "not_option" => {
            let (x, y) = get_element_center(client, session_id, selector).await?;
            click_at(client, session_id, x, y, Some(initial_url)).await
        }
        _ => bail!("Unexpected response from page during click handling: {res_val}"),
    }
}

async fn wait_for_navigation(client: &mut CdpClient, session_id: &str) -> Result<()> {
    // Get the main frame ID to ensure we only wait for top-level navigations
    let frame_tree = client
        .send_to_target(session_id, "Page.getFrameTree", json!({}))
        .await?;
    let main_frame_id = frame_tree
        .get("frameTree")
        .and_then(|t| t.get("frame"))
        .and_then(|f| f.get("id"))
        .and_then(|i| i.as_str())
        .ok_or_else(|| {
            anyhow!(
                "Failed to determine main frame ID from Page.getFrameTree response: {frame_tree}"
            )
        })?
        .to_string();

    let nav_events = ["Page.frameStartedLoading", "Page.navigatedWithinDocument"];
    if let Ok((method, _)) = client
        .wait_for_event_match(
            &nav_events,
            std::time::Duration::from_millis(500),
            |m, p| {
                if m == "Page.frameStartedLoading" || m == "Page.navigatedWithinDocument" {
                    p.get("frameId").and_then(|f| f.as_str()) == Some(&main_frame_id)
                } else {
                    true
                }
            },
        )
        .await
    {
        if method == "Page.frameStartedLoading" {
            // A full navigation started on the main frame, so wait for it to finish loading
            let _ = client
                .wait_for_event_match(
                    &["Page.loadEventFired", "Page.frameStoppedLoading"],
                    std::time::Duration::from_secs(10),
                    |m, p| {
                        if m == "Page.frameStoppedLoading" {
                            p.get("frameId").and_then(|f| f.as_str()) == Some(&main_frame_id)
                        } else {
                            true // Page.loadEventFired is page-wide
                        }
                    },
                )
                .await;
        }
    }
    Ok(())
}

/// Click at absolute page coordinates (x, y).
pub async fn click_at(
    client: &mut CdpClient,
    session_id: &str,
    x: f64,
    y: f64,
    initial_url: Option<String>,
) -> Result<CommandResult> {
    let initial_url = match initial_url {
        Some(url) => url,
        None => client.current_url(session_id).await?,
    };
    dispatch_mouse(client, session_id, "mouseMoved", x, y, "none", 0).await?;
    dispatch_mouse(client, session_id, "mousePressed", x, y, "left", 1).await?;
    dispatch_mouse(client, session_id, "mouseReleased", x, y, "left", 1).await?;

    let _ = wait_for_navigation(client, session_id).await;

    let new_url = client.current_url(session_id).await?;
    Ok(CommandResult::output(format!("Clicked at ({x}, {y})"))
        .with_navigated_to_if_changed(new_url, initial_url))
}

/// Hover over an element identified by CSS selector.
pub async fn hover(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
) -> Result<CommandResult> {
    let initial_url = client.current_url(session_id).await?;
    let (x, y) = get_element_center(client, session_id, selector).await?;
    dispatch_mouse(client, session_id, "mouseMoved", x, y, "none", 0).await?;
    let new_url = client.current_url(session_id).await?;
    Ok(CommandResult::output(format!("Hovered: {selector}"))
        .with_navigated_to_if_changed(new_url, initial_url))
}

/// Fill an input field identified by CSS selector with the given value.
///
/// On a file input, `value` names a local file instead, and `path` is its
/// absolute form, resolved by the CLI against its own working directory
/// (`None` if that was unavailable).
pub async fn fill(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
    value: &str,
    path: Option<&str>,
) -> Result<CommandResult> {
    let initial_url = client.current_url(session_id).await?;
    let escaped_sel = serde_json::to_string(selector)?;
    // Escape value for safely injecting into JS
    let escaped_val = serde_json::to_string(value)?;

    let expr = format!(
        r#"(() => {{
            const el = document.querySelector({escaped_sel});
            if (!el) return 'not_found';

            const tagName = el.tagName.toLowerCase();
            const type = el.type ? el.type.toLowerCase() : '';

            if (tagName === 'select') {{
                let optionFound = false;
                for (const option of el.options) {{
                    if (option.value === {escaped_val} || option.text === {escaped_val}) {{
                        el.value = option.value;
                        optionFound = true;
                        break;
                    }}
                }}
                if (!optionFound) {{
                    return 'option_not_found';
                }}
                el.dispatchEvent(new Event('input', {{bubbles: true}}));
                el.dispatchEvent(new Event('change', {{bubbles: true}}));
                return 'select_ok';
            }}

if (tagName === 'input' && (type === 'checkbox' || type === 'radio')) {{
                 const isTrue = {escaped_val}.toLowerCase() === 'true';
                 if (el.checked !== isTrue) {{
                     el.checked = isTrue;
                     el.dispatchEvent(new Event('input', {{bubbles: true}}));
                     el.dispatchEvent(new Event('change', {{bubbles: true}}));
                 }}
                 return 'checkbox_ok';
             }}

             if (tagName === 'textarea') {{
                 el.value = {escaped_val};
                 el.dispatchEvent(new Event('input', {{bubbles: true}}));
                 el.dispatchEvent(new Event('change', {{bubbles: true}}));
                 return 'textarea_ok';
             }}

             if (type === 'file') {{
                 return 'file_input';
             }}

             // For contenteditable elements or other text-like elements
             el.focus();
             el.value = '';
             el.dispatchEvent(new Event('input', {{bubbles: true}}));
             return 'ok';
        }})()"#
    );

    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": expr, "returnByValue": true}),
        )
        .await?;

    if let Some(exception) = result.get("exceptionDetails") {
        let text = exception["text"].as_str().unwrap_or("Unknown error");
        let desc = exception["exception"]["description"]
            .as_str()
            .unwrap_or(text);
        bail!("JavaScript error during fill handling: {desc}");
    }

    let res_val = result["result"]["value"].as_str().unwrap_or("error");

    if res_val == "not_found" {
        bail!("Element not found: {selector}");
    } else if res_val == "option_not_found" {
        bail!("Could not find option with text or value '{value}' in select element: {selector}");
    } else if res_val == "file_input" {
        return set_file(client, session_id, selector, value, path, initial_url).await;
    } else if res_val == "select_ok" || res_val == "checkbox_ok" || res_val == "textarea_ok" {
        // For select/checkbox/textarea, the work is done entirely in JS
        let new_url = client.current_url(session_id).await?;
        return Ok(
            CommandResult::output(format!("Filled '{selector}' with: {value}"))
                .with_navigated_to_if_changed(new_url, initial_url),
        );
    }

    client
        .send_to_target(session_id, "Input.insertText", json!({"text": value}))
        .await?;

    let change_expr = format!(
        r#"(() => {{
            const el = document.querySelector({escaped_sel});
            if (el) {{
                el.dispatchEvent(new Event('input', {{bubbles: true}}));
                el.dispatchEvent(new Event('change', {{bubbles: true}}));
            }}
        }})()"#
    );
    client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": change_expr, "returnByValue": true}),
        )
        .await?;

    let new_url = client.current_url(session_id).await?;
    Ok(
        CommandResult::output(format!("Filled '{selector}' with: {value}"))
            .with_navigated_to_if_changed(new_url, initial_url),
    )
}

/// A file as the page sees it in an input's `files`.
struct HeldFile {
    name: String,
    size: u64,
    mime: String,
}

/// Give the file at `path` to the file input `selector` matched.
///
/// Chrome opens the file itself, so there is no size cap and the bytes never
/// pass through the CLI, the daemon or page JavaScript, which can't read the disk.
async fn set_file(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
    value: &str,
    path: Option<&str>,
    initial_url: String,
) -> Result<CommandResult> {
    let (path, size_on_disk) = upload_file(value, path)?;

    let escaped_sel = serde_json::to_string(selector)?;
    let handle = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": format!("document.querySelector({escaped_sel})")}),
        )
        .await?;
    // `fill`'s check just found the element, but a page script may have removed it since.
    let object_id = handle["result"]["objectId"]
        .as_str()
        .ok_or_else(|| anyhow!("Element not found: {selector}"))?
        .to_string();

    // Chrome fires `input` and `change` on the element itself. Dispatching them
    // again could make an upload widget start the upload twice.
    let set = client
        .send_to_target(
            session_id,
            "DOM.setFileInputFiles",
            json!({"files": [&path], "objectId": object_id}),
        )
        .await;
    let held = if set.is_ok() {
        read_back_file(client, session_id, &object_id).await
    } else {
        None
    };
    // The daemon's session outlives this command, so an unreleased handle would
    // keep the element alive until the page navigates.
    let _ = client
        .send_to_target(
            session_id,
            "Runtime.releaseObject",
            json!({"objectId": object_id}),
        )
        .await;
    set.map_err(|e| anyhow!("Can't set the file on '{selector}': {e}"))?;

    let new_url = client.current_url(session_id).await?;
    Ok(CommandResult::output(file_set_report(
        selector,
        held.as_ref(),
        &path,
        size_on_disk,
    ))
    .with_navigated_to_if_changed(new_url, initial_url))
}

/// The file to give a file input, and its size on disk: `path`, the CLI's
/// absolute form of `value`, if it names a regular file. Checked before Chrome
/// sees it, so a bad path fails with a message that names it.
fn upload_file(value: &str, path: Option<&str>) -> Result<(String, u64)> {
    let path = path.ok_or_else(|| {
        anyhow!(
            "Can't resolve {value}: the CLI's working directory is unavailable; pass an absolute path"
        )
    })?;
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok((path.to_string(), meta.len())),
        Ok(_) => bail!("Not a file: {path}"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("File not found: {path}"),
        Err(e) => bail!("Can't access {path}: {e}"),
    }
}

/// The first file the input behind `object_id` holds, as the page sees it.
///
/// Best-effort: the file is set by now and Chrome has fired `change`, so an
/// error here would invite a retry, and a retry would start a second upload.
async fn read_back_file(
    client: &mut CdpClient,
    session_id: &str,
    object_id: &str,
) -> Option<HeldFile> {
    let result = client
        .send_to_target(
            session_id,
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": "function () { const f = this.files[0]; return f && {name: f.name, size: f.size, type: f.type}; }",
                "returnByValue": true,
            }),
        )
        .await
        .ok()?;
    let file = &result["result"]["value"];
    Some(HeldFile {
        name: file["name"].as_str()?.to_string(),
        size: file["size"].as_u64()?,
        mime: file["type"].as_str().unwrap_or_default().to_string(),
    })
}

/// `fill`'s report on a file input. It describes what the page now holds, read
/// back from `files`, rather than echoing the argument.
fn file_set_report(
    selector: &str,
    held: Option<&HeldFile>,
    path: &str,
    size_on_disk: u64,
) -> String {
    let Some(file) = held else {
        // Upload widgets often empty the input once they have read the file.
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path);
        return format!(
            "Set file on '{selector}': {name} ({}); the page has since cleared the input",
            format_size(size_on_disk)
        );
    };
    let details = if file.mime.is_empty() {
        format_size(file.size)
    } else {
        format!("{}, {}", format_size(file.size), file.mime)
    };
    let mut report = format!("Set file on '{selector}': {} ({details})", file.name);
    if file.size == 0 && size_on_disk > 0 {
        report.push_str(&format!(
            "\nChrome reports 0 bytes, but the file holds {}, so Chrome can't read it. On macOS, \
             give Chrome access to that folder in System Settings > Privacy & Security > \
             Files & Folders, or move the file out of Desktop, Documents and Downloads.",
            format_size(size_on_disk)
        ));
    }
    report
}

/// A byte count in decimal units: `5 B`, `5.3 MB`.
fn format_size(bytes: u64) -> String {
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    for unit in ["kB", "MB", "GB"] {
        size /= 1000.0;
        // From 999.95 up, one decimal place would print "1000.0".
        if size < 999.95 {
            return format!("{size:.1} {unit}");
        }
    }
    format!("{:.1} TB", size / 1000.0)
}

/// Insert text at the current focus point, optionally pressing a submit key.
pub async fn type_text(
    client: &mut CdpClient,
    session_id: &str,
    text: &str,
    submit_key: Option<&str>,
) -> Result<CommandResult> {
    let initial_url = client.current_url(session_id).await?;
    client
        .send_to_target(session_id, "Input.insertText", json!({"text": text}))
        .await?;

    if let Some(key) = submit_key {
        press_key(client, session_id, key).await?;
    }

    let new_url = client.current_url(session_id).await?;
    Ok(CommandResult::output(format!(
        "Typed: {text}{}",
        submit_key.map(|k| format!(" + {k}")).unwrap_or_default()
    ))
    .with_navigated_to_if_changed(new_url, initial_url))
}

/// Press a key or key combination (e.g. "Enter", "Ctrl+A").
pub async fn press_key(
    client: &mut CdpClient,
    session_id: &str,
    key: &str,
) -> Result<CommandResult> {
    let initial_url = client.current_url(session_id).await?;
    let parts: Vec<&str> = key.split('+').collect();
    let main_key = parts.last().ok_or_else(|| anyhow::anyhow!("Empty key"))?;

    let mut modifiers: i32 = 0;
    for &part in &parts[..parts.len().saturating_sub(1)] {
        match part.to_lowercase().as_str() {
            "alt" => modifiers |= 1,
            "ctrl" | "control" => modifiers |= 2,
            "meta" | "cmd" | "command" => modifiers |= 4,
            "shift" => modifiers |= 8,
            _ => bail!("Unknown modifier: {part}"),
        }
    }

    let (key_name, code, key_code) = map_key(main_key);

    client
        .send_to_target(
            session_id,
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyDown",
                "key": key_name,
                "code": code,
                "windowsVirtualKeyCode": key_code,
                "modifiers": modifiers,
            }),
        )
        .await?;

    client
        .send_to_target(
            session_id,
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": key_name,
                "code": code,
                "windowsVirtualKeyCode": key_code,
                "modifiers": modifiers,
            }),
        )
        .await?;

    let _ = wait_for_navigation(client, session_id).await;

    let new_url = client.current_url(session_id).await?;
    Ok(CommandResult::output(format!("Pressed: {key}"))
        .with_navigated_to_if_changed(new_url, initial_url))
}

fn map_key(key: &str) -> (&str, &str, i32) {
    match key.to_lowercase().as_str() {
        "enter" | "return" => ("Enter", "Enter", 13),
        "tab" => ("Tab", "Tab", 9),
        "escape" | "esc" => ("Escape", "Escape", 27),
        "backspace" => ("Backspace", "Backspace", 8),
        "delete" => ("Delete", "Delete", 46),
        "space" | " " => (" ", "Space", 32),
        "arrowup" | "up" => ("ArrowUp", "ArrowUp", 38),
        "arrowdown" | "down" => ("ArrowDown", "ArrowDown", 40),
        "arrowleft" | "left" => ("ArrowLeft", "ArrowLeft", 37),
        "arrowright" | "right" => ("ArrowRight", "ArrowRight", 39),
        "home" => ("Home", "Home", 36),
        "end" => ("End", "End", 35),
        "pageup" => ("PageUp", "PageUp", 33),
        "pagedown" => ("PageDown", "PageDown", 34),
        "a" => ("a", "KeyA", 65),
        "b" => ("b", "KeyB", 66),
        "c" => ("c", "KeyC", 67),
        "v" => ("v", "KeyV", 86),
        "x" => ("x", "KeyX", 88),
        "z" => ("z", "KeyZ", 90),
        "f5" => ("F5", "F5", 116),
        _ => (key, key, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upload_file_accepts_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let path = file.to_str().unwrap();
        assert_eq!(
            upload_file("a.txt", Some(path)).unwrap(),
            (path.to_string(), 5)
        );
    }

    #[test]
    fn test_upload_file_names_the_absolute_path_in_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.txt");
        let missing = missing.to_str().unwrap();
        let err = upload_file("missing.txt", Some(missing)).unwrap_err();
        assert_eq!(err.to_string(), format!("File not found: {missing}"));

        let d = dir.path().to_str().unwrap();
        let err = upload_file("d", Some(d)).unwrap_err();
        assert_eq!(err.to_string(), format!("Not a file: {d}"));
    }

    #[test]
    fn test_upload_file_without_a_resolved_path_asks_for_an_absolute_one() {
        let err = upload_file("clip.mp4", None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Can't resolve clip.mp4: the CLI's working directory is unavailable; pass an absolute path"
        );
    }

    fn held(name: &str, size: u64, mime: &str) -> HeldFile {
        HeldFile {
            name: name.into(),
            size,
            mime: mime.into(),
        }
    }

    #[test]
    fn test_file_set_report_describes_what_the_page_holds() {
        assert_eq!(
            file_set_report("#one", Some(&held("a.txt", 5, "text/plain")), "/x/a.txt", 5),
            "Set file on '#one': a.txt (5 B, text/plain)"
        );
        assert_eq!(
            file_set_report(
                r#"input[type="file"]"#,
                Some(&held("clip.mp4", 5_300_000, "video/mp4")),
                "/x/clip.mp4",
                5_300_000
            ),
            r#"Set file on 'input[type="file"]': clip.mp4 (5.3 MB, video/mp4)"#
        );
        // Chrome gives no type for an extension it doesn't know.
        assert_eq!(
            file_set_report("#one", Some(&held("data.qqq", 5, "")), "/x/data.qqq", 5),
            "Set file on '#one': data.qqq (5 B)"
        );
    }

    #[test]
    fn test_file_set_report_when_the_page_cleared_the_input() {
        assert_eq!(
            file_set_report("#one", None, "/x/d/../b.txt", 5),
            "Set file on '#one': b.txt (5 B); the page has since cleared the input"
        );
    }

    #[test]
    fn test_file_set_report_hints_when_chrome_cannot_read_the_file() {
        let report = file_set_report(
            "#one",
            Some(&held("clip.mp4", 0, "video/mp4")),
            "/x/clip.mp4",
            5_300_000,
        );
        assert!(
            report.starts_with(
                "Set file on '#one': clip.mp4 (0 B, video/mp4)\n\
                 Chrome reports 0 bytes, but the file holds 5.3 MB, so Chrome can't read it."
            ),
            "{report}"
        );
        // A file that really is empty gets no hint.
        assert_eq!(
            file_set_report(
                "#one",
                Some(&held("empty.txt", 0, "text/plain")),
                "/x/empty.txt",
                0
            ),
            "Set file on '#one': empty.txt (0 B, text/plain)"
        );
    }

    #[test]
    fn test_format_size_uses_decimal_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1000), "1.0 kB");
        assert_eq!(format_size(5_300_000), "5.3 MB");
        assert_eq!(format_size(999_960), "1.0 MB");
        assert_eq!(format_size(1_500_000_000), "1.5 GB");
    }
}
