//! Cobra `__complete` fallback (IRIS-style): short timeout, isolated session, LRU cache.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use crate::process;
use crate::query::matches_query;
use crate::runtime::Suggestion;

const TIMEOUT: Duration = Duration::from_millis(300);
const MAX_RESULTS: usize = 50;
const MAX_CACHED: usize = 48;

struct CacheEntry {
    mtime: Option<SystemTime>,
    suggestions: Vec<Suggestion>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, CacheEntry>,
    order: VecDeque<String>,
}

impl Cache {
    fn touch(&mut self, key: &str) {
        let Some(pos) = self.order.iter().position(|existing| existing == key) else {
            return;
        };
        if pos + 1 == self.order.len() {
            return;
        }
        let key = self.order.remove(pos).expect("index from position()");
        self.order.push_back(key);
    }

    fn get_fresh(&mut self, key: &str, mtime: Option<SystemTime>) -> Option<&[Suggestion]> {
        match self.entries.get(key).map(|entry| entry.mtime == mtime) {
            Some(true) => {
                self.touch(key);
                self.entries.get(key).map(|entry| entry.suggestions.as_slice())
            },
            Some(false) => {
                self.entries.remove(key);
                if let Some(pos) = self.order.iter().position(|existing| existing == key) {
                    self.order.remove(pos);
                }
                None
            },
            None => None,
        }
    }

    fn insert(&mut self, key: String, entry: CacheEntry) {
        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), entry);
            self.touch(&key);
            return;
        }
        if self.entries.len() >= MAX_CACHED {
            while let Some(old) = self.order.pop_front() {
                if self.entries.remove(&old).is_some() {
                    break;
                }
            }
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, entry);
    }
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(Cache::default()));

pub fn complete(tokens: &[String], cwd: &str, fuzzy: bool) -> Vec<Suggestion> {
    let Some(argv0) = tokens.first() else {
        return Vec::new();
    };
    let Some(binary) = resolve_binary(argv0, cwd) else {
        return Vec::new();
    };
    let mtime = std::fs::metadata(&binary).and_then(|meta| meta.modified()).ok();
    let rest = if tokens.len() > 1 { &tokens[1..] } else { &[] };
    let key = cache_key(&binary, rest);

    if let Ok(mut cache) = CACHE.lock() {
        if let Some(suggestions) = cache.get_fresh(&key, mtime) {
            return filter_cached(suggestions, tokens, fuzzy);
        }
    }

    let mut args = vec!["__complete".to_string()];
    args.extend(rest.iter().cloned());
    let Some(stdout) = process::try_execute_isolated(&binary.display().to_string(), &args, cwd, TIMEOUT) else {
        return Vec::new();
    };
    let suggestions = parse_complete_output(&stdout);
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(
            key,
            CacheEntry {
                mtime,
                suggestions: suggestions.clone(),
            },
        );
    }
    filter_cached(&suggestions, tokens, fuzzy)
}

fn filter_cached(suggestions: &[Suggestion], tokens: &[String], fuzzy: bool) -> Vec<Suggestion> {
    if tokens.len() <= 1 {
        return suggestions.to_vec();
    }
    let query = tokens.last().map_or("", String::as_str);
    if query.is_empty() {
        return suggestions.to_vec();
    }
    suggestions
        .iter()
        .filter(|item| matches_query(&item.name, query, fuzzy))
        .cloned()
        .collect()
}

fn cache_key(binary: &Path, rest: &[String]) -> String {
    format!("{}|{}", binary.display(), rest.join("\u{1f}"))
}

fn resolve_binary(name: &str, cwd: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if name.contains('/') || path.is_absolute() {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            Path::new(cwd).join(path)
        };
        return resolved.is_file().then_some(resolved);
    }
    // The terminal's `PATH`, not the desktop app's: a binary the user can run
    // is usually not in the system directories LaunchServices starts us with.
    let search = process::search_path()?;
    for dir in std::env::split_paths(&search) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn parse_complete_output(stdout: &str) -> Vec<Suggestion> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('_') && *line != ":4" && *line != ":0")
        .filter_map(|line| {
            // Cobra completion directives look like `:4` on the last line.
            if line.starts_with(':') && line[1..].chars().all(|ch| ch.is_ascii_digit()) {
                return None;
            }
            let (name, description) = line.split_once('\t').unwrap_or((line, ""));
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some(Suggestion::new(name, description.trim(), "arg").with_insert_value(name))
        })
        .take(MAX_RESULTS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn parses_tab_separated_and_plain_lines() {
        let suggestions = parse_complete_output("alpha\tfirst\nbeta\n:4\n");
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].name, "alpha");
        assert_eq!(suggestions[0].description, "first");
        assert_eq!(suggestions[1].name, "beta");
    }

    #[test]
    fn probes_fake_binary() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fakecobra");
        fs::write(&bin, "#!/bin/sh\nprintf 'feature-x\tbranch\\nfeature-y\\n:4\\n'\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let cwd = dir.path().display().to_string();
        let suggestions = complete(&[bin.display().to_string(), "fea".into()], &cwd, false);
        let names: Vec<_> = suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"feature-x"), "{names:?}");
        assert!(names.contains(&"feature-y"), "{names:?}");
    }

    #[test]
    fn filters_last_token_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fakecobra");
        fs::write(&bin, "#!/bin/sh\nprintf 'alpha\\nbeta\\n:4\\n'\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let cwd = dir.path().display().to_string();
        let suggestions = complete(&[bin.display().to_string(), "al".into()], &cwd, false);
        let names: Vec<_> = suggestions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha"]);
    }

    #[test]
    fn cache_evicts_least_recently_used() {
        let mut cache = Cache::default();
        for i in 0..MAX_CACHED {
            cache.insert(
                format!("k{i}"),
                CacheEntry {
                    mtime: None,
                    suggestions: Vec::new(),
                },
            );
        }
        assert!(cache.get_fresh("k0", None).is_some());
        cache.insert(
            "k-new".into(),
            CacheEntry {
                mtime: None,
                suggestions: Vec::new(),
            },
        );
        assert!(cache.entries.contains_key("k0"));
        assert!(!cache.entries.contains_key("k1"));
        assert!(cache.entries.contains_key("k-new"));
        assert_eq!(cache.entries.len(), MAX_CACHED);
    }
}
