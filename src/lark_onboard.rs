//! Feishu/Lark bot onboarding, the way `cc-connect` does it: instead of asking
//! somebody to go build an app in the console by hand, walk them through
//! Feishu's own registration flow. The person scans a code with their phone,
//! taps allow, and muxloom gets back an app id and secret for a bot that was
//! created for them — with the messaging permissions onboarding pre-provisions
//! already attached, which is exactly the part hand-building an app so often
//! gets wrong.
//!
//! P2P (one-to-one) support rides on that same permission set: the
//! `PersonalAgent` archetype is meant to cover direct messages as well as
//! groups. Scopes cannot be widened from this code — if a tenant's private
//! messages never arrive, the app's permission set is what to check in the
//! open platform console (the `im` read scopes) and enable by hand there.
//!
//! The endpoint is Feishu's account service, not the open API: `init` asks
//! what this device can do, `begin` starts a device-code session and returns a
//! scan URL, and `poll` waits until whoever scanned it has approved and Feishu
//! hands over the new bot's credentials. Feishu and Lark are two hosts for the
//! same service, and the tenant brand comes back in the poll, so the host is
//! switched when the scan says which one is real.

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::http;

/// The host a scan starts against. Stays Feishu unless the tenant turns out
/// to be Lark.
pub const FEISHU: &str = "https://accounts.feishu.cn";
const LARK: &str = "https://accounts.larksuite.com";

/// How a scan resolves: still waiting, or a finished bot.
#[derive(Debug, Clone)]
pub enum Scan {
    /// Nobody has finished authorizing yet. Ask again after a short wait; the
    /// daemon's own pacing is the interval Feishu suggested.
    Waiting { interval: u64 },
    /// Feishu created the bot and handed over its credentials.
    Connected(Onboarded),
}

/// A bot created through onboarding, ready to speak.
#[derive(Debug, Clone)]
pub struct Onboarded {
    /// Whether the tenant is Feishu (`feishu.cn`) or Lark (`larksuite.com`).
    pub lark: bool,
    pub app_id: String,
    pub app_secret: String,
}

/// The response shape the registration endpoint uses. It carries a flat
/// `error`/`error_description` rather than the open API's `code`/`msg`; both
/// matter to the caller, so both are surfaced where a caller can say which.
fn verify(value: &Value, what: &str) -> Result<()> {
    match value.get("error").and_then(Value::as_str) {
        Some(message) => bail!(
            "Feishu refused {what}: {message}{}",
            value
                .get("error_description")
                .and_then(Value::as_str)
                .map(|detail| format!(" — {detail}"))
                .unwrap_or_default(),
        ),
        None => Ok(()),
    }
}

fn call(
    host: &str,
    action: &str,
    params: &[(&str, &str)],
    environment: &[(String, String)],
) -> Result<Value> {
    let mut fields = vec![("action".to_string(), action.to_string())];
    fields.extend(
        params
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string())),
    );
    http::post_form_free(
        &format!("{host}/oauth/v1/app/registration"),
        &[],
        &fields,
        environment,
    )
    .with_context(|| format!("Feishu registration `{action}` failed"))
}

/// Start onboarding: `init` (what auth this environment can use) then `begin`
/// (a device-code session and the URL to scan). Returns the scan link, the
/// device code to poll with, and the poll interval the session asks for. The
/// host to poll starts as Feishu's.
pub fn begin(environment: &[(String, String)]) -> Result<(String, String, u64)> {
    let init = call(FEISHU, "init", &[], environment)?;
    verify(&init, "the registration request")?;
    let supported = init
        .get("supported_auth_methods")
        .and_then(Value::as_array)
        .map(|methods| {
            methods
                .iter()
                .any(|method| method.as_str() == Some("client_secret"))
        })
        .unwrap_or(true);
    if !supported {
        bail!("this device cannot register a Feishu bot (client_secret auth unavailable)");
    }

    let begun = call(
        FEISHU,
        "begin",
        &[
            ("archetype", "PersonalAgent"),
            ("auth_method", "client_secret"),
            ("request_user_info", "open_id"),
        ],
        environment,
    )?;
    verify(&begun, "the registration request")?;
    let scan = begun
        .get("verification_uri_complete")
        .and_then(Value::as_str)
        .with_context(|| "Feishu began onboarding but gave no scan URL")?
        .to_string();
    if scan.trim().is_empty() {
        bail!("Feishu began onboarding but gave an empty scan URL");
    }
    let device_code = begun
        .get("device_code")
        .and_then(Value::as_str)
        .with_context(|| "Feishu began onboarding but gave no device code")?
        .to_string();
    let interval = begun
        .get("interval")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .max(1);
    Ok((scan, device_code, interval))
}

/// Poll a session started by [`begin`]. `host` starts as Feishu and switches
/// to Lark if the tenant brand says so.
///
/// `device_code` is what associates the poll with the scan; the returned
/// [`Scan`] is `Waiting` until somebody actually approves, at which point it
/// is the created bot's credentials.
pub fn poll(
    host: &str,
    device_code: &str,
    environment: &[(String, String)],
) -> Result<(String, Scan)> {
    let answer = call(host, "poll", &[("device_code", device_code)], environment)?;

    // A tenant that turns out to be Lark answers under the Feishu host too,
    // but the onboarding domain for the rest of the flow should be Lark's.
    let tenant = answer
        .pointer("/user_info/tenant_brand")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let host = if tenant == "lark" {
        if host != LARK {
            // Re-ask on the right host; the flow continues there.
            return poll(LARK, device_code, environment);
        }
        host
    } else {
        host
    };

    if let Some(app_id) = answer.get("client_id").and_then(Value::as_str) {
        let app_secret = answer
            .get("client_secret")
            .and_then(Value::as_str)
            .with_context(|| "Feishu created the bot but gave no secret")?;
        return Ok((
            host.to_string(),
            Scan::Connected(Onboarded {
                lark: tenant == "lark",
                app_id: app_id.to_string(),
                app_secret: app_secret.to_string(),
            }),
        ));
    }

    let error = answer.get("error").and_then(Value::as_str).unwrap_or("");
    match error {
        "" | "authorization_pending" => Ok((
            host.to_string(),
            Scan::Waiting {
                interval: answer
                    .get("interval")
                    .and_then(Value::as_u64)
                    .unwrap_or(5)
                    .max(1),
            },
        )),
        "slow_down" => Ok((
            host.to_string(),
            Scan::Waiting {
                interval: answer
                    .get("interval")
                    .and_then(Value::as_u64)
                    .map(|i| i + 5)
                    .unwrap_or(10),
            },
        )),
        "access_denied" => bail!("the scan was declined"),
        "expired_token" => bail!("the onboarding session expired — scan again"),
        other => bail!(
            "{other}: {}",
            answer
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or("no reason given"),
        ),
    }
}
