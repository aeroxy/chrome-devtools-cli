use anyhow::{bail, Result};
use serde_json::json;

use crate::cdp::CdpClient;

async fn get_element_center(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
) -> Result<(f64, f64)> {
    let escaped = selector.replace('\\', "\\\\").replace('\'', "\\'");
    let expr = format!(
        r#"(() => {{
            const el = document.querySelector('{escaped}');
            if (!el) return JSON.stringify({{error: 'Element not found: {escaped}'}});
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

pub async fn click(client: &mut CdpClient, session_id: &str, selector: &str) -> Result<String> {
    let (x, y) = get_element_center(client, session_id, selector).await?;
    click_at(client, session_id, x, y).await?;
    Ok(format!("Clicked: {selector}"))
}

pub async fn click_at(client: &mut CdpClient, session_id: &str, x: f64, y: f64) -> Result<String> {
    dispatch_mouse(client, session_id, "mouseMoved", x, y, "none", 0).await?;
    dispatch_mouse(client, session_id, "mousePressed", x, y, "left", 1).await?;
    dispatch_mouse(client, session_id, "mouseReleased", x, y, "left", 1).await?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    Ok(format!("Clicked at ({x}, {y})"))
}

pub async fn hover(client: &mut CdpClient, session_id: &str, selector: &str) -> Result<String> {
    let (x, y) = get_element_center(client, session_id, selector).await?;
    dispatch_mouse(client, session_id, "mouseMoved", x, y, "none", 0).await?;
    Ok(format!("Hovered: {selector}"))
}

pub async fn fill(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
    value: &str,
) -> Result<String> {
    let escaped_sel = selector.replace('\\', "\\\\").replace('\'', "\\'");
    // Escape value for safely injecting into JS
    let escaped_val = value.replace('\\', "\\\\").replace('\'', "\\'");

    let expr = format!(
        r#"(() => {{
            const el = document.querySelector('{escaped_sel}');
            if (!el) return 'not_found';
            
            const tagName = el.tagName.toLowerCase();
            const type = el.type ? el.type.toLowerCase() : '';
            
            if (tagName === 'select') {{
                let optionFound = false;
                for (const option of el.options) {{
                    if (option.value === '{escaped_val}' || option.text === '{escaped_val}') {{
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
                const isTrue = '{escaped_val}'.toLowerCase() === 'true';
                if (el.checked !== isTrue) {{
                    el.checked = isTrue;
                    el.dispatchEvent(new Event('input', {{bubbles: true}}));
                    el.dispatchEvent(new Event('change', {{bubbles: true}}));
                }}
                return 'checkbox_ok';
            }}
            
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

    let res_val = result["result"]["value"].as_str().unwrap_or("error");

    if res_val == "not_found" {
        bail!("Element not found: {selector}");
    } else if res_val == "option_not_found" {
        bail!("Could not find option with text or value '{value}' in select element: {selector}");
    } else if res_val == "select_ok" || res_val == "checkbox_ok" {
        // For select/checkbox, the work is done entirely in JS
        return Ok(format!("Filled '{selector}' with: {value}"));
    }

    client
        .send_to_target(session_id, "Input.insertText", json!({"text": value}))
        .await?;

    let change_expr = format!(
        r#"(() => {{
            const el = document.querySelector('{escaped_sel}');
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

    Ok(format!("Filled '{selector}' with: {value}"))
}

pub async fn type_text(
    client: &mut CdpClient,
    session_id: &str,
    text: &str,
    submit_key: Option<&str>,
) -> Result<String> {
    client
        .send_to_target(session_id, "Input.insertText", json!({"text": text}))
        .await?;

    if let Some(key) = submit_key {
        press_key(client, session_id, key).await?;
    }

    Ok(format!(
        "Typed: {text}{}",
        submit_key.map(|k| format!(" + {k}")).unwrap_or_default()
    ))
}

pub async fn press_key(client: &mut CdpClient, session_id: &str, key: &str) -> Result<String> {
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

    Ok(format!("Pressed: {key}"))
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
