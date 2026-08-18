use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env, fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

/// The one file inside the user's SSH configuration muxloom writes, named
/// relative to the directory their `config` lives in. Everything muxloom adds
/// goes here and nowhere else, so deleting this file undoes all of it.
pub const MANAGED_INCLUDE: &str = "config.d/muxloom.conf";

const MANAGED_HEADER: &str = "\
# Managed by muxloom — edits are overwritten.
#
# muxloom writes the aliases below through its ssh_host tool and includes this
# file from your SSH config. Hosts you maintain yourself belong in that config:
# muxloom refuses to write an alias your own files already define, and deleting
# this file (plus the Include line) removes everything muxloom put here.
";

/// Returns concrete aliases from `Host` directives. Wildcard patterns are
/// configuration rules, not connectable destinations, so they are omitted.
pub fn load_hosts(path: &Path) -> Result<Vec<String>> {
    Ok(load_host_sources(path)?.into_keys().collect())
}

/// Every concrete alias with the files that define it, include files walked in
/// the order ssh reads them. An alias can appear in more than one file, which
/// is how a caller tells "muxloom wrote this" from "the user wrote this".
pub fn load_host_sources(path: &Path) -> Result<BTreeMap<String, Vec<PathBuf>>> {
    let mut hosts = BTreeMap::new();
    if !path.exists() {
        return Ok(hosts);
    }
    let mut visited = HashSet::new();
    load_file(path, &mut visited, &mut hosts)?;
    Ok(hosts)
}

/// The files other than muxloom's own that define this alias. A non-empty
/// answer means writing the alias would shadow something the user maintains.
pub fn defined_outside(ssh_config: &Path, alias: &str) -> Result<Vec<PathBuf>> {
    let managed = normalize(&managed_path(ssh_config));
    Ok(load_host_sources(ssh_config)?
        .remove(alias)
        .unwrap_or_default()
        .into_iter()
        .filter(|source| source != &managed)
        .collect())
}

/// Where muxloom's managed include lives for a given SSH config.
pub fn managed_path(ssh_config: &Path) -> PathBuf {
    ssh_config
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(MANAGED_INCLUDE)
}

/// A path as it identifies a file, so two spellings of the same include
/// compare equal. Files that do not exist yet resolve through their directory.
pub fn normalize(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => fs::canonicalize(parent)
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(name),
        _ => path.to_path_buf(),
    }
}

pub fn parse_hosts(text: &str) -> Vec<String> {
    let mut hosts = BTreeSet::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        for host in parts.take_while(|part| !part.starts_with('#')) {
            if !host.starts_with('!') && !host.contains(['*', '?', '[', ']']) {
                hosts.insert(host.to_string());
            }
        }
    }
    hosts.into_iter().collect()
}

fn load_file(
    path: &Path,
    visited: &mut HashSet<PathBuf>,
    hosts: &mut BTreeMap<String, Vec<PathBuf>>,
) -> Result<()> {
    let canonical = normalize(path);
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read SSH config {}", path.display()))?;
    for host in parse_hosts(&text) {
        hosts.entry(host).or_default().push(canonical.clone());
    }

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        if !parts
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("include"))
        {
            continue;
        }
        for pattern in parts.take_while(|part| !part.starts_with('#')) {
            for include in expand_include(path, pattern) {
                load_file(&include, visited, hosts)?;
            }
        }
    }
    Ok(())
}

/// The path an `Include` pattern names, before globbing: tilde-expanded, and
/// resolved against the including file's directory when relative.
fn resolve_include_path(source: &Path, pattern: &str) -> PathBuf {
    if pattern == "~" || pattern.starts_with("~/") {
        let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        if pattern == "~" {
            home
        } else {
            home.join(pattern.trim_start_matches("~/"))
        }
    } else {
        let path = PathBuf::from(pattern);
        if path.is_absolute() {
            path
        } else {
            source.parent().unwrap_or_else(|| Path::new(".")).join(path)
        }
    }
}

fn expand_include(source: &Path, pattern: &str) -> Vec<PathBuf> {
    let expanded = resolve_include_path(source, pattern);
    let Some(name_pattern) = expanded.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    if !name_pattern.contains(['*', '?']) {
        return expanded.is_file().then_some(expanded).into_iter().collect();
    }
    let parent = expanded.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            (wildcard_matches(name_pattern, name) && entry.path().is_file()).then(|| entry.path())
        })
        .collect();
    paths.sort();
    paths
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut p, mut v, mut star, mut matched) = (0, 0, None, 0);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            matched = v;
            p += 1;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            matched += 1;
            v = matched;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// One alias in muxloom's managed include, with its option lines in the order
/// they are written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedHost {
    pub alias: String,
    pub options: Vec<(String, String)>,
}

/// The managed include, parsed. muxloom writes every line of that file, so
/// reading it back only has to understand what [`ManagedHosts::render`] wrote —
/// anything else in there is a sign a human edited it, and survives the round
/// trip unchanged as long as it is one keyword per line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManagedHosts {
    pub entries: Vec<ManagedHost>,
}

impl ManagedHosts {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Self {
        let mut entries: Vec<ManagedHost> = Vec::new();
        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (keyword, value) = match line.split_once(char::is_whitespace) {
                Some((keyword, value)) => (keyword, value.trim()),
                None => (line, ""),
            };
            if keyword.eq_ignore_ascii_case("host") {
                entries.push(ManagedHost {
                    alias: value.to_string(),
                    options: Vec::new(),
                });
            } else if let Some(entry) = entries.last_mut() {
                entry.options.push((keyword.to_string(), value.to_string()));
            }
        }
        Self { entries }
    }

    pub fn render(&self) -> String {
        let mut text = String::from(MANAGED_HEADER);
        for entry in &self.entries {
            text.push_str(&format!("\nHost {}\n", entry.alias));
            for (keyword, value) in &entry.options {
                text.push_str(&format!("    {keyword} {value}\n"));
            }
        }
        text
    }

    pub fn get(&self, alias: &str) -> Option<&ManagedHost> {
        self.entries.iter().find(|entry| entry.alias == alias)
    }

    /// Write an alias, replacing it in place when it is already there so the
    /// file keeps the order a reader has gotten used to.
    pub fn set(&mut self, alias: &str, options: Vec<(String, String)>) {
        let entry = ManagedHost {
            alias: alias.to_string(),
            options,
        };
        match self.entries.iter().position(|host| host.alias == alias) {
            Some(index) => self.entries[index] = entry,
            None => self.entries.push(entry),
        }
    }

    pub fn remove(&mut self, alias: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.alias != alias);
        self.entries.len() != before
    }
}

/// Make the user's SSH config read the managed file, without rewriting a line
/// they wrote. Returns whether an `Include` had to be added.
///
/// Call this after the managed file exists: an `Include` that resolves to
/// nothing is how ssh treats a missing file, and this check reads the same way.
pub fn ensure_include(ssh_config: &Path, managed: &Path) -> Result<bool> {
    let existing = match fs::read_to_string(ssh_config) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read SSH config {}", ssh_config.display()));
        }
    };
    if let Some(text) = &existing
        && includes_file(ssh_config, text, managed)
    {
        return Ok(false);
    }
    let reference = include_reference(ssh_config, managed);
    // ssh applies a directive to the block it appears in, so the Include has
    // to lead the file: under a `Host` line it would only apply to that host.
    let mut text = format!(
        "# Added by muxloom: the aliases muxloom manages live in {reference}.\nInclude {reference}\n"
    );
    if let Some(existing) = existing.filter(|text| !text.trim().is_empty()) {
        text.push('\n');
        text.push_str(&existing);
    }
    write_private(ssh_config, &text)?;
    Ok(true)
}

/// How the `Include` line spells the managed file: relative to the config's
/// own directory when it lives there, which is how ssh's own docs write it.
fn include_reference(ssh_config: &Path, managed: &Path) -> String {
    let parent = ssh_config.parent().unwrap_or_else(|| Path::new("."));
    managed
        .strip_prefix(parent)
        .map(|relative| relative.to_string_lossy().into_owned())
        .unwrap_or_else(|_| managed.to_string_lossy().into_owned())
}

/// Whether this config text already pulls in `target`, by any spelling.
fn includes_file(source: &Path, text: &str, target: &Path) -> bool {
    let target = normalize(target);
    text.lines().any(|raw_line| {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            return false;
        }
        let mut parts = line.split_whitespace();
        if !parts
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("include"))
        {
            return false;
        }
        parts
            .take_while(|part| !part.starts_with('#'))
            .any(|pattern| {
                // A glob-free pattern names the file whether or not it exists yet.
                (!pattern.contains(['*', '?'])
                    && normalize(&resolve_include_path(source, pattern)) == target)
                    || expand_include(source, pattern)
                        .iter()
                        .any(|path| normalize(path) == target)
            })
    })
}

/// Replace a file's contents in one step, keeping it readable only by its
/// owner. A partly written SSH config would lock the user out of their own
/// machines, so the new text lands under a temporary name and is renamed over.
pub fn write_private(path: &Path, text: &str) -> Result<()> {
    // Follow a symlink rather than replacing it: a config linked out of a
    // dotfiles repository is still the file the user means.
    let path = &fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let Some(parent) = path.parent() else {
        bail!("cannot write {}: it has no directory", path.display());
    };
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let temporary = parent.join(format!(
        ".muxloom-{}-{}",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ssh".into()),
        std::process::id()
    ));
    fs::write(&temporary, text)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        let _ = fs::set_permissions(&temporary, fs::Permissions::from_mode(mode));
    }
    fs::rename(&temporary, path).with_context(|| {
        let _ = fs::remove_file(&temporary);
        format!("failed to replace {}", path.display())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn parses_aliases_and_ignores_patterns() {
        let input = r#"
Host *
  ServerAliveInterval 30
host work gpu-a !blocked
Host gpu-? *.corp
HOST staging
"#;
        assert_eq!(parse_hosts(input), vec!["gpu-a", "staging", "work"]);
    }

    #[test]
    fn matches_include_globs() {
        assert!(wildcard_matches("*.conf", "work.conf"));
        assert!(wildcard_matches("host-?", "host-a"));
        assert!(!wildcard_matches("host-?", "host-ab"));
    }

    fn temp_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("muxloom-ssh-{name}-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn loads_hosts_from_include_files() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("muxloom-ssh-{nonce}"));
        let includes = root.join("conf.d");
        fs::create_dir_all(&includes).unwrap();
        fs::write(root.join("config"), "Host primary\nInclude conf.d/*.conf\n").unwrap();
        fs::write(includes.join("work.conf"), "Host work-a work-b\n").unwrap();
        fs::write(includes.join("ignored.txt"), "Host ignored\n").unwrap();

        let hosts = load_hosts(&root.join("config")).unwrap();
        assert_eq!(hosts, vec!["primary", "work-a", "work-b"]);
        // Which file defined an alias is what tells muxloom's own entries from
        // the ones the user maintains.
        let sources = load_host_sources(&root.join("config")).unwrap();
        assert_eq!(
            sources["work-a"],
            vec![normalize(&includes.join("work.conf"))]
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_hosts_survive_a_render_and_read_back() {
        let mut managed = ManagedHosts::default();
        managed.set(
            "gpu",
            vec![
                ("HostName".into(), "10.0.0.5".into()),
                ("User".into(), "ada".into()),
            ],
        );
        managed.set("bastion", vec![("HostName".into(), "edge.example".into())]);
        // Rewriting an alias keeps its place in the file rather than moving it
        // to the end, so a human reading the file keeps their bearings.
        managed.set("gpu", vec![("HostName".into(), "10.0.0.6".into())]);

        let text = managed.render();
        assert!(text.starts_with("# Managed by muxloom"));
        let reread = ManagedHosts::parse(&text);
        assert_eq!(reread, managed);
        assert_eq!(
            reread
                .entries
                .iter()
                .map(|host| host.alias.as_str())
                .collect::<Vec<_>>(),
            ["gpu", "bastion"]
        );
        assert_eq!(reread.get("gpu").unwrap().options[0].1, "10.0.0.6");
        assert_eq!(parse_hosts(&text), vec!["bastion", "gpu"]);

        assert!(managed.remove("gpu"));
        assert!(!managed.remove("gpu"));
        assert!(managed.get("gpu").is_none());
    }

    #[test]
    fn the_include_leads_the_config_and_is_added_only_once() {
        let root = temp_root("include");
        let config = root.join("config");
        fs::write(&config, "Host mine\n  HostName mine.example\n").unwrap();
        let managed = managed_path(&config);
        assert!(managed.ends_with("config.d/muxloom.conf"));

        write_private(
            &managed,
            &ManagedHosts::parse("Host gpu\n  HostName 10.0.0.5\n").render(),
        )
        .unwrap();
        assert!(ensure_include(&config, &managed).unwrap());
        let text = fs::read_to_string(&config).unwrap();
        // An Include below a Host line would only apply to that host, so it
        // has to lead the file — and the user's own lines must be untouched.
        assert!(text.starts_with("# Added by muxloom"));
        assert_eq!(
            text.lines().nth(1).unwrap(),
            "Include config.d/muxloom.conf"
        );
        assert!(text.contains("Host mine\n  HostName mine.example\n"));

        // Idempotent: a second pass finds its own line and changes nothing.
        assert!(!ensure_include(&config, &managed).unwrap());
        assert_eq!(fs::read_to_string(&config).unwrap(), text);

        // The alias now reads back through the include, attributed to the
        // managed file, which is what lets muxloom refuse to shadow a user's.
        assert_eq!(load_hosts(&config).unwrap(), ["gpu", "mine"]);
        assert!(defined_outside(&config, "gpu").unwrap().is_empty());
        assert_eq!(
            defined_outside(&config, "mine").unwrap(),
            [normalize(&config)]
        );

        // A config that does not exist yet gets one that only has the include.
        let fresh = temp_root("include-fresh").join("config");
        assert!(ensure_include(&fresh, &managed_path(&fresh)).unwrap());
        assert!(
            fs::read_to_string(&fresh)
                .unwrap()
                .contains("Include config.d/muxloom.conf")
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_include_written_by_hand_counts_as_already_there() {
        let root = temp_root("include-by-hand");
        let config = root.join("config");
        let managed = managed_path(&config);
        // Absolute, and pointing at a file that does not exist yet: still the
        // same file, so muxloom must not add a second line for it.
        fs::write(
            &config,
            format!("Include {}\nHost mine\n", managed.display()),
        )
        .unwrap();
        assert!(!ensure_include(&config, &managed).unwrap());

        // A glob over the directory covers it too, once the file is there.
        fs::write(&config, "Include config.d/*.conf\n").unwrap();
        write_private(&managed, "Host gpu\n").unwrap();
        assert!(!ensure_include(&config, &managed).unwrap());

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn writes_land_whole_and_stay_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("write");
        let managed = root.join("config.d/muxloom.conf");
        write_private(&managed, "Host gpu\n").unwrap();
        let mode = fs::metadata(&managed).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a fresh SSH file must not be world-readable");

        // An existing file keeps the mode its owner chose.
        fs::set_permissions(&managed, fs::Permissions::from_mode(0o640)).unwrap();
        write_private(&managed, "Host gpu\nHost cpu\n").unwrap();
        assert_eq!(
            fs::metadata(&managed).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            fs::read_to_string(&managed).unwrap(),
            "Host gpu\nHost cpu\n"
        );
        // Nothing temporary is left behind next to it.
        let leftovers: Vec<_> = fs::read_dir(managed.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".muxloom-"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files must be renamed away");

        fs::remove_dir_all(root).unwrap();
    }
}
