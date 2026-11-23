use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::cdp::{AuthenticatorOptions, CdpClient, VirtualAuthenticator};
use crate::format::{format_structured, OutputFormat};
use crate::result::CommandResult;

const PROTOCOLS: &[&str] = &["ctap2", "u2f"];
const TRANSPORTS: &[&str] = &["internal", "usb", "nfc", "ble", "hybrid"];

/// Arguments of `configure-webauthn`. `None` keeps the current value, or the
/// default when there is no authenticator yet.
#[derive(Debug, Default)]
pub struct WebAuthnParams {
    pub protocol: Option<String>,
    pub transport: Option<String>,
    pub has_resident_key: Option<bool>,
    pub has_user_verification: Option<bool>,
    pub is_user_verified: Option<bool>,
    pub remove: bool,
}

impl WebAuthnParams {
    /// Checked before any CDP call, so a bad combination changes nothing.
    pub fn validate(&self) -> Result<()> {
        if self.remove
            && (self.protocol.is_some()
                || self.transport.is_some()
                || self.has_resident_key.is_some()
                || self.has_user_verification.is_some()
                || self.is_user_verified.is_some())
        {
            bail!("--remove cannot be combined with other configure-webauthn flags");
        }
        if let Some(p) = &self.protocol {
            if !PROTOCOLS.contains(&p.as_str()) {
                bail!(
                    "Invalid --protocol '{p}': expected {}",
                    PROTOCOLS.join(", ")
                );
            }
        }
        if let Some(t) = &self.transport {
            if !TRANSPORTS.contains(&t.as_str()) {
                bail!(
                    "Invalid --transport '{t}': expected {}",
                    TRANSPORTS.join(", ")
                );
            }
        }
        if self.protocol.as_deref() == Some("u2f")
            && (self.has_resident_key == Some(true) || self.has_user_verification == Some(true))
        {
            bail!(
                "protocol u2f supports neither resident keys nor user verification: \
                 omit --has-resident-key and --has-user-verification, or set them to false"
            );
        }
        Ok(())
    }
}

/// The options the authenticator should have after this call: the given flags
/// over the current authenticator's options over the defaults.
fn resolve_options(
    params: &WebAuthnParams,
    current: Option<&AuthenticatorOptions>,
) -> AuthenticatorOptions {
    let protocol = params
        .protocol
        .clone()
        .or_else(|| current.map(|c| c.protocol.clone()))
        .unwrap_or_else(|| "ctap2".to_string());
    let transport = params
        .transport
        .clone()
        .or_else(|| current.map(|c| c.transport.clone()))
        .unwrap_or_else(|| "internal".to_string());
    let is_user_verified = params
        .is_user_verified
        .or(current.map(|c| c.is_user_verified))
        .unwrap_or(true);
    // u2f has neither, also when switching from a ctap2 authenticator that had them.
    let u2f = protocol == "u2f";
    let has_resident_key = !u2f
        && params
            .has_resident_key
            .or(current.map(|c| c.has_resident_key))
            .unwrap_or(true);
    let has_user_verification = !u2f
        && params
            .has_user_verification
            .or(current.map(|c| c.has_user_verification))
            .unwrap_or(true);
    AuthenticatorOptions {
        protocol,
        transport,
        has_resident_key,
        has_user_verification,
        is_user_verified,
    }
}

/// What it takes to get from the current authenticator to `next`. The
/// variants that touch an existing authenticator carry its id.
#[derive(Debug, PartialEq)]
enum Plan {
    Add,
    /// CDP can't change protocol, transport, or key/verification support on
    /// an existing authenticator, so it is removed and added again.
    Replace(String),
    /// Only `isUserVerified` changes, which CDP can update in place, so
    /// credentials the page registered stay.
    SetUserVerified(String),
    Keep(String),
}

fn plan(current: Option<&VirtualAuthenticator>, next: &AuthenticatorOptions) -> Plan {
    let Some(VirtualAuthenticator { id, options: cur }) = current else {
        return Plan::Add;
    };
    let same_creation = cur.protocol == next.protocol
        && cur.transport == next.transport
        && cur.has_resident_key == next.has_resident_key
        && cur.has_user_verification == next.has_user_verification;
    if !same_creation {
        Plan::Replace(id.clone())
    } else if cur.is_user_verified != next.is_user_verified {
        Plan::SetUserVerified(id.clone())
    } else {
        Plan::Keep(id.clone())
    }
}

/// CDP returns credential IDs as padded base64; the page sees
/// `PublicKeyCredential.id` as unpadded base64url, so print that one.
fn to_base64url(b64: &str) -> String {
    b64.trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// The authenticator and its credentials as shown to the caller. Private keys
/// are left out.
fn authenticator_json(options: &AuthenticatorOptions, credentials: &[Value]) -> Value {
    let credentials: Vec<Value> = credentials
        .iter()
        .map(|c| {
            let mut out = json!({
                "credentialId": to_base64url(c["credentialId"].as_str().unwrap_or("")),
                "rpId": c["rpId"].as_str().unwrap_or("unknown"),
                "discoverable": c["isResidentCredential"].as_bool().unwrap_or(false),
                "signCount": c["signCount"].as_u64().unwrap_or(0),
            });
            if let Some(name) = c["userName"].as_str().filter(|n| !n.is_empty()) {
                out["userName"] = json!(name);
            }
            out
        })
        .collect();
    json!({
        "protocol": options.protocol,
        "transport": options.transport,
        "hasResidentKey": options.has_resident_key,
        "hasUserVerification": options.has_user_verification,
        "isUserVerified": options.is_user_verified,
        "credentials": credentials,
    })
}

fn format_text(action: &str, authenticator: &Value) -> String {
    let mut lines = Vec::new();
    match action {
        "added" => lines.push("Added a virtual authenticator.".to_string()),
        "replaced" => lines
            .push("Replaced the virtual authenticator; its credentials were deleted.".to_string()),
        "updated" => lines.push("Updated isUserVerified in place.".to_string()),
        _ => {}
    }
    let a = authenticator;
    lines.push(format!(
        "Virtual authenticator: protocol={}, transport={}, hasResidentKey={}, hasUserVerification={}, isUserVerified={}",
        a["protocol"].as_str().unwrap_or(""),
        a["transport"].as_str().unwrap_or(""),
        a["hasResidentKey"],
        a["hasUserVerification"],
        a["isUserVerified"],
    ));
    let credentials = a["credentials"].as_array().cloned().unwrap_or_default();
    if credentials.is_empty() {
        lines.push("No credentials.".to_string());
    } else {
        lines.push(format!("Credentials ({}):", credentials.len()));
        for c in &credentials {
            let mut fields = vec![
                format!("credentialId={}", c["credentialId"].as_str().unwrap_or("")),
                format!("rpId={}", c["rpId"].as_str().unwrap_or("")),
                format!("discoverable={}", c["discoverable"]),
                format!("signCount={}", c["signCount"]),
            ];
            if let Some(name) = c["userName"].as_str() {
                fields.push(format!("userName={name}"));
            }
            lines.push(format!("- {}", fields.join(", ")));
        }
    }
    lines.join("\n")
}

/// Chrome answers WebAuthn calls only from the front tab, and turns the rest
/// down with a NotAllowedError that doesn't say why in every case.
const BACKGROUND_TAB_NOTE: &str = "Note: this tab is in the background, where Chrome rejects \
     navigator.credentials (NotAllowedError). Bring it to the front with select-page first.";

fn output(
    format: OutputFormat,
    action: &str,
    authenticator: Value,
    background_tab: bool,
) -> Result<CommandResult> {
    if format.is_text() {
        let mut text = format_text(action, &authenticator);
        if background_tab {
            text.push('\n');
            text.push_str(BACKGROUND_TAB_NOTE);
        }
        return Ok(CommandResult::output(text));
    }
    let mut value = json!({"action": action, "authenticator": authenticator});
    if background_tab {
        value["warning"] = json!(BACKGROUND_TAB_NOTE);
    }
    Ok(CommandResult::output(format_structured(&value, format)?))
}

/// Option pairs Chrome refuses to add.
fn check_supported(options: &AuthenticatorOptions) -> Result<()> {
    if options.protocol == "u2f" && options.transport == "hybrid" {
        bail!(
            "protocol u2f supports only the usb, nfc and ble transports, not hybrid: add \
             --transport usb (or nfc, ble)"
        );
    }
    Ok(())
}

/// Whether Chrome's error says the authenticator no longer exists, rather than
/// that this one call failed. These are Chrome's own messages, not page text.
fn authenticator_gone(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    text.contains("Could not find a Virtual Authenticator")
        || text.contains("Virtual Authenticator Environment has not been enabled")
}

/// Whether the tab is hidden behind another one. Unknown counts as no, since
/// the note is advice and the authenticator works either way.
async fn is_background_tab(client: &mut CdpClient, session_id: &str) -> bool {
    client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": "document.visibilityState", "returnByValue": true}),
        )
        .await
        .is_ok_and(|v| v["result"]["value"].as_str() == Some("hidden"))
}

/// Add, change, show, or remove the tab's virtual WebAuthn authenticator.
///
/// Chrome removes a virtual authenticator when the CDP session that added it
/// detaches, so it is added on the daemon's persistent session for the tab,
/// which stays attached between commands. The tab's emulation state records it
/// so that it is re-created when the CLI comes back from another tab.
pub async fn configure_webauthn(
    client: &mut CdpClient,
    session_id: &str,
    params: WebAuthnParams,
    format: OutputFormat,
) -> Result<CommandResult> {
    params.validate()?;

    if params.remove {
        // Disabling the environment removes every authenticator on the tab,
        // including ones another client such as the DevTools panel added.
        let disabled = client
            .send_to_target(session_id, "WebAuthn.disable", json!({}))
            .await;
        let had_one = client.webauthn.take().is_some();
        if let Err(e) = disabled {
            if had_one {
                return Err(e.context("Failed to remove the virtual authenticator"));
            }
        }
        return if format.is_text() {
            Ok(CommandResult::output(if had_one {
                "Removed the virtual authenticator."
            } else {
                "No virtual authenticator on this page."
            }))
        } else {
            let value = json!({"action": "removed", "authenticator": null});
            Ok(CommandResult::output(format_structured(&value, format)?))
        };
    }

    let current = client.webauthn.clone();
    let next = resolve_options(&params, current.as_ref().map(|c| &c.options));
    // Checked on the merged options, before anything is removed: a flag can
    // combine with the current authenticator into a pair Chrome refuses, and
    // replacing would already have deleted the old one and its credentials.
    check_supported(&next)?;
    let plan = plan(current.as_ref(), &next);
    // Adding or replacing can leave the tab with no authenticator when it
    // fails; anything else leaves the current one as it was.
    let creates = matches!(plan, Plan::Add | Plan::Replace(_));

    if matches!(plan, Plan::Add | Plan::Replace(_))
        && client.persistent_session.as_deref() != Some(session_id)
    {
        bail!(
            "configure-webauthn needs the CLI's background daemon: Chrome removes a virtual \
             authenticator as soon as the CDP session that added it detaches, and this command \
             is running on a one-off session (the daemon is unavailable or could not attach to \
             this tab), so the authenticator would be gone before the next command."
        );
    }

    let applied: Result<(String, &str)> = async {
        match plan {
            Plan::Add => Ok((
                client.add_virtual_authenticator(session_id, &next).await?,
                "added",
            )),
            Plan::Replace(old) => {
                // Best effort: if it is already gone, adding the new one is all
                // that is left to do.
                let _ = client
                    .send_to_target(
                        session_id,
                        "WebAuthn.removeVirtualAuthenticator",
                        json!({"authenticatorId": old}),
                    )
                    .await;
                Ok((
                    client.add_virtual_authenticator(session_id, &next).await?,
                    "replaced",
                ))
            }
            Plan::SetUserVerified(id) => {
                client
                    .send_to_target(
                        session_id,
                        "WebAuthn.setUserVerified",
                        json!({"authenticatorId": id, "isUserVerified": next.is_user_verified}),
                    )
                    .await?;
                Ok((id, "updated"))
            }
            Plan::Keep(id) => Ok((id, "unchanged")),
        }
    }
    .await;

    let (id, action) = match applied {
        Ok(v) => v,
        Err(e) => {
            // Start from scratch next time rather than reuse an id that may be
            // stale. An interrupted update (a dialog, say) keeps the
            // authenticator and its credentials, so a retry updates it again.
            if creates || authenticator_gone(&e) {
                client.webauthn = None;
            }
            return Err(e.context("Failed to configure the virtual authenticator"));
        }
    };
    client.webauthn = Some(VirtualAuthenticator {
        id: id.clone(),
        options: next.clone(),
    });

    let credentials = match client
        .send_to_target(
            session_id,
            "WebAuthn.getCredentials",
            json!({"authenticatorId": id}),
        )
        .await
    {
        Ok(v) => v["credentials"].as_array().cloned().unwrap_or_default(),
        Err(e) if authenticator_gone(&e) => {
            // The tab crashed, or another client disabled the environment.
            client.webauthn = None;
            return Err(e).context(
                "The virtual authenticator is gone; run configure-webauthn again to add one",
            );
        }
        // Anything else (a dialog, a slow page) leaves the authenticator and
        // its credentials in place; listing them can simply be retried.
        Err(e) => return Err(e).context("Failed to read the virtual authenticator's credentials"),
    };

    let background_tab = is_background_tab(client, session_id).await;
    output(
        format,
        action,
        authenticator_json(&next, &credentials),
        background_tab,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctap2() -> AuthenticatorOptions {
        AuthenticatorOptions {
            protocol: "ctap2".into(),
            transport: "internal".into(),
            has_resident_key: true,
            has_user_verification: true,
            is_user_verified: true,
        }
    }

    #[test]
    fn defaults_are_a_platform_passkey_authenticator() {
        assert_eq!(resolve_options(&WebAuthnParams::default(), None), ctap2());
    }

    #[test]
    fn omitted_flags_keep_the_current_values() {
        let current = AuthenticatorOptions {
            transport: "usb".into(),
            has_resident_key: false,
            ..ctap2()
        };
        let params = WebAuthnParams {
            is_user_verified: Some(false),
            ..Default::default()
        };
        let next = resolve_options(&params, Some(&current));
        assert_eq!(next.transport, "usb");
        assert!(!next.has_resident_key);
        assert!(!next.is_user_verified);
        assert_eq!(
            plan(Some(&added(current)), &next),
            Plan::SetUserVerified("a1".into())
        );
    }

    /// A switch to u2f turns off what u2f can't do, even though the ctap2
    /// authenticator had both and the flags were omitted.
    #[test]
    fn u2f_has_no_resident_key_or_user_verification() {
        let params = WebAuthnParams {
            protocol: Some("u2f".into()),
            ..Default::default()
        };
        let next = resolve_options(&params, Some(&ctap2()));
        assert!(!next.has_resident_key);
        assert!(!next.has_user_verification);
        assert_eq!(
            plan(Some(&added(ctap2())), &next),
            Plan::Replace("a1".into())
        );
    }

    fn added(options: AuthenticatorOptions) -> VirtualAuthenticator {
        VirtualAuthenticator {
            id: "a1".into(),
            options,
        }
    }

    #[test]
    fn plan_follows_what_changed() {
        assert_eq!(plan(None, &ctap2()), Plan::Add);
        assert_eq!(
            plan(Some(&added(ctap2())), &ctap2()),
            Plan::Keep("a1".into())
        );
        let usb = AuthenticatorOptions {
            transport: "usb".into(),
            ..ctap2()
        };
        assert_eq!(
            plan(Some(&added(ctap2())), &usb),
            Plan::Replace("a1".into())
        );
    }

    /// `--protocol u2f` on a ctap2/hybrid authenticator inherits hybrid,
    /// which Chrome refuses; that must be caught before the old one is removed.
    #[test]
    fn merged_options_chrome_refuses_are_caught() {
        let current = resolve_options(
            &WebAuthnParams {
                transport: Some("hybrid".into()),
                ..Default::default()
            },
            None,
        );
        assert!(check_supported(&current).is_ok());
        let switched = resolve_options(
            &WebAuthnParams {
                protocol: Some("u2f".into()),
                ..Default::default()
            },
            Some(&current),
        );
        assert!(check_supported(&switched).is_err());
        let to_usb = resolve_options(
            &WebAuthnParams {
                protocol: Some("u2f".into()),
                transport: Some("usb".into()),
                ..Default::default()
            },
            Some(&current),
        );
        assert!(check_supported(&to_usb).is_ok());

        let gone = anyhow::anyhow!("CDP error in WebAuthn.getCredentials: Could not find a Virtual Authenticator matching the ID");
        assert!(authenticator_gone(&gone));
        assert!(!authenticator_gone(&anyhow::anyhow!(
            "A javascript dialog is open"
        )));
    }

    #[test]
    fn validate_rejects_bad_values_and_combinations() {
        let bad_protocol = WebAuthnParams {
            protocol: Some("fido3".into()),
            ..Default::default()
        };
        assert!(bad_protocol.validate().is_err());
        let bad_transport = WebAuthnParams {
            transport: Some("wifi".into()),
            ..Default::default()
        };
        assert!(bad_transport.validate().is_err());
        let u2f_rk = WebAuthnParams {
            protocol: Some("u2f".into()),
            has_resident_key: Some(true),
            ..Default::default()
        };
        assert!(u2f_rk.validate().is_err());
        let u2f_off = WebAuthnParams {
            protocol: Some("u2f".into()),
            has_resident_key: Some(false),
            ..Default::default()
        };
        assert!(u2f_off.validate().is_ok());
        let remove_and_set = WebAuthnParams {
            remove: true,
            transport: Some("usb".into()),
            ..Default::default()
        };
        assert!(remove_and_set.validate().is_err());
    }

    #[test]
    fn credential_ids_print_as_the_page_sees_them() {
        assert_eq!(
            to_base64url("h/SrC/3MkGSiRXurJMGR3pGRoJ30OUIKjfgDSWgSW5U="),
            "h_SrC_3MkGSiRXurJMGR3pGRoJ30OUIKjfgDSWgSW5U"
        );
        assert_eq!(to_base64url("a+b/"), "a-b_");
    }

    #[test]
    fn text_output_lists_credentials_without_private_keys() {
        let creds = vec![json!({
            "credentialId": "AAEC+/8=",
            "isResidentCredential": true,
            "rpId": "localhost",
            "privateKey": "SECRET",
            "signCount": 2,
            "userName": "alice",
        })];
        let a = authenticator_json(&ctap2(), &creds);
        assert!(!a.to_string().contains("SECRET"));
        assert_eq!(
            format_text("unchanged", &a),
            "Virtual authenticator: protocol=ctap2, transport=internal, hasResidentKey=true, \
             hasUserVerification=true, isUserVerified=true\n\
             Credentials (1):\n\
             - credentialId=AAEC-_8, rpId=localhost, discoverable=true, signCount=2, userName=alice"
        );
        let empty = authenticator_json(&ctap2(), &[]);
        assert!(format_text("added", &empty)
            .starts_with("Added a virtual authenticator.\nVirtual authenticator:"));
        assert!(format_text("added", &empty).ends_with("No credentials."));
    }

    #[test]
    fn background_tab_gets_a_note() {
        let a = authenticator_json(&ctap2(), &[]);
        let text = output(OutputFormat::Text, "added", a.clone(), true)
            .unwrap()
            .output;
        assert!(text.ends_with(BACKGROUND_TAB_NOTE));
        let front = output(OutputFormat::Text, "added", a.clone(), false)
            .unwrap()
            .output;
        assert!(!front.contains("Note:"));
        let json = output(OutputFormat::Json, "added", a, true).unwrap().output;
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["warning"], BACKGROUND_TAB_NOTE);
        assert_eq!(v["action"], "added");
    }
}
