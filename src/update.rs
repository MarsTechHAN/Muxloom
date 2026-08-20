//! In-place self-update ("OTA") for the muxloom controller bundle.
//!
//! Flow: work out the newest published build on the configured channel,
//! compare it to the running one, and — when running from a real installed
//! release bundle — download `muxloom-v<version>-<triple>.tar.gz` from that
//! release, verify its SHA-256, extract it in process (pure-Rust gzip + tar),
//! and atomically replace the bundle's files so the next launch runs it.
//!
//! Two channels publish builds. Stable is the tagged releases, read from the
//! `releases/latest` redirect (which avoids the rate-limited GitHub API).
//! Nightly is a rolling prerelease built from every green commit on `main`; it
//! carries a `nightly.json` manifest naming the commit it came from, because a
//! rolling tag cannot say that in its name and every nightly between two
//! releases carries the same `CARGO_PKG_VERSION`.
//!
//! By default an install follows the stream it came from — nightly to nightly,
//! release to release — so nobody is moved onto a cadence they did not ask
//! for. `muxloom update --nightly` is the way across, and it needs no
//! configuration to stick: the build it installs is itself stamped nightly.
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
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    http,
    model::{parse_version, version_is_newer as is_newer},
};

const REPO_LATEST: &str = "https://github.com/MarsTechHAN/Muxloom/releases/latest";
const DOWNLOAD_BASE: &str = "https://github.com/MarsTechHAN/Muxloom/releases/download";
const RELEASES_PAGE: &str = "https://github.com/MarsTechHAN/Muxloom/releases";
/// The rolling tag every green commit on `main` republishes. It is a
/// *prerelease*, so `releases/latest` — which the stable check and the remote
/// companion pull both read — keeps pointing at the newest tagged release.
pub const NIGHTLY_TAG: &str = "nightly";
const NIGHTLY_MANIFEST: &str = "nightly.json";
/// What CI stamps into a build made from the nightly workflow.
const NIGHTLY_BUILD: &str = "nightly";
static UPDATE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The compiled-in package version, e.g. `"0.4.3"`.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The commit CI built this binary from, when CI built it.
pub fn build_commit() -> Option<&'static str> {
    option_env!("MUXLOOM_BUILD_ID")
        .map(str::trim)
        .filter(|commit| !commit.is_empty())
}

/// How many commits deep on `main` this build was made. It is the only thing
/// that orders two builds carrying the same package version, which every
/// nightly between one release and the next does.
pub fn build_height() -> Option<u64> {
    option_env!("MUXLOOM_BUILD_HEIGHT").and_then(|height| height.trim().parse().ok())
}

/// Which stream CI built this binary for: `"nightly"`, `"stable"`, or nothing
/// at all for a build made outside CI.
pub fn build_channel() -> Option<&'static str> {
    option_env!("MUXLOOM_BUILD_CHANNEL")
        .map(str::trim)
        .filter(|channel| !channel.is_empty())
}

/// Whether the running binary came off the nightly workflow.
pub fn running_a_nightly() -> bool {
    build_channel() == Some(NIGHTLY_BUILD)
}

fn short_commit(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

fn build_label(version: &str, height: Option<u64>, commit: Option<&str>) -> String {
    let mut label = version.to_string();
    if let Some(height) = height {
        label.push_str(&format!("+{height}"));
    }
    if let Some(commit) = commit {
        label.push_str(&format!(" ({})", short_commit(commit)));
    }
    label
}

/// The running build as it should be named: `0.5.4` for a plain build,
/// `0.5.4+142 (a1b2c3d)` for one CI stamped, and `nightly 0.5.4+142 (a1b2c3d)`
/// when it came off the nightly workflow.
pub fn current_build_label() -> String {
    let label = build_label(current_version(), build_height(), build_commit());
    if running_a_nightly() {
        format!("nightly {label}")
    } else {
        label
    }
}

/// Which stream of builds an install follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Whichever stream this build came from: a nightly is offered nightlies,
    /// and a tagged release — like a build from source — is offered tagged
    /// releases. This is the default, and it is what keeps someone who never
    /// asked for nightlies on the release cadence they installed.
    Auto,
    Stable,
    Nightly,
}

impl Channel {
    /// The config field is validated when it loads, so a value that reaches
    /// here unrecognised came from somewhere that should not silently pick a
    /// stream for the user: it follows the build, same as the default.
    pub fn from_config(value: &str) -> Self {
        match value.trim() {
            "stable" => Channel::Stable,
            "nightly" => Channel::Nightly,
            _ => Channel::Auto,
        }
    }

    fn wants_nightly_for(self, build: Option<&str>) -> bool {
        match self {
            Channel::Nightly => true,
            Channel::Stable => false,
            Channel::Auto => build == Some(NIGHTLY_BUILD),
        }
    }

    fn wants_nightly(self) -> bool {
        self.wants_nightly_for(build_channel())
    }

    /// Whether asking for this channel means stepping *onto* a nightly from a
    /// build that is not one. Crossing streams is not an upgrade, so ordering
    /// must not veto it: the release someone is on today carries no commit
    /// count at all, and refusing it a same-version nightly would leave
    /// `--nightly` with nothing to say but "already up to date".
    fn joins_nightly_for(self, build: Option<&str>) -> bool {
        self == Channel::Nightly && build != Some(NIGHTLY_BUILD)
    }

    /// The same crossing in the other direction: the newest release counts as
    /// an update even though the nightly being left has run past its version.
    fn leaves_nightly_for(self, build: Option<&str>) -> bool {
        self == Channel::Stable && build == Some(NIGHTLY_BUILD)
    }
}

/// A published build this updater can install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The release tag its assets hang off: `v0.5.5`, or `nightly`.
    pub tag: String,
    /// The package version its asset names carry. A nightly's archives are
    /// named after the version in `Cargo.toml`, exactly as a release's are.
    pub version: String,
    /// How it is named to the user.
    pub label: String,
}

fn stable_release(version: &str) -> Release {
    Release {
        tag: format!("v{version}"),
        version: version.to_string(),
        label: version.to_string(),
    }
}

/// What the nightly release says it is, published beside the archives as
/// `nightly.json` because a rolling tag cannot carry the identity of the
/// commit it was built from in its name.
#[derive(Debug, Clone, Deserialize)]
pub struct NightlyBuild {
    pub version: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub height: u64,
    #[serde(default)]
    pub built_at: String,
}

impl NightlyBuild {
    fn release(&self) -> Release {
        let commit = (!self.commit.is_empty()).then_some(self.commit.as_str());
        Release {
            tag: NIGHTLY_TAG.to_string(),
            version: self.version.clone(),
            label: format!(
                "nightly {}",
                build_label(&self.version, Some(self.height), commit)
            ),
        }
    }

    /// Whether this published nightly is ahead of a build of `running_version`
    /// made `running_height` commits deep. Equal versions are decided by height
    /// alone; a build with no height was made outside CI, so it cannot be
    /// ordered against a nightly and is left alone rather than offered an
    /// update it may already be ahead of.
    fn is_newer_than(&self, running_version: &str, running_height: Option<u64>) -> bool {
        if is_newer(&self.version, running_version) {
            return true;
        }
        if is_newer(running_version, &self.version) {
            return false;
        }
        running_height.is_some_and(|running| self.height > running)
    }

    fn is_newer_than_running(&self) -> bool {
        self.is_newer_than(current_version(), build_height())
    }
}

/// Outcome of a background check.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub current: String,
    /// The published build that is ahead of this one, if any.
    pub release: Option<Release>,
    /// True when that build was downloaded and staged this run.
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

/// Read what the rolling nightly release currently holds.
pub fn detect_nightly(environment: &[(String, String)]) -> Result<NightlyBuild> {
    let url = format!("{DOWNLOAD_BASE}/{NIGHTLY_TAG}/{NIGHTLY_MANIFEST}");
    let manifest = http::fetch_text(&url, environment)
        .context("could not reach GitHub to check for a nightly build")?;
    let build: NightlyBuild =
        serde_json::from_str(&manifest).context("unexpected nightly manifest")?;
    if parse_version(&build.version).is_none() {
        bail!("unexpected nightly version: {:?}", build.version);
    }
    Ok(build)
}

/// The build worth offering on this channel, if any is ahead of the running
/// one. A nightly install still falls back to the stable check: a nightly no
/// newer than this build — or a manifest that could not be read at all —
/// leaves the tagged releases to answer for, and the two streams are ordered
/// against each other by the same commit height. Naming a stream this build
/// is not on skips the ordering entirely, in both directions: crossing over is
/// what was asked for, not an upgrade to argue about.
pub fn detect_update(
    channel: Channel,
    environment: &[(String, String)],
) -> Result<Option<Release>> {
    let crossing_over = channel.joins_nightly_for(build_channel());
    if channel.wants_nightly()
        && let Ok(build) = detect_nightly(environment)
        && (crossing_over || build.is_newer_than_running())
    {
        return Ok(Some(build.release()));
    }
    let latest = detect_latest(environment)?;
    let switching_back = channel.leaves_nightly_for(build_channel());
    Ok((switching_back || is_newer(&latest, current_version())).then(|| stable_release(&latest)))
}

/// Directory containing the running muxloom executable (the bundle root).
fn bundle_dir() -> Result<PathBuf> {
    let exe = env::current_exe().context("cannot locate the muxloom executable")?;
    exe.parent()
        .map(Path::to_path_buf)
        .context("muxloom executable has no parent directory")
}

/// The file a package manager drops beside `muxloom` to say the install is
/// its own. Its first line names the manager, so a notice can point at the
/// command that will actually work instead of only refusing.
const MANAGED_MARKER: &str = ".muxloom-managed";

/// The package manager that owns the bundle at `dir`, if one claimed it.
fn managed_by_at(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join(MANAGED_MARKER)).ok()?;
    let name = text.lines().next().unwrap_or_default().trim();
    Some(match name {
        // A marker with nothing in it still means hands off; only the advice
        // gets vaguer.
        "" => "a package manager".to_string(),
        name => name.to_string(),
    })
}

/// The package manager that owns the running install, if any.
pub fn managed_by() -> Option<String> {
    managed_by_at(&bundle_dir().ok()?)
}

/// The installed bundle directory, if we're actually running from one *and*
/// the files are ours to replace. See [`updatable_bundle_at`].
fn installed_bundle() -> Option<PathBuf> {
    updatable_bundle_at(&bundle_dir().ok()?)
}

/// The bundle at `dir`, if updating it in place would be right. A real bundle
/// keeps the `muxloomd` companion beside `muxloom` and is not inside a Cargo
/// `target/` tree — this guards against clobbering a `cargo run` build. A
/// package manager's marker rules it out for the same reason from the other
/// side: those files belong to the manager, which would hand back the version
/// it installed on the next upgrade and undo the update anyway.
fn updatable_bundle_at(dir: &Path) -> Option<PathBuf> {
    if dir
        .components()
        .any(|component| component.as_os_str() == "target")
        || managed_by_at(dir).is_some()
    {
        return None;
    }
    let companion = dir.join(format!("muxloomd{}", env::consts::EXE_SUFFIX));
    companion.is_file().then(|| dir.to_path_buf())
}

/// Whether an auto-update could actually replace files on this install.
pub fn is_installed_bundle() -> bool {
    installed_bundle().is_some()
}

/// Detect the newest build on `channel` and, when `auto_apply` is set and we're
/// running from a real Unix bundle, download and stage it. Safe to call from a
/// worker thread; returns a result for status/logging rather than panicking.
pub fn check_and_maybe_apply(
    channel: Channel,
    auto_apply: bool,
    environment: &[(String, String)],
) -> Result<CheckResult> {
    let current = current_build_label();
    let release = detect_update(channel, environment)?;
    let mut applied = false;
    if let Some(release) = &release
        && auto_apply
        && cfg!(unix)
        && is_installed_bundle()
    {
        apply(release, environment, |_, _| {})?;
        applied = true;
    }
    Ok(CheckResult {
        current,
        release,
        applied,
    })
}

/// `muxloom update` — synchronous, prints progress to stdout.
pub fn run_cli(channel: Channel, environment: &[(String, String)]) -> Result<()> {
    use std::io::Write;

    println!("muxloom {}", current_build_label());
    print!("Checking for updates… ");
    let _ = std::io::stdout().flush();

    let Some(release) = detect_update(channel, environment)? else {
        println!("already up to date.");
        return Ok(());
    };
    let label = release.label.clone();
    println!("found {label}.");

    if !cfg!(unix) {
        println!(
            "Automatic install is not supported on this platform; download {label} from {RELEASES_PAGE}"
        );
        return Ok(());
    }
    if let Some(manager) = managed_by() {
        println!(
            "This install is managed by {manager}; update it there — muxloom will not write \
             over files it does not own."
        );
        return Ok(());
    }
    if !is_installed_bundle() {
        println!(
            "Running from a development build, not an installed release bundle; download {label} from {RELEASES_PAGE}"
        );
        return Ok(());
    }

    let mut last_percent = u8::MAX;
    apply(&release, environment, |done, total| {
        if let Some(total) = total.filter(|total| *total > 0) {
            let percent = ((done * 100) / total).min(100) as u8;
            if percent != last_percent {
                last_percent = percent;
                print!("\rDownloading {label}… {percent}%   ");
                let _ = std::io::stdout().flush();
            }
        }
    })?;
    println!("\rInstalled {label}. Restart muxloom to use it.   ");
    Ok(())
}

/// Download a release's bundle, verify it, extract it, and replace the files
/// of the installed bundle in place. `on_progress(downloaded, total)` fires as
/// the archive downloads.
pub fn apply<F>(release: &Release, environment: &[(String, String)], on_progress: F) -> Result<()>
where
    F: FnMut(u64, Option<u64>),
{
    if !cfg!(unix) {
        bail!("automatic replacement is only supported on Unix");
    }
    let triple = target_triple().context("no release build is published for this platform")?;
    let bundle = installed_bundle().context("not running from an installed release bundle")?;
    let version = release.version.as_str();
    let archive_name = release_archive_name(version, triple);
    let base = format!("{DOWNLOAD_BASE}/{}", release.tag);

    // Stage inside the bundle dir so the final renames stay on one filesystem.
    let nonce = UPDATE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let workdir = bundle.join(format!(".muxloom-update-{}-{nonce}", std::process::id()));
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

/// Replace known bundle files atomically one at a time, with the controller as
/// the final commit point. A failure before that leaves the old controller with
/// newer compatible companions rather than a new controller with missing
/// support files. Docs (README/LICENSE) and stale extra companions are retained.
fn apply_over(src: &Path, dst: &Path) -> Result<()> {
    let suffix = env::consts::EXE_SUFFIX;
    let controller = src.join(format!("muxloom{suffix}"));
    let companion = src.join(format!("muxloomd{suffix}"));
    if !controller.is_file() || !companion.is_file() {
        bail!("release bundle is missing muxloom or muxloomd");
    }

    let companions_src = src.join("companions");
    if companions_src.is_dir() {
        overlay_tree(&companions_src, &dst.join("companions")).context("replace companions")?;
    }
    for base in ["muxloomd", "ffmpeg"] {
        let name = format!("{base}{suffix}");
        let from = src.join(&name);
        if from.is_file() {
            replace_file(&from, &dst.join(&name)).with_context(|| format!("replace {name}"))?;
        }
    }
    let controller_name = format!("muxloom{suffix}");
    replace_file(&controller, &dst.join(&controller_name))
        .with_context(|| format!("replace {controller_name}"))?;
    Ok(())
}

fn replace_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(from) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(permissions.mode() | 0o755);
            fs::set_permissions(from, permissions)?;
        }
    }
    fs::rename(from, to).with_context(|| {
        format!(
            "atomic rename from {} to {} failed",
            from.display(),
            to.display()
        )
    })?;
    Ok(())
}

fn overlay_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let child_from = entry.path();
        let child_to = to.join(entry.file_name());
        if child_from.is_dir() {
            overlay_tree(&child_from, &child_to)?;
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

    #[test]
    fn a_build_follows_the_stream_it_came_from_unless_it_is_told_otherwise() {
        assert_eq!(Channel::from_config("stable"), Channel::Stable);
        assert_eq!(Channel::from_config(" nightly\n"), Channel::Nightly);
        assert_eq!(Channel::from_config("auto"), Channel::Auto);
        // A value nothing recognises must not pick a stream for the user.
        assert_eq!(Channel::from_config(""), Channel::Auto);
        assert_eq!(Channel::from_config("bleeding"), Channel::Auto);

        // The whole point: a release install is never dragged onto nightlies,
        // and a nightly install keeps getting them.
        assert!(!Channel::Auto.wants_nightly_for(Some("stable")));
        assert!(!Channel::Auto.wants_nightly_for(None));
        assert!(Channel::Auto.wants_nightly_for(Some("nightly")));
        // Asking outright wins either way.
        assert!(Channel::Nightly.wants_nightly_for(Some("stable")));
        assert!(!Channel::Stable.wants_nightly_for(Some("nightly")));

        // Stepping off nightly is a switch, not an upgrade: the newest release
        // counts even when the nightly ran past its version.
        assert!(Channel::Stable.leaves_nightly_for(Some("nightly")));
        assert!(!Channel::Stable.leaves_nightly_for(Some("stable")));
        assert!(!Channel::Auto.leaves_nightly_for(Some("nightly")));

        // And so is stepping on. Every release anyone is running today carries
        // no commit count, so ordering alone would answer `--nightly` with
        // "already up to date" and there would be no way onto the stream at all.
        assert!(Channel::Nightly.joins_nightly_for(Some("stable")));
        assert!(Channel::Nightly.joins_nightly_for(None));
        assert!(!Channel::Nightly.joins_nightly_for(Some("nightly")));
        // Following a stream is never a crossing, in either direction.
        assert!(!Channel::Auto.joins_nightly_for(Some("stable")));
        assert!(!Channel::Stable.joins_nightly_for(Some("stable")));
    }

    #[test]
    fn a_nightly_is_ordered_against_the_running_build_by_version_then_commit_count() {
        let nightly = |version: &str, height: u64| NightlyBuild {
            version: version.to_string(),
            commit: "a1b2c3d4e5f6".into(),
            height,
            built_at: String::new(),
        };

        // A newer package version wins outright, whatever the counts say.
        assert!(nightly("0.5.5", 1).is_newer_than("0.5.4", Some(999)));
        // A nightly left behind by a release must never be offered as an
        // upgrade, which is what makes the channel safe to leave on.
        assert!(!nightly("0.5.4", 999).is_newer_than("0.5.5", Some(1)));
        // Every nightly between two releases carries the same version, so the
        // commit count is the only thing that orders them.
        assert!(nightly("0.5.4", 143).is_newer_than("0.5.4", Some(142)));
        assert!(!nightly("0.5.4", 142).is_newer_than("0.5.4", Some(142)));
        assert!(!nightly("0.5.4", 141).is_newer_than("0.5.4", Some(142)));
        // An unstamped build (a local `cargo build`, or a release made before
        // the stamp existed) cannot be placed, so a same-version nightly is
        // never pushed at it.
        assert!(!nightly("0.5.4", 143).is_newer_than("0.5.4", None));
        assert!(nightly("0.5.5", 1).is_newer_than("0.5.4", None));
    }

    #[test]
    fn a_nightly_manifest_names_the_build_and_points_at_the_rolling_tag() {
        let manifest = r#"{
            "version": "0.5.4",
            "commit": "a1b2c3d4e5f60718",
            "height": 142,
            "built_at": "2026-08-19T04:05:06Z"
        }"#;
        let build: NightlyBuild = serde_json::from_str(manifest).unwrap();
        let release = build.release();
        assert_eq!(release.tag, NIGHTLY_TAG);
        // The archive is named after the package version exactly as a tagged
        // release's is; only the tag it hangs off differs.
        assert_eq!(
            release_archive_name(&release.version, "aarch64-apple-darwin"),
            "muxloom-v0.5.4-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(release.label, "nightly 0.5.4+142 (a1b2c3d)");
        assert_eq!(build_label("0.5.4", None, None), "0.5.4");
        assert_eq!(build_label("0.5.4", Some(142), None), "0.5.4+142");
    }

    #[test]
    fn a_package_managed_install_is_reported_but_never_written_over() {
        let nonce = UPDATE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!(
            "muxloom-managed-test-{}-{nonce}",
            std::process::id()
        ));
        let bundle = root.join("libexec");
        let companion = format!("muxloomd{}", env::consts::EXE_SUFFIX);
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join(&companion), b"companion").unwrap();

        // A bundle nobody claimed is ours to replace in place.
        assert_eq!(managed_by_at(&bundle), None);
        assert_eq!(
            updatable_bundle_at(&bundle).as_deref(),
            Some(bundle.as_path())
        );

        // Homebrew's Cellar would hand its own build back on the next upgrade,
        // so an update there is worth saying and not worth applying.
        fs::write(bundle.join(MANAGED_MARKER), "homebrew\n").unwrap();
        assert_eq!(managed_by_at(&bundle).as_deref(), Some("homebrew"));
        assert_eq!(updatable_bundle_at(&bundle), None);

        // A marker naming nobody still means hands off; only the advice blurs.
        fs::write(bundle.join(MANAGED_MARKER), "\n").unwrap();
        assert_eq!(managed_by_at(&bundle).as_deref(), Some("a package manager"));
        assert_eq!(updatable_bundle_at(&bundle), None);

        // And an unmarked `cargo build` is still not an install.
        let build = root.join("target/release");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join(&companion), b"companion").unwrap();
        assert_eq!(updatable_bundle_at(&build), None);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn bundle_overlay_replaces_core_files_and_preserves_extra_companions() {
        let nonce = UPDATE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!(
            "muxloom-update-test-{}-{nonce}",
            std::process::id()
        ));
        let src = root.join("new");
        let dst = root.join("installed");
        let suffix = env::consts::EXE_SUFFIX;
        fs::create_dir_all(src.join("companions/test-target")).unwrap();
        fs::create_dir_all(dst.join("companions/old-target")).unwrap();
        for (base, contents) in [
            ("muxloom", b"new-controller".as_slice()),
            ("muxloomd", b"new-companion".as_slice()),
            ("ffmpeg", b"new-ffmpeg".as_slice()),
        ] {
            fs::write(src.join(format!("{base}{suffix}")), contents).unwrap();
            fs::write(dst.join(format!("{base}{suffix}")), b"old").unwrap();
        }
        fs::write(
            src.join(format!("companions/test-target/muxloomd{suffix}")),
            b"new-target",
        )
        .unwrap();
        fs::write(
            dst.join(format!("companions/old-target/muxloomd{suffix}")),
            b"old-target",
        )
        .unwrap();

        apply_over(&src, &dst).unwrap();

        assert_eq!(
            fs::read(dst.join(format!("muxloom{suffix}"))).unwrap(),
            b"new-controller"
        );
        assert_eq!(
            fs::read(dst.join(format!("muxloomd{suffix}"))).unwrap(),
            b"new-companion"
        );
        assert!(
            dst.join(format!("companions/test-target/muxloomd{suffix}"))
                .is_file()
        );
        assert!(
            dst.join(format!("companions/old-target/muxloomd{suffix}"))
                .is_file()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
