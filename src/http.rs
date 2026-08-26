//! Minimal in-process HTTP(S) client built on rustls (via `minreq`).
//!
//! muxloom ships this instead of shelling out to `curl`, so a stock machine
//! with no `curl` installed can still fetch companion binaries, agent packages,
//! and self-updates — and so any machine can reach a chat API on its own.
//!
//! Every build gets the JSON pair, because the companion sends channel messages
//! itself rather than borrowing the controller's network:
//!
//! - [`get_json`] and [`post_json`] for a chat API request and its answer.
//!
//! The download shapes are the controller's, since unpacking what they fetch
//! needs `flate2`/`tar`/`zstd`, which only the controller build carries:
//!
//! - [`fetch_text`] for a short body (e.g. a `.sha256` sidecar),
//! - [`redirect_location`] to read a redirect target without downloading it
//!   (used to discover the latest release tag without the rate-limited GitHub
//!   API),
//! - [`effective_url`] to resolve a redirect chain without buffering the body,
//!   and
//! - [`download`] to stream a file to disk with byte-level progress.
//!
//! Every request honors the standard `*_PROXY` and `NO_PROXY` variables. Values
//! explicitly carried in `environment` override the calling process's own
//! environment, matching the behavior of the former curl subprocess.

#[cfg(feature = "controller")]
use std::{
    fs::File,
    io::{BufWriter, Read, Write},
    path::Path,
};
use std::{thread, time::Duration};

use anyhow::{Context, Result, anyhow, bail};

/// GitHub requires a User-Agent; this also identifies us to any proxy.
const USER_AGENT: &str = concat!("muxloom/", env!("CARGO_PKG_VERSION"));
/// Total request deadline for short metadata requests.
const REQUEST_TIMEOUT_SECS: u64 = 60;
/// Package downloads can be hundreds of MiB. minreq applies its timeout to the
/// whole response rather than each socket read, so use a larger bounded window
/// for streamed assets.
#[cfg(feature = "controller")]
const DOWNLOAD_TIMEOUT_SECS: u64 = 30 * 60;
/// Match the retry budget used by the former `curl --retry 3` paths.
const ATTEMPTS: usize = 3;
/// Download copy buffer size.
#[cfg(feature = "controller")]
const CHUNK: usize = 64 * 1024;

#[cfg(feature = "controller")]
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn environment_value(environment: &[(String, String)], keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| {
            environment
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        })
        .or_else(|| keys.iter().find_map(|key| std::env::var(key).ok()))
}

fn url_host_and_port(url: &str) -> Option<(String, u16)> {
    let (scheme, remainder) = url.split_once("://")?;
    let authority = remainder.split('/').next()?.rsplit('@').next()?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    if let Some(authority) = authority.strip_prefix('[') {
        let end = authority.find(']')?;
        let host = authority[..end].to_ascii_lowercase();
        let port = authority[end + 1..]
            .strip_prefix(':')
            .and_then(|value| value.parse().ok())
            .unwrap_or(default_port);
        return Some((host, port));
    }
    let (host, port) = authority
        .rsplit_once(':')
        .and_then(|(host, port)| port.parse().ok().map(|port| (host, port)))
        .unwrap_or((authority, default_port));
    Some((host.trim_end_matches('.').to_ascii_lowercase(), port))
}

fn no_proxy_matches(url: &str, no_proxy: &str) -> bool {
    let Some((host, port)) = url_host_and_port(url) else {
        return false;
    };
    no_proxy.split(',').any(|entry| {
        let entry = entry.trim();
        if entry == "*" {
            return true;
        }
        if entry.is_empty() || entry.contains('/') {
            return false;
        }
        let entry = entry.trim_start_matches("*.");
        let (pattern, required_port) = entry
            .rsplit_once(':')
            .and_then(|(pattern, port)| port.parse::<u16>().ok().map(|port| (pattern, Some(port))))
            .unwrap_or((entry, None));
        if required_port.is_some_and(|required| required != port) {
            return false;
        }
        let pattern = pattern
            .trim_start_matches('.')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        !pattern.is_empty()
            && (host == pattern
                || host
                    .strip_suffix(&pattern)
                    .is_some_and(|prefix| prefix.ends_with('.')))
    })
}

/// Build a proxy from the first usable `*_PROXY` variable, if any. Unparsable or
/// non-HTTP proxies are ignored so the request falls back to a direct connection.
fn proxy_for(url: &str, environment: &[(String, String)]) -> Option<minreq::Proxy> {
    const KEYS: [&str; 6] = [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ];
    if environment_value(environment, &["NO_PROXY", "no_proxy"])
        .is_some_and(|value| no_proxy_matches(url, &value))
    {
        return None;
    }
    let value = environment_value(environment, &KEYS)?;
    let value = value.trim();
    (!value.is_empty())
        .then(|| minreq::Proxy::new(value).ok())
        .flatten()
}

fn prepare(
    request: minreq::Request,
    url: &str,
    environment: &[(String, String)],
) -> minreq::Request {
    let mut request = request
        .with_header("User-Agent", USER_AGENT)
        .with_header("Accept", "*/*")
        .with_timeout(REQUEST_TIMEOUT_SECS);
    if let Some(proxy) = proxy_for(url, environment) {
        request = request.with_proxy(proxy);
    }
    request
}

fn request(url: &str, environment: &[(String, String)]) -> minreq::Request {
    prepare(minreq::get(url), url, environment)
}

fn headed(request: minreq::Request, headers: &[(&str, &str)]) -> minreq::Request {
    headers.iter().fold(request, |request, (name, value)| {
        request.with_header(*name, *value)
    })
}

fn retry_delay(attempt: usize) {
    thread::sleep(Duration::from_millis(200 * attempt as u64));
}

fn send(
    request: minreq::Request,
    url: &str,
    accepted_status: impl Fn(u16) -> bool,
) -> Result<minreq::Response> {
    let mut last_error = None;
    for attempt in 1..=ATTEMPTS {
        match request.clone().send() {
            Ok(response) if accepted_status(response.status_code) => return Ok(response),
            Ok(response) => {
                let error = anyhow!("{url} returned HTTP {}", response.status_code);
                if response.status_code < 500 || attempt == ATTEMPTS {
                    return Err(error);
                }
                last_error = Some(error);
            }
            Err(error) => {
                last_error = Some(anyhow!(error).context(format!("request to {url} failed")));
            }
        }
        if attempt < ATTEMPTS {
            retry_delay(attempt);
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("request to {url} failed")))
}

/// Keep an error readable. A chat API puts the reason in the body, not in the
/// status line, so the body has to travel with the error — but it can also be
/// kilobytes of echoed request, which nobody wants in a log line.
fn excerpt(body: &str) -> String {
    const LIMIT: usize = 240;
    let body = body.trim();
    if body.is_empty() {
        return "(empty body)".to_string();
    }
    match body.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}…", &body[..cut]),
        None => body.to_string(),
    }
}

fn json(response: minreq::Response, url: &str) -> Result<serde_json::Value> {
    let status = response.status_code;
    let body = response.as_str().unwrap_or_default().trim();
    if !(200..300).contains(&status) {
        bail!("{url} returned HTTP {status}: {}", excerpt(body));
    }
    serde_json::from_str(body)
        .with_context(|| format!("{url} did not answer with JSON: {}", excerpt(body)))
}

/// GET a JSON document, with caller-supplied headers (an `Authorization`, say).
pub fn get_json(
    url: &str,
    headers: &[(&str, &str)],
    environment: &[(String, String)],
) -> Result<serde_json::Value> {
    let response = headed(request(url, environment), headers);
    // A read can be replayed, so a server that is briefly unwell is worth
    // waiting out.
    json(send(response, url, |status| status < 500)?, url)
}

/// GET or POST a JSON document from an endpoint that holds the request open
/// until it has something to say.
///
/// A long poll ending in silence is the ordinary case, not a failure, so the
/// deadline running out answers `Ok(None)` and the caller simply asks again.
/// One attempt only: retrying a request that was *meant* to take a while would
/// turn a quiet minute into three of them.
pub fn poll_json(
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&serde_json::Value>,
    seconds: u64,
    environment: &[(String, String)],
) -> Result<Option<serde_json::Value>> {
    let request = match body {
        Some(body) => headed(prepare(minreq::post(url), url, environment), headers)
            .with_header("Content-Type", "application/json; charset=utf-8")
            .with_body(serde_json::to_vec(body).context("failed to encode the request body")?),
        None => headed(request(url, environment), headers),
    };
    match request.with_timeout(seconds).send() {
        Ok(response) => json(response, url).map(Some),
        Err(minreq::Error::IoError(error)) if error.kind() == std::io::ErrorKind::TimedOut => {
            Ok(None)
        }
        Err(error) => Err(anyhow!(error).context(format!("request to {url} failed"))),
    }
}

/// POST a JSON document and read the JSON answer.
pub fn post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
    environment: &[(String, String)],
) -> Result<serde_json::Value> {
    let request = headed(prepare(minreq::post(url), url, environment), headers)
        .with_header("Content-Type", "application/json; charset=utf-8")
        .with_body(serde_json::to_vec(body).context("failed to encode the request body")?);
    // Unlike a GET, this is not replayable: a 5xx may still have posted the
    // message, so only a request that never reached the server is retried —
    // which is exactly what `send` does when the transport itself fails.
    json(send(request, url, |_| true)?, url)
}

/// POST `application/x-www-form-urlencoded`, used by the flows that take a
/// plain form rather than JSON — Feishu's bot-registration endpoint among
/// them.
pub fn post_form(
    url: &str,
    headers: &[(&str, &str)],
    fields: &[(String, String)],
    environment: &[(String, String)],
) -> Result<serde_json::Value> {
    json(post_form_response(url, headers, fields, environment)?, url)
}

/// The same POST as [`post_form`], but the body is parsed as JSON without
/// asking that the status be 2xx. Feishu's onboarding answers "still waiting
/// for the scan" with HTTP 400 and a JSON body; a caller that wants that
/// answer instead of an error uses this and reads the body itself.
///
/// The overall request is still bounded: a connection timeout and the usual
/// retry on a transport that never answered.
pub fn post_form_free(
    url: &str,
    headers: &[(&str, &str)],
    fields: &[(String, String)],
    environment: &[(String, String)],
) -> Result<serde_json::Value> {
    let response = post_form_response(url, headers, fields, environment)?;
    let body = response.as_str().unwrap_or_default();
    serde_json::from_str(body).with_context(|| format!("{url} did not answer with JSON: {body}"))
}

fn post_form_response(
    url: &str,
    headers: &[(&str, &str)],
    fields: &[(String, String)],
    environment: &[(String, String)],
) -> Result<minreq::Response> {
    let mut encoded = String::new();
    for (index, (name, value)) in fields.iter().enumerate() {
        if index > 0 {
            encoded.push('&');
        }
        encoded.push_str(&form_encode(name));
        encoded.push('=');
        encoded.push_str(&form_encode(value));
    }
    let request = headed(prepare(minreq::post(url), url, environment), headers)
        .with_header("Content-Type", "application/x-www-form-urlencoded")
        .with_body(encoded);
    send(request, url, |_| true)
}

/// Percent-encode one form field, the way `application/x-www-form-urlencoded`
/// wants it: everything outside the unreserved set becomes percent-triplets.
/// Iterating bytes keeps each UTF-8 code point intact as its own `%XX` runs.
fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Fetch a small text body (follows redirects).
#[cfg(feature = "controller")]
pub fn fetch_text(url: &str, environment: &[(String, String)]) -> Result<String> {
    let response = send(request(url, environment), url, |status| {
        (200..300).contains(&status)
    })?;
    Ok(response
        .as_str()
        .with_context(|| format!("{url} returned a non-text body"))?
        .to_string())
}

/// Return the `Location` a URL redirects to, without downloading the target.
/// Used to read the newest release tag from `.../releases/latest`.
#[cfg(feature = "controller")]
pub fn redirect_location(url: &str, environment: &[(String, String)]) -> Result<String> {
    let response = send(
        request(url, environment).with_follow_redirects(false),
        url,
        |status| (300..400).contains(&status),
    )?;
    header(&response.headers, "location")
        .map(str::to_string)
        .with_context(|| format!("{url} redirect had no Location header"))
}

/// Follow redirects and return the final response URL without buffering its
/// body. This replaces `curl -w %{url_effective}` for release discovery.
#[cfg(feature = "controller")]
pub fn effective_url(url: &str, environment: &[(String, String)]) -> Result<String> {
    let mut last_error = None;
    for attempt in 1..=ATTEMPTS {
        match request(url, environment).send_lazy() {
            Ok(response) if (200..300).contains(&response.status_code) => return Ok(response.url),
            Ok(response) => {
                let error = anyhow!("{url} returned HTTP {}", response.status_code);
                if response.status_code < 500 || attempt == ATTEMPTS {
                    return Err(error);
                }
                last_error = Some(error);
            }
            Err(error) => {
                last_error = Some(anyhow!(error).context(format!("request to {url} failed")));
            }
        }
        if attempt < ATTEMPTS {
            retry_delay(attempt);
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("request to {url} failed")))
}

/// Stream `url` into `destination`, invoking `on_progress(downloaded, total)` as
/// bytes arrive (`total` is the Content-Length when the server reports it).
/// Follows redirects, since release asset URLs redirect to a CDN.
#[cfg(feature = "controller")]
pub fn download<F>(
    url: &str,
    destination: &Path,
    environment: &[(String, String)],
    mut on_progress: F,
) -> Result<()>
where
    F: FnMut(u64, Option<u64>),
{
    let mut last_error = None;
    for attempt in 1..=ATTEMPTS {
        match download_once(url, destination, environment, &mut on_progress) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        if attempt < ATTEMPTS {
            retry_delay(attempt);
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("download from {url} failed")))
}

#[cfg(feature = "controller")]
fn download_once(
    url: &str,
    destination: &Path,
    environment: &[(String, String)],
    on_progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let mut response = request(url, environment)
        .with_timeout(DOWNLOAD_TIMEOUT_SECS)
        .send_lazy()
        .with_context(|| format!("request to {url} failed"))?;
    if !(200..300).contains(&response.status_code) {
        bail!("{url} returned HTTP {}", response.status_code);
    }
    let total =
        header(&response.headers, "content-length").and_then(|value| value.parse::<u64>().ok());
    let file = File::create(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let mut writer = BufWriter::new(file);
    let mut buffer = vec![0u8; CHUNK];
    let mut downloaded = 0u64;
    on_progress(0, total);
    loop {
        let read = response
            .read(&mut buffer)
            .with_context(|| format!("error while downloading {url}"))?;
        if read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..read])
            .with_context(|| format!("failed writing {}", destination.display()))?;
        downloaded += read as u64;
        on_progress(downloaded, total);
    }
    writer.flush()?;
    if total.is_some_and(|total| downloaded != total) {
        bail!(
            "download from {url} ended after {downloaded} bytes, expected {}",
            total.unwrap_or_default()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_bypass_matches_hosts_subdomains_and_ports() {
        assert!(no_proxy_matches(
            "https://api.example.com/releases",
            ".example.com,localhost"
        ));
        assert!(no_proxy_matches(
            "https://example.com/releases",
            "example.com"
        ));
        assert!(no_proxy_matches("http://localhost:8118/", "localhost:8118"));
        assert!(!no_proxy_matches(
            "http://localhost:8080/",
            "localhost:8118"
        ));
        assert!(!no_proxy_matches("https://notexample.com/", "example.com"));
        assert!(no_proxy_matches("https://anything.invalid/", "*"));
    }

    #[test]
    fn explicit_download_environment_takes_precedence() {
        let environment = vec![("HTTPS_PROXY".into(), "http://configured:8118".into())];
        assert_eq!(
            environment_value(&environment, &["HTTPS_PROXY", "https_proxy"]).as_deref(),
            Some("http://configured:8118")
        );
    }

    #[test]
    fn error_excerpts_stay_short_and_never_split_a_character() {
        assert_eq!(excerpt("  "), "(empty body)");
        assert_eq!(excerpt(" {\"code\":99991663} "), "{\"code\":99991663}");
        let long = "验".repeat(400);
        let cut = excerpt(&long);
        assert!(cut.ends_with('…'));
        assert_eq!(cut.chars().count(), 241);
    }

    // Network smoke test; run explicitly: cargo test --lib http -- --ignored --nocapture
    #[cfg(feature = "controller")]
    #[test]
    #[ignore]
    fn hits_github_release_endpoints() {
        let env: Vec<(String, String)> = Vec::new();
        let location = redirect_location(
            "https://github.com/MarsTechHAN/Muxloom/releases/latest",
            &env,
        )
        .expect("redirect");
        println!("latest -> {location}");
        assert!(location.contains("/releases/tag/v"));

        let sha = fetch_text(
            "https://github.com/MarsTechHAN/Muxloom/releases/latest/download/muxloomd-aarch64-apple-darwin.sha256",
            &env,
        )
        .expect("sha text");
        println!("sha256 sidecar: {}", sha.trim());
        assert_eq!(sha.split_whitespace().next().unwrap().len(), 64);

        let tmp = std::env::temp_dir().join("muxloom-http-smoke.sha256");
        download(
            "https://github.com/MarsTechHAN/Muxloom/releases/latest/download/muxloomd-aarch64-apple-darwin.sha256",
            &tmp,
            &env,
            |done, total| println!("progress {done}/{total:?}"),
        )
        .expect("download");
        assert!(tmp.exists());
        std::fs::remove_file(tmp).expect("remove smoke-test download");
    }
}
