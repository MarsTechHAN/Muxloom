use std::{
    collections::{BTreeSet, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

/// Returns concrete aliases from `Host` directives. Wildcard patterns are
/// configuration rules, not connectable destinations, so they are omitted.
pub fn load_hosts(path: &Path) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut hosts = BTreeSet::new();
    let mut visited = HashSet::new();
    load_file(path, &mut visited, &mut hosts)?;
    Ok(hosts.into_iter().collect())
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
    hosts: &mut BTreeSet<String>,
) -> Result<()> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return Ok(());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read SSH config {}", path.display()))?;
    hosts.extend(parse_hosts(&text));

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

fn expand_include(source: &Path, pattern: &str) -> Vec<PathBuf> {
    let expanded = if pattern == "~" || pattern.starts_with("~/") {
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
    };
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

        fs::remove_dir_all(root).unwrap();
    }
}
