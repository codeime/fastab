//! Frecency ranking over spec suggestions, plus history command matches.

#[cfg(test)]
use std::cmp::Ordering;
use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::history::ArgSlot;
use crate::query::MatchScore;
use crate::runtime::{CompleteResult, Suggestion};

/// Fig did not cap unique history rows. Keep a high ceiling so ranking cannot
/// drop a recent match the overlay would have shown, without walking an
/// unbounded database on every keystroke.
const HISTORY_LIMIT: usize = 10_000;
const CUSTOM_HISTORY_TIMEOUT: Duration = Duration::from_secs(5);
/// State key used for the native equivalent of the WebView's recency index.
/// The value is a JSON object `{ command: { acceptedName: unixMillis } }`.
pub const ACCEPTANCE_STATE_KEY: &str = "autocomplete.acceptanceRecency";
const MAX_ACCEPTANCES: usize = 2048;
const MAX_ACCEPTANCE_BYTES: usize = 256 * 1024;
const MAX_ACCEPTANCE_ROOT_BYTES: usize = 256;
const MAX_ACCEPTANCE_NAME_BYTES: usize = 1024;
/// Separate from the old root-command index so existing settings remain
/// readable and argument values cannot inherit their global recency.
pub(crate) const SCOPED_ACCEPTANCE_STATE_KEY: &str = "autocomplete.argumentAcceptanceRecencyV1";
const MAX_SCOPED_ACCEPTANCES: usize = 512;
const MAX_SCOPED_NAME_BYTES: usize = 128;
const MAX_SCOPED_STATE_BYTES: usize = 256 * 1024;
const SCOPE_PREFIX: &str = "v1:";

/// Opaque identity carried with an argument row through the UI. The hash is
/// built from the parser's actual argument slot plus the request cwd; no
/// filesystem lookup or canonicalization occurs on the completion path.
pub(crate) fn argument_scope(cwd: &str, slot: &ArgSlot) -> Option<String> {
    if cwd.is_empty()
        || !Path::new(cwd).is_absolute()
        || slot.root.is_empty()
        || slot.path.iter().any(String::is_empty)
        || slot.option.as_ref().is_some_and(String::is_empty)
    {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(b"fastab.argumentAcceptance.v1");
    hash_part(&mut hash, cwd.as_bytes());
    hash_part(&mut hash, slot.root.as_bytes());
    hash.update((slot.path.len() as u64).to_le_bytes());
    for name in &slot.path {
        hash_part(&mut hash, name.as_bytes());
    }
    hash.update([u8::from(slot.option.is_some())]);
    if let Some(option) = &slot.option {
        hash_part(&mut hash, option.as_bytes());
    }
    hash.update((slot.index as u64).to_le_bytes());
    let digest = hash.finalize();
    let mut scope = String::with_capacity(SCOPE_PREFIX.len() + digest.len() * 2);
    scope.push_str(SCOPE_PREFIX);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest.iter().copied() {
        scope.push(HEX[(byte >> 4) as usize] as char);
        scope.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Some(scope)
}

fn hash_part(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn valid_scope(scope: &str) -> bool {
    scope.strip_prefix(SCOPE_PREFIX).is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ScopedAcceptanceIndex {
    entries: HashMap<String, HashMap<String, u64>>,
}

impl ScopedAcceptanceIndex {
    fn load() -> Self {
        let loaded: Self = fastab_settings::state::get_value(SCOPED_ACCEPTANCE_STATE_KEY)
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        let mut bounded = Self::default();
        let mut entries: Vec<_> = loaded
            .entries
            .into_iter()
            .flat_map(|(scope, names)| {
                names
                    .into_iter()
                    .map(move |(name, timestamp)| (scope.clone(), name, timestamp))
            })
            .collect();
        entries.sort_by_key(|(_, _, timestamp)| *timestamp);
        for (scope, name, timestamp) in entries {
            bounded.record_at(&scope, &name, timestamp);
        }
        bounded
    }

    pub(crate) fn timestamp(&self, scope: &str, name: &str) -> Option<u64> {
        self.entries.get(scope).and_then(|names| names.get(name)).copied()
    }

    /// Replaying the same client timestamp is idempotent. When full, an older
    /// queued replay cannot evict a newer choice already kept in memory.
    pub(crate) fn record_at(&mut self, scope: &str, name: &str, timestamp: u64) -> bool {
        if !valid_scope(scope)
            || name.is_empty()
            || name.len() > MAX_SCOPED_NAME_BYTES
            || name.chars().any(char::is_control)
            || matches!(name, "↪" | "../")
        {
            return false;
        }
        if let Some(existing) = self.entries.get_mut(scope).and_then(|names| names.get_mut(name)) {
            *existing = (*existing).max(timestamp);
            return true;
        }
        if self.entries.values().map(HashMap::len).sum::<usize>() >= MAX_SCOPED_ACCEPTANCES {
            let oldest = self
                .entries
                .iter()
                .flat_map(|(scope, names)| names.iter().map(move |(name, timestamp)| (scope, name, timestamp)))
                .min_by_key(|(_, _, timestamp)| *timestamp)
                .map(|(scope, name, timestamp)| (scope.clone(), name.clone(), *timestamp));
            if let Some((old_scope, old_name, oldest_timestamp)) = oldest {
                if timestamp <= oldest_timestamp {
                    return true;
                }
                let names = self.entries.get_mut(&old_scope).expect("oldest scope exists");
                names.remove(&old_name);
                if names.is_empty() {
                    self.entries.remove(&old_scope);
                }
            }
        }
        self.entries
            .entry(scope.to_owned())
            .or_default()
            .insert(name.to_owned(), timestamp);
        true
    }

    pub(crate) fn persist(&self) {
        if let Ok(bytes) = serde_json::to_vec(self)
            && bytes.len() <= MAX_SCOPED_STATE_BYTES
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
        {
            let _ = fastab_settings::state::set_value(SCOPED_ACCEPTANCE_STATE_KEY, value);
        }
    }
}

/// The shell families used by the legacy history source selector.  Keep this
/// deliberately small: Fig only loaded shell history for these three shells;
/// an unknown process fell back to the local database source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryShell {
    Zsh,
    Bash,
    Fish,
    Unknown,
}

/// Inputs which affect the history source.  Runtime keeps one of these beside
/// the frecency cache so changing settings or switching terminals reloads the
/// source once, rather than reading it for every keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistorySourceConfig {
    pub custom_command: Option<String>,
    pub all_shells: bool,
    pub current_shell: HistoryShell,
    pub files: crate::shell_history::HistoryFiles,
}

pub(crate) fn history_source_config(
    custom_command: Option<String>,
    all_shells: bool,
    current_shell: Option<&str>,
    current_process: Option<&str>,
    environment: &[(String, String)],
) -> HistorySourceConfig {
    let shell = normalize_history_shell(current_shell);
    let current_shell = if shell != HistoryShell::Unknown {
        shell
    } else {
        normalize_history_shell(current_process)
    };
    let files = if custom_command.is_some() {
        crate::shell_history::HistoryFiles::default()
    } else {
        crate::shell_history::HistoryFiles::resolve(current_shell, all_shells, environment)
    };
    HistorySourceConfig {
        custom_command,
        all_shells,
        current_shell,
        files,
    }
}

pub(crate) fn normalize_history_shell(value: Option<&str>) -> HistoryShell {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return HistoryShell::Unknown;
    };
    let value = value
        .strip_suffix(" (fterm)")
        .or_else(|| value.strip_suffix(" (fastabterm)"))
        .or_else(|| value.strip_suffix(" (figterm)"))
        .or_else(|| value.strip_suffix(" (ecterm)"))
        .or_else(|| value.strip_suffix(" (qterm)"))
        .unwrap_or(value);
    let value = value.rsplit('/').next().unwrap_or(value);
    match value {
        "zsh" => HistoryShell::Zsh,
        "bash" => HistoryShell::Bash,
        "fish" => HistoryShell::Fish,
        _ => HistoryShell::Unknown,
    }
}

#[derive(Debug, Clone, Default)]
pub struct Frecency {
    /// Occurrence order, including repeats. Identical command text shares one
    /// `Arc<str>`; the `history` template reads this same list.
    lines: Arc<Vec<Arc<str>>>,
    /// Next-token counts keyed by exact prefixes, including irregular spacing.
    next_words: NextWords,
}

#[derive(Debug, Clone, Default)]
struct NextWords {
    // Node indices make both destruction and diagnostics iterative, even for
    // a history line with tens of thousands of tokens.
    nodes: Vec<WordNode>,
    words: HashMap<String, usize>,
}

#[derive(Debug, Clone, Default)]
struct WordNode {
    // Edges split only at ASCII spaces, retaining empty segments, tabs and
    // Unicode whitespace verbatim instead of copying each full prefix.
    children: HashMap<String, usize>,
    counts: HashMap<usize, usize>,
}

#[derive(Clone, Copy)]
struct NextWordCounts<'a> {
    words: &'a HashMap<String, usize>,
    counts: Option<&'a HashMap<usize, usize>>,
}

impl NextWordCounts<'_> {
    fn get(&self, word: &str) -> Option<&usize> {
        self.counts?.get(self.words.get(word)?)
    }

    #[cfg(test)]
    fn to_owned_map(self) -> HashMap<String, usize> {
        self.words
            .iter()
            .filter_map(|(word, id)| self.counts?.get(id).map(|count| (word.clone(), *count)))
            .collect()
    }
}

impl Frecency {
    pub fn from_commands(commands: impl IntoIterator<Item = (String, u64)>) -> Self {
        Self::build(commands, || Ok(())).expect("uncancelled history construction")
    }

    pub(crate) fn try_from_commands(
        commands: impl IntoIterator<Item = (String, u64)>,
    ) -> Result<Self, crate::cancellation::CompletionCancelled> {
        Self::build(commands, crate::cancellation::check)
    }

    fn build(
        commands: impl IntoIterator<Item = (String, u64)>,
        mut check: impl FnMut() -> Result<(), crate::cancellation::CompletionCancelled>,
    ) -> Result<Self, crate::cancellation::CompletionCancelled> {
        let mut lines = Vec::new();
        let mut intern: HashSet<Arc<str>> = HashSet::new();
        let mut next_words = NextWords::default();
        for (command, _) in commands {
            check()?;
            let command = command.trim();
            if command.is_empty() {
                continue;
            }
            let line = if let Some(existing) = intern.get(command) {
                Arc::clone(existing)
            } else {
                let line: Arc<str> = Arc::from(command);
                intern.insert(Arc::clone(&line));
                line
            };
            next_words.record(command, &mut check)?;
            lines.push(line);
        }
        check()?;
        Ok(Self {
            lines: Arc::new(lines),
            next_words,
        })
    }

    /// Commands that finished after this list was built. The history store
    /// keeps the old list until the next request rebinds it.
    pub(crate) fn append_commands(&mut self, commands: impl IntoIterator<Item = String>) {
        let lines = Arc::make_mut(&mut self.lines);
        for command in commands {
            let command = command.trim();
            if command.is_empty() {
                continue;
            }
            let _ = self.next_words.record(command, &mut || Ok(()));
            lines.push(Arc::from(command));
        }
    }

    pub(crate) fn command_lines(&self) -> Arc<Vec<Arc<str>>> {
        Arc::clone(&self.lines)
    }

    pub(crate) fn next_words_diagnostics(&self) -> (usize, usize, usize) {
        self.next_words.diagnostics()
    }

    pub fn history_suggestions(&self, query: &str, fuzzy: bool, include_empty_query: bool) -> Vec<Suggestion> {
        if query.is_empty() && !include_empty_query {
            return Vec::new();
        }
        // Count raw entries before query filtering or deduplication. Repeated
        // identical commands are what make the old history row more frequent.
        let first_word_counts = self.next_word_counts("");
        let mut seen = HashSet::new();
        self.lines
            .iter()
            .rev()
            .filter(|command| query.is_empty() || crate::query::matches_query(command, query, fuzzy))
            .filter_map(|command| {
                let command = command.trim_end();
                seen.insert(command.to_string()).then_some(command)
            })
            .take(HISTORY_LIMIT)
            .map(|command| {
                let priority = first_word_counts
                    .get(command.split_whitespace().next().unwrap_or_default())
                    .copied()
                    .map_or(50, history_priority_base);
                Suggestion::new(command, "past command", "history")
                    .with_insert_value(command)
                    .with_priority(priority)
            })
            .collect()
    }

    /// Return history lines after a command prefix.  The WebView's full
    /// history generator strips the already-typed prefix before presenting a
    /// row; the raw insert value is that same suffix, so accepting `git co`
    /// inserts `checkout ...`, never `git git ...`.
    pub(crate) fn history_suffix_suggestions(&self, prefix: &str, query: &str, fuzzy: bool) -> Vec<Suggestion> {
        let first_word_counts = self.next_word_counts(prefix.trim_end());
        let mut seen = HashSet::new();

        self.lines
            .iter()
            .rev()
            .filter_map(|command| {
                if !command.starts_with(prefix) {
                    return None;
                }
                let suffix = command[prefix.len()..].trim_end();
                if suffix.is_empty() || (!query.is_empty() && !crate::query::matches_query(suffix, query, fuzzy)) {
                    return None;
                }
                seen.insert(suffix.to_string()).then_some(suffix.to_string())
            })
            .take(HISTORY_LIMIT)
            .map(|suffix| {
                let first_word = suffix.split_whitespace().next().unwrap_or_default();
                let priority = first_word_counts
                    .get(first_word)
                    .copied()
                    .map_or(50, history_priority_base);
                Suggestion::new(suffix.clone(), "past command", "history")
                    .with_insert_value(suffix)
                    .with_priority(priority)
            })
            .collect()
    }

    fn next_word_counts(&self, prefix: &str) -> NextWordCounts<'_> {
        self.next_words.counts(prefix)
    }
}

impl NextWords {
    fn counts(&self, prefix: &str) -> NextWordCounts<'_> {
        let missing = NextWordCounts {
            words: &self.words,
            counts: None,
        };
        let Some(mut node) = self.nodes.first() else {
            return missing;
        };
        if !prefix.is_empty() {
            for segment in prefix.split(' ') {
                match node.children.get(segment) {
                    Some(child) => node = &self.nodes[*child],
                    None => return missing,
                }
            }
        }
        NextWordCounts {
            words: &self.words,
            counts: Some(&node.counts),
        }
    }

    fn word_id(&mut self, word: &str) -> usize {
        if let Some(id) = self.words.get(word) {
            return *id;
        }
        let id = self.words.len();
        self.words.insert(word.to_string(), id);
        id
    }

    fn child(&mut self, parent: usize, segment: &str) -> usize {
        if let Some(child) = self.nodes[parent].children.get(segment) {
            return *child;
        }
        let child = self.nodes.len();
        self.nodes.push(WordNode::default());
        self.nodes[parent].children.insert(segment.to_string(), child);
        child
    }

    /// Scan characters once to find Unicode whitespace words and ASCII-space
    /// prefix boundaries, then advance both cursors monotonically. This keeps
    /// the original `suffix.split_whitespace().next()` meaning without
    /// rescanning long whitespace runs or revisiting a prefix from the root.
    fn record(
        &mut self,
        command: &str,
        check: &mut impl FnMut() -> Result<(), crate::cancellation::CompletionCancelled>,
    ) -> Result<(), crate::cancellation::CompletionCancelled> {
        let mut words = Vec::new();
        let mut spaces = Vec::new();
        let mut start = None;
        for (position, (offset, character)) in command.char_indices().enumerate() {
            if position % 1024 == 0 {
                check()?;
            }
            if character == ' ' {
                spaces.push(offset);
            }
            if character.is_whitespace() {
                if let Some(start) = start.take() {
                    words.push((start, self.word_id(&command[start..offset])));
                }
            } else {
                start.get_or_insert(offset);
            }
        }
        if let Some(start) = start {
            words.push((start, self.word_id(&command[start..])));
        }
        if self.nodes.is_empty() {
            self.nodes.push(WordNode::default());
        }
        if let Some(&(_, id)) = words.first() {
            *self.nodes[0].counts.entry(id).or_default() += 1;
        }
        let mut node = 0;
        let mut segment_start = 0;
        let mut word = 0;
        for space in spaces {
            check()?;
            node = self.child(node, &command[segment_start..space]);
            segment_start = space + 1;
            while word < words.len() && words[word].0 <= space {
                word += 1;
            }
            if let Some(&(_, id)) = words.get(word) {
                *self.nodes[node].counts.entry(id).or_default() += 1;
            }
        }
        Ok(())
    }

    fn diagnostics(&self) -> (usize, usize, usize) {
        let mut entries = 0;
        let mut bytes = self.nodes.capacity() * std::mem::size_of::<WordNode>()
            + self.words.capacity() * std::mem::size_of::<(String, usize)>()
            + self.words.keys().map(String::capacity).sum::<usize>();
        for node in &self.nodes {
            entries += node.counts.len();
            bytes += node.children.capacity() * std::mem::size_of::<(String, usize)>()
                + node.counts.capacity() * std::mem::size_of::<(usize, usize)>();
            bytes += node.children.keys().map(String::capacity).sum::<usize>();
        }
        (self.nodes.len(), entries, bytes)
    }
}

/// Recent acceptance data used by the old WebView `updatePriorities` helper.
///
/// This intentionally stores the command and accepted spelling as separate
/// keys.  A `git add` acceptance must not promote an identically named `docker
/// add`, and aliases remain isolated by their primary spelling just as the
/// WebView's `makeArray(name)[0]` lookup was.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AcceptanceIndex {
    entries: HashMap<String, HashMap<String, u64>>,
    #[serde(skip)]
    scoped: ScopedAcceptanceIndex,
}

impl AcceptanceIndex {
    pub(crate) fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
    }

    pub fn load() -> Self {
        let loaded: Self = fastab_settings::state::get_value(ACCEPTANCE_STATE_KEY)
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        let (mut index, pruned) = Self::prune_loaded(loaded);
        if pruned {
            index.persist();
        }
        index.scoped = ScopedAcceptanceIndex::load();
        index
    }

    /// Consume old allocations instead of cloning an unbounded legacy map.
    /// Only the newest count-limited candidates survive until the byte pass.
    fn prune_loaded(loaded: Self) -> (Self, bool) {
        let Self { entries, scoped } = loaded;
        let mut newest = BTreeSet::new();
        let mut timestamps = HashMap::<(Arc<str>, Arc<str>), u64>::new();
        let mut original_count = 0usize;
        let mut pruned = false;
        for (command, names) in entries {
            original_count = original_count.saturating_add(names.len());
            pruned |= names.is_empty();
            let root = command.trim();
            pruned |= root != command;
            if root.is_empty() || root.len() > MAX_ACCEPTANCE_ROOT_BYTES {
                pruned = true;
                continue;
            }
            let root: Arc<str> = Arc::from(root);
            for (name, timestamp) in names {
                let trimmed = name.trim();
                if !valid_acceptance(&root, trimmed) {
                    pruned = true;
                    continue;
                }
                pruned |= trimmed != name;
                // Normalize before limiting candidates: alternate whitespace
                // spellings of one key must not displace distinct acceptances.
                // Both bounded indexes share the strings, never the old maps.
                match timestamps.entry((root.clone(), Arc::from(trimmed))) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        pruned = true;
                        if timestamp > *entry.get() {
                            let (root, name) = entry.key();
                            newest.remove(&(*entry.get(), root.clone(), name.clone()));
                            newest.insert((timestamp, root.clone(), name.clone()));
                            entry.insert(timestamp);
                        }
                    },
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let (root, name) = entry.key();
                        newest.insert((timestamp, root.clone(), name.clone()));
                        entry.insert(timestamp);
                        if newest.len() > MAX_ACCEPTANCES {
                            let (_, root, name) = newest.pop_first().expect("candidate set is full");
                            timestamps.remove(&(root, name));
                            pruned = true;
                        }
                    },
                }
            }
        }
        drop(timestamps);
        let mut bounded = Self {
            scoped,
            ..Self::default()
        };
        for (timestamp, command, name) in newest.into_iter().rev() {
            bounded.record_at(&command, &name, timestamp);
        }
        pruned |= bounded.entry_count() != original_count;
        (bounded, pruned)
    }

    fn entry_count(&self) -> usize {
        self.entries.values().map(HashMap::len).sum()
    }

    fn estimated_bytes(&self) -> usize {
        self.entries.iter().fold(0usize, |total, (command, names)| {
            names
                .keys()
                .fold(total.saturating_add(acceptance_root_bytes(command)), |total, name| {
                    total.saturating_add(acceptance_name_bytes(name))
                })
        })
    }

    fn fits_new_entry(&self, command: &str, name: &str) -> bool {
        let root_bytes = if self.entries.contains_key(command) {
            0
        } else {
            acceptance_root_bytes(command)
        };
        self.entry_count() < MAX_ACCEPTANCES
            && self.estimated_bytes() + root_bytes + acceptance_name_bytes(name) <= MAX_ACCEPTANCE_BYTES
    }

    /// Budget the new item alongside every item it is too old to evict before
    /// making any removals. A large delayed replay must not partially prune
    /// the index and then discover that it cannot fit without losing new data.
    fn fits_with_newer_entries(&self, command: &str, name: &str, timestamp: u64) -> bool {
        let mut count = 1usize;
        let mut bytes = acceptance_root_bytes(command) + acceptance_name_bytes(name);
        for (root, names) in &self.entries {
            let mut charged_root = root == command;
            for (name, accepted) in names {
                if *accepted < timestamp {
                    continue;
                }
                if !charged_root {
                    bytes += acceptance_root_bytes(root);
                    charged_root = true;
                }
                count += 1;
                bytes += acceptance_name_bytes(name);
                if count > MAX_ACCEPTANCES || bytes > MAX_ACCEPTANCE_BYTES {
                    return false;
                }
            }
        }
        true
    }

    fn remove_oldest(&mut self) {
        let oldest = self
            .entries
            .iter()
            .flat_map(|(command, names)| names.iter().map(move |(name, timestamp)| (command, name, timestamp)))
            .min_by(|left, right| {
                left.2
                    .cmp(right.2)
                    .then_with(|| left.0.cmp(right.0))
                    .then_with(|| left.1.cmp(right.1))
            })
            .map(|(command, name, _)| (command.clone(), name.clone()));
        if let Some((command, name)) = oldest {
            let names = self.entries.get_mut(&command).expect("oldest command exists");
            names.remove(&name);
            if names.is_empty() {
                self.entries.remove(&command);
                self.entries.shrink_to_fit();
            } else {
                names.shrink_to_fit();
            }
        }
    }

    /// Return the last acceptance time in Unix milliseconds for a command and
    /// primary suggestion name.
    pub fn timestamp(&self, command: &str, name: &str) -> Option<u64> {
        self.entries.get(command).and_then(|items| items.get(name)).copied()
    }

    pub(crate) fn scoped_timestamp(&self, scope: &str, name: &str) -> Option<u64> {
        self.scoped.timestamp(scope, name)
    }

    pub(crate) fn record_scoped_at(&mut self, scope: &str, name: &str, timestamp: u64) -> bool {
        self.scoped.record_at(scope, name, timestamp)
    }

    pub(crate) fn scoped_snapshot(&self) -> ScopedAcceptanceIndex {
        self.scoped.clone()
    }

    /// Update the in-memory index with an explicit timestamp.  This pure
    /// operation is useful to the engine and keeps tests independent of the
    /// user's settings database.
    pub fn record_at(&mut self, command: &str, name: &str, timestamp: u64) -> bool {
        let command = command.trim();
        let name = name.trim();
        if !valid_acceptance(command, name) {
            return false;
        }
        if let Some(existing) = self.entries.get_mut(command).and_then(|names| names.get_mut(name)) {
            *existing = (*existing).max(timestamp);
            return true;
        }
        if !self.fits_new_entry(command, name) {
            if !self.fits_with_newer_entries(command, name, timestamp) {
                return true;
            }
            while !self.fits_new_entry(command, name) {
                self.remove_oldest();
            }
        }
        self.entries
            .entry(command.to_string())
            .or_default()
            .insert(name.to_string(), timestamp);
        true
    }

    /// Record an acceptance and persist the complete map to SQLite-backed
    /// state. Persistence failures are deliberately best-effort: accepting a
    /// command must never fail the shell insertion or the completion worker.
    pub fn record(&mut self, command: &str, name: &str) -> bool {
        let timestamp = Self::now_millis();
        if !self.record_at(command, name, timestamp) {
            return false;
        }
        self.persist();
        true
    }

    pub fn persist(&self) {
        if self.entry_count() <= MAX_ACCEPTANCES
            && self.estimated_bytes() <= MAX_ACCEPTANCE_BYTES
            && let Ok(value) = serde_json::to_value(self)
        {
            let _ = fastab_settings::state::set_value(ACCEPTANCE_STATE_KEY, value);
        }
    }

    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (String, String, u64)>) -> Self {
        let mut index = Self::default();
        for (command, name, timestamp) in entries {
            index.record_at(&command, &name, timestamp);
        }
        index
    }
}

fn valid_acceptance(command: &str, name: &str) -> bool {
    !command.is_empty()
        && command.len() <= MAX_ACCEPTANCE_ROOT_BYTES
        && !name.is_empty()
        && name.len() <= MAX_ACCEPTANCE_NAME_BYTES
        && !matches!(name, "↪" | "../")
}

fn acceptance_string_bytes(value: &str) -> usize {
    value
        .bytes()
        .map(|byte| match byte {
            b'"' | b'\\' => 2,
            0..=0x1f => 6,
            _ => 1,
        })
        .sum()
}

// Charge conservative map/string bookkeeping and JSON escaping, including
// room for punctuation and a u64 timestamp. Roots are charged only once.
fn acceptance_root_bytes(command: &str) -> usize {
    256 + acceptance_string_bytes(command)
}

fn acceptance_name_bytes(name: &str) -> usize {
    96 + acceptance_string_bytes(name)
}

fn first_word_count(counts: NextWordCounts<'_>, value: &str) -> usize {
    counts
        .get(value.split_whitespace().next().unwrap_or_default())
        .copied()
        .unwrap_or_default()
}

/// Spellings the WebView walked via `makeArray(suggestion.name)`. Flattening
/// keeps the selected name, the primary spelling, and the rest of `name[]`.
fn suggestion_name_spellings(suggestion: &Suggestion) -> impl Iterator<Item = &str> {
    suggestion
        .alias_names
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(suggestion.name.as_str()))
        .chain(suggestion.primary_name.as_deref())
}

fn suggestion_first_word_count(suggestion: &Suggestion, history_counts: NextWordCounts<'_>) -> usize {
    suggestion_name_spellings(suggestion)
        .map(|name| first_word_count(history_counts, name))
        .max()
        .unwrap_or_default()
}

fn history_priority_base(count: usize) -> i64 {
    if count > 1 { 75 } else { 50 }
}

/// The old value is `75 + min(count, 10) / 10`. Compare in tenths so a
/// frequency of two remains 75.2 instead of being rounded up to 77.
fn history_priority_tenths(count: usize) -> i64 {
    match count {
        0 => 0,
        1 => 500,
        _ => 750 + count.min(10) as i64,
    }
}

#[allow(dead_code)]
pub fn apply(result: &mut CompleteResult, tokens: &[String], frecency: &Frecency) {
    apply_with_acceptance(result, tokens, frecency, &AcceptanceIndex::default(), "", false);
}

/// Rank a result using the same stable match/priority buckets as the legacy
/// WebView.  `alphabetical` disables acceptance recency, but deliberately does
/// not introduce a name tie-break: static lookup supplies category order and
/// stable sorting preserves it when priorities and match scores tie.
pub fn apply_with_acceptance(
    result: &mut CompleteResult,
    tokens: &[String],
    frecency: &Frecency,
    acceptance: &AcceptanceIndex,
    root_command: &str,
    alphabetical: bool,
) {
    let prefix = completion_prefix(tokens, matching_term(result));
    let query = matching_term(result).to_string();
    let history_counts = frecency.next_word_counts(&prefix);
    let root_command = if root_command.is_empty() {
        tokens.first().map(String::as_str).unwrap_or_default()
    } else {
        root_command
    };

    // Cache only for this sort: matching allocates folded/UTF-16 strings,
    // and history/acceptance lookup need only run once per candidate.
    result.suggestions.sort_by_cached_key(|suggestion| {
        // `lookup` already filtered using the request's fuzzy flag. Preserve
        // exact/prefix/fuzzy buckets and each row's query-term override.
        let match_key = if query.is_empty() {
            None
        } else {
            best_match(suggestion, &query)
        }
        .map_or((u8::MAX, Reverse(0)), |score| (score.bucket(), Reverse(score.score)));
        let priority = effective_priority_score(suggestion, history_counts, acceptance, root_command, !alphabetical);
        // This score is finite and nonnegative: bounded integer priority
        // plus an optional u64 timestamp fraction. Its IEEE bits therefore
        // have exactly the same order as the previous f64 comparison.
        (
            suggestion.kind != "auto-execute",
            match_key,
            Reverse(priority.to_bits()),
        )
    });

    // This mirrors the old `deduplicateSuggestions` guard. It intentionally
    // stays off for large result sets so a generator cannot pay an O(n²) cost
    // or lose rows merely because the list is still being streamed.
    if result.suggestions.len() <= 50 {
        let mut deduplicated = Vec::with_capacity(result.suggestions.len());
        for suggestion in result.suggestions.drain(..) {
            if deduplicated.iter().any(|existing: &Suggestion| {
                existing.name == suggestion.name
                    && existing.insert_value == suggestion.insert_value
                    && existing.display_name == suggestion.display_name
                    && existing.args_hint == suggestion.args_hint
            }) {
                continue;
            }
            deduplicated.push(suggestion);
        }
        result.suggestions = deduplicated;
    }
}

#[cfg(test)]
fn compare_priority(
    left: &Suggestion,
    right: &Suggestion,
    history_counts: NextWordCounts<'_>,
    acceptance: &AcceptanceIndex,
    root_command: &str,
    use_acceptance: bool,
) -> Ordering {
    effective_priority_score(left, history_counts, acceptance, root_command, use_acceptance)
        .partial_cmp(&effective_priority_score(
            right,
            history_counts,
            acceptance,
            root_command,
            use_acceptance,
        ))
        .unwrap_or(Ordering::Equal)
}

#[cfg(test)]
fn compare_auto_execute(left: &Suggestion, right: &Suggestion) -> Ordering {
    let left_auto = left.kind == "auto-execute";
    let right_auto = right.kind == "auto-execute";
    right_auto.cmp(&left_auto)
}

fn effective_priority_tenths(suggestion: &Suggestion, history_counts: NextWordCounts<'_>) -> i64 {
    // Runtime constructors already apply `priority || 50` and clamp to
    // 0..100. Do not normalize zero again here: an explicit negative value
    // has already become a meaningful zero by this point.
    let ordinary = suggestion.priority.clamp(0, 100) * 10;
    let history = if suggestion.argument_value {
        0
    } else {
        history_priority_tenths(suggestion_first_word_count(suggestion, history_counts))
    };
    ordinary.max(history)
}

fn effective_priority_score(
    suggestion: &Suggestion,
    history_counts: NextWordCounts<'_>,
    acceptance: &AcceptanceIndex,
    root_command: &str,
    use_acceptance: bool,
) -> f64 {
    let mut priority = effective_priority_tenths(suggestion, history_counts) as f64 / 10.0;

    if use_acceptance && suggestion.kind != "auto-execute" && suggestion.name != "../" && suggestion.name != "↪" {
        let name = suggestion.primary_name.as_deref().unwrap_or(suggestion.name.as_str());
        let timestamp = if suggestion.argument_value {
            suggestion
                .acceptance_scope
                .as_deref()
                .and_then(|scope| acceptance.scoped_timestamp(scope, name))
        } else {
            acceptance.timestamp(root_command, name)
        };
        if let Some(timestamp) = timestamp {
            // updatePriorities promotes priorities in [50, 75] to 75 before
            // adding the millisecond-derived fraction. A timestamp divided
            // by 1e13 is the exact scale used by the old JavaScript helper.
            if (50.0..=75.0).contains(&priority) {
                priority = 75.0;
            }
            priority += timestamp as f64 / 10_000_000_000_000.0;
        }
    }
    priority
}

#[cfg(test)]
fn compare_match(left: &Suggestion, right: &Suggestion, query: &str) -> Ordering {
    let left_score = best_match(left, query);
    let right_score = best_match(right, query);
    match (left_score, right_score) {
        (Some(left), Some(right)) => left
            .bucket()
            .cmp(&right.bucket())
            .then_with(|| right.score.cmp(&left.score)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn best_match(suggestion: &Suggestion, query: &str) -> Option<MatchScore> {
    let query = suggestion.query_term.as_deref().unwrap_or(query);
    let mut best = crate::query::match_score(&suggestion.name, query, true);
    if let Some(display_name) = suggestion.display_name.as_deref()
        && let Some(candidate) = crate::query::match_score(display_name, query, true)
        && best.is_none_or(|current| better_match(candidate, current))
    {
        best = Some(candidate);
    }
    best
}

fn better_match(left: MatchScore, right: MatchScore) -> bool {
    left.bucket() < right.bucket() || (left.bucket() == right.bucket() && left.score > right.score)
}

fn completion_prefix(tokens: &[String], search_term: &str) -> String {
    if tokens.is_empty() || (tokens.len() == 1 && !search_term.is_empty()) {
        return String::new();
    }
    if search_term.is_empty() {
        tokens.join(" ")
    } else {
        tokens[..tokens.len().saturating_sub(1)].join(" ")
    }
}

#[cfg(test)]
pub fn merge_history(result: &mut CompleteResult, tokens: &[String], frecency: &Frecency, fuzzy: bool) {
    merge_history_with_prefix(result, tokens, None, frecency, fuzzy);
}

pub fn merge_history_with_prefix(
    result: &mut CompleteResult,
    tokens: &[String],
    buffer_prefix: Option<String>,
    frecency: &Frecency,
    fuzzy: bool,
) {
    if tokens.is_empty() {
        return;
    }

    let query = matching_term(result);
    let history = if let Some(prefix) = buffer_prefix.or_else(|| history_prefix(tokens, query)) {
        frecency.history_suffix_suggestions(&prefix, query, fuzzy)
    } else {
        // While completing the first token, the old UI keeps the complete
        // command as both the display name and insertion value.
        frecency.history_suggestions(query, fuzzy, false)
    };

    // The WebView filters history against every spelling in a static
    // suggestion's `name` array. The IR keeps the selected spelling plus its
    // primary spelling; those are the alias identities that survive the
    // flattening step (displayName is presentation text, not an alias).
    let static_names: HashSet<&str> = result.suggestions.iter().flat_map(suggestion_name_spellings).collect();
    let fresh: Vec<Suggestion> = history
        .into_iter()
        .filter(|item| !static_names.contains(item.name.as_str()))
        .collect();
    result.suggestions.extend(fresh);
}

fn matching_term(result: &CompleteResult) -> &str {
    if result.match_term.is_empty() {
        result.search_term.as_str()
    } else {
        result.match_term.as_str()
    }
}

pub(crate) fn history_prefix(tokens: &[String], query: &str) -> Option<String> {
    if tokens.is_empty() || (tokens.len() == 1 && !query.is_empty()) {
        return None;
    }
    let prefix = if query.is_empty() {
        tokens.join(" ")
    } else {
        tokens[..tokens.len().saturating_sub(1)].join(" ")
    };
    if prefix.is_empty() {
        None
    } else {
        Some(format!("{prefix} "))
    }
}

/// WebView `getFullHistorySuggestions` slices the original command buffer:
/// `originalNode.startIndex + text.length - innerText.length`. Quotes and
/// doubled spaces stay in the prefix; alias expansion does not.
pub(crate) fn history_prefix_from_buffer(
    command_buffer: &str,
    ends_with_space: bool,
    tokens: &[String],
) -> Option<String> {
    if tokens.is_empty() || (tokens.len() == 1 && !ends_with_space) {
        return None;
    }
    if ends_with_space {
        let mut prefix = command_buffer.to_string();
        if !prefix.ends_with(' ') {
            prefix.push(' ');
        }
        return Some(prefix);
    }
    let raw = crate::lookup::current_token_raw(command_buffer);
    let inner = tokens.last().map(String::as_str).unwrap_or_default();
    if command_buffer.len() < raw.len() {
        return history_prefix(tokens, inner);
    }
    let start = command_buffer.len() - raw.len();
    // Quotes are part of the original node text and stay in the prefix.
    // Backslash-escaped spaces are not: copying `text.length - innerText`
    // would swallow the first character of the token and miss history rows.
    let prefix_end = if raw.starts_with(['\'', '"']) || raw.starts_with("$'") {
        start.saturating_add(raw.len().saturating_sub(inner.len()))
    } else {
        start
    };
    let prefix = command_buffer.get(..prefix_end.min(command_buffer.len()))?;
    if prefix.is_empty() {
        None
    } else {
        Some(prefix.to_string())
    }
}

/// Load the complete local history database.  This remains public for the
/// headless/diagnostic callers; the engine uses [`load_commands_for`] so the
/// selected source follows the legacy custom-command and shell settings.
#[allow(dead_code)]
pub fn load_commands() -> Vec<(String, u64)> {
    load_commands_for(&HistorySourceConfig {
        custom_command: None,
        all_shells: true,
        current_shell: HistoryShell::Unknown,
        files: crate::shell_history::HistoryFiles::default(),
    })
}

/// Load history for the source selected by the current request and settings.
///
/// A configured custom command is run through the same default zsh path as
/// the old WebView implementation. Its output wins when it is non-empty; a
/// failure, timeout, or empty output falls back to the local history
/// database, both capped at the recent window. Without a custom command,
/// known shells read their own history files (the current shell's, or
/// zsh+bash when `allShells` is enabled), plus the database rows newer than
/// each file, and fall back to the database rows of those shells; unknown
/// shells use the database source.
pub(crate) fn load_commands_for(config: &HistorySourceConfig) -> Vec<(String, u64)> {
    if let Some(command) = config.custom_command.as_deref() {
        if let Some(commands) = run_custom_history(command) {
            return history_commands_from_output(&commands);
        }
        return load_database_commands(|_| true);
    }

    let mut from_shell = Vec::new();
    let mut read = |shell| {
        let commands = crate::shell_history::commands(&config.files, shell, MAX_DATABASE_HISTORY_COMMANDS);
        if commands.is_empty() {
            return;
        }
        from_shell.extend(commands.into_iter().map(|command| (command, 0)));
        let written = crate::shell_history::modified(&config.files, shell);
        from_shell.extend(unflushed_database_commands(
            &fastab_settings::history::History::new(),
            shell,
            written,
        ));
    };
    match config.current_shell {
        HistoryShell::Zsh | HistoryShell::Bash | HistoryShell::Fish => read(config.current_shell),
        HistoryShell::Unknown => {},
    }
    if config.all_shells {
        if config.current_shell != HistoryShell::Zsh {
            read(HistoryShell::Zsh);
        }
        if config.current_shell != HistoryShell::Bash {
            read(HistoryShell::Bash);
        }
    }
    if from_shell.is_empty() {
        match config.current_shell {
            HistoryShell::Unknown => load_database_commands(|_| true),
            _ => load_database_commands(|row_shell| includes_database_shell(config, *row_shell)),
        }
    } else {
        from_shell
    }
}

/// Commands of `shell` that started after its history file was last written.
/// zsh and bash write the file when a shell exits by default, so those of a
/// shell still open are only in the database fterm writes after each command.
fn unflushed_database_commands(
    history: &fastab_settings::history::History,
    shell: HistoryShell,
    written: Option<SystemTime>,
) -> Vec<(String, u64)> {
    use fastab_settings::history::{HistoryColumn, Order, OrderBy};
    let Some(written) = written else {
        return Vec::new();
    };
    let mut rows = history
        .command_rows(
            vec![OrderBy::new(HistoryColumn::Id, Order::Desc)],
            MAX_NEW_DATABASE_ROWS,
            0,
        )
        .unwrap_or_default();
    rows.reverse();
    rows.into_iter()
        .filter(|row| normalize_history_shell(row.shell.as_deref()) == shell)
        .filter(|row| row.start_time.is_some_and(|started| started > written))
        .filter_map(|row| Some((non_empty_history_command(row.command.as_deref()?)?, 0)))
        .collect()
}

/// The database row new commands are read after, for a source that follows
/// the database. A custom history command replaces it.
pub(crate) fn database_cursor(config: &HistorySourceConfig) -> Option<i64> {
    if config.custom_command.is_some() {
        return None;
    }
    fastab_settings::history::History::new().latest_id().ok()
}

/// Commands fterm recorded after row `after`, and the row to continue from.
pub(crate) fn new_database_commands(config: &HistorySourceConfig, after: i64) -> Option<(Vec<String>, i64)> {
    new_commands_from(&fastab_settings::history::History::new(), config, after)
}

fn new_commands_from(
    history: &fastab_settings::history::History,
    config: &HistorySourceConfig,
    after: i64,
) -> Option<(Vec<String>, i64)> {
    let rows = history.command_rows_after(after, MAX_NEW_DATABASE_ROWS).ok()?;
    let last = rows.last().map_or(after, |(id, _)| *id);
    let commands = rows
        .into_iter()
        .filter(|(_, row)| includes_database_shell(config, normalize_history_shell(row.shell.as_deref())))
        .filter_map(|(_, row)| non_empty_history_command(row.command.as_deref()?))
        .collect();
    Some((commands, last))
}

fn includes_database_shell(config: &HistorySourceConfig, row_shell: HistoryShell) -> bool {
    match config.current_shell {
        HistoryShell::Unknown => true,
        HistoryShell::Zsh | HistoryShell::Bash if config.all_shells => {
            matches!(row_shell, HistoryShell::Zsh | HistoryShell::Bash)
        },
        current => row_shell == current,
    }
}

fn non_empty_history_command(command: &str) -> Option<String> {
    (!command.trim().is_empty()).then(|| command.to_string())
}

fn history_commands_from_output(output: &str) -> Vec<(String, u64)> {
    let mut commands: Vec<(String, u64)> = output
        .lines()
        .filter_map(non_empty_history_command)
        .map(|command| (command, 0))
        .collect();
    // Custom commands list oldest-first, so the recent window is the tail.
    if commands.len() > MAX_DATABASE_HISTORY_COMMANDS {
        let start = commands.len() - MAX_DATABASE_HISTORY_COMMANDS;
        commands = commands.split_off(start);
    }
    commands
}

/// Cap on how much shell history feeds frecency and history suggestions.
///
/// The engine reloads this after every watchdog reset, and ranking walks the
/// loaded list per request, so an unbounded scan made both scale with the
/// lifetime size of the history database and with `fc` / `history` output.
/// Recent commands are the only ones frecency meaningfully weights anyway.
const MAX_DATABASE_HISTORY_COMMANDS: usize = 10_000;
/// Rows read at once when following the database.
const MAX_NEW_DATABASE_ROWS: usize = 1_000;
/// Loaded plus appended commands before a reload trims them to the recent window.
pub(crate) const MAX_LOADED_HISTORY_COMMANDS: usize = 2 * MAX_DATABASE_HISTORY_COMMANDS;

fn load_database_commands<F>(include: F) -> Vec<(String, u64)>
where
    F: Fn(&HistoryShell) -> bool,
{
    use fastab_settings::history::{HistoryColumn, Order, OrderBy};
    let history = fastab_settings::history::History::new();
    // Newest first via the integer primary key (start_time is unindexed),
    // then restored to chronological order, which history_suggestions
    // depends on to surface the most recent match.
    let mut rows = history
        .command_rows(
            vec![OrderBy::new(HistoryColumn::Id, Order::Desc)],
            MAX_DATABASE_HISTORY_COMMANDS,
            0,
        )
        .unwrap_or_default();
    rows.reverse();
    rows.into_iter()
        .filter_map(|row| {
            let shell = normalize_history_shell(row.shell.as_deref());
            if !include(&shell) {
                return None;
            }
            let command = non_empty_history_command(row.command.as_deref()?)?;
            let ts = row
                .start_time
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            Some((command, ts))
        })
        .collect()
}

/// Execute the configured history command without tying up the desktop event
/// loop. This function runs inside the engine attempt worker, and also has its
/// own legacy five-second deadline so a shell command cannot consume the
/// worker watchdog indefinitely.
fn run_custom_history(command: &str) -> Option<String> {
    // Killing only `zsh` can leave a background descendant holding stdout
    // open forever. The shared runner isolates and kills the whole process
    // group, drains output without blocking, and always reaps the shell.
    let output = crate::process::try_execute_isolated_success(
        "zsh",
        &["-c".into(), command.into()],
        "",
        CUSTOM_HISTORY_TIMEOUT,
    )?;
    (!output.trim().is_empty()).then_some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_ranking_matches_comparator_for_unicode_aliases_scopes_and_stable_ties() {
        let frecency = Frecency::from_commands([
            ("git co one".into(), 10),
            ("git co two".into(), 20),
            ("git status".into(), 30),
        ]);
        let scope = argument_scope(
            "/project",
            &ArgSlot {
                root: "git".into(),
                path: vec!["checkout".into()],
                option: None,
                index: 0,
            },
        )
        .unwrap();
        let mut acceptance = AcceptanceIndex::from_entries([
            ("git".into(), "checkout".into(), 2_000_000_000_000),
            ("git".into(), "status".into(), u64::MAX),
        ]);
        acceptance.record_scoped_at(&scope, "ΟΣ", 3_000_000_000_000);
        let mut rows = Vec::new();
        for priority in [0, 49, 50, 75, 76, 100] {
            for name in [
                "checkout",
                "co",
                "commit",
                "status",
                "ΟΣ",
                "οσ",
                "İ",
                "../",
                "↪",
                "unmatched",
            ] {
                let mut row = Suggestion::new(name, priority.to_string(), "subcommand");
                row.priority = priority;
                if name == "co" {
                    row.primary_name = Some("checkout".into());
                    row.alias_names = vec!["checkout".into(), "co".into()];
                    row.query_term = Some("co".into());
                } else if name == "commit" {
                    row.display_name = Some("co".into());
                } else if name == "ΟΣ" {
                    row.argument_value = true;
                    row.acceptance_scope = Some(scope.clone());
                } else if name == "↪" {
                    row.kind = "auto-execute".into();
                } else if name == "unmatched" {
                    row.query_term = Some(String::new());
                }
                rows.push(row);
            }
        }
        // More than 50 rows keeps deduplication off, so every stable tie is
        // checked, including differently-described rows with identical names.
        for query in ["", "co", "s", "ΟΣ", "i", "unmatched"] {
            for alphabetical in [false, true] {
                let tokens = vec!["git".into(), query.into()];
                let counts = frecency.next_word_counts(&completion_prefix(&tokens, query));
                let mut expected = rows.clone();
                expected.sort_by(|left, right| {
                    compare_auto_execute(left, right)
                        .then_with(|| {
                            if query.is_empty() {
                                Ordering::Equal
                            } else {
                                compare_match(left, right, query)
                            }
                        })
                        .then_with(|| compare_priority(right, left, counts, &acceptance, "git", !alphabetical))
                });
                let mut result = CompleteResult {
                    suggestions: rows.clone(),
                    search_term: query.into(),
                    match_term: query.into(),
                    ..CompleteResult::default()
                };
                apply_with_acceptance(&mut result, &tokens, &frecency, &acceptance, "git", alphabetical);
                assert_eq!(
                    result.suggestions, expected,
                    "query={query:?}, alphabetical={alphabetical}"
                );
            }
        }
    }

    #[test]
    fn equal_match_priority_keeps_input_order_without_shell_frecency_tie_break() {
        let frecency = Frecency::from_commands([("checkout".into(), 900), ("cherry-pick".into(), 1)]);
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("cherry-pick", "Apply commits", "subcommand"),
                Suggestion::new("checkout", "Switch branches", "subcommand"),
            ],
            search_term: "ch".into(),
            match_term: "ch".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply(&mut result, &["ch".into()], &frecency);
        assert_eq!(result.suggestions[0].name, "cherry-pick");
        assert_eq!(result.suggestions[1].name, "checkout");
    }

    #[test]
    fn exact_and_case_insensitive_matches_beat_prefixes() {
        let frecency = Frecency::default();
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("git-status", "", "subcommand"),
                Suggestion::new("Git", "", "subcommand"),
                Suggestion::new("git", "", "subcommand"),
            ],
            search_term: "git".into(),
            match_term: "git".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply(&mut result, &["git".into()], &frecency);
        assert_eq!(
            result
                .suggestions
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["git", "Git", "git-status"]
        );
    }

    #[test]
    fn ranking_uses_each_suggestions_query_term_override() {
        let frecency = Frecency::default();
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("scope@foobar", "", "arg"),
                Suggestion::new("foo", "", "arg").with_query_term(Some("foo".into())),
            ],
            search_term: "scope@foo".into(),
            match_term: "scope@foo".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply(&mut result, &["cmd".into(), "scope@foo".into()], &frecency);
        assert_eq!(result.suggestions[0].name, "foo");
        assert_eq!(result.suggestions[1].name, "scope@foobar");
    }

    #[test]
    fn empty_query_keeps_equal_priority_input_order() {
        let frecency = Frecency::from_commands([("git alpha".into(), 10_000), ("git zeta".into(), 1)]);
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("zeta", "", "subcommand"),
                Suggestion::new("alpha", "", "subcommand"),
            ],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply(&mut result, &["git".into()], &frecency);
        assert_eq!(
            result
                .suggestions
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
    }

    #[test]
    fn auto_execute_precedes_clamped_normal_priorities() {
        let frecency = Frecency::default();
        let low = Suggestion::new("low", "", "arg").with_priority(-4);
        let zero = Suggestion::new("zero", "", "arg").with_priority(0);
        let high = Suggestion::new("high", "", "arg").with_priority(900);
        let mut auto = Suggestion::new("↪", "", "auto-execute");
        auto.priority = 0;
        let mut result = CompleteResult {
            suggestions: vec![low, zero, high, auto],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply(&mut result, &[], &frecency);
        assert_eq!(
            result
                .suggestions
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["↪", "high", "zero", "low"]
        );
        assert_eq!(result.suggestions[1].priority, 100);
        assert_eq!(result.suggestions[2].priority, 50);
        assert_eq!(result.suggestions[3].priority, 0);
    }

    #[test]
    fn acceptance_recency_promotes_only_the_matching_root_and_name() {
        let acceptance = AcceptanceIndex::from_entries([
            ("git".into(), "push".into(), 2_000_000_000_000),
            ("docker".into(), "run".into(), 2_000_000_000_000),
        ]);
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("status", "", "subcommand"),
                Suggestion::new("push", "", "subcommand"),
            ],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply_with_acceptance(
            &mut result,
            &["git".into()],
            &Frecency::default(),
            &acceptance,
            "git",
            false,
        );
        assert_eq!(result.suggestions[0].name, "push");

        let mut isolated = CompleteResult {
            suggestions: vec![
                Suggestion::new("run", "", "subcommand"),
                Suggestion::new("status", "", "subcommand"),
            ],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply_with_acceptance(
            &mut isolated,
            &["git".into()],
            &Frecency::default(),
            &acceptance,
            "git",
            false,
        );
        assert_eq!(
            isolated.suggestions[0].name, "run",
            "docker's acceptance must not leak into git"
        );
    }

    #[test]
    fn acceptance_update_is_visible_to_the_next_ranking_request() {
        let mut acceptance = AcceptanceIndex::default();
        let mut before = CompleteResult {
            suggestions: vec![
                Suggestion::new("status", "", "subcommand"),
                Suggestion::new("push", "", "subcommand"),
            ],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply_with_acceptance(
            &mut before,
            &["git".into()],
            &Frecency::default(),
            &acceptance,
            "git",
            false,
        );
        assert_eq!(before.suggestions[0].name, "status");

        acceptance.record_at("git", "push", 2_000_000_000_000);
        let mut after = CompleteResult {
            suggestions: vec![
                Suggestion::new("status", "", "subcommand"),
                Suggestion::new("push", "", "subcommand"),
            ],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply_with_acceptance(
            &mut after,
            &["git".into()],
            &Frecency::default(),
            &acceptance,
            "git",
            false,
        );
        assert_eq!(after.suggestions[0].name, "push");
    }

    #[test]
    fn argument_rows_without_scope_ignore_global_acceptance_and_history_words() {
        let frecency = Frecency::from_commands((0..8).map(|_| ("git checkout zeta".into(), 1_000)));
        let acceptance = AcceptanceIndex::from_entries([("git".into(), "zeta".into(), 3_000_000_000_000)]);
        let mut alpha = Suggestion::new("alpha", "", "arg");
        alpha.argument_value = true;
        let mut zeta = Suggestion::new("zeta", "", "arg");
        zeta.argument_value = true;
        let mut result = CompleteResult {
            suggestions: vec![alpha, zeta],
            ..CompleteResult::default()
        };
        apply_with_acceptance(
            &mut result,
            &["git".into(), "checkout".into()],
            &frecency,
            &acceptance,
            "git",
            false,
        );
        assert_eq!(result.suggestions[0].name, "alpha");

        result.suggestions[1].priority = 90;
        apply_with_acceptance(
            &mut result,
            &["git".into(), "checkout".into()],
            &frecency,
            &acceptance,
            "git",
            false,
        );
        assert_eq!(result.suggestions[0].name, "zeta", "explicit priority must still work");
    }

    #[test]
    fn scoped_acceptance_is_bounded_idempotent_and_separate_from_legacy_json() {
        let slot = ArgSlot {
            root: "git".into(),
            path: vec!["checkout".into()],
            option: None,
            index: 0,
        };
        assert!(argument_scope("relative/project", &slot).is_none());
        let scope = argument_scope("/project", &slot).unwrap();
        let mut acceptance = AcceptanceIndex::from_entries([("git".into(), "status".into(), 10)]);
        let legacy_json = serde_json::to_value(&acceptance).unwrap();
        for index in 0..=MAX_SCOPED_ACCEPTANCES {
            assert!(acceptance.record_scoped_at(&scope, &format!("branch-{index}"), index as u64 + 1));
        }
        assert_eq!(
            acceptance.scoped.entries.values().map(HashMap::len).sum::<usize>(),
            MAX_SCOPED_ACCEPTANCES
        );
        assert_eq!(acceptance.scoped_timestamp(&scope, "branch-0"), None);
        assert_eq!(acceptance.scoped_timestamp(&scope, "branch-512"), Some(513));
        // A delayed worker replay of the oldest client update cannot undo
        // eviction of that update or replace a newer one.
        assert!(acceptance.record_scoped_at(&scope, "branch-0", 1));
        assert_eq!(acceptance.scoped_timestamp(&scope, "branch-0"), None);
        assert!(acceptance.record_scoped_at(&scope, "branch-512", 513));
        assert_eq!(acceptance.scoped_timestamp(&scope, "branch-512"), Some(513));
        assert_eq!(serde_json::to_value(&acceptance).unwrap(), legacy_json);
        assert!(serde_json::to_vec(&acceptance.scoped).unwrap().len() < 128 * 1024);
    }

    #[test]
    fn acceptance_index_round_trips_and_excludes_navigation_rows() {
        let mut index = AcceptanceIndex::default();
        assert!(!index.record_at("git", "↪", 10));
        assert!(!index.record_at("git", "../", 10));
        assert!(index.record_at("git", "status", 123));
        let value = serde_json::to_value(&index).expect("serialize acceptance index");
        let restored: AcceptanceIndex = serde_json::from_value(value).expect("deserialize acceptance index");
        assert_eq!(restored.timestamp("git", "status"), Some(123));
        assert_eq!(restored.timestamp("git", "↪"), None);
    }

    #[test]
    fn legacy_acceptance_evicts_oldest_at_the_count_limit_and_preserves_roots() {
        let mut index = AcceptanceIndex::default();
        for number in 0..MAX_ACCEPTANCES {
            assert!(index.record_at("git", &format!("name-{number}"), number as u64 + 10));
        }
        assert_eq!(index.entry_count(), MAX_ACCEPTANCES);
        assert!(index.estimated_bytes() <= MAX_ACCEPTANCE_BYTES);
        assert!(index.record_at("docker", "name-0", u64::MAX));
        assert_eq!(index.entry_count(), MAX_ACCEPTANCES);
        assert_eq!(index.timestamp("git", "name-0"), None);
        assert_eq!(index.timestamp("docker", "name-0"), Some(u64::MAX));
        // An old worker replay cannot reverse an eviction or lower a timestamp.
        assert!(index.record_at("git", "name-0", 10));
        assert!(index.record_at("docker", "name-0", 1));
        assert_eq!(index.timestamp("git", "name-0"), None);
        assert_eq!(index.timestamp("docker", "name-0"), Some(u64::MAX));
    }

    #[test]
    fn legacy_acceptance_byte_budget_includes_json_escaping() {
        let mut index = AcceptanceIndex::default();
        let mut old_names = HashMap::new();
        for number in 0..100 {
            let name = format!("{number:04}{}", "\0".repeat(MAX_ACCEPTANCE_NAME_BYTES - 4));
            assert!(index.record_at("git", &name, number));
            old_names.insert(name, number);
            assert!(index.estimated_bytes() <= MAX_ACCEPTANCE_BYTES);
        }
        assert!(
            index.entry_count() < 100,
            "bytes must limit entries before the count cap"
        );
        assert!(serde_json::to_vec(&index).unwrap().len() <= MAX_ACCEPTANCE_BYTES);
        let newest = format!("0099{}", "\0".repeat(MAX_ACCEPTANCE_NAME_BYTES - 4));
        assert_eq!(index.timestamp("git", &newest), Some(99));
        let oldest = format!("0000{}", "\0".repeat(MAX_ACCEPTANCE_NAME_BYTES - 4));
        assert_eq!(index.timestamp("git", &oldest), None);
        let loaded = AcceptanceIndex {
            entries: HashMap::from([("git".into(), old_names)]),
            ..AcceptanceIndex::default()
        };
        let (bounded, pruned) = AcceptanceIndex::prune_loaded(loaded);
        assert!(pruned);
        assert_eq!(bounded, index, "load must keep the same newest byte-bounded records");
    }

    #[test]
    fn legacy_acceptance_rejects_oversize_fields_after_trimming() {
        let mut index = AcceptanceIndex::default();
        let root = "r".repeat(MAX_ACCEPTANCE_ROOT_BYTES);
        let name = "é".repeat(MAX_ACCEPTANCE_NAME_BYTES / 2);
        assert!(index.record_at(&format!(" {root} "), &format!(" {name} "), 42));
        assert_eq!(index.timestamp(&root, &name), Some(42));
        assert!(!index.record_at(&format!("{root}r"), "valid", 43));
        assert!(!index.record_at("valid", &format!("{name}é"), 43));
        assert!(!index.record_at("git", " ../ ", 43));
        assert!(!index.record_at("git", " ↪ ", 43));
        assert!(!index.record_at(" \t ", "valid", 43));
        assert!(!index.record_at("git", " \t ", 43));
        assert_eq!(index.entry_count(), 1);
    }

    #[test]
    fn a_large_old_acceptance_cannot_partially_evict_the_index() {
        let mut index = AcceptanceIndex::default();
        assert!(index.record_at("git", "old", 1));
        let mut number = 0;
        loop {
            let name = format!("{number:04}{}", "x".repeat(MAX_ACCEPTANCE_NAME_BYTES - 4));
            if !index.fits_new_entry("git", &name) {
                break;
            }
            assert!(index.record_at("git", &name, 1000 + number));
            number += 1;
        }
        let before = index.clone();
        let large = "\0".repeat(MAX_ACCEPTANCE_NAME_BYTES);
        assert!(index.record_at("git", &large, 2));
        assert_eq!(index, before, "reject before evicting even the one older item");
    }

    #[test]
    fn legacy_acceptance_load_keeps_newest_entries_and_releases_old_capacity() {
        let mut names = HashMap::with_capacity(MAX_ACCEPTANCES * 8);
        for number in 0..MAX_ACCEPTANCES + 8 {
            names.insert(format!("name-{number}"), number as u64);
        }
        let old_names_capacity = names.capacity();
        let mut entries = HashMap::with_capacity(MAX_ACCEPTANCES);
        entries.insert("git".into(), names);
        let old_roots_capacity = entries.capacity();
        let loaded = AcceptanceIndex {
            entries,
            ..AcceptanceIndex::default()
        };
        let (bounded, pruned) = AcceptanceIndex::prune_loaded(loaded);
        assert!(pruned);
        assert_eq!(bounded.entry_count(), MAX_ACCEPTANCES);
        assert_eq!(bounded.timestamp("git", "name-7"), None);
        assert_eq!(bounded.timestamp("git", "name-8"), Some(8));
        assert_eq!(
            bounded.timestamp("git", &format!("name-{}", MAX_ACCEPTANCES + 7)),
            Some((MAX_ACCEPTANCES + 7) as u64)
        );
        assert!(bounded.entries.capacity() < old_roots_capacity);
        assert!(bounded.entries["git"].capacity() < old_names_capacity);
        assert!(bounded.estimated_bytes() <= MAX_ACCEPTANCE_BYTES);
        let (_, needs_another_migration) = AcceptanceIndex::prune_loaded(bounded);
        assert!(!needs_another_migration);
    }

    #[test]
    fn legacy_acceptance_load_preserves_json_shape_and_prunes_invalid_entries() {
        let value = serde_json::json!({
            " git ": {" status ": 7, "../": 9, "↪": 11, "": 13},
            "git": {"status": 10},
            "docker": {"status": 8},
            "empty": {},
        });
        let mut loaded: AcceptanceIndex = serde_json::from_value(value).unwrap();
        loaded.entries.insert(
            "r".repeat(MAX_ACCEPTANCE_ROOT_BYTES + 1),
            HashMap::from([("valid".into(), 99)]),
        );
        loaded
            .entries
            .get_mut("git")
            .unwrap()
            .insert("n".repeat(MAX_ACCEPTANCE_NAME_BYTES + 1), 99);
        let (bounded, pruned) = AcceptanceIndex::prune_loaded(loaded);
        assert!(pruned);
        assert_eq!(
            serde_json::to_value(&bounded).unwrap(),
            serde_json::json!({"git": {"status": 10}, "docker": {"status": 8}})
        );
        assert_eq!(bounded.scoped, ScopedAcceptanceIndex::default());
        let (_, needs_another_migration) = AcceptanceIndex::prune_loaded(bounded);
        assert!(!needs_another_migration);
    }

    #[test]
    fn legacy_acceptance_load_deduplicates_trimmed_keys_before_limiting_candidates() {
        let mut names = HashMap::new();
        for number in 0..=MAX_ACCEPTANCES {
            names.insert(format!("status{}", " ".repeat(number)), number as u64 + 10);
        }
        let loaded = AcceptanceIndex {
            entries: HashMap::from([
                (" git ".into(), names),
                ("git".into(), HashMap::from([("status".into(), 9)])),
                ("docker".into(), HashMap::from([("run".into(), 1)])),
            ]),
            ..AcceptanceIndex::default()
        };
        let (bounded, pruned) = AcceptanceIndex::prune_loaded(loaded);
        assert!(pruned);
        assert_eq!(bounded.entry_count(), 2);
        assert_eq!(bounded.timestamp("git", "status"), Some(MAX_ACCEPTANCES as u64 + 10));
        assert_eq!(bounded.timestamp("docker", "run"), Some(1));
    }

    #[test]
    fn alphabetical_ranking_disables_acceptance_but_deduplicates_stably() {
        let acceptance = AcceptanceIndex::from_entries([("git".into(), "alpha".into(), 2_000_000_000_000)]);
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("zeta", "", "subcommand"),
                Suggestion::new("alpha", "", "subcommand"),
                Suggestion::new("alpha", "", "subcommand"),
            ],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply_with_acceptance(
            &mut result,
            &["git".into()],
            &Frecency::default(),
            &acceptance,
            "git",
            true,
        );
        assert_eq!(
            result
                .suggestions
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
    }

    #[test]
    fn first_token_inserts_matching_history() {
        let frecency = Frecency::from_commands([("git checkout -b feature".into(), 40)]);
        let mut result = CompleteResult {
            suggestions: Vec::new(),
            search_term: "git".into(),
            match_term: "git".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        merge_history(&mut result, &["git".into()], &frecency, false);
        assert!(
            result
                .suggestions
                .iter()
                .any(|s| s.name.starts_with("git checkout") && s.kind == "history"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn history_uses_recent_source_order_and_last_duplicate_wins() {
        // History rows arrive oldest-first (including custom-command output),
        // so the old generator walks them backwards before deduplicating.
        // Timestamps are intentionally misleading: they must not reorder the
        // source sequence by shell frecency.
        let frecency = Frecency::from_commands([("git old".into(), 900), ("git new".into(), 1), ("git old".into(), 2)]);
        assert_eq!(
            frecency
                .history_suffix_suggestions("git ", "", false)
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["old", "new"]
        );
        assert_eq!(
            frecency
                .history_suggestions("git", false, false)
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["git old", "git new"]
        );
    }

    #[test]
    fn history_dedup_considers_flattened_static_aliases() {
        let frecency = Frecency::from_commands([("git checkout".into(), 1)]);
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("co", "", "subcommand")
                    .with_primary_name(Some("checkout".into()))
                    .with_alias_names(vec!["checkout".into(), "co".into()]),
            ],
            search_term: "co".into(),
            match_term: "co".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        merge_history(&mut result, &["git".into(), "co".into()], &frecency, false);
        assert!(result.suggestions.iter().all(|item| item.kind != "history"));
    }

    #[test]
    fn history_after_partial_second_token_uses_only_the_suffix() {
        let frecency = Frecency::from_commands([
            ("git commit -m feature".into(), 40),
            ("git cherry-pick main".into(), 30),
        ]);
        assert!(crate::query::matches_query("commit -m feature", "co", false));
        assert_eq!(
            frecency
                .history_suffix_suggestions("git ", "co", false)
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["commit -m feature"]
        );
        let mut result = CompleteResult {
            suggestions: vec![Suggestion::new("commit", "Record changes", "subcommand")],
            search_term: "'co".into(),
            match_term: "co".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        merge_history(&mut result, &["git".into(), "co".into()], &frecency, false);
        let history = result
            .suggestions
            .iter()
            .find(|item| item.kind == "history")
            .unwrap_or_else(|| panic!("history suffix missing from {:?}", result.suggestions));
        assert_eq!(history.name, "commit -m feature");
        assert_eq!(history.insert_value.as_deref(), Some("commit -m feature"));
        assert!(!history.name.starts_with("git "));
    }

    #[test]
    fn history_frequency_boosts_the_matching_static_first_word() {
        let frecency =
            Frecency::from_commands([("git checkout -b one".into(), 10), ("git checkout -b two".into(), 20)]);
        let mut explicit_76 = Suggestion::new("cherry-pick", "", "subcommand");
        explicit_76.priority = 76;
        let mut result = CompleteResult {
            suggestions: vec![explicit_76, Suggestion::new("checkout", "", "subcommand")],
            search_term: "ch".into(),
            match_term: "ch".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        merge_history(&mut result, &["git".into(), "ch".into()], &frecency, false);
        apply(&mut result, &["git".into(), "ch".into()], &frecency);

        // Two history hits are 75.2, so they boost static checkout above 75
        // but do not leap over an explicit priority of 76.
        assert_eq!(result.suggestions[0].name, "cherry-pick");
        let checkout = result
            .suggestions
            .iter()
            .position(|item| item.name == "checkout")
            .expect("static checkout");
        let checkout_history = result
            .suggestions
            .iter()
            .position(|item| item.kind == "history")
            .expect("checkout history");
        assert!(checkout < checkout_history);
    }

    #[test]
    fn history_frequency_boosts_spec_rows_via_any_alias_spelling() {
        // `git co foo` never mentions "checkout". The WebView still promoted
        // that spec row because `co` is another spelling in `name[]`.
        let frecency = Frecency::from_commands([("git co foo".into(), 10), ("git co bar".into(), 20)]);
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("commit", "", "subcommand"),
                Suggestion::new("checkout", "", "subcommand")
                    .with_primary_name(Some("checkout".into()))
                    .with_alias_names(vec!["checkout".into(), "co".into()]),
            ],
            search_term: "c".into(),
            match_term: "c".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply(&mut result, &["git".into(), "c".into()], &frecency);
        assert_eq!(
            result
                .suggestions
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["checkout", "commit"]
        );
    }

    #[test]
    fn duplicate_history_entries_count_before_dedup() {
        let frecency = Frecency::from_commands([
            ("git checkout -b feature".into(), 10),
            ("git checkout -b feature".into(), 20),
        ]);
        let history = frecency.history_suffix_suggestions("git ", "ch", false);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].priority, 75);
    }

    #[test]
    fn trailing_space_inserts_history_suffixes_without_repeating_the_command() {
        let frecency = Frecency::from_commands([("git checkout -b feature".into(), 40), ("npm test".into(), 50)]);
        let mut result = CompleteResult {
            suggestions: vec![Suggestion::new("checkout", "", "subcommand")],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        merge_history(&mut result, &["git".into()], &frecency, false);
        assert!(
            result.suggestions.iter().all(|s| !s.name.starts_with("git ")),
            "{:?}",
            result.suggestions
        );
        assert!(
            result
                .suggestions
                .iter()
                .any(|s| s.name == "checkout -b feature" && s.kind == "history"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn dedup_matches_old_fields_and_ignores_description_and_kind() {
        let frecency = Frecency::default();
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("status", "one", "subcommand"),
                Suggestion::new("status", "two", "subcommand"),
                Suggestion::new("status", "", "history").with_insert_value("status"),
                Suggestion::new("status", "", "history").with_insert_value("status"),
            ],
            search_term: "st".into(),
            match_term: "st".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply(&mut result, &["git".into(), "st".into()], &frecency);
        assert_eq!(result.suggestions.len(), 2);
        assert_eq!(result.suggestions[0].kind, "subcommand");
        assert_eq!(result.suggestions[1].kind, "history");
    }

    #[test]
    fn history_shell_normalization_accepts_paths_and_figterm_suffixes() {
        assert_eq!(normalize_history_shell(Some("/bin/zsh")), HistoryShell::Zsh);
        assert_eq!(normalize_history_shell(Some("bash (figterm)")), HistoryShell::Bash);
        assert_eq!(normalize_history_shell(Some("/bin/zsh (fterm)")), HistoryShell::Zsh);
        assert_eq!(normalize_history_shell(Some("zsh (fastabterm)")), HistoryShell::Zsh);
        assert_eq!(normalize_history_shell(Some("bash (ecterm)")), HistoryShell::Bash);
        assert_eq!(normalize_history_shell(Some("/usr/local/bin/fish")), HistoryShell::Fish);
        assert_eq!(normalize_history_shell(Some("nu")), HistoryShell::Unknown);
        assert_eq!(normalize_history_shell(None), HistoryShell::Unknown);
    }

    #[test]
    fn history_source_key_changes_only_with_source_inputs() {
        let zsh = history_source_config(None, false, Some("/bin/zsh"), None, &[]);
        let zsh_again = history_source_config(None, false, Some("zsh"), Some("bash"), &[]);
        let zsh_from_process = history_source_config(None, false, Some("unknown"), Some("/bin/zsh"), &[]);
        let bash = history_source_config(None, false, Some("bash"), None, &[]);
        let all_shells = history_source_config(None, true, Some("zsh"), None, &[]);
        let custom = history_source_config(Some("fc -ln 1".into()), false, Some("zsh"), None, &[]);

        assert_eq!(zsh, zsh_again);
        assert_eq!(zsh, zsh_from_process);
        assert_ne!(zsh, bash);
        assert_ne!(zsh, all_shells);
        assert_ne!(zsh, custom);
    }

    #[test]
    fn custom_history_output_keeps_lines_and_discards_blank_lines() {
        assert_eq!(
            history_commands_from_output("git status\n\n git add . \n"),
            vec![("git status".into(), 0), (" git add . ".into(), 0)]
        );
        assert!(history_commands_from_output("\n  \n").is_empty());
    }

    #[test]
    fn history_output_keeps_the_recent_window_in_source_order() {
        let mut oldest_first = String::new();
        let total = MAX_DATABASE_HISTORY_COMMANDS + 3;
        for i in 0..total {
            oldest_first.push_str(&format!("cmd{i}\n"));
        }
        let parsed = history_commands_from_output(&oldest_first);
        assert_eq!(parsed.len(), MAX_DATABASE_HISTORY_COMMANDS);
        assert_eq!(parsed[0].0, "cmd3");
        assert_eq!(parsed.last().unwrap().0, format!("cmd{}", total - 1));
    }

    #[test]
    fn shell_history_comes_from_the_shells_own_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".zsh_history"), ": 1:0;git status\n: 2:0;cargo build\n").unwrap();
        std::fs::write(dir.path().join(".bash_history"), "ls\n").unwrap();
        let env = vec![("HOME".to_string(), dir.path().display().to_string())];

        let zsh = history_source_config(None, false, Some("zsh"), None, &env);
        assert_eq!(
            load_commands_for(&zsh),
            vec![("git status".to_string(), 0), ("cargo build".to_string(), 0)]
        );
        let merged = history_source_config(None, true, Some("bash"), None, &env);
        let merged: Vec<_> = load_commands_for(&merged)
            .into_iter()
            .map(|(command, _)| command)
            .collect();
        assert_eq!(merged, vec!["ls", "git status", "cargo build"]);
    }

    fn insert_history(history: &fastab_settings::history::History, shell: &str, command: &str, started: u64) {
        history
            .insert_command_history(
                &fastab_settings::history::CommandInfo {
                    command: Some(command.into()),
                    shell: Some(shell.into()),
                    start_time: Some(UNIX_EPOCH + Duration::from_secs(started)),
                    ..fastab_settings::history::CommandInfo::default()
                },
                false,
            )
            .unwrap();
    }

    #[test]
    fn new_database_rows_follow_the_cursor_and_the_source_shell() {
        let history = fastab_settings::history::History::mock();
        insert_history(&history, "zsh", "git status", 1);
        insert_history(&history, "bash", "ls", 2);
        insert_history(&history, "/bin/zsh", "cargo build", 3);
        let zsh = history_source_config(None, false, Some("zsh"), None, &[]);
        let (commands, last) = new_commands_from(&history, &zsh, 0).unwrap();
        assert_eq!(commands, ["git status", "cargo build"]);
        assert_eq!(last, history.latest_id().unwrap());
        assert_eq!(new_commands_from(&history, &zsh, last).unwrap(), (Vec::new(), last));
        // A row the source filters out still moves the cursor past it.
        insert_history(&history, "bash", "pwd", 4);
        let (commands, moved) = new_commands_from(&history, &zsh, last).unwrap();
        assert!(commands.is_empty());
        assert!(moved > last);
    }

    #[test]
    fn unflushed_rows_are_those_newer_than_the_history_file() {
        let history = fastab_settings::history::History::mock();
        insert_history(&history, "zsh", "in the file", 100);
        insert_history(&history, "bash", "another shell", 300);
        insert_history(&history, "zsh", "still open", 200);
        let written = Some(UNIX_EPOCH + Duration::from_secs(150));
        assert_eq!(
            unflushed_database_commands(&history, HistoryShell::Zsh, written),
            vec![("still open".to_string(), 0)]
        );
        assert!(unflushed_database_commands(&history, HistoryShell::Zsh, None).is_empty());
    }

    #[test]
    fn appended_commands_reach_history_and_counts_without_touching_the_old_list() {
        let mut frecency = Frecency::from_commands([("git status".to_string(), 1)]);
        let before = frecency.command_lines();
        frecency.append_commands(["git commit".to_string(), "  ".to_string()]);
        assert_eq!(before.len(), 1);
        assert_eq!(frecency.command_lines().len(), 2);
        assert_eq!(frecency.next_word_counts("git").get("commit"), Some(&1));
        assert_eq!(frecency.next_word_counts("git").get("status"), Some(&1));
    }

    #[test]
    fn history_shell_selection_matches_legacy_sources() {
        let zsh = history_source_config(None, false, Some("zsh"), None, &[]);
        assert!(includes_database_shell(&zsh, HistoryShell::Zsh));
        assert!(!includes_database_shell(&zsh, HistoryShell::Bash));
        assert!(!includes_database_shell(&zsh, HistoryShell::Fish));

        let merged = history_source_config(None, true, Some("/bin/bash"), None, &[]);
        assert!(includes_database_shell(&merged, HistoryShell::Zsh));
        assert!(includes_database_shell(&merged, HistoryShell::Bash));
        assert!(!includes_database_shell(&merged, HistoryShell::Fish));

        let fish = history_source_config(None, true, Some("fish"), None, &[]);
        assert!(includes_database_shell(&fish, HistoryShell::Fish));
        assert!(!includes_database_shell(&fish, HistoryShell::Zsh));

        let unknown = history_source_config(None, false, Some("nu"), None, &[]);
        assert!(includes_database_shell(&unknown, HistoryShell::Zsh));
        assert!(includes_database_shell(&unknown, HistoryShell::Bash));
        assert!(includes_database_shell(&unknown, HistoryShell::Fish));
    }

    fn walk_next_word_counts(commands: &[(String, u64)], prefix: &str) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        let full_prefix = (!prefix.is_empty()).then(|| format!("{prefix} "));
        for (command, _) in commands {
            let command = command.trim();
            let suffix = match full_prefix.as_deref() {
                Some(prefix) => match command.strip_prefix(prefix) {
                    Some(suffix) => suffix,
                    None => continue,
                },
                None => command,
            };
            let first_word = suffix.split_whitespace().next().unwrap_or_default();
            if !first_word.is_empty() {
                *counts.entry(first_word.to_string()).or_default() += 1;
            }
        }
        counts
    }

    #[test]
    fn repeated_history_commands_share_one_string() {
        let frecency = Frecency::from_commands([
            ("git checkout".into(), 1),
            ("git status".into(), 2),
            ("git checkout".into(), 3),
        ]);
        let lines = frecency.command_lines();
        assert_eq!(lines.len(), 3);
        assert!(Arc::ptr_eq(&lines[0], &lines[2]));
        assert!(!Arc::ptr_eq(&lines[0], &lines[1]));
        let rows = frecency.history_suggestions("", false, true);
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["git checkout", "git status"]);
    }

    #[test]
    fn next_word_index_matches_the_per_request_walk() {
        let commands = [
            ("git checkout main".into(), 1_u64),
            ("git status".into(), 2),
            ("echo hi".into(), 3),
            ("git  checkout".into(), 4),
            ("git  checkout main".into(), 5),
            ("git\tcheckout main".into(), 6),
            ("git \t\u{2003}checkout\u{a0} main".into(), 7),
            ("git checkout\n main".into(), 8),
            ("git  checkout main".into(), 9),
            ("git checkout main   ".into(), 10),
            (" \tgit checkout main\u{2003} ".into(), 11),
        ];
        let frecency = Frecency::from_commands(commands.clone());
        let mut prefixes = vec![
            "",
            "missing",
            "gi",
            "git check",
            "git checkout main",
            "git  checkout main ",
        ];
        for (command, _) in &commands {
            prefixes.extend(command.match_indices(' ').map(|(offset, _)| &command[..offset]));
        }
        for prefix in prefixes {
            assert_eq!(
                frecency.next_word_counts(prefix).to_owned_map(),
                walk_next_word_counts(&commands, prefix),
                "prefix {prefix:?}"
            );
        }
    }

    #[test]
    fn history_index_build_and_drop_stay_flat_for_a_deep_irregular_prefix() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let command = format!("echo  {}", "x ".repeat(10_000));
                let frecency = Frecency::from_commands([(command.clone(), 0), (command.clone(), 1)]);
                let trimmed = command.trim();
                let prefix = trimmed.rsplit_once(' ').unwrap().0;
                assert_eq!(frecency.next_word_counts(prefix).get("x"), Some(&2));
                let (nodes, _, bytes) = frecency.next_words_diagnostics();
                assert!(nodes <= 10_003);
                assert!(
                    bytes < command.len() * 256,
                    "{bytes} bytes for {} bytes of history",
                    command.len()
                );
                assert_eq!(frecency.history_suggestions("", false, true).len(), 1);
                drop(frecency);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn many_spaces_before_one_large_word_share_the_next_word() {
        let word = "x".repeat(32 * 1024);
        let command = format!("echo {}{word}", " ".repeat(4096));
        let frecency = Frecency::from_commands([(command.clone(), 0)]);
        assert_eq!(frecency.next_word_counts("echo").get(&word), Some(&1));
        assert_eq!(
            frecency
                .next_word_counts(&format!("echo {}", " ".repeat(4095)))
                .get(&word),
            Some(&1)
        );
        assert_eq!(frecency.next_words.words.len(), 2);
        assert!(frecency.next_words_diagnostics().2 < command.len() * 64);
    }

    #[test]
    fn cancellation_during_a_long_history_build_discards_the_partial_arena() {
        let token = crate::cancellation::CancellationToken::new();
        let _scope = crate::cancellation::enter(token.clone());
        let mut checkpoints = 0;
        let result = Frecency::build([(format!("echo  {}", "x ".repeat(10_000)), 0)], || {
            checkpoints += 1;
            if checkpoints == 30 {
                token.cancel();
            }
            crate::cancellation::check()
        });
        assert!(result.is_err());
        assert_eq!(checkpoints, 30, "the build stops at the cancellation boundary");
    }

    #[test]
    fn history_prefix_keeps_quotes_and_double_spaces_from_the_buffer() {
        let (tokens, ends) = crate::lookup::tokenize("git  'ch");
        assert_eq!(
            history_prefix_from_buffer("git  'ch", ends, &tokens).as_deref(),
            Some("git  '")
        );
        let (tokens, ends) = crate::lookup::tokenize("g checkout");
        assert_eq!(
            history_prefix_from_buffer("g checkout", ends, &tokens).as_deref(),
            Some("g ")
        );
        let (tokens, ends) = crate::lookup::tokenize("git ");
        assert_eq!(
            history_prefix_from_buffer("git ", ends, &tokens).as_deref(),
            Some("git ")
        );
        let (tokens, ends) = crate::lookup::tokenize("git");
        assert_eq!(history_prefix_from_buffer("git", ends, &tokens), None);
    }
}
