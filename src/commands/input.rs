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
        .ok_or_else(|| anyhow!("Failed to determine main frame ID from Page.getFrameTree response: {frame_tree}"))?
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

pub async fn fill(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
    value: &str,
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
         bail!("Cannot fill file input with text: {selector}");
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
