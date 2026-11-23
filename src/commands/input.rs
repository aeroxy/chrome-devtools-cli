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
    // Mouse events go to whatever is at the point, so an element off screen
    // or without a box would send the click elsewhere (a hidden button's
    // centre is the page's top-left corner) while reporting success.
    //
    // Scrolling doesn't settle that either: a fixed overlay or header can
    // cover the point, and a fixed element off screen stays there. So the
    // point must hit the element. A label counts as its control, since a
    // click on it activates the control (styled checkboxes hide theirs
    // behind it), and each line of a wrapped link is tried, because the
    // centre of its box can fall between the lines.
    let expr = format!(
        r#"(() => {{
            const el = document.querySelector({escaped});
            if (!el) return JSON.stringify({{error: "Element not found: " + {escaped}}});
            let rect = el.getBoundingClientRect();
            if (rect.width === 0 || rect.height === 0) {{
                return JSON.stringify({{error: "Element has no size, so it can't be clicked or hovered (hidden with display:none, collapsed, or empty): " + {escaped}}});
            }}
            if (getComputedStyle(el).visibility === 'hidden') {{
                return JSON.stringify({{error: "Element is hidden with visibility:hidden: " + {escaped}}});
            }}
            const cx = rect.x + rect.width / 2, cy = rect.y + rect.height / 2;
            if (cx < 0 || cy < 0 || cx >= innerWidth || cy >= innerHeight) {{
                el.scrollIntoView({{block: 'center', inline: 'center', behavior: 'instant'}});
                rect = el.getBoundingClientRect();
            }}
            const reaches = hit => hit && (el.contains(hit) || hit.closest('label')?.control === el);
            for (const r of [rect, ...el.getClientRects()]) {{
                const x = r.x + r.width / 2, y = r.y + r.height / 2;
                if (reaches(document.elementFromPoint(x, y))) return JSON.stringify({{x, y}});
            }}
            const x = Math.round(rect.x + rect.width / 2), y = Math.round(rect.y + rect.height / 2);
            const top = document.elementFromPoint(x, y);
            if (!top) {{
                return JSON.stringify({{error: "Element is outside the visible page, and scrolling doesn't bring it in: " + {escaped}}});
            }}
            const name = top.tagName.toLowerCase() + (top.id ? '#' + top.id : '') +
                [...top.classList].slice(0, 2).map(c => '.' + c).join('');
            return JSON.stringify({{error: "Element is under <" + name + "> at (" + x + ", " + y +
                "), which would get the click or hover instead (click-at " + x + " " + y +
                " sends one there anyway): " + {escaped}}});
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
        let desc = crate::remote_object::describe_exception(exception);
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

/// A dialog opened by a step that only gets the action ready (moving the
/// pointer, focusing a field) means the action itself was never sent. The
/// command fails, so a retry is safe, and says what opened the dialog.
fn before_action(error: anyhow::Error, what_happened: &'static str) -> anyhow::Error {
    if error.downcast_ref::<crate::cdp::DialogOpen>().is_some() {
        error.context(what_happened)
    } else {
        error
    }
}

/// Click an element identified by CSS selector.
pub async fn click(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
) -> Result<CommandResult> {
    let initial_url = client.current_url(session_id).await?;
    // Native <option> elements don't always respond to mouse clicks, so an
    // option is selected from script instead. This check changes nothing: a
    // dialog that blocks it was not opened by the click, so it must fail the
    // command rather than count as one. The evaluation that selects repeats
    // it, because the page can disable the option or its select in between.
    let escaped = serde_json::to_string(selector)?;
    let option_check = format!(
        r#"const el = document.querySelector({escaped});
            if (!el) return 'not_found';
            if (el.tagName.toLowerCase() !== 'option') return 'not_option';
            if (el.disabled) return 'option_disabled';
            const select = el.closest('select');
            if (!select) return 'option_no_select';
            if (select.disabled) return 'select_disabled';
            if (select.multiple) return 'select_multiple';"#
    );
    let check_expr = format!("(() => {{ {option_check} return 'option'; }})()");
    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": check_expr, "returnByValue": true}),
        )
        .await?;

    if let Some(exception) = result.get("exceptionDetails") {
        let desc = crate::remote_object::describe_exception(exception);
        bail!("JavaScript error during click handling: {desc}");
    }

    let res_val = result["result"]["value"].as_str().unwrap_or("error");
    match res_val {
        "option" => {
            // This evaluate is the click: a dialog from the change handler
            // means it worked.
            client.input_sent = true;
            let select_expr = format!(
                r#"(() => {{
                    {option_check}
                    select.value = el.value;
                    select.dispatchEvent(new Event('input', {{bubbles: true}}));
                    select.dispatchEvent(new Event('change', {{bubbles: true}}));
                    return 'selected';
                }})()"#
            );
            let result = client
                .send_to_target(
                    session_id,
                    "Runtime.evaluate",
                    json!({"expression": select_expr, "returnByValue": true}),
                )
                .await?;
            if let Some(exception) = result.get("exceptionDetails") {
                let desc = crate::remote_object::describe_exception(exception);
                bail!("JavaScript error while selecting the option: {desc}");
            }
            match result["result"]["value"].as_str().unwrap_or("error") {
                "selected" => {}
                refused => return Err(option_refusal(refused, selector)),
            }
            let new_url = client.current_url(session_id).await?;
            Ok(
                CommandResult::output(format!("Selected option: {selector}"))
                    .with_navigated_to_if_changed(new_url, initial_url),
            )
        }
        "not_option" => {
            let (x, y) = get_element_center(client, session_id, selector).await?;
            click_at(client, session_id, x, y, Some(initial_url)).await
        }
        refused => Err(option_refusal(refused, selector)),
    }
}

/// Why `click` didn't select an `<option>`, from the status its check returned.
fn option_refusal(status: &str, selector: &str) -> anyhow::Error {
    match status {
        "not_found" => anyhow!("Element not found: {selector}"),
        "option_disabled" => anyhow!("Cannot click disabled option: {selector}"),
        "option_no_select" => anyhow!("Option element is not inside a select: {selector}"),
        "select_disabled" => anyhow!("Cannot click option in disabled select: {selector}"),
        "select_multiple" => anyhow!("Manual clicking on <option> is not supported for <select multiple>. Use `evaluate` to update selection instead: {selector}"),
        _ => anyhow!("Unexpected response from page during click handling: {status}"),
    }
}

/// Why `fill` set nothing, from the status its check returned.
fn fill_refusal(status: &str, selector: &str, value: &str) -> anyhow::Error {
    match status {
        "not_found" => anyhow!("Element not found: {selector}"),
        "option_not_found" => anyhow!(
            "Could not find option with text or value '{value}' in select element: {selector}"
        ),
        // A different kind of element by the time it was filled.
        _ => anyhow!("The page changed the element before it could be filled: {selector}"),
    }
}

/// Wait briefly for a navigation that an action may have started, and for it
/// to load. Returns whether a new document committed in the main frame, which
/// is a navigation even when the URL stays the same (a reload, a form posting
/// back to its own page).
///
/// A script that sets `location.href` or submits a form only requests the
/// navigation; it starts after the script's task ends, so the request counts
/// as a start too, or the caller would read the old page's URL and race the
/// load.
/// The id of the page's main frame, so waits can ignore iframe navigations.
pub(crate) async fn main_frame_id(client: &mut CdpClient, session_id: &str) -> Result<String> {
    let frame_tree = client
        .send_to_target(session_id, "Page.getFrameTree", json!({}))
        .await?;
    frame_tree["frameTree"]["frame"]["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!(
                "Failed to determine main frame ID from Page.getFrameTree response: {frame_tree}"
            )
        })
}

pub(crate) async fn wait_for_navigation(client: &mut CdpClient, session_id: &str) -> Result<bool> {
    let main_frame_id = main_frame_id(client, session_id).await?;
    let in_main_frame = |m: &str, p: &serde_json::Value| {
        let main = p.get("frameId").and_then(|f| f.as_str()) == Some(&main_frame_id);
        // A navigation into a new tab or a download leaves this page alone.
        let same_tab = m != "Page.frameRequestedNavigation"
            || p["disposition"].as_str().is_none_or(|d| d == "currentTab");
        main && same_tab
    };

    let started = client
        .wait_for_event_match(
            &[
                "Page.frameStartedLoading",
                "Page.navigatedWithinDocument",
                "Page.frameRequestedNavigation",
            ],
            std::time::Duration::from_millis(500),
            in_main_frame,
        )
        .await;
    let mut method = match started {
        Ok((method, _)) => method,
        Err(_) => return Ok(false),
    };
    if method == "Page.frameRequestedNavigation" {
        match client
            .wait_for_event_match(
                &["Page.frameStartedLoading", "Page.navigatedWithinDocument"],
                std::time::Duration::from_secs(5),
                in_main_frame,
            )
            .await
        {
            Ok((m, _)) => method = m,
            Err(_) => return Ok(false),
        }
    }
    if method != "Page.frameStartedLoading" {
        // Same document: the URL comparison reports it.
        return Ok(false);
    }

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
    // Loading can stop without a new document (a download, a 204 reply).
    Ok(client.events.iter().any(|e| {
        e["method"] == "Page.frameNavigated"
            && e["sessionId"].as_str() == Some(session_id)
            && e["params"]["frame"]["id"].as_str() == Some(&main_frame_id)
    }))
}

/// IDs of the open tabs, to tell afterwards which ones an action opened.
async fn page_ids(client: &mut CdpClient) -> Result<std::collections::HashSet<String>> {
    Ok(client
        .get_page_targets()
        .await?
        .into_iter()
        .map(|t| t.target_id)
        .collect())
}

/// One stderr note per tab that appeared since `before`, so the caller learns
/// its name instead of carrying on in the old tab.
async fn opened_pages(
    client: &mut CdpClient,
    session_id: &str,
    before: &std::collections::HashSet<String>,
) -> Vec<String> {
    let Ok(targets) = client.send("Target.getTargets", json!({})).await else {
        return Vec::new();
    };
    let infos = targets["targetInfos"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let notes = new_tab_notes(infos, before, None);
    if notes.is_empty() {
        return notes;
    }
    // Which tab acted, asked only once there is a new one to place.
    let own = client
        .send_to_target(session_id, "Target.getTargetInfo", json!({}))
        .await
        .ok();
    let own_id = own
        .as_ref()
        .and_then(|info| info["targetInfo"]["targetId"].as_str());
    new_tab_notes(infos, before, own_id)
}

/// Notes for the page targets in `infos` (`Target.getTargets`) missing from
/// `before`. Chrome names the tab that opened one, even through `noopener`:
/// `[opened: …]` is a tab `acting` opened (a `target=_blank` link, a
/// `window.open` button). Any other, such as one a person opened meanwhile,
/// is only `[new tab: …]`, since the action can't be shown to have opened it.
fn new_tab_notes(
    infos: &[serde_json::Value],
    before: &std::collections::HashSet<String>,
    acting: Option<&str>,
) -> Vec<String> {
    infos
        .iter()
        .filter(|t| t["type"] == "page")
        .filter_map(|t| {
            let id = t["targetId"].as_str()?;
            if before.contains(id) {
                return None;
            }
            let label = match t["openerId"].as_str() {
                Some(opener) if Some(opener) == acting => "opened",
                _ => "new tab",
            };
            let url = t["url"].as_str().unwrap_or_default();
            Some(format!(
                "[{label}: {} {url}]",
                crate::friendly::to_friendly(id)
            ))
        })
        .collect()
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
    let pages_before = page_ids(client).await?;
    // Everything Chrome sent before that reply has been read by now, so what
    // gets buffered from here on came after the action began.
    client.clear_events();
    dispatch_mouse(client, session_id, "mouseMoved", x, y, "none", 0)
        .await
        .map_err(|e| {
            before_action(
                e,
                "Moving the pointer there opened a dialog, so the click was not sent",
            )
        })?;
    // The press starts the click: the page may act on it, as on a person's.
    client.input_sent = true;
    dispatch_mouse(client, session_id, "mousePressed", x, y, "left", 1).await?;
    dispatch_mouse(client, session_id, "mouseReleased", x, y, "left", 1).await?;

    let new_document = wait_for_navigation(client, session_id)
        .await
        .unwrap_or(false);

    let new_url = client.current_url(session_id).await?;
    let mut result = CommandResult::output(format!("Clicked at ({x}, {y})")).with_navigation(
        new_url,
        initial_url,
        new_document,
    );
    result.notes = opened_pages(client, session_id, &pages_before).await;
    Ok(result)
}

/// Hover over an element identified by CSS selector.
pub async fn hover(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
) -> Result<CommandResult> {
    let initial_url = client.current_url(session_id).await?;
    let (x, y) = get_element_center(client, session_id, selector).await?;
    // Here moving the pointer is the action.
    client.input_sent = true;
    dispatch_mouse(client, session_id, "mouseMoved", x, y, "none", 0).await?;
    let new_url = client.current_url(session_id).await?;
    Ok(CommandResult::output(format!("Hovered: {selector}"))
        .with_navigated_to_if_changed(new_url, initial_url))
}

/// Fill an input field identified by CSS selector with the given value.
///
/// On a file input, `value` names a local file instead, resolved against
/// `cwd`, the CLI's working directory (`None` if it couldn't be read).
pub async fn fill(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
    value: &str,
    cwd: Option<&str>,
) -> Result<CommandResult> {
    let initial_url = client.current_url(session_id).await?;
    let escaped_sel = serde_json::to_string(selector)?;
    // Escape value for safely injecting into JS
    let escaped_val = serde_json::to_string(value)?;

    // What the element is, and for a select which option matches. This check
    // changes nothing: a dialog that blocks it was not opened by the fill, so
    // it must fail the command rather than count as one. The evaluation that
    // fills repeats it, because the page can change the element in between.
    let classify = format!(
        r#"const el = document.querySelector({escaped_sel});
            const tagName = el ? el.tagName.toLowerCase() : '';
            const type = el && el.type ? el.type.toLowerCase() : '';
            const option = tagName === 'select'
                ? [...el.options].find(o => o.value === {escaped_val} || o.text === {escaped_val})
                : undefined;
            const kind = !el ? 'not_found'
                : tagName === 'select' ? (option ? 'select' : 'option_not_found')
                : tagName === 'input' && (type === 'checkbox' || type === 'radio') ? 'checkbox'
                : tagName === 'textarea' ? 'textarea'
                : type === 'file' ? 'file_input'
                : 'text';"#
    );
    let check_expr = format!("(() => {{ {classify} return kind; }})()");
    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": check_expr, "returnByValue": true}),
        )
        .await?;
    if let Some(exception) = result.get("exceptionDetails") {
        let desc = crate::remote_object::describe_exception(exception);
        bail!("JavaScript error during fill handling: {desc}");
    }

    let kind = result["result"]["value"].as_str().unwrap_or("error");
    match kind {
        "not_found" | "option_not_found" => return Err(fill_refusal(kind, selector, value)),
        "select" | "checkbox" | "textarea" => {
            // This evaluate is the fill: it sets the value and fires input and
            // change, so a dialog from their handlers means it worked.
            client.input_sent = true;
            let expected = serde_json::to_string(kind)?;
            let fill_expr = format!(
                r#"(() => {{
                    {classify}
                    if (kind !== {expected}) return kind;
                    if (kind === 'select') {{
                        el.value = option.value;
                    }} else if (kind === 'checkbox') {{
                        const isTrue = {escaped_val}.toLowerCase() === 'true';
                        if (el.checked === isTrue) return 'filled';
                        el.checked = isTrue;
                    }} else {{
                        el.value = {escaped_val};
                    }}
                    el.dispatchEvent(new Event('input', {{bubbles: true}}));
                    el.dispatchEvent(new Event('change', {{bubbles: true}}));
                    return 'filled';
                }})()"#
            );
            let result = client
                .send_to_target(
                    session_id,
                    "Runtime.evaluate",
                    json!({"expression": fill_expr, "returnByValue": true}),
                )
                .await?;
            if let Some(exception) = result.get("exceptionDetails") {
                let desc = crate::remote_object::describe_exception(exception);
                bail!("JavaScript error during fill handling: {desc}");
            }
            match result["result"]["value"].as_str().unwrap_or("error") {
                "filled" => {}
                refused => return Err(fill_refusal(refused, selector, value)),
            }
            let new_url = client.current_url(session_id).await?;
            return Ok(
                CommandResult::output(format!("Filled '{selector}' with: {value}"))
                    .with_navigated_to_if_changed(new_url, initial_url),
            );
        }
        "file_input" => {
            return set_file(client, session_id, selector, value, cwd, initial_url).await;
        }
        // Contenteditable and other text-like elements, typed into below.
        _ => {}
    }

    // Focusing and clearing the field only get it ready: a dialog a focus or
    // input handler opens means the value was never entered.
    let prepare_expr = format!(
        r#"(() => {{
            const el = document.querySelector({escaped_sel});
            if (!el) return 'not_found';
            el.focus();
            el.value = '';
            el.dispatchEvent(new Event('input', {{bubbles: true}}));
            return 'ready';
        }})()"#
    );
    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": prepare_expr, "returnByValue": true}),
        )
        .await
        .map_err(|e| {
            before_action(
                e,
                "Focusing or clearing the field opened a dialog, so the value was not entered",
            )
        })?;
    if let Some(exception) = result.get("exceptionDetails") {
        let desc = crate::remote_object::describe_exception(exception);
        bail!("JavaScript error during fill handling: {desc}");
    }
    if result["result"]["value"] == "not_found" {
        bail!("Element not found: {selector}");
    }

    client.input_sent = true;
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

/// What a file input holds when read back after its file was set.
enum ReadBack {
    /// A file, as the page sees it.
    File {
        name: String,
        size: u64,
        mime: String,
    },
    /// Nothing: the page emptied the input after Chrome fired `change`.
    Cleared,
    /// Unknown, because the read failed, for example when the page navigated.
    Unread(String),
}

/// Give the file `value` names to the file input `selector` matched.
///
/// Chrome opens the file itself, so there is no size cap and the bytes never
/// pass through the CLI, the daemon or page JavaScript, which can't read the disk.
async fn set_file(
    client: &mut CdpClient,
    session_id: &str,
    selector: &str,
    value: &str,
    cwd: Option<&str>,
    initial_url: String,
) -> Result<CommandResult> {
    let (path, size_on_disk) = upload_file(value, cwd)?;

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
    // again could make an upload widget start the upload twice. Chrome sets
    // the file before firing them, so a dialog their handlers open means the
    // file is set.
    client.input_sent = true;
    let set = client
        .send_to_target(
            session_id,
            "DOM.setFileInputFiles",
            json!({"files": [&path], "objectId": object_id}),
        )
        .await;
    let read_back = match set {
        Ok(_) => Ok(read_back_file(client, session_id, &object_id).await),
        // Kept as it is, so the executor reports the file as set.
        Err(e) if e.downcast_ref::<crate::cdp::DialogOpen>().is_some() => Err(e),
        Err(e) => Err(anyhow!("Can't set the file on '{selector}': {e}")),
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
    let read_back = read_back?;

    // From here nothing may fail the command: the file is set and Chrome has
    // fired `change`, so an error would invite a retry, and a retry would start
    // a second upload. A URL that can't be read reports no navigation.
    let report = CommandResult::output(file_set_report(selector, &read_back, &path, size_on_disk));
    Ok(match client.current_url(session_id).await {
        Ok(new_url) => report.with_navigated_to_if_changed(new_url, initial_url),
        Err(_) => report,
    })
}

/// The file to give a file input, as an absolute path, and its size on disk.
/// A relative `value` resolves against `cwd`, the CLI's working directory,
/// because the daemon keeps its own. The file is checked before Chrome sees it,
/// so a bad path fails with a message that names it.
fn upload_file(value: &str, cwd: Option<&str>) -> Result<(String, u64)> {
    let path = if std::path::Path::new(value).is_absolute() {
        value.to_string()
    } else {
        let cwd = cwd.ok_or_else(|| {
            anyhow!(
                "Can't resolve {value}: the CLI's working directory is unavailable; pass an absolute path"
            )
        })?;
        std::path::Path::new(cwd)
            .join(value)
            .to_string_lossy()
            .into_owned()
    };
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => Ok((path, meta.len())),
        Ok(_) => bail!("Not a file: {path}"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("File not found: {path}"),
        Err(e) => bail!("Can't access {path}: {e}"),
    }
}

/// What the input behind `object_id` holds now. A failed read is reported as
/// [`ReadBack::Unread`], never as an error, because the file is already set.
async fn read_back_file(client: &mut CdpClient, session_id: &str, object_id: &str) -> ReadBack {
    let result = match client
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
    {
        Ok(result) => result,
        Err(e) => return ReadBack::Unread(e.to_string()),
    };
    if let Some(exception) = result.get("exceptionDetails") {
        let desc = crate::remote_object::describe_exception(exception);
        return ReadBack::Unread(desc.to_string());
    }
    let file = &result["result"]["value"];
    match (file["name"].as_str(), file["size"].as_u64()) {
        (Some(name), Some(size)) => ReadBack::File {
            name: name.to_string(),
            size,
            mime: file["type"].as_str().unwrap_or_default().to_string(),
        },
        // `undefined` comes back without a value: the input is empty.
        _ if file.is_null() => ReadBack::Cleared,
        _ => ReadBack::Unread(format!("unexpected value {file}")),
    }
}

/// `fill`'s report on a file input. It describes what the page now holds, read
/// back from `files`, rather than echoing the argument.
fn file_set_report(selector: &str, read_back: &ReadBack, path: &str, size_on_disk: u64) -> String {
    // With no file to describe from the page, name the one given, with its size on disk.
    let given = || {
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path);
        format!(
            "Set file on '{selector}': {name} ({})",
            format_size(size_on_disk)
        )
    };
    let (name, size, mime) = match read_back {
        ReadBack::File { name, size, mime } => (name, *size, mime),
        // Upload widgets often empty the input once they have read the file.
        ReadBack::Cleared => return format!("{}; the page has since cleared the input", given()),
        ReadBack::Unread(reason) => {
            return format!(
                "{}; couldn't read back what the input holds: {reason}",
                given()
            )
        }
    };
    let details = if mime.is_empty() {
        format_size(size)
    } else {
        format!("{}, {mime}", format_size(size))
    };
    let mut report = format!("Set file on '{selector}': {name} ({details})");
    if size == 0 && size_on_disk > 0 {
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
    // Checked before typing: a bad key found afterwards would leave the text
    // typed, and a retry would type it twice.
    let submit = submit_key.map(parse_key_spec).transpose()?;
    let initial_url = client.current_url(session_id).await?;
    client.input_sent = true;
    client
        .send_to_target(session_id, "Input.insertText", json!({"text": text}))
        .await?;

    let mut new_document = false;
    if let Some(spec) = &submit {
        // Only navigations from the key press count.
        client.clear_events();
        dispatch_key(client, session_id, spec).await?;
        new_document = wait_for_navigation(client, session_id)
            .await
            .unwrap_or(false);
    }

    let new_url = client.current_url(session_id).await?;
    Ok(CommandResult::output(format!(
        "Typed: {text}{}",
        submit_key.map(|k| format!(" + {k}")).unwrap_or_default()
    ))
    .with_navigation(new_url, initial_url, new_document))
}

/// Press a key or key combination (e.g. "Enter", "Ctrl+A").
pub async fn press_key(
    client: &mut CdpClient,
    session_id: &str,
    key: &str,
) -> Result<CommandResult> {
    let spec = parse_key_spec(key)?;
    let initial_url = client.current_url(session_id).await?;
    let pages_before = page_ids(client).await?;
    // Everything Chrome sent before that reply has been read by now, so what
    // gets buffered from here on came after the key press.
    client.clear_events();
    dispatch_key(client, session_id, &spec).await?;
    let new_document = wait_for_navigation(client, session_id)
        .await
        .unwrap_or(false);

    let new_url = client.current_url(session_id).await?;
    let mut result = CommandResult::output(format!("Pressed: {key}")).with_navigation(
        new_url,
        initial_url,
        new_document,
    );
    result.notes = opened_pages(client, session_id, &pages_before).await;
    Ok(result)
}

/// A parsed key or key combination, ready for `Input.dispatchKeyEvent`.
#[derive(Debug, PartialEq)]
struct KeySpec {
    key: String,
    code: String,
    key_code: i32,
    modifiers: i32,
    /// What the key types, for printable keys pressed without Ctrl, Alt or
    /// Meta; Chrome inserts nothing for a key event without it.
    text: Option<String>,
}

const ALT: i32 = 1;
const CTRL: i32 = 2;
const META: i32 = 4;
const SHIFT: i32 = 8;

/// Parse `Enter`, `Ctrl+S`, `Shift+Tab`, `a`, `F5`, `Ctrl++`.
///
/// Unknown names are errors rather than events with an empty key code, which
/// the page ignored while the command reported success.
fn parse_key_spec(spec: &str) -> Result<KeySpec> {
    let (mods, main) = if spec == "+" {
        ("", "+")
    } else if let Some(mods) = spec.strip_suffix("++") {
        (mods, "+")
    } else {
        spec.rsplit_once('+').unwrap_or(("", spec))
    };
    if main.is_empty() {
        bail!("No key after the modifiers in '{spec}' (e.g. Ctrl+A)");
    }

    let mut modifiers = 0;
    for part in mods
        .split('+')
        .filter(|p| !mods.is_empty() || !p.is_empty())
    {
        modifiers |= match part.to_lowercase().as_str() {
            "alt" | "option" => ALT,
            "ctrl" | "control" => CTRL,
            "meta" | "cmd" | "command" => META,
            "shift" => SHIFT,
            _ => bail!("Unknown modifier '{part}' in '{spec}'. Use Alt, Ctrl, Meta or Shift."),
        };
    }

    let (key, code, key_code) = match named_key(main) {
        Some(named) => named,
        None => match single_char_key(main, modifiers & SHIFT != 0) {
            Some(k) => k,
            None => bail!(
                "Unknown key '{main}' in '{spec}'. Use a key name (Enter, Tab, Escape, \
                 Backspace, Delete, Space, ArrowUp, Home, PageDown, Insert, F1-F12) or a single \
                 character."
            ),
        },
    };
    let printable = key.chars().count() == 1;
    let text = (printable && modifiers & (CTRL | ALT | META) == 0).then(|| key.clone());
    Ok(KeySpec {
        key,
        code,
        key_code,
        modifiers,
        text,
    })
}

fn named_key(name: &str) -> Option<(String, String, i32)> {
    let (key, code, key_code) = match name.to_lowercase().as_str() {
        "enter" | "return" => ("Enter", "Enter", 13),
        "tab" => ("Tab", "Tab", 9),
        "escape" | "esc" => ("Escape", "Escape", 27),
        "backspace" => ("Backspace", "Backspace", 8),
        "delete" | "del" => ("Delete", "Delete", 46),
        "insert" => ("Insert", "Insert", 45),
        "space" => (" ", "Space", 32),
        "arrowup" | "up" => ("ArrowUp", "ArrowUp", 38),
        "arrowdown" | "down" => ("ArrowDown", "ArrowDown", 40),
        "arrowleft" | "left" => ("ArrowLeft", "ArrowLeft", 37),
        "arrowright" | "right" => ("ArrowRight", "ArrowRight", 39),
        "home" => ("Home", "Home", 36),
        "end" => ("End", "End", 35),
        "pageup" => ("PageUp", "PageUp", 33),
        "pagedown" => ("PageDown", "PageDown", 34),
        lower => {
            let n: i32 = lower.strip_prefix('f')?.parse().ok()?;
            if !(1..=12).contains(&n) {
                return None;
            }
            let f = format!("F{n}");
            return Some((f.clone(), f, 111 + n));
        }
    };
    Some((key.to_string(), code.to_string(), key_code))
}

/// Letters, digits and US-layout punctuation, with the code and Windows key
/// code Chrome expects for each.
fn single_char_key(s: &str, shift: bool) -> Option<(String, String, i32)> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    if c.is_ascii_alphabetic() {
        let upper = c.to_ascii_uppercase();
        let key = if shift { upper } else { c };
        return Some((key.to_string(), format!("Key{upper}"), upper as i32));
    }
    // Shift+1 types "!" on a US layout, and the page reads that as the key.
    const UNSHIFTED: &str = "1234567890-=,./;'[]\\`";
    const SHIFTED: &str = "!@#$%^&*()_+<>?:\"{}|~";
    let c = match UNSHIFTED.find(c) {
        Some(i) if shift => SHIFTED.as_bytes()[i] as char,
        _ => c,
    };
    if c.is_ascii_digit() {
        return Some((c.to_string(), format!("Digit{c}"), c as i32));
    }
    let (code, key_code) = match c {
        ' ' => ("Space", 32),
        '-' | '_' => ("Minus", 189),
        '=' | '+' => ("Equal", 187),
        ',' | '<' => ("Comma", 188),
        '.' | '>' => ("Period", 190),
        '/' | '?' => ("Slash", 191),
        ';' | ':' => ("Semicolon", 186),
        '\'' | '"' => ("Quote", 222),
        '[' | '{' => ("BracketLeft", 219),
        ']' | '}' => ("BracketRight", 221),
        '\\' | '|' => ("Backslash", 220),
        '`' | '~' => ("Backquote", 192),
        ')' => ("Digit0", 48),
        '!' => ("Digit1", 49),
        '@' => ("Digit2", 50),
        '#' => ("Digit3", 51),
        '$' => ("Digit4", 52),
        '%' => ("Digit5", 53),
        '^' => ("Digit6", 54),
        '&' => ("Digit7", 55),
        '*' => ("Digit8", 56),
        '(' => ("Digit9", 57),
        _ => return None,
    };
    Some((c.to_string(), code.to_string(), key_code))
}

async fn dispatch_key(client: &mut CdpClient, session_id: &str, spec: &KeySpec) -> Result<()> {
    client.input_sent = true;
    let mut down = json!({
        "type": "keyDown",
        "key": spec.key,
        "code": spec.code,
        "windowsVirtualKeyCode": spec.key_code,
        "modifiers": spec.modifiers,
    });
    if let Some(text) = &spec.text {
        down["text"] = json!(text);
    }
    client
        .send_to_target(session_id, "Input.dispatchKeyEvent", down)
        .await?;
    client
        .send_to_target(
            session_id,
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": spec.key,
                "code": spec.code,
                "windowsVirtualKeyCode": spec.key_code,
                "modifiers": spec.modifiers,
            }),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_specs_parse_before_anything_is_sent() {
        let enter = parse_key_spec("Enter").unwrap();
        assert_eq!(
            (enter.key.as_str(), enter.code.as_str(), enter.key_code),
            ("Enter", "Enter", 13)
        );
        assert_eq!(enter.text, None);

        let save = parse_key_spec("Ctrl+S").unwrap();
        assert_eq!(
            (save.code.as_str(), save.key_code, save.modifiers),
            ("KeyS", 83, CTRL)
        );
        assert_eq!(save.text, None, "a shortcut types nothing");

        let shift_tab = parse_key_spec("shift+tab").unwrap();
        assert_eq!(
            (shift_tab.key.as_str(), shift_tab.modifiers),
            ("Tab", SHIFT)
        );

        let a = parse_key_spec("a").unwrap();
        assert_eq!(a.text.as_deref(), Some("a"));
        assert_eq!(parse_key_spec("Shift+a").unwrap().key, "A");
        assert_eq!(parse_key_spec("7").unwrap().code, "Digit7");
        assert_eq!(parse_key_spec("F12").unwrap().key_code, 123);
        let plus = parse_key_spec("Ctrl++").unwrap();
        assert_eq!((plus.key.as_str(), plus.modifiers), ("+", CTRL));
        assert_eq!(parse_key_spec("+").unwrap().code, "Equal");
        let bang = parse_key_spec("!").unwrap();
        assert_eq!(
            (bang.code.as_str(), bang.text.as_deref()),
            ("Digit1", Some("!"))
        );
        assert_eq!(parse_key_spec("(").unwrap().key_code, 57);

        let shift_1 = parse_key_spec("Shift+1").unwrap();
        assert_eq!(
            (
                shift_1.key.as_str(),
                shift_1.code.as_str(),
                shift_1.key_code,
                shift_1.text.as_deref(),
                shift_1.modifiers
            ),
            ("!", "Digit1", 49, Some("!"), SHIFT)
        );
        let shift_slash = parse_key_spec("Shift+/").unwrap();
        assert_eq!(
            (
                shift_slash.key.as_str(),
                shift_slash.code.as_str(),
                shift_slash.text.as_deref()
            ),
            ("?", "Slash", Some("?"))
        );
        assert_eq!(parse_key_spec("Shift+!").unwrap().key, "!");
        assert_eq!(parse_key_spec("Ctrl+Shift+=").unwrap().key, "+");
    }

    #[test]
    fn bad_key_specs_are_errors() {
        for (spec, expected) in [
            ("Ener", "Unknown key 'Ener'"),
            ("Foo+Enter", "Unknown modifier 'Foo'"),
            ("Ctrl+", "No key after the modifiers"),
            ("F13", "Unknown key 'F13'"),
            ("é", "Unknown key"),
        ] {
            let err = parse_key_spec(spec).unwrap_err().to_string();
            assert!(err.contains(expected), "{spec}: {err}");
        }
    }

    #[test]
    fn test_upload_file_resolves_a_relative_value_against_the_cli_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), "world").unwrap();
        let cli_dir = dir.path().join("d");
        std::fs::create_dir(&cli_dir).unwrap();
        assert_eq!(
            upload_file("../b.txt", cli_dir.to_str()).unwrap(),
            (cli_dir.join("../b.txt").to_str().unwrap().to_string(), 5)
        );
    }

    #[test]
    fn test_upload_file_takes_an_absolute_value_as_given() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let path = file.to_str().unwrap();
        // Even when the CLI's directory couldn't be read.
        assert_eq!(upload_file(path, None).unwrap(), (path.to_string(), 5));
    }

    #[test]
    fn test_upload_file_names_the_absolute_path_in_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        let cli_dir = dir.path().to_str();
        let missing = dir.path().join("missing.txt");
        let err = upload_file("missing.txt", cli_dir).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("File not found: {}", missing.to_str().unwrap())
        );

        let d = dir.path().join("d");
        let err = upload_file("d", cli_dir).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("Not a file: {}", d.to_str().unwrap())
        );
    }

    #[test]
    fn test_upload_file_without_a_resolved_path_asks_for_an_absolute_one() {
        let err = upload_file("clip.mp4", None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Can't resolve clip.mp4: the CLI's working directory is unavailable; pass an absolute path"
        );
    }

    fn held(name: &str, size: u64, mime: &str) -> ReadBack {
        ReadBack::File {
            name: name.into(),
            size,
            mime: mime.into(),
        }
    }

    #[test]
    fn test_file_set_report_describes_what_the_page_holds() {
        assert_eq!(
            file_set_report("#one", &held("a.txt", 5, "text/plain"), "/x/a.txt", 5),
            "Set file on '#one': a.txt (5 B, text/plain)"
        );
        assert_eq!(
            file_set_report(
                r#"input[type="file"]"#,
                &held("clip.mp4", 5_300_000, "video/mp4"),
                "/x/clip.mp4",
                5_300_000
            ),
            r#"Set file on 'input[type="file"]': clip.mp4 (5.3 MB, video/mp4)"#
        );
        // Chrome gives no type for an extension it doesn't know.
        assert_eq!(
            file_set_report("#one", &held("data.qqq", 5, ""), "/x/data.qqq", 5),
            "Set file on '#one': data.qqq (5 B)"
        );
    }

    #[test]
    fn test_file_set_report_when_the_page_cleared_the_input() {
        assert_eq!(
            file_set_report("#one", &ReadBack::Cleared, "/x/d/../b.txt", 5),
            "Set file on '#one': b.txt (5 B); the page has since cleared the input"
        );
    }

    /// A failed read shows nothing about the page, so it must not claim the
    /// page cleared the input.
    #[test]
    fn test_file_set_report_when_the_input_could_not_be_read_back() {
        let unread = ReadBack::Unread("Cannot find context with specified id".into());
        assert_eq!(
            file_set_report("#one", &unread, "/x/clip.mp4", 5_300_000),
            "Set file on '#one': clip.mp4 (5.3 MB); couldn't read back what the input holds: \
             Cannot find context with specified id"
        );
    }

    #[test]
    fn test_file_set_report_hints_when_chrome_cannot_read_the_file() {
        let report = file_set_report(
            "#one",
            &held("clip.mp4", 0, "video/mp4"),
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
                &held("empty.txt", 0, "text/plain"),
                "/x/empty.txt",
                0
            ),
            "Set file on '#one': empty.txt (0 B, text/plain)"
        );
    }

    /// A tab counts as opened by the action only when this tab opened it;
    /// Chrome names the opener even of a `noopener` link.
    #[test]
    fn new_tabs_are_credited_to_the_action_only_when_this_tab_opened_them() {
        let before: std::collections::HashSet<String> = ["A".to_string(), "B".to_string()].into();
        let infos = [
            json!({"targetId": "A", "type": "page", "url": "https://a.test/"}),
            json!({"targetId": "C", "type": "page", "url": "https://a.test/pop", "openerId": "A"}),
            json!({"targetId": "D", "type": "page", "url": "about:blank"}),
            json!({"targetId": "E", "type": "page", "url": "https://b.test/", "openerId": "B"}),
            json!({"targetId": "F", "type": "service_worker", "url": "https://a.test/sw.js", "openerId": "A"}),
        ];
        let name = crate::friendly::to_friendly;
        assert_eq!(
            new_tab_notes(&infos, &before, Some("A")),
            [
                format!("[opened: {} https://a.test/pop]", name("C")),
                format!("[new tab: {} about:blank]", name("D")),
                format!("[new tab: {} https://b.test/]", name("E")),
            ]
        );
        // Without knowing which tab acted, no tab is credited to the action.
        let unplaced = new_tab_notes(&infos, &before, None);
        assert_eq!(unplaced.len(), 3);
        assert!(
            unplaced.iter().all(|n| n.starts_with("[new tab: ")),
            "{unplaced:?}"
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
