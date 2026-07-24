//! Minimal in-process HTTP(S) client built on rustls (via `minreq`).
//!
//! muxloom ships this instead of shelling out to `curl`, so a stock machine
//! with no `curl` installed can still fetch companion binaries, agent packages,
//! and self-updates. Only four shapes are needed:
//!
//! - [`fetch_text`] for a short body (e.g. a `.sha256` sidecar),
//! - [`redirect_location`] to read a redirect target without downloading it
//!   (used to discover the latest release tag without the rate-limited GitHub
//!   API),
//! - [`effective_url`] to resolve a redirect chain without buffering the body,
//!   and
//! - [`download`] to stream a file to disk with byte-level progress.
//!
//! Every request honors the standard `*_PROXY` environment variables carried in
//! the `environment` pairs the controller already threads through for downloads.

use std::{
    fs::File,
    io::{BufWriter, Read, Write},
    path::Path,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};

/// GitHub requires a User-Agent; this also identifies us to any proxy.
const USER_AGENT: &str = concat!("muxloom/", env!("CARGO_PKG_VERSION"));
/// Total request deadline for short metadata requests.
const REQUEST_TIMEOUT_SECS: u64 = 60;
/// Package downloads can be hundreds of MiB. minreq applies its timeout to the
/// whole response rather than each socket read, so use a larger bounded window
/// for streamed assets.
const DOWNLOAD_TIMEOUT_SECS: u64 = 30 * 60;
/// Match the retry budget used by the former `curl --retry 3` paths.
const ATTEMPTS: usize = 3;
/// Download copy buffer size.
const CHUNK: usize = 64 * 1024;

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Build a proxy from the first usable `*_PROXY` variable, if any. Unparsable or
/// non-HTTP proxies are ignored so the request falls back to a direct connection.
fn proxy_for(environment: &[(String, String)]) -> Option<minreq::Proxy> {
    const KEYS: [&str; 6] = [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ];
    for key in KEYS {
        if let Some((_, value)) = environment.iter().find(|(name, _)| name == key) {
            let value = value.trim();
            if !value.is_empty()
                && let Ok(proxy) = minreq::Proxy::new(value)
            {
                return Some(proxy);
            }
        }
    }
    None
}

fn request(url: &str, environment: &[(String, String)]) -> minreq::Request {
    let mut request = minreq::get(url)
        .with_header("User-Agent", USER_AGENT)
        .with_header("Accept", "*/*")
        .with_timeout(REQUEST_TIMEOUT_SECS);
    if let Some(proxy) = proxy_for(environment) {
        request = request.with_proxy(proxy);
    }
    request
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

/// Fetch a small text body (follows redirects).
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

    // Network smoke test; run explicitly: cargo test --lib http -- --ignored --nocapture
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
