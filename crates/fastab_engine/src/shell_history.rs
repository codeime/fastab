//! Shell history read from the shell's own history file.
//!
//! The WebView listed it through an interactive login shell (`zsh -lic
//! 'fc -R; fc -ln 1'`). The engine ran `zsh -lc` instead, which never
//! sources `/etc/zshrc`, where macOS sets `HISTFILE`: `fc` failed with "no
//! such event", a non-interactive bash keeps no history so its `fc` printed
//! nothing, and every first completion paid for a login shell and the
//! profile it sources (the Fastab zprofile shims included) before falling
//! back to the database anyway. These readers parse each file the way its
//! shell reads it back and return the rows its listing printed, without
//! running anything.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::rank::HistoryShell;

/// The engine keeps the most recent commands only, so only the end of a
/// history file is read.
const MAX_HISTORY_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// zsh stores a byte it uses internally as this marker followed by the
/// byte XOR 0x20 (`metafy`); UTF-8 continuation bytes are among them.
const ZSH_META: u8 = 0x83;

/// The history file of each shell the source reads. It is part of the
/// history source key, so a terminal exporting another `HISTFILE` or
/// `ZDOTDIR` reloads history once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HistoryFiles {
    zsh: Option<PathBuf>,
    bash: Option<PathBuf>,
    fish: Option<PathBuf>,
}

impl HistoryFiles {
    /// Files for the current shell, plus zsh and bash when `merge_zsh_bash`
    /// is set. An exported `HISTFILE` belongs to the current shell; otherwise
    /// each shell's default applies: `${ZDOTDIR:-$HOME}/.zsh_history` (what
    /// macOS's `/etc/zshrc` sets), `~/.bash_history`, and
    /// `${XDG_DATA_HOME:-~/.local/share}/fish/fish_history`.
    pub(crate) fn resolve(current: HistoryShell, merge_zsh_bash: bool, environment: &[(String, String)]) -> Self {
        let var = |name: &str| {
            environment
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| PathBuf::from(value))
                .filter(|path| path.is_absolute())
        };
        let home = var("HOME").or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        });
        let reads = |shell: HistoryShell| {
            shell == current || (merge_zsh_bash && matches!(shell, HistoryShell::Zsh | HistoryShell::Bash))
        };
        let file = |shell: HistoryShell, default: Option<PathBuf>| {
            if !reads(shell) {
                None
            } else if shell == current {
                var("HISTFILE").or(default)
            } else {
                default
            }
        };
        Self {
            zsh: file(
                HistoryShell::Zsh,
                var("ZDOTDIR")
                    .or_else(|| home.clone())
                    .map(|dir| dir.join(".zsh_history")),
            ),
            bash: file(HistoryShell::Bash, home.as_ref().map(|dir| dir.join(".bash_history"))),
            fish: file(
                HistoryShell::Fish,
                var("XDG_DATA_HOME")
                    .or_else(|| home.as_ref().map(|dir| dir.join(".local/share")))
                    .map(|dir| dir.join("fish/fish_history")),
            ),
        }
    }

    fn path(&self, shell: HistoryShell) -> Option<&Path> {
        match shell {
            HistoryShell::Zsh => self.zsh.as_deref(),
            HistoryShell::Bash => self.bash.as_deref(),
            HistoryShell::Fish => self.fish.as_deref(),
            HistoryShell::Unknown => None,
        }
    }
}

/// The most recent `limit` rows of `shell`'s history, oldest first. A
/// missing or unreadable file has none.
pub(crate) fn commands(files: &HistoryFiles, shell: HistoryShell, limit: usize) -> Vec<String> {
    let Some((bytes, cut)) = files
        .path(shell)
        .and_then(|path| read_tail(path, MAX_HISTORY_FILE_BYTES))
    else {
        return Vec::new();
    };
    let mut rows = match shell {
        HistoryShell::Zsh => zsh_rows(&bytes, cut),
        HistoryShell::Bash => bash_rows(&bytes, cut),
        HistoryShell::Fish => fish_rows(&bytes, limit),
        HistoryShell::Unknown => Vec::new(),
    };
    if rows.len() > limit {
        rows.drain(..rows.len() - limit);
    }
    rows
}

/// The last `max` bytes of a regular file, and whether anything before them
/// was left out. Anything else (a FIFO, a directory) is not read.
fn read_tail(path: &Path, max: u64) -> Option<(Vec<u8>, bool)> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let start = metadata.len().saturating_sub(max);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes).ok()?;
    Some((bytes, start > 0))
}

/// zsh's `readhistfile`: a line ending in a backslash continues the event
/// (an embedded newline is saved as backslash + newline), and
/// EXTENDED_HISTORY prefixes each event with `: <start>:<elapsed>;`. The
/// first event of a cut file is partial and left out.
fn zsh_rows(bytes: &[u8], cut: bool) -> Vec<String> {
    let mut rows = Vec::new();
    let mut event = Vec::new();
    let mut partial = cut;
    for line in bytes.split(|&byte| byte == b'\n') {
        if let Some(head) = line.strip_suffix(b"\\") {
            event.extend_from_slice(head);
            event.push(b'\n');
            continue;
        }
        event.extend_from_slice(line);
        if !std::mem::take(&mut partial) {
            rows.extend(zsh_row(&event));
        }
        event.clear();
    }
    rows
}

fn zsh_row(event: &[u8]) -> Option<String> {
    let text = match event.strip_prefix(b":") {
        Some(rest) => {
            let elapsed = rest.iter().position(|&byte| byte == b':')? + 1;
            let command = elapsed + rest[elapsed..].iter().position(|&byte| byte == b';')? + 1;
            &rest[command..]
        },
        // A plain event starting with a colon is saved as `\:`.
        None if event.starts_with(b"\\:") => &event[1..],
        None => event,
    };
    let row = fc_listing(&String::from_utf8_lossy(&unmetafy(text)));
    (!row.trim().is_empty()).then_some(row)
}

fn unmetafy(text: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(text.len());
    let mut bytes = text.iter();
    while let Some(&byte) = bytes.next() {
        if byte == ZSH_META {
            if let Some(&next) = bytes.next() {
                plain.push(next ^ 0x20);
            }
        } else {
            plain.push(byte);
        }
    }
    plain
}

/// `fc -l` prints each event through `nicezputs`: an embedded newline or
/// tab as `\n` / `\t` and any other control character in caret notation,
/// so a multi-line command stayed one row.
fn fc_listing(command: &str) -> String {
    let mut row = String::with_capacity(command.len());
    for character in command.chars() {
        match character {
            '\n' => row.push_str("\\n"),
            '\t' => row.push_str("\\t"),
            '\x7f' => row.push_str("^?"),
            control if control < ' ' => {
                row.push('^');
                row.push(char::from(control as u8 + 0x40));
            },
            other => row.push(other),
        }
    }
    row
}

/// One command per line; with `HISTTIMEFORMAT` set, bash writes a `#<epoch>`
/// line before each one. `fc -l` printed a multi-line entry line by line.
fn bash_rows(bytes: &[u8], cut: bool) -> Vec<String> {
    bytes
        .split(|&byte| byte == b'\n')
        .skip(usize::from(cut))
        .filter(|line| {
            !line
                .strip_prefix(b"#")
                .is_some_and(|epoch| !epoch.is_empty() && epoch.iter().all(u8::is_ascii_digit))
        })
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .filter(|line| !line.trim().is_empty())
        .collect()
}

/// fish's YAML-like file: each entry starts `- cmd: ` with `\\` and `\n`
/// escaped, followed by indented `when:` and `paths:` lines. Lines before
/// the first entry of a cut file belong to none. `history search
/// --max=<limit>` listed each command once at its newest use, a multi-line
/// one line by line.
fn fish_rows(bytes: &[u8], limit: usize) -> Vec<String> {
    let entries: Vec<String> = bytes
        .split(|&byte| byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"- cmd: "))
        .map(unescape_fish)
        .collect();
    let mut seen = HashSet::new();
    let mut newest: Vec<&str> = entries
        .iter()
        .rev()
        .map(String::as_str)
        .filter(|entry| seen.insert(*entry))
        .take(limit)
        .collect();
    newest.reverse();
    newest
        .into_iter()
        .flat_map(|entry| entry.split('\n'))
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

fn unescape_fish(escaped: &[u8]) -> String {
    let mut command = Vec::with_capacity(escaped.len());
    let mut bytes = escaped.iter().peekable();
    while let Some(&byte) = bytes.next() {
        match (byte, bytes.peek()) {
            (b'\\', Some(b'\\')) => {
                command.push(b'\\');
                bytes.next();
            },
            (b'\\', Some(b'n')) => {
                command.push(b'\n');
                bytes.next();
            },
            (byte, _) => command.push(byte),
        }
    }
    String::from_utf8_lossy(&command).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn files(shell: HistoryShell, path: PathBuf) -> HistoryFiles {
        let mut files = HistoryFiles::default();
        match shell {
            HistoryShell::Zsh => files.zsh = Some(path),
            HistoryShell::Bash => files.bash = Some(path),
            HistoryShell::Fish => files.fish = Some(path),
            HistoryShell::Unknown => {},
        }
        files
    }

    #[test]
    fn zsh_rows_match_what_fc_listed() {
        let dir = tempfile::tempdir().unwrap();
        // "文" is E6 96 87; zsh metafies 0x96 and 0x87.
        let history = b": 1700000000:0;git status\n\
            : 1700000001:3;for i in 1 2; do\\\necho $i\\\ndone\n\
            plain command\n\
            \\:colon first\n\
            : 1700000002:0;echo \xe6\x83\xb6\x83\xa7\n\
            : 1700000003:0;   \n\
            : 1700000004:0;printf 'a\tb'\n";
        let path = write(dir.path(), ".zsh_history", history);
        assert_eq!(
            commands(&files(HistoryShell::Zsh, path), HistoryShell::Zsh, 100),
            vec![
                "git status",
                "for i in 1 2; do\\necho $i\\ndone",
                "plain command",
                ":colon first",
                "echo 文",
                "printf 'a\\tb'",
            ]
        );
    }

    #[test]
    fn bash_rows_skip_timestamps_and_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            ".bash_history",
            b"#1700000000\nls -la\n\n#1700000001\ncd /tmp\n#not-a-time\n",
        );
        assert_eq!(
            commands(&files(HistoryShell::Bash, path), HistoryShell::Bash, 100),
            vec!["ls -la", "cd /tmp", "#not-a-time"]
        );
    }

    #[test]
    fn fish_rows_list_each_command_once_at_its_newest_use() {
        let dir = tempfile::tempdir().unwrap();
        let history = b"- cmd: git status\n  when: 1\n\
            - cmd: cd src\n  when: 2\n  paths:\n    - src\n\
            - cmd: echo a\\\\b\n  when: 3\n\
            - cmd: git status\n  when: 4\n\
            - cmd: begin\\n  echo hi\\nend\n  when: 5\n";
        let path = write(dir.path(), "fish_history", history);
        let fish = files(HistoryShell::Fish, path);
        assert_eq!(
            commands(&fish, HistoryShell::Fish, 100),
            vec!["cd src", "echo a\\b", "git status", "begin", "  echo hi", "end"]
        );
        // The window keeps the newest rows.
        assert_eq!(commands(&fish, HistoryShell::Fish, 2), vec!["  echo hi", "end"]);
    }

    #[test]
    fn rows_keep_the_most_recent_window() {
        let dir = tempfile::tempdir().unwrap();
        let history: String = (0..10).map(|i| format!(": {i}:0;cmd{i}\n")).collect();
        let path = write(dir.path(), ".zsh_history", history.as_bytes());
        assert_eq!(
            commands(&files(HistoryShell::Zsh, path), HistoryShell::Zsh, 3),
            vec!["cmd7", "cmd8", "cmd9"]
        );
    }

    #[test]
    fn a_cut_file_leaves_out_its_partial_first_event() {
        let dir = tempfile::tempdir().unwrap();
        let history = b": 1:0;first line\\\nsecond line\n: 2:0;whole\n";
        let path = write(dir.path(), ".zsh_history", history);
        let (bytes, cut) = read_tail(&path, 20).unwrap();
        assert!(cut);
        assert_eq!(zsh_rows(&bytes, cut), vec!["whole"]);
        let (bytes, cut) = read_tail(&path, 1024).unwrap();
        assert!(!cut);
        assert_eq!(zsh_rows(&bytes, cut), vec!["first line\\nsecond line", "whole"]);
    }

    #[test]
    fn missing_and_non_regular_files_have_no_rows() {
        let dir = tempfile::tempdir().unwrap();
        let missing = files(HistoryShell::Zsh, dir.path().join("absent"));
        assert!(commands(&missing, HistoryShell::Zsh, 10).is_empty());
        let directory = files(HistoryShell::Zsh, dir.path().to_path_buf());
        assert!(commands(&directory, HistoryShell::Zsh, 10).is_empty());
    }

    #[test]
    fn files_follow_the_terminal_environment_for_the_current_shell() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect()
        };
        let home = env(&[("HOME", "/home/u")]);
        let zsh = HistoryFiles::resolve(HistoryShell::Zsh, false, &home);
        assert_eq!(zsh.zsh.as_deref(), Some(Path::new("/home/u/.zsh_history")));
        assert_eq!((zsh.bash, zsh.fish), (None, None));

        let zdotdir = env(&[("HOME", "/home/u"), ("ZDOTDIR", "/home/u/.config/zsh")]);
        let zsh = HistoryFiles::resolve(HistoryShell::Zsh, false, &zdotdir);
        assert_eq!(zsh.zsh.as_deref(), Some(Path::new("/home/u/.config/zsh/.zsh_history")));

        // An exported HISTFILE is the current shell's; the merged one keeps
        // its default.
        let exported = env(&[("HOME", "/home/u"), ("HISTFILE", "/data/bash-history")]);
        let merged = HistoryFiles::resolve(HistoryShell::Bash, true, &exported);
        assert_eq!(merged.bash.as_deref(), Some(Path::new("/data/bash-history")));
        assert_eq!(merged.zsh.as_deref(), Some(Path::new("/home/u/.zsh_history")));

        let relative = env(&[("HOME", "/home/u"), ("HISTFILE", "history")]);
        let bash = HistoryFiles::resolve(HistoryShell::Bash, false, &relative);
        assert_eq!(bash.bash.as_deref(), Some(Path::new("/home/u/.bash_history")));

        let fish = HistoryFiles::resolve(HistoryShell::Fish, true, &home);
        assert_eq!(
            fish.fish.as_deref(),
            Some(Path::new("/home/u/.local/share/fish/fish_history"))
        );
        assert!(fish.zsh.is_some() && fish.bash.is_some());

        assert_eq!(
            HistoryFiles::resolve(HistoryShell::Unknown, false, &home),
            HistoryFiles::default()
        );
    }
}
