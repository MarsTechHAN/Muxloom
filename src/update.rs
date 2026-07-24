//! In-place self-update ("OTA") for the muxloom controller bundle.
//!
//! Flow: read the newest release tag from the `releases/latest` redirect
//! (avoids the rate-limited GitHub API), compare it to the compiled-in version,
//! and — when running from a real installed release bundle — download
//! `muxloom-<version>-<triple>.tar.gz`, verify its SHA-256, extract it in
//! process (pure-Rust gzip + tar), and atomically replace the bundle's files so
//! the next launch runs the new version.
//!
//! On Unix, renaming a new file over a running executable keeps the running
//! process's inode alive, so this is safe while muxloom (and any local
//! muxloomd) are running; the change takes effect on the next launch. Windows
//! cannot replace a running `.exe`, so auto-apply is Unix-only there.

use std::{
    env,
    fs::{self, File},
    io::{BufReader, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::http;

const REPO_LATEST: &str = "https://github.com/MarsTechHAN/Muxloom/releases/latest";
const DOWNLOAD_BASE: &str = "https://github.com/MarsTechHAN/Muxloom/releases/download";
const RELEASES_PAGE: &str = "https://github.com/MarsTechHAN/Muxloom/releases";

/// The compiled-in package version, e.g. `"0.4.2"`.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Outcome of a background check.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub current: String,
    pub latest: String,
    pub update_available: bool,
    /// True when a newer bundle was downloaded and staged this run.
    pub applied: bool,
}

/// Release-archive triple for this platform, or `None` if unsupported.
fn target_triple() -> Option<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// Parse a semantic version into comparable numbers, ignoring any pre-release or
/// build suffix (`0.4.3-rc1` -> `(0, 4, 3)`). `None` if it does not look like one.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let core = text.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

fn release_archive_name(version: &str, triple: &str) -> String {
    format!("muxloom-v{version}-{triple}.tar.gz")
}

/// Read the newest published version (e.g. `"0.4.3"`) from the latest-release
/// redirect target (`.../releases/tag/v0.4.3`).
pub fn detect_latest(environment: &[(String, String)]) -> Result<String> {
    let location = http::redirect_location(REPO_LATEST, environment)
        .context("could not reach GitHub to check for updates")?;
    let tag = location.rsplit('/').next().unwrap_or_default().trim();
    let version = tag.trim_start_matches('v');
    if parse_version(version).is_none() {
        bail!("unexpected latest-release tag: {tag:?}");
    }
    Ok(version.to_string())
}

/// Directory containing the running muxloom executable (the bundle root).
fn bundle_dir() -> Result<PathBuf> {
    let exe = env::current_exe().context("cannot locate the muxloom executable")?;
    exe.parent()
        .map(Path::to_path_buf)
        .context("muxloom executable has no parent directory")
}

/// The installed bundle directory, if we're actually running from one. A real
/// bundle keeps the `muxloomd` companion beside `muxloom` and is not inside a
/// Cargo `target/` tree — this guards against clobbering a `cargo run` build.
fn installed_bundle() -> Option<PathBuf> {
    let dir = bundle_dir().ok()?;
    if dir
        .components()
        .any(|component| component.as_os_str() == "target")
    {
        return None;
    }
    let companion = dir.join(format!("muxloomd{}", env::consts::EXE_SUFFIX));
    companion.is_file().then_some(dir)
}

/// Whether an auto-update could actually replace files on this install.
pub fn is_installed_bundle() -> bool {
    installed_bundle().is_some()
}

/// Detect the latest version and, when `auto_apply` is set and we're running
/// from a real Unix bundle, download and stage it. Safe to call from a worker
/// thread; returns a result for status/logging rather than panicking.
pub fn check_and_maybe_apply(
    auto_apply: bool,
    environment: &[(String, String)],
) -> Result<CheckResult> {
    let current = current_version().to_string();
    let latest = detect_latest(environment)?;
    let update_available = is_newer(&latest, &current);
    let mut applied = false;
    if update_available && auto_apply && cfg!(unix) && is_installed_bundle() {
        download_and_apply(&latest, environment, |_, _| {})?;
        applied = true;
    }
    Ok(CheckResult {
        current,
        latest,
        update_available,
        applied,
    })
}

/// `muxloom update` — synchronous, prints progress to stdout.
pub fn run_cli(environment: &[(String, String)]) -> Result<()> {
    use std::io::Write;

    let current = current_version();
    println!("muxloom {current}");
    print!("Checking for updates… ");
    let _ = std::io::stdout().flush();

    let latest = detect_latest(environment)?;
    if !is_newer(&latest, current) {
        println!("already up to date.");
        return Ok(());
    }
    println!("found {latest}.");

    if !cfg!(unix) {
        println!(
            "Automatic install is not supported on this platform; download {latest} from {RELEASES_PAGE}"
        );
        return Ok(());
    }
    if !is_installed_bundle() {
        println!(
            "Running from a development build, not an installed release bundle; download {latest} from {RELEASES_PAGE}"
        );
        return Ok(());
    }

    let mut last_percent = u8::MAX;
    download_and_apply(&latest, environment, |done, total| {
        if let Some(total) = total.filter(|total| *total > 0) {
            let percent = ((done * 100) / total).min(100) as u8;
            if percent != last_percent {
                last_percent = percent;
                print!("\rDownloading {latest}… {percent}%   ");
                let _ = std::io::stdout().flush();
            }
        }
    })?;
    println!("\rInstalled {latest}. Restart muxloom to use it.   ");
    Ok(())
}

/// Download the versioned bundle, verify it, extract it, and replace the files
/// of the installed bundle in place. `on_progress(downloaded, total)` fires as
/// the archive downloads.
fn download_and_apply<F>(
    version: &str,
    environment: &[(String, String)],
    on_progress: F,
) -> Result<()>
where
    F: FnMut(u64, Option<u64>),
{
    if !cfg!(unix) {
        bail!("automatic replacement is only supported on Unix");
    }
    let triple = target_triple().context("no release build is published for this platform")?;
    let bundle = installed_bundle().context("not running from an installed release bundle")?;
    let archive_name = release_archive_name(version, triple);
    let base = format!("{DOWNLOAD_BASE}/v{version}");

    // Stage inside the bundle dir so the final renames stay on one filesystem.
    let workdir = bundle.join(".muxloom-update");
    let _ = fs::remove_dir_all(&workdir);
    fs::create_dir_all(&workdir).with_context(|| format!("create {}", workdir.display()))?;

    let result = (|| -> Result<()> {
        let sidecar = http::fetch_text(&format!("{base}/{archive_name}.sha256"), environment)
            .context("fetch checksum")?;
        let expected = expected_sha(&sidecar)?;

        let archive_path = workdir.join(&archive_name);
        http::download(
            &format!("{base}/{archive_name}"),
            &archive_path,
            environment,
            on_progress,
        )
        .context("download archive")?;

        let actual = sha256_file(&archive_path)?;
        if actual != expected {
            bail!("checksum mismatch (expected {expected}, got {actual})");
        }

        let extracted = workdir.join("extracted");
        fs::create_dir_all(&extracted)?;
        extract_tar_gz(&archive_path, &extracted).context("extract archive")?;
        let root = extracted.join(format!("muxloom-v{version}-{triple}"));
        let root = if root.is_dir() { root } else { extracted };
        apply_over(&root, &bundle).context("install files")?;
        Ok(())
    })();

    let _ = fs::remove_dir_all(&workdir);
    result
}

fn extract_tar_gz(archive: &Path, into: &Path) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut tar = tar::Archive::new(GzDecoder::new(BufReader::new(file)));
    tar.set_preserve_permissions(true);
    tar.unpack(into)
        .with_context(|| format!("unpack into {}", into.display()))?;
    Ok(())
}

/// Replace the bundle's known files (executables + the `companions/` tree) with
/// the freshly extracted versions. Docs (README/LICENSE) are left untouched.
fn apply_over(src: &Path, dst: &Path) -> Result<()> {
    let suffix = env::consts::EXE_SUFFIX;
    for base in ["muxloom", "muxloomd", "ffmpeg"] {
        let name = format!("{base}{suffix}");
        let from = src.join(&name);
        if from.is_file() {
            replace_file(&from, &dst.join(&name)).with_context(|| format!("replace {name}"))?;
        }
    }
    let companions_src = src.join("companions");
    if companions_src.is_dir() {
        let companions_dst = dst.join("companions");
        let _ = fs::remove_dir_all(&companions_dst);
        move_tree(&companions_src, &companions_dst).context("replace companions")?;
    }
    Ok(())
}

fn replace_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(from, to).is_err() {
        fs::copy(from, to)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(to) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(permissions.mode() | 0o755);
            let _ = fs::set_permissions(to, permissions);
        }
    }
    Ok(())
}

fn move_tree(from: &Path, to: &Path) -> Result<()> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // Cross-filesystem fallback: recursive copy.
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let child_from = entry.path();
        let child_to = to.join(entry.file_name());
        if child_from.is_dir() {
            move_tree(&child_from, &child_to)?;
        } else {
            replace_file(&child_from, &child_to)?;
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        BufReader::new(File::open(path).with_context(|| format!("open {}", path.display()))?);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn expected_sha(sidecar: &str) -> Result<String> {
    let token = sidecar
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(token)
    } else {
        bail!("malformed checksum sidecar");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing_and_comparison() {
        assert_eq!(parse_version("v0.4.2"), Some((0, 4, 2)));
        assert_eq!(parse_version("0.4.10-rc1"), Some((0, 4, 10)));
        assert_eq!(parse_version("1.0"), Some((1, 0, 0)));
        assert_eq!(parse_version("not-a-version"), None);
        assert!(is_newer("0.4.3", "0.4.2"));
        assert!(is_newer("0.5.0", "0.4.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.4.2", "0.4.2"));
        assert!(!is_newer("0.4.1", "0.4.2"));
        assert!(!is_newer("garbage", "0.4.2"));
        assert_eq!(
            release_archive_name("0.4.3", "aarch64-apple-darwin"),
            "muxloom-v0.4.3-aarch64-apple-darwin.tar.gz"
        );
    }
}
