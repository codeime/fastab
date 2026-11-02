//! IRIS-style token walk over static spec IR.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::ir::{ArgSpec, LoadSpec, OptionSpec, ParserDirectives, Registry, Spec, SuggestionMeta, Template};
use crate::query::matches_query;
use crate::runtime::{CompleteRequest, CompleteResult, CurrentArg, Suggestion};

pub fn parse_alias_map(raw: &str, shell_path: Option<&str>) -> std::collections::HashMap<String, String> {
    let separator = if shell_path.is_some_and(|path| path.contains("fish")) {
        ' '
    } else {
        '='
    };
    let mut aliases = std::collections::HashMap::new();
    for line in raw.lines() {
        let line = line.trim();
        let line = line.strip_prefix("alias ").unwrap_or(line);
        let Some((key, value)) = line.split_once(separator) else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim().trim_matches('\'').to_string();
        aliases.insert(key.to_string(), value);
    }
    aliases
}

/// Fig only substitutes when the command has more than one token. A trailing
/// space counts as that extra token (`g ` → `git `).
pub fn expand_alias_tokens(
    tokens: &mut Vec<String>,
    ends_with_space: bool,
    raw_aliases: &str,
    shell_path: Option<&str>,
) {
    if tokens.is_empty() || (tokens.len() == 1 && !ends_with_space) {
        return;
    }
    let aliases = parse_alias_map(raw_aliases, shell_path);
    expand_alias_tokens_with(tokens, ends_with_space, &aliases);
}

/// [`expand_alias_tokens`] over an already parsed map, for callers that
/// expand many commands against the same shell aliases.
pub(crate) fn expand_alias_tokens_with(
    tokens: &mut Vec<String>,
    ends_with_space: bool,
    aliases: &std::collections::HashMap<String, String>,
) {
    if tokens.is_empty() || (tokens.len() == 1 && !ends_with_space) {
        return;
    }
    let mut used = std::collections::HashSet::new();
    loop {
        if tokens.len() == 1 && !ends_with_space {
            break;
        }
        let Some(name) = tokens.first() else {
            break;
        };
        if !used.insert(name.clone()) {
            break;
        }
        let Some(value) = aliases.get(name) else {
            break;
        };
        let (alias_tokens, _) = tokenize(value);
        if alias_tokens.is_empty() {
            break;
        }
        tokens.splice(0..1, alias_tokens);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    Single,
    Double,
    AnsiC,
}

fn quote_closer(quote: Quote) -> char {
    match quote {
        Quote::Double => '"',
        Quote::Single | Quote::AnsiC => '\'',
    }
}

fn double_quote_escape(ch: char) -> bool {
    matches!(ch, '$' | '`' | '"' | '\\' | '\n')
}

pub fn tokenize(buffer: &str) -> (Vec<String>, bool) {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut started = false;
    let mut quote = None;
    let mut escaped = false;
    let mut trailing_space = false;
    let mut chars = buffer.chars().peekable();

    while let Some(ch) = chars.next() {
        if escaped {
            if quote == Some(Quote::AnsiC) {
                // ANSI-C quoting keeps the escape spelling in innerText, but
                // the escaped character cannot close the surrounding quote.
                token.push(ch);
            } else {
                if quote == Some(Quote::Double) && !double_quote_escape(ch) {
                    token.push('\\');
                }
                if ch != '\n' || quote != Some(Quote::Double) {
                    token.push(ch);
                }
            }
            started = true;
            escaped = false;
            trailing_space = false;
            continue;
        }
        if ch == '\\' {
            match quote {
                Some(Quote::Single) => {},
                Some(Quote::AnsiC) => {
                    token.push(ch);
                    escaped = true;
                },
                Some(Quote::Double) | None => {
                    escaped = true;
                    started = true;
                    trailing_space = false;
                    continue;
                },
            }
            if quote == Some(Quote::Single) {
                token.push(ch);
            }
            started = true;
            trailing_space = false;
            continue;
        }
        if let Some(active) = quote {
            if ch == quote_closer(active) {
                quote = None;
            } else {
                token.push(ch);
            }
            started = true;
            trailing_space = false;
            continue;
        }
        if ch == '$' && chars.peek() == Some(&'\'') {
            chars.next();
            quote = Some(Quote::AnsiC);
            started = true;
            trailing_space = false;
            continue;
        }
        match ch {
            '\'' => {
                quote = Some(Quote::Single);
                started = true;
                trailing_space = false;
            },
            '"' => {
                quote = Some(Quote::Double);
                started = true;
                trailing_space = false;
            },
            ' ' => {
                if started {
                    tokens.push(std::mem::take(&mut token));
                    started = false;
                }
                trailing_space = true;
            },
            '\t' => {
                if started {
                    tokens.push(std::mem::take(&mut token));
                    started = false;
                }
                trailing_space = false;
            },
            '\n' | '\r' => {
                if started {
                    tokens.push(std::mem::take(&mut token));
                    started = false;
                }
                trailing_space = false;
            },
            ch => {
                token.push(ch);
                started = true;
                trailing_space = false;
            },
        }
    }
    if escaped {
        // The old shell-parser drops a dangling escape from a Word/String's
        // innerText.  ANSI-C strings are the exception: their escape spelling
        // is retained, while the state machine still treats a preceding `\\`
        // as protecting a quote from closing the string.
        // The backslash was already appended when ANSI-C entered the escape
        // state; outside/inside double quotes it is omitted for a dangling
        // escape, matching the parser's `innerText`.
        started = true;
        trailing_space = false;
    }
    if started {
        tokens.push(token);
    }
    (tokens, trailing_space && quote.is_none())
}

/// Return the parser's inner text for a raw shell token.
///
/// The completion result keeps [`current_token_raw`] separately because the
/// insertion layer must delete the bytes the shell received (quotes and
/// backslash escapes included).  Generators and hooks, however, see Fig's
/// parser token text: shell quoting syntax is removed before it is used for
/// filesystem lookup, filtering, or `context.searchTerm`.
pub fn parser_inner_text(raw: &str) -> String {
    let (tokens, _) = tokenize(raw);
    match tokens.as_slice() {
        [] => String::new(),
        [token] => token.clone(),
        // Production callers pass the raw text of one shell token.  Keep an
        // already-normalized value containing literal spaces intact for
        // compatibility with direct generator callers and unit tests.
        _ => raw.to_string(),
    }
}

/// Return the raw shell token under the caret.  Matching uses the normalized
/// token returned by [`tokenize`], while insertion needs the exact bytes that
/// must be deleted (including quotes and escaped spaces).
pub fn current_token_raw(buffer: &str) -> String {
    let mut start = None;
    let mut quote = None;
    let mut ansi_c_open = false;
    let mut escaped = false;
    for (index, ch) in buffer.char_indices() {
        if escaped {
            escaped = false;
            if start.is_none() {
                start = Some(index);
            }
            continue;
        }
        if let Some(active) = quote {
            if ch == '\\' && active != Quote::Single {
                escaped = true;
                if start.is_none() {
                    start = Some(index);
                }
                continue;
            }
            if ansi_c_open {
                ansi_c_open = false;
                continue;
            }
            if ch == quote_closer(active) {
                quote = None;
            }
            continue;
        }
        if ch == '\\' {
            escaped = true;
            if start.is_none() {
                start = Some(index);
            }
        } else if ch == '$'
            && buffer
                .get(index + ch.len_utf8()..)
                .is_some_and(|rest| rest.starts_with('\''))
        {
            quote = Some(Quote::AnsiC);
            ansi_c_open = true;
            if start.is_none() {
                start = Some(index);
            }
        } else if ch == '\'' {
            quote = Some(Quote::Single);
            if start.is_none() {
                start = Some(index);
            }
        } else if ch == '"' {
            quote = Some(Quote::Double);
            if start.is_none() {
                start = Some(index);
            }
        } else if ch.is_whitespace() {
            start = None;
        } else if start.is_none() {
            start = Some(index);
        }
    }
    start.map_or_else(String::new, |index| buffer[index..].to_string())
}

/// Return the innermost command text under the caret after shell separators.
///
/// `complete` and `ranking_root_command` must share this slice so
/// `echo x && git ch` ranks and completes as `git`, not `echo`.
pub fn current_command_slice(buffer: &str) -> &str {
    let start = innermost_command_start(buffer);
    skip_leading_assignments(buffer.get(start..).unwrap_or_default())
}

/// Buffer used by lookup, ranking, and history: caret slice, then the current
/// command after separators and leading assignments.
pub fn completion_buffer(buffer: &str, cursor: Option<u32>) -> &str {
    current_command_slice(buffer_before_cursor(buffer, cursor))
}

/// True when the caret is in a new command that has no tokens yet
/// (`echo x && `), as opposed to an empty prompt or an assignment-only line.
pub fn is_fresh_command_position(buffer: &str) -> bool {
    let start = innermost_command_start(buffer);
    start > 0
        && buffer.ends_with(|ch: char| ch.is_whitespace())
        && buffer
            .get(start..)
            .is_some_and(|rest| rest.chars().all(char::is_whitespace))
}

fn peek_char(buffer: &str, index: usize) -> Option<char> {
    buffer.get(index..)?.chars().next()
}

fn skip_ws_bytes(buffer: &str, mut index: usize) -> usize {
    while index < buffer.len() {
        let Some(ch) = peek_char(buffer, index) else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

/// Shell list/pipeline operators, longest match first. Redirections such as
/// `2>&1` and `&>` are not command separators.
fn operator_len(buffer: &str, index: usize) -> Option<usize> {
    let rest = buffer.get(index..)?;
    let mut chars = rest.chars();
    let ch = chars.next()?;
    let next = chars.next();
    match (ch, next) {
        ('&' | '|', Some('&')) | ('&', Some(';')) | ('|', Some('|')) => Some(2),
        (';' | '|' | '\n', _) => Some(1),
        ('&', _) => {
            let prev = buffer.get(..index).and_then(|prefix| prefix.chars().next_back());
            if matches!(prev, Some('>' | '<')) || matches!(next, Some('>')) {
                None
            } else {
                Some(1)
            }
        },
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    Root,
    Subshell,
    Compound,
    Backtick,
}

struct CommandFrame {
    start: usize,
    kind: FrameKind,
}

fn at_statement_boundary(buffer: &str, command_start: usize, current: usize) -> bool {
    buffer
        .get(command_start..current)
        .is_some_and(|prefix| skip_leading_assignments(prefix).chars().all(char::is_whitespace))
}

fn pop_frame(frames: &mut Vec<CommandFrame>, kind: FrameKind) -> bool {
    if frames.last().is_some_and(|frame| frame.kind == kind) {
        frames.pop();
        true
    } else {
        false
    }
}

fn innermost_command_start(buffer: &str) -> usize {
    let mut frames = vec![CommandFrame {
        start: 0,
        kind: FrameKind::Root,
    }];
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0;
    while index < buffer.len() {
        let Some(ch) = peek_char(buffer, index) else {
            break;
        };
        let ch_len = ch.len_utf8();
        if escaped {
            escaped = false;
            index += ch_len;
            continue;
        }
        if ch == '\\' && quote != Some(Quote::Single) {
            escaped = true;
            index += ch_len;
            continue;
        }
        if let Some(active) = quote {
            if ch == quote_closer(active) {
                quote = None;
            }
            index += ch_len;
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(if ch == '\'' { Quote::Single } else { Quote::Double });
            index += ch_len;
            continue;
        }
        if ch == '$' {
            let next = peek_char(buffer, index + ch_len);
            if next == Some('\'') {
                quote = Some(Quote::AnsiC);
                index += ch_len + 1;
                continue;
            }
            if next == Some('(') {
                let inner = peek_char(buffer, index + ch_len + 1);
                if inner == Some('(') {
                    index += ch_len;
                    continue;
                }
                frames.push(CommandFrame {
                    start: index + ch_len + 1,
                    kind: FrameKind::Subshell,
                });
                index += ch_len + 1;
                continue;
            }
            index += ch_len;
            continue;
        }
        if ch == '`' {
            if !pop_frame(&mut frames, FrameKind::Backtick) {
                frames.push(CommandFrame {
                    start: index + ch_len,
                    kind: FrameKind::Backtick,
                });
            }
            index += ch_len;
            continue;
        }
        let command_start = frames.last().map_or(0, |frame| frame.start);
        if ch == '(' && at_statement_boundary(buffer, command_start, index) {
            frames.push(CommandFrame {
                start: index + ch_len,
                kind: FrameKind::Subshell,
            });
            index += ch_len;
            continue;
        }
        if ch == '{' && at_statement_boundary(buffer, command_start, index) {
            frames.push(CommandFrame {
                start: index + ch_len,
                kind: FrameKind::Compound,
            });
            index += ch_len;
            continue;
        }
        if ch == ')' && pop_frame(&mut frames, FrameKind::Subshell) {
            index += ch_len;
            continue;
        }
        if ch == '}' && pop_frame(&mut frames, FrameKind::Compound) {
            index += ch_len;
            continue;
        }
        if let Some(op_len) = operator_len(buffer, index) {
            let next_start = skip_ws_bytes(buffer, index + op_len);
            if let Some(frame) = frames.last_mut() {
                frame.start = next_start;
            }
            index += op_len;
            continue;
        }
        index += ch_len;
    }
    frames.last().map_or(0, |frame| frame.start)
}

fn is_fd_redirection_token(token: &str) -> bool {
    let bytes = token.as_bytes();
    let mut index = 0;
    let mut saw_fd = false;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        saw_fd = true;
        index += 1;
    }
    match bytes.get(index) {
        Some(b'>' | b'<') => {
            index += 1;
            let mut saw_amp = false;
            if bytes.get(index) == Some(&b'&') {
                saw_amp = true;
                index += 1;
            }
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            index == bytes.len() && (saw_fd || saw_amp)
        },
        _ => false,
    }
}

fn is_assignment_token(token: &str) -> bool {
    let mut chars = token.chars();
    let mut saw_name = false;
    for ch in chars.by_ref() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '[' || ch == ']' {
            saw_name = true;
            continue;
        }
        if ch == '+' {
            return saw_name && chars.next() == Some('=');
        }
        return saw_name && ch == '=';
    }
    false
}

fn raw_first_token_end(buffer: &str) -> usize {
    let mut started = false;
    let mut quote = None;
    let mut ansi_c_open = false;
    let mut escaped = false;
    let mut end = 0;
    for (index, ch) in buffer.char_indices() {
        if escaped {
            escaped = false;
            started = true;
            end = index + ch.len_utf8();
            continue;
        }
        if let Some(active) = quote {
            if ch == '\\' && active != Quote::Single {
                escaped = true;
                started = true;
                end = index + ch.len_utf8();
                continue;
            }
            if ansi_c_open {
                ansi_c_open = false;
                started = true;
                end = index + ch.len_utf8();
                continue;
            }
            started = true;
            end = index + ch.len_utf8();
            if ch == quote_closer(active) {
                quote = None;
            }
            continue;
        }
        if ch == '\\' {
            escaped = true;
            started = true;
            end = index + ch.len_utf8();
            continue;
        }
        if ch == '$'
            && buffer
                .get(index + ch.len_utf8()..)
                .is_some_and(|rest| rest.starts_with('\''))
        {
            quote = Some(Quote::AnsiC);
            ansi_c_open = true;
            started = true;
            end = index + ch.len_utf8();
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(if ch == '\'' { Quote::Single } else { Quote::Double });
            started = true;
            end = index + ch.len_utf8();
            continue;
        }
        if ch.is_whitespace() {
            if started {
                return index;
            }
            continue;
        }
        started = true;
        end = index + ch.len_utf8();
    }
    if started { end } else { 0 }
}

fn skip_leading_assignments(slice: &str) -> &str {
    let mut rest = slice;
    loop {
        let start = skip_ws_bytes(rest, 0);
        if start >= rest.len() {
            return "";
        }
        let candidate = rest.get(start..).unwrap_or_default();
        let Some(first) = tokenize(candidate).0.into_iter().next() else {
            return "";
        };
        let token_end = raw_first_token_end(candidate);
        if token_end == 0 {
            return candidate;
        }
        let raw_token = candidate.get(..token_end).unwrap_or_default();
        // Quoted words are never assignments; the old parser matches `name=`
        // against the raw buffer, so `'FOO=1'`/`$'FOO=1'` keep the literal as
        // argv0.
        if (raw_token.starts_with(['\'', '"']) || raw_token.starts_with("$'"))
            || !(is_assignment_token(&first) || is_fd_redirection_token(&first))
        {
            return candidate;
        }
        rest = candidate.get(token_end..).unwrap_or_default();
    }
}

/// Commands listed by `autocomplete.disableForCommands` are intentionally a
/// hard opt-out: do not load their spec-backed rows or invoke any fallback
/// generator.  The setting is an array in the WebView configuration, so a
/// malformed value is treated as unset rather than disabling completion for
/// every command.
fn command_is_disabled_from(commands: &[String], command: &str) -> bool {
    commands.iter().any(|disabled| disabled == command)
}

pub(crate) fn help_template_suggestions(root: &Spec, current: &Spec, query: &str, fuzzy: bool) -> Vec<Suggestion> {
    let triggering: HashSet<&str> = current.names.iter().map(String::as_str).collect();
    root.subcommands
        .iter()
        .filter(|sub| !sub.names.iter().any(|name| triggering.contains(name.as_str())))
        .filter_map(|sub| {
            let name = sub
                .names
                .iter()
                .find(|candidate| query.is_empty() || matches_query(candidate, query, fuzzy))?;
            Some(
                Suggestion::new(name.clone(), sub.description.clone(), "special")
                    .with_insert_value(name.clone())
                    .with_primary_name(sub.names.first().cloned()),
            )
        })
        .collect()
}

fn command_is_disabled(settings: &fastab_settings::settings::Settings, command: &str) -> bool {
    settings
        .get::<Vec<String>>("autocomplete.disableForCommands")
        .ok()
        .flatten()
        .is_some_and(|commands| command_is_disabled_from(&commands, command))
}

#[derive(Debug)]
pub(crate) struct ActiveArg {
    pub arg: ArgSpec,
    /// The parser's real value slot, including a loadSpec root switch and
    /// option argument index. Missing for values beyond a variadic slot.
    pub slot: Option<crate::history::ArgSlot>,
    /// Normalized value used to filter generator output.
    pub query: String,
    /// Raw shell text used as the result search term for deletion.
    pub search_term: String,
    /// Fig `onlySuggestArgs`: the caret is inside an attached option value
    /// (`--opt=value`, `-ovalue`). Subcommands and sibling options stay out
    /// of that list. A separated value is not this case; the walker's
    /// `canConsume*` flags decide those.
    pub only_suggest_args: bool,
    pub source: ActiveArgSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveArgSource {
    OptionName,
    OptionValue,
    PositionalValue,
}

#[derive(Debug)]
pub(crate) struct CompletionContext {
    pub spec: Arc<Spec>,
    pub active_arg: Option<ActiveArg>,
    /// Spec whose subcommands `template: help` lists: the node entered just
    /// before `current`, matching Fig's last-subcommand-before-help walk.
    pub help_parent: Arc<Spec>,
    /// The effective persistent option set after walking through any parent
    /// subcommands/loadSpecs. The WebView parser mutates this set onto the
    /// child completion object before collecting rows. Entries share the
    /// spec's own `Arc`s, so walking a subcommand does not copy the options.
    pub persistent_options: Vec<Arc<OptionSpec>>,
    /// Options consumed in the current parser scope. Entering a subcommand or
    /// a loadSpec resets this list, matching `passedOptions` in the WebView.
    pub passed_options: Vec<OptionSpec>,
    /// Parser directives inherited along the current spec path.
    pub parser_directives: ParserDirectives,
    /// Fig `canConsumeOptions` at the final parser state.
    pub options_allowed: bool,
    /// Fig `canConsumeSubcommands` at the final parser state. Entering a
    /// positional argument clears this until the next subcommand or loadSpec.
    pub subcommands_allowed: bool,
    /// Fig `isEndOfOptions`. Combined with [`ActiveArg::only_suggest_args`]
    /// this is `onlySuggestArgs`, which suppresses both subcommands and options.
    pub end_of_options: bool,
    /// The parser entered a spec through an argument loadSpec, command, or
    /// script transition. Public AI only accepts literal paths in the pinned
    /// tree, so this remains set even if the loaded node reuses pinned names.
    pub crossed_loaded_spec: bool,
    /// Argument slots the token being typed fills, for `template: history`.
    pub history_slots: Vec<crate::history::ArgSlot>,
}

#[derive(Debug, Clone)]
struct OptionArgState {
    /// The option whose argument is being filled, for the history trace.
    option: OptionSpec,
    arg: ArgSpec,
    /// Number of values already consumed for this option argument. A zero
    /// count means the flag was entered and is waiting for its first value.
    count: usize,
}

fn merge_parser_directives(parent: &ParserDirectives, spec: &Spec) -> ParserDirectives {
    let child = spec.parser_directives.as_ref();
    ParserDirectives {
        options_must_precede_arguments: child
            .and_then(|directives| directives.options_must_precede_arguments)
            .or(parent.options_must_precede_arguments),
        flags_are_posix_noncompliant: child
            .and_then(|directives| directives.flags_are_posix_noncompliant)
            .or(parent.flags_are_posix_noncompliant),
        option_arg_separators: child
            .and_then(|directives| directives.option_arg_separators.clone())
            .or_else(|| parent.option_arg_separators.clone()),
        alias: child
            .and_then(|directives| directives.alias.clone())
            .or_else(|| parent.alias.clone()),
        js_alias: child
            .and_then(|directives| directives.js_alias.clone())
            .or_else(|| parent.js_alias.clone()),
    }
}

fn inherit_subcommand_directives(parent: &ParserDirectives, spec: &Spec) -> ParserDirectives {
    spec.parser_directives.clone().unwrap_or_else(|| parent.clone())
}

fn options_can_break_variadic_arg(arg: &ArgSpec) -> bool {
    arg.options_can_break_variadic_arg != Some(false)
}

fn can_consume_subcommands(option_arg: Option<&OptionArgState>, entered_args: bool) -> bool {
    !option_arg.is_some_and(|state| state.arg.is_variadic || !state.arg.is_optional) && !entered_args
}

fn can_consume_options(
    directives: &ParserDirectives,
    after_double_dash: bool,
    entered_args: bool,
    option_arg: Option<&OptionArgState>,
    subcommand_arg: Option<&ArgSpec>,
    subcommand_variadic_count: usize,
) -> bool {
    if after_double_dash {
        return false;
    }
    if directives.options_must_precede_arguments == Some(true) && entered_args {
        return false;
    }
    if let Some(state) = option_arg
        && (state.arg.is_variadic || !state.arg.is_optional)
    {
        return state.arg.is_variadic && state.count > 0 && options_can_break_variadic_arg(&state.arg);
    }
    if let Some(arg) = subcommand_arg
        && arg.is_variadic
        && subcommand_variadic_count > 0
        && !options_can_break_variadic_arg(arg)
    {
        return false;
    }
    true
}

#[derive(Debug, Clone, Copy)]
struct ShortOptionChain<'spec, 'token> {
    option: &'spec OptionSpec,
    /// Portion of the token occupied by short options, excluding an attached
    /// value (`-amfoo` -> `-am`).
    prefix: &'token str,
    attached_value: Option<&'token str>,
}

#[derive(Debug, Clone, Copy)]
struct ResolvedOptionToken<'spec, 'token> {
    option: &'spec OptionSpec,
    attached_value: Option<&'token str>,
    attached_separator: Option<char>,
    chain: Option<ShortOptionChain<'spec, 'token>>,
}

pub(crate) fn resolve_context(
    root: Arc<Spec>,
    tokens: &mut Vec<String>,
    ends_with_space: bool,
    query: &str,
    raw_query: &str,
    registry: Option<&mut Registry>,
) -> CompletionContext {
    let current_limit = if ends_with_space {
        tokens.len()
    } else {
        tokens.len().saturating_sub(1)
    };
    let mut trace = crate::history::WalkTrace::new(root.as_ref());
    let walked = walk_spec(root, tokens, current_limit, registry, Some(&mut trace));
    let persistent_refs: Vec<&OptionSpec> = walked.persistent_options.iter().map(Arc::as_ref).collect();
    // `walk_spec` splices `parserDirectives.alias` expansions into `tokens`
    // in place (Fig: `tokens = currentCommand.tokens.slice(startIndex)`), so
    // the boundary of the token being typed is the walker's, not the one
    // computed from the buffer: `git co ` with `co = checkout -b` has to
    // land on `-b`'s argument, not `checkout`'s first positional.
    let active = active_arg(
        walked.spec.as_ref(),
        &persistent_refs,
        tokens,
        walked.path_end,
        walked.limit,
        ends_with_space,
        query,
        raw_query,
        &walked.parser_directives,
        &trace,
    );
    CompletionContext {
        spec: walked.spec,
        active_arg: active,
        help_parent: walked.parent,
        persistent_options: walked.persistent_options,
        passed_options: walked.passed_options,
        parser_directives: walked.parser_directives,
        options_allowed: walked.options_allowed,
        subcommands_allowed: walked.subcommands_allowed,
        end_of_options: walked.end_of_options,
        crossed_loaded_spec: walked.crossed_loaded_spec,
        history_slots: trace.current,
    }
}

struct WalkedSpec {
    spec: Arc<Spec>,
    parent: Arc<Spec>,
    path_end: usize,
    /// Index of the token being typed after alias expansion grew `tokens`.
    limit: usize,
    persistent_options: Vec<Arc<OptionSpec>>,
    passed_options: Vec<OptionSpec>,
    parser_directives: ParserDirectives,
    options_allowed: bool,
    subcommands_allowed: bool,
    end_of_options: bool,
    crossed_loaded_spec: bool,
}

/// Parse a finished history command with the same walker the buffer uses,
/// recording which argument slot every token filled into `trace`. `limit`
/// is the full length: there is no token being typed.
pub(crate) fn annotate_history_command(
    root: Arc<Spec>,
    tokens: &mut Vec<String>,
    registry: &mut Registry,
    trace: &mut crate::history::WalkTrace,
) {
    let limit = tokens.len();
    walk_spec(root, tokens, limit, Some(registry), Some(trace));
}

fn walk_spec(
    root: Arc<Spec>,
    tokens: &mut Vec<String>,
    mut limit: usize,
    mut registry: Option<&mut Registry>,
    mut trace: Option<&mut crate::history::WalkTrace>,
) -> WalkedSpec {
    let mut current = root;
    apply_generate_spec(&mut current, tokens);
    let mut parent = current.clone();
    let mut index = 1;
    let mut path_end = 1usize;
    let mut positional = 0usize;
    let mut after_double_dash = false;
    let mut persistent_options = merge_persistent_options(&[], current.as_ref());
    let mut passed_options = Vec::new();
    let mut parser_directives = current.parser_directives.clone().unwrap_or_default();
    let mut option_arg = None;
    let mut entered_args = false;
    let mut subcommand_variadic_count = 0usize;
    let mut crossed_loaded_spec = false;
    let mut substituted_aliases = HashSet::new();
    while index < limit {
        let token = &tokens[index];
        let persistent_refs: Vec<&OptionSpec> = persistent_options.iter().map(Arc::as_ref).collect();
        let subcommand_arg = positional_arg(&current.args, positional);
        let options_allowed = can_consume_options(
            &parser_directives,
            after_double_dash,
            entered_args,
            option_arg.as_ref(),
            subcommand_arg,
            subcommand_variadic_count,
        );
        let subcommands_allowed = can_consume_subcommands(option_arg.as_ref(), entered_args);
        if !after_double_dash && token == "--" && options_allowed {
            after_double_dash = true;
            index += 1;
            substituted_aliases.clear();
            continue;
        }
        if !after_double_dash && token.starts_with('-') && options_allowed {
            if let Some(resolved) =
                resolve_option_token_with_persistent(current.as_ref(), &persistent_refs, token, &parser_directives)
            {
                let option = resolved.option.clone();
                for passed in options_in_token(current.as_ref(), &persistent_refs, token, &parser_directives) {
                    passed_options.push(passed.clone());
                }
                if let Some(arg) = option.args.first() {
                    if let Some(attached) = resolved.attached_value {
                        if let Some(trace) = trace.as_deref_mut() {
                            trace.record_option_arg(&option, 0, attached);
                        }
                        option_arg = arg.is_variadic.then(|| OptionArgState {
                            option: option.clone(),
                            arg: arg.clone(),
                            count: 1,
                        });
                        index += 1;
                        if let Some(next) = next_spec_after_arg(registry.as_deref_mut(), arg, attached) {
                            if let Some(trace) = trace.as_deref_mut() {
                                trace.enter_root(next.as_ref());
                            }
                            let restart_parser = load_restarts_parser(arg);
                            crossed_loaded_spec = true;
                            enter_loaded_spec(
                                &mut current,
                                &mut parent,
                                &mut path_end,
                                &mut positional,
                                &mut persistent_options,
                                &mut parser_directives,
                                &mut passed_options,
                                &mut option_arg,
                                &mut entered_args,
                                &mut subcommand_variadic_count,
                                next,
                                index,
                                tokens,
                                restart_parser,
                                &mut after_double_dash,
                            );
                        }
                        substituted_aliases.clear();
                        continue;
                    }
                    if index + 1 < limit
                        && (should_consume_option_value(
                            current.as_ref(),
                            &option,
                            &tokens[index + 1],
                            &parser_directives,
                        ) || !arg.is_optional)
                    {
                        let value = tokens[index + 1].clone();
                        if let Some(trace) = trace.as_deref_mut() {
                            trace.record_option_arg(&option, 0, &value);
                        }
                        option_arg = arg.is_variadic.then(|| OptionArgState {
                            option: option.clone(),
                            arg: arg.clone(),
                            count: 1,
                        });
                        index += 2;
                        if let Some(next) = next_spec_after_arg(registry.as_deref_mut(), arg, &value) {
                            if let Some(trace) = trace.as_deref_mut() {
                                trace.enter_root(next.as_ref());
                            }
                            let restart_parser = load_restarts_parser(arg);
                            crossed_loaded_spec = true;
                            enter_loaded_spec(
                                &mut current,
                                &mut parent,
                                &mut path_end,
                                &mut positional,
                                &mut persistent_options,
                                &mut parser_directives,
                                &mut passed_options,
                                &mut option_arg,
                                &mut entered_args,
                                &mut subcommand_variadic_count,
                                next,
                                index,
                                tokens,
                                restart_parser,
                                &mut after_double_dash,
                            );
                        }
                        substituted_aliases.clear();
                        continue;
                    }
                    option_arg = Some(OptionArgState {
                        option: option.clone(),
                        arg: arg.clone(),
                        count: 0,
                    });
                }
                index += 1;
                substituted_aliases.clear();
                continue;
            }
        }
        if option_arg.is_some() {
            let token = tokens[index].clone();
            if let Some(state) = option_arg.as_ref()
                && let Some(alias_value) = resolve_arg_alias(&state.arg, &token)
                && substituted_aliases.insert(token.clone())
                && let Some(extra) = substitute_token_alias(tokens, index, &alias_value)
            {
                limit += extra;
                continue;
            }
            let state = option_arg.expect("checked above");
            if let Some(trace) = trace.as_deref_mut() {
                trace.record_option_arg(&state.option, state.count, &token);
            }
            option_arg = state.arg.is_variadic.then_some(OptionArgState {
                option: state.option,
                arg: state.arg,
                count: state.count + 1,
            });
            index += 1;
            substituted_aliases.clear();
            continue;
        }
        if !after_double_dash && subcommands_allowed && !token.starts_with('-') {
            if let Some(next) = current.find_subcommand(token) {
                if let Some(trace) = trace.as_deref_mut() {
                    trace.enter_subcommand(next);
                }
                let path_reference = match next.load_spec.as_ref() {
                    Some(LoadSpec::Path(reference)) => Some(reference.clone()),
                    _ => None,
                };
                parent = Arc::clone(&current);
                let mut entered = Arc::clone(next);
                if let Some(reference) = path_reference {
                    if let Some(registry) = registry.as_deref_mut() {
                        if let Some(loaded) = registry.load_referenced_spec(&reference) {
                            // Keep the command-line names. The new node shares
                            // the loaded file's child Arcs. The cached file and
                            // the parent stub stay on their own Arcs.
                            let mut owned = (*entered).clone();
                            crate::ir::replace_spec_with_loaded(&mut owned, (*loaded).clone());
                            owned.meta.ai_resolved_reference = true;
                            entered = Arc::new(owned);
                        }
                    }
                }
                current = entered;
                apply_generate_spec(&mut current, tokens);
                apply_js_load_spec(&mut current, token);
                index += 1;
                path_end = index;
                positional = 0;
                persistent_options = merge_persistent_options(&persistent_options, current.as_ref());
                parser_directives = inherit_subcommand_directives(&parser_directives, current.as_ref());
                passed_options.clear();
                entered_args = false;
                subcommand_variadic_count = 0;
                substituted_aliases.clear();
                continue;
            }
        }
        // A completed positional argument can replace the active spec.  The
        // boundary moves past that token so the loaded spec does not recount
        // the value as its own first positional argument.
        if let Some(arg) = subcommand_arg {
            let token = tokens[index].clone();
            if let Some(alias_value) = resolve_arg_alias(arg, &token)
                && substituted_aliases.insert(token.clone())
                && let Some(extra) = substitute_token_alias(tokens, index, &alias_value)
            {
                limit += extra;
                continue;
            }
            entered_args = true;
            // Fig's `walkSubcommand` counts every positional annotation in
            // the scope, variadic or not, and looks up `args[n]` by that count.
            let annotation_index = positional + subcommand_variadic_count;
            if arg.is_variadic {
                subcommand_variadic_count += 1;
            } else {
                positional += 1;
            }
            index += 1;
            let next = next_spec_after_arg(registry.as_deref_mut(), arg, &token);
            if let Some(trace) = trace.as_deref_mut() {
                // Fig re-parses from a command-valued token as a new root
                // (`sudo curl …` annotates `curl` as the curl spec), so it
                // is not a value of `sudo`'s argument.
                match next.as_ref() {
                    Some(next) => trace.enter_root(next.as_ref()),
                    None => trace.record_positional(&current.args, annotation_index, &token),
                }
            }
            let restart_parser = next.is_some() && load_restarts_parser(arg);
            if let Some(next) = next {
                crossed_loaded_spec = true;
                enter_loaded_spec(
                    &mut current,
                    &mut parent,
                    &mut path_end,
                    &mut positional,
                    &mut persistent_options,
                    &mut parser_directives,
                    &mut passed_options,
                    &mut option_arg,
                    &mut entered_args,
                    &mut subcommand_variadic_count,
                    next,
                    index,
                    tokens,
                    restart_parser,
                    &mut after_double_dash,
                );
            }
            substituted_aliases.clear();
            continue;
        }
        positional += 1;
        index += 1;
        substituted_aliases.clear();
    }
    if let Some(trace) = trace {
        trace.set_current(
            &current.args,
            positional + subcommand_variadic_count,
            option_arg.as_ref().map(|state| (&state.option, state.count)),
        );
    }
    let subcommand_arg = positional_arg(&current.args, positional);
    let options_allowed = can_consume_options(
        &parser_directives,
        after_double_dash,
        entered_args,
        option_arg.as_ref(),
        subcommand_arg,
        subcommand_variadic_count,
    );
    let subcommands_allowed = can_consume_subcommands(option_arg.as_ref(), entered_args);
    WalkedSpec {
        spec: current,
        parent,
        path_end,
        limit,
        persistent_options,
        passed_options,
        parser_directives,
        options_allowed,
        subcommands_allowed,
        end_of_options: after_double_dash,
        crossed_loaded_spec,
    }
}

fn resolve_arg_alias(arg: &ArgSpec, token: &str) -> Option<String> {
    let directives = arg.parser_directives.as_ref()?;
    if let Some(literal) = directives.alias.as_deref().filter(|value| !value.is_empty()) {
        return Some(literal.to_string());
    }
    let hook_id = directives.js_alias.as_deref()?;
    let cwd = crate::hook_backend::current_cwd()?;
    let timeout = dynamic_hook_timeout();
    crate::hook_backend::dispatch_alias(hook_id, token, &cwd, timeout)
}

fn substitute_token_alias(tokens: &mut Vec<String>, index: usize, alias_value: &str) -> Option<usize> {
    let (alias_tokens, _) = tokenize(alias_value.trim());
    if alias_tokens.is_empty() {
        return None;
    }
    let extra = alias_tokens.len().saturating_sub(1);
    tokens.splice(index..index + 1, alias_tokens);
    Some(extra)
}

fn apply_js_load_spec(current: &mut Arc<Spec>, token: &str) {
    let Some(hook_id) = current.js_load_spec.clone() else {
        return;
    };
    let Some(cwd) = crate::hook_backend::current_cwd() else {
        return;
    };
    let timeout = dynamic_hook_timeout();
    let Some(loaded) = crate::hook_backend::dispatch_load_spec(&hook_id, token, &cwd, timeout) else {
        return;
    };
    *current = Arc::new(crate::hook_backend::merge_generated_spec(current.as_ref(), loaded));
}

fn apply_generate_spec(current: &mut Arc<Spec>, tokens: &[String]) {
    let Some(hook_id) = current.js_generate_spec.clone() else {
        return;
    };
    let Some(cwd) = crate::hook_backend::current_cwd() else {
        return;
    };
    let timeout = dynamic_hook_timeout();
    let generated = if let Some(key) = current.generate_spec_cache_key.as_deref() {
        let cache_key = format!("{}:{key}", tokens.first().cloned().unwrap_or_default());
        cached_generated_spec(&cache_key, || {
            crate::hook_backend::dispatch_generate_spec(&hook_id, tokens, &cwd, timeout)
        })
    } else {
        crate::hook_backend::dispatch_generate_spec(&hook_id, tokens, &cwd, timeout)
    };
    let Some(generated) = generated else {
        return;
    };
    *current = Arc::new(crate::hook_backend::merge_generated_spec(current.as_ref(), generated));
}

fn cached_generated_spec(cache_key: &str, run: impl FnOnce() -> Option<Spec>) -> Option<Spec> {
    crate::hook_cache::cached_spec(cache_key, run)
}

/// Dynamic parser hooks do not have an argument/generator object from which
/// to inherit a timeout.  They use the same live user setting and negative
/// value normalization as native generators: an absent or malformed setting
/// falls back to the 5-second default, while a negative value becomes zero
/// when converted to a Rust duration.
fn dynamic_hook_timeout() -> Duration {
    dynamic_hook_timeout_from_settings(&fastab_settings::settings::Settings::new())
}

fn dynamic_hook_timeout_from_settings(settings: &fastab_settings::settings::Settings) -> Duration {
    duration_from_script_timeout_ms(crate::generate::configured_script_timeout_ms_from(settings))
}

fn duration_from_script_timeout_ms(milliseconds: i64) -> Duration {
    Duration::from_millis(u64::try_from(milliseconds).unwrap_or(0))
}

#[allow(clippy::too_many_arguments)]
fn enter_loaded_spec(
    current: &mut Arc<Spec>,
    parent: &mut Arc<Spec>,
    path_end: &mut usize,
    positional: &mut usize,
    persistent_options: &mut Vec<Arc<OptionSpec>>,
    parser_directives: &mut ParserDirectives,
    passed_options: &mut Vec<OptionSpec>,
    option_arg: &mut Option<OptionArgState>,
    entered_args: &mut bool,
    subcommand_variadic_count: &mut usize,
    next: Arc<Spec>,
    index: usize,
    tokens: &[String],
    restart_parser: bool,
    after_double_dash: &mut bool,
) {
    *parent = current.clone();
    *current = next;
    apply_generate_spec(current, tokens);
    if let Some(token) = tokens.get(index.saturating_sub(1)) {
        apply_js_load_spec(current, token);
    }
    *path_end = index;
    *positional = 0;
    *persistent_options = merge_persistent_options(persistent_options, current.as_ref());
    *parser_directives = merge_parser_directives(parser_directives, current.as_ref());
    passed_options.clear();
    *option_arg = None;
    *entered_args = false;
    *subcommand_variadic_count = 0;
    // Fig re-parses a path/`isCommand` loadSpec from a fresh command, which
    // clears `isEndOfOptions`. An inline object or a JS loadSpec that returns
    // a spec object is merged into the current state and keeps the flag.
    if restart_parser {
        *after_double_dash = false;
    }
}

/// A string `loadSpec`, `isCommand`, `isScript`, or `isModule` starts a new
/// parser. A JS hook or an inline spec object does not.
fn load_restarts_parser(arg: &ArgSpec) -> bool {
    if arg.js_load_spec.is_some() {
        return false;
    }
    match &arg.load_spec {
        Some(LoadSpec::Path(_)) => true,
        Some(LoadSpec::Inline(_)) => false,
        None => arg.is_command || arg.is_script || arg.is_module.is_some(),
    }
}

/// Fig prefers a static `loadSpec` on the argument. Only when that is absent
/// do `isCommand` / `isScript` / `isModule` load another bundled spec.
fn next_spec_after_arg(registry: Option<&mut Registry>, arg: &ArgSpec, token: &str) -> Option<Arc<Spec>> {
    if let Some(hook_id) = arg.js_load_spec.as_deref() {
        return crate::hook_backend::current_cwd().and_then(|cwd| {
            let timeout = dynamic_hook_timeout();
            crate::hook_backend::dispatch_load_spec(hook_id, token, &cwd, timeout).map(Arc::new)
        });
    }
    if let Some(LoadSpec::Path(reference)) = &arg.load_spec {
        let reference = reference.clone();
        let loaded = registry?.load_referenced_spec(&reference)?;
        // The loaded file's own names become the new parser root
        // (`sudo curl` files later tokens under curl). The walk holds that
        // cached Arc and does not write it back onto the argument.
        return Some(loaded);
    }
    if let Some(LoadSpec::Inline(_)) = &arg.load_spec {
        return arg.resolved_spec.as_deref().map(|spec| Arc::new(spec.clone()));
    }
    if let Some(resolved) = arg.resolved_spec.as_deref() {
        return Some(Arc::new(resolved.clone()));
    }
    let name = dynamic_spec_name(arg, token)?;
    if name.is_empty() || name == "?" {
        return None;
    }
    registry.and_then(|registry| registry.get_arc(&name))
}

fn dynamic_spec_name(arg: &ArgSpec, token: &str) -> Option<String> {
    if let Some(prefix) = arg.is_module.as_deref() {
        if prefix.is_empty() {
            return None;
        }
        return Some(format!("{prefix}{token}"));
    }
    if arg.is_command || arg.is_script {
        return Some(command_lookup_name(token, arg.is_script));
    }
    None
}

fn command_lookup_name(token: &str, is_script: bool) -> String {
    let pathish = token.starts_with('/') || token.starts_with("./") || token.starts_with("~/");
    if is_script || pathish {
        token
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(token)
            .to_string()
    } else {
        token.to_string()
    }
}

#[allow(clippy::too_many_arguments)]
fn active_arg<'a>(
    spec: &'a Spec,
    persistent_options: &[&'a OptionSpec],
    tokens: &[String],
    path_end: usize,
    limit: usize,
    ends_with_space: bool,
    query: &str,
    raw_query: &str,
    parser_directives: &ParserDirectives,
    trace: &crate::history::WalkTrace,
) -> Option<ActiveArg> {
    let current_index = if ends_with_space {
        tokens.len()
    } else {
        tokens.len().saturating_sub(1)
    };
    let mut positional = 0usize;
    let mut index = path_end;
    let mut option_arg = None;
    let mut entered_args = false;
    let mut subcommand_variadic_count = 0usize;
    // A loadSpec transition may move `path_end` past the `--` sentinel. Keep
    // that parser state when deciding whether a leading dash is positional.
    let mut after_double_dash = tokens.iter().take(path_end).any(|token| token == "--");

    while index < limit {
        let token = &tokens[index];
        let subcommand_arg = positional_arg(&spec.args, positional);
        let options_allowed = can_consume_options(
            parser_directives,
            after_double_dash,
            entered_args,
            option_arg.as_ref(),
            subcommand_arg,
            subcommand_variadic_count,
        );
        if !after_double_dash && token == "--" && options_allowed {
            after_double_dash = true;
            index += 1;
            continue;
        }
        if !after_double_dash && token.starts_with('-') && options_allowed {
            if let Some(resolved) =
                resolve_option_token_with_persistent(spec, persistent_options, token, parser_directives)
            {
                let option = resolved.option;
                option_arg = None;
                if let Some(value) = resolved.attached_value {
                    if !ends_with_space && index == current_index {
                        let raw_value = attached_search_term(raw_query, resolved, value);
                        return option.args.first().map(|arg| ActiveArg {
                            arg: arg.clone(),
                            slot: Some(trace.slot(Some(option), 0)),
                            query: value.to_string(),
                            search_term: raw_value,
                            only_suggest_args: true,
                            source: ActiveArgSource::OptionValue,
                        });
                    }
                    if let Some(arg) = option.args.first().filter(|arg| arg.is_variadic) {
                        option_arg = Some(OptionArgState {
                            option: option.clone(),
                            arg: arg.clone(),
                            count: 1,
                        });
                    }
                    index += 1;
                    continue;
                }
                if !option.args.is_empty() {
                    if index + 1 < limit
                        && (should_consume_option_value(spec, option, &tokens[index + 1], parser_directives)
                            || option.args.first().is_some_and(|arg| !arg.is_optional))
                    {
                        option_arg = option
                            .args
                            .first()
                            .filter(|arg| arg.is_variadic)
                            .map(|arg| OptionArgState {
                                option: option.clone(),
                                arg: arg.clone(),
                                count: 1,
                            });
                        // The next completed token is the option argument.
                        index += 2;
                        continue;
                    }
                    // The token under the caret (or the empty token after a
                    // trailing space) is this option's argument.  Do not
                    // consume it as a positional argument.
                    return option.args.first().map(|arg| ActiveArg {
                        arg: arg.clone(),
                        slot: Some(trace.slot(Some(option), 0)),
                        query: query.to_string(),
                        search_term: raw_query.to_string(),
                        only_suggest_args: false,
                        source: ActiveArgSource::OptionValue,
                    });
                }
            }
        }
        if let Some(state) = option_arg {
            option_arg = Some(OptionArgState {
                option: state.option,
                arg: state.arg,
                count: state.count + 1,
            });
            index += 1;
            continue;
        }
        if let Some(arg) = subcommand_arg {
            if arg.is_variadic {
                subcommand_variadic_count += 1;
            } else {
                positional += 1;
            }
        } else {
            positional += 1;
        }
        entered_args = true;
        index += 1;
    }

    if let Some(state) = option_arg {
        return Some(ActiveArg {
            arg: state.arg,
            slot: (state.count < state.option.args.len()).then(|| trace.slot(Some(&state.option), state.count)),
            query: query.to_string(),
            search_term: raw_query.to_string(),
            only_suggest_args: false,
            source: ActiveArgSource::OptionValue,
        });
    }

    // With no trailing space the current token is intentionally excluded from
    // the consumed-token scan.  An attached option value (`--opt=value`) is
    // the one exception: inspect that raw token so its value arg remains the
    // active context and only `value` becomes the search term.
    if !ends_with_space {
        if let Some(token) = tokens.get(current_index) {
            let subcommand_arg = positional_arg(&spec.args, positional);
            let options_allowed = can_consume_options(
                parser_directives,
                after_double_dash,
                entered_args,
                option_arg.as_ref(),
                subcommand_arg,
                subcommand_variadic_count,
            );
            if options_allowed {
                if let Some(resolved) =
                    resolve_option_token_with_persistent(spec, persistent_options, token, parser_directives)
                {
                    let option = resolved.option;
                    if let Some(value) = resolved.attached_value {
                        let raw_value = attached_search_term(raw_query, resolved, value);
                        if let Some(arg) = option.args.first() {
                            return Some(ActiveArg {
                                arg: arg.clone(),
                                slot: Some(trace.slot(Some(option), 0)),
                                query: value.to_string(),
                                search_term: raw_value,
                                only_suggest_args: true,
                                source: ActiveArgSource::OptionValue,
                            });
                        }
                    }
                    // The WebView treats the last short option in a composite
                    // token as the active option. Its argument rows use an empty
                    // query, so accepting one appends to `-am` instead of deleting
                    // the chain. A lone `-m` keeps the ordinary option path.
                    if resolved.chain.is_some()
                        && let Some(arg) = option.args.first()
                    {
                        return Some(ActiveArg {
                            arg: arg.clone(),
                            slot: Some(trace.slot(Some(option), 0)),
                            query: String::new(),
                            search_term: String::new(),
                            only_suggest_args: false,
                            source: ActiveArgSource::OptionName,
                        });
                    }
                }
            }
            if token.starts_with('-') && !after_double_dash && options_allowed {
                // The token is still an option/sentinel query, not a value
                // for the command's positional arg.  Once `--` has been
                // completed, a leading dash is a legitimate positional value.
                return None;
            }
        }
    }

    let slot_index = positional + subcommand_variadic_count;
    positional_arg(&spec.args, positional).map(|arg| ActiveArg {
        arg: arg.clone(),
        slot: (slot_index < spec.args.len()).then(|| trace.slot(None, slot_index)),
        query: query.to_string(),
        search_term: raw_query.to_string(),
        only_suggest_args: false,
        source: ActiveArgSource::PositionalValue,
    })
}

fn positional_arg(args: &[ArgSpec], index: usize) -> Option<&ArgSpec> {
    let arg = args.get(index).or_else(|| args.last().filter(|arg| arg.is_variadic))?;
    if arg.name.is_empty()
        && arg.description.is_empty()
        && arg.builtin.is_none()
        && arg.builtins.is_empty()
        && arg.script.is_empty()
        && arg.templates.is_empty()
        && arg.suggestions.is_empty()
        && arg.generators.is_empty()
        && arg.js_custom.is_none()
        && arg.js_script.is_none()
        && arg.js_post_process.is_none()
        && arg.js_load_spec.is_none()
        && !arg.is_command
        && !arg.is_script
        && arg.is_module.is_none()
    {
        // An empty placeholder is still a real positional slot when it is
        // variadic; otherwise there is no useful current-arg context.
        return arg.is_variadic.then_some(arg);
    }
    Some(arg)
}

fn merge_persistent_options(parent: &[Arc<OptionSpec>], spec: &Spec) -> Vec<Arc<OptionSpec>> {
    let mut options = parent.to_vec();
    for option in &spec.persistent_options {
        if let Some(existing) = options.iter_mut().find(|existing| options_are_equal(existing, option)) {
            *existing = Arc::clone(option);
        } else {
            options.push(Arc::clone(option));
        }
    }
    options
}

fn options_are_equal(a: &OptionSpec, b: &OptionSpec) -> bool {
    a.names
        .iter()
        .any(|name| b.names.iter().any(|candidate| candidate == name))
}

fn separator_at(token: &str, directives: &ParserDirectives) -> Option<(usize, char)> {
    token.char_indices().find(|(_, character)| {
        directives
            .option_arg_separators
            .as_ref()
            .map_or(*character == '=', |separators| {
                separators
                    .iter()
                    .any(|separator| separator.chars().count() == 1 && separator.starts_with(*character))
            })
    })
}

fn find_option_in<'a>(
    spec: &'a Spec,
    persistent: &[&'a OptionSpec],
    token: &str,
    directives: &ParserDirectives,
) -> Option<&'a OptionSpec> {
    let option_name = separator_at(token, directives).map_or(token, |(index, _)| &token[..index]);
    spec.options
        .iter()
        .map(Arc::as_ref)
        .find(|option| option.names.iter().any(|name| name == option_name))
        .or_else(|| {
            persistent
                .iter()
                .copied()
                .find(|option| option.names.iter().any(|name| name == option_name))
        })
}

fn short_option_name(option: &OptionSpec) -> Option<&str> {
    option.names.iter().find_map(|name| {
        let rest = name.strip_prefix('-')?;
        (!rest.starts_with('-') && rest.chars().count() == 1).then_some(name.as_str())
    })
}

fn find_short_option_in<'a>(spec: &'a Spec, persistent: &[&'a OptionSpec], letter: char) -> Option<&'a OptionSpec> {
    spec.options
        .iter()
        .map(Arc::as_ref)
        .chain(persistent.iter().copied())
        .find(|option| {
            option.names.iter().any(|name| {
                let Some(rest) = name.strip_prefix('-') else {
                    return false;
                };
                !rest.starts_with('-') && {
                    let mut chars = rest.chars();
                    chars.next() == Some(letter) && chars.next().is_none()
                }
            })
        })
}

fn parse_short_option_chain<'spec, 'token>(
    spec: &'spec Spec,
    persistent: &[&'spec OptionSpec],
    token: &'token str,
    directives: &ParserDirectives,
) -> Option<ShortOptionChain<'spec, 'token>> {
    if directives.flags_are_posix_noncompliant == Some(true)
        || !token.starts_with('-')
        || token.starts_with("--")
        || token[1..].chars().count() < 2
    {
        return None;
    }

    let mut last = None;
    for (offset, letter) in token[1..].char_indices() {
        let option = find_short_option_in(spec, persistent, letter)?;
        let end = 1 + offset + letter.len_utf8();
        let remainder = &token[end..];
        last = Some((option, end));

        if option.args.is_empty() || remainder.is_empty() {
            continue;
        }

        let mandatory = option.args.first().is_some_and(|arg| !arg.is_optional);
        let next_is_short_option = remainder
            .chars()
            .next()
            .and_then(|next| find_short_option_in(spec, persistent, next))
            .is_some();
        if mandatory || remainder.starts_with('=') || !next_is_short_option {
            return Some(ShortOptionChain {
                option,
                prefix: &token[..end],
                attached_value: Some(remainder.strip_prefix('=').unwrap_or(remainder)),
            });
        }
    }

    let (option, end) = last?;
    Some(ShortOptionChain {
        option,
        prefix: &token[..end],
        attached_value: None,
    })
}

fn resolve_option_token_with_persistent<'spec, 'token>(
    spec: &'spec Spec,
    persistent: &[&'spec OptionSpec],
    token: &'token str,
    directives: &ParserDirectives,
) -> Option<ResolvedOptionToken<'spec, 'token>> {
    let long_form = directives.flags_are_posix_noncompliant == Some(true) || token.starts_with("--");
    // In POSIX mode a multi-letter single-dash token is parsed as a short
    // option chain before exact lookup.  This is observable when a spec has
    // both `-word` and `-w`/`-o` options: the WebView consumes the chain.
    if !long_form && token.starts_with('-') && token.len() > 1 && token[1..].chars().count() >= 2 {
        if let Some(chain) = parse_short_option_chain(spec, persistent, token, directives) {
            return Some(ResolvedOptionToken {
                option: chain.option,
                attached_value: chain.attached_value,
                attached_separator: None,
                chain: Some(chain),
            });
        }
    }
    let option = find_option_in(spec, persistent, token, directives)?;
    let (attached_separator, attached_value) = separator_at(token, directives)
        .map_or((None, None), |(index, separator)| {
            (Some(separator), Some(&token[index + separator.len_utf8()..]))
        });
    Some(ResolvedOptionToken {
        option,
        attached_value: option.args.first().and(attached_value),
        attached_separator: option.args.first().and(attached_separator),
        chain: None,
    })
}

/// Every option represented by a completed token. The active-argument walker
/// only needs the final option in a short chain, while `passedOptions` counts
/// each flag (for example both `-a` and `-b` in `-ab`).
fn options_in_token<'a>(
    spec: &'a Spec,
    persistent: &[&'a OptionSpec],
    token: &str,
    directives: &ParserDirectives,
) -> Vec<&'a OptionSpec> {
    if token == "--" || !token.starts_with('-') {
        return Vec::new();
    }
    if token.starts_with("--") || directives.flags_are_posix_noncompliant == Some(true) {
        return find_option_in(spec, persistent, token, directives)
            .into_iter()
            .collect();
    }

    let mut options = Vec::new();
    let chars = token[1..].char_indices();
    for (offset, letter) in chars {
        let Some(option) = find_short_option_in(spec, persistent, letter) else {
            break;
        };
        options.push(option);
        let end = offset + letter.len_utf8();
        let remainder = &token[1 + end..];
        if option.args.first().is_some_and(|arg| !arg.is_optional)
            || separator_at(remainder, directives).is_some()
            || remainder
                .chars()
                .next()
                .is_some_and(|next| find_short_option_in(spec, persistent, next).is_none())
        {
            break;
        }
    }
    options
}

fn attached_search_term(raw_query: &str, resolved: ResolvedOptionToken<'_, '_>, fallback: &str) -> String {
    if let Some(chain) = resolved.chain {
        return raw_query.strip_prefix(chain.prefix).map_or_else(
            || fallback.to_string(),
            |value| value.strip_prefix('=').unwrap_or(value).to_string(),
        );
    }
    resolved.attached_separator.map_or_else(
        || fallback.to_string(),
        |separator| {
            raw_query.find(separator).map_or_else(
                || fallback.to_string(),
                |index| raw_query[index + separator.len_utf8()..].to_string(),
            )
        },
    )
}

fn should_consume_option_value(spec: &Spec, option: &OptionSpec, next: &str, _directives: &ParserDirectives) -> bool {
    if next == "--" || next.starts_with('-') {
        return false;
    }
    if option.args.first().is_some_and(|arg| !arg.is_optional) {
        return true;
    }
    // Optional option arguments are ambiguous.  Prefer a child subcommand
    // when one is available, otherwise treat the next token as the value.
    spec.find_subcommand(next).is_none()
}

const FIRST_TOKEN_LOGIN_TIMEOUT: Duration = Duration::from_millis(1_500);
const FIRST_TOKEN_CACHE_TTL: Duration = Duration::from_secs(10);

fn slash_command_spec_name(command: &str) -> &str {
    if command == "bin/console" || command.ends_with("/bin/console") {
        "php/bin-console"
    } else {
        "dotslash"
    }
}

/// Root spec for a lone first token that contains a `/`. Fig checked only
/// `bin/console` before handing every other path to `dotslash`; neither a
/// `.fig` script spec nor a bundled spec with the same basename is consulted
/// until a space finishes the token.
fn single_slash_token_spec(registry: &mut Registry, command: &str) -> Option<Arc<Spec>> {
    let name = if command == "bin/console" {
        "php/bin-console"
    } else {
        "dotslash"
    };
    registry.get_arc(name)
}

fn first_token_command_names(query: &str, request: &CompleteRequest) -> Vec<(String, String)> {
    let mut names = first_token_cached_listing(request);
    if let Some(alias) = request.alias.as_deref() {
        for key in parse_alias_map(alias, request.current_shell.as_deref()).into_keys() {
            if !names.iter().any(|existing| existing == &key) {
                names.push(key);
            }
        }
    }
    let mut matched: Vec<(String, String)> = names
        .into_iter()
        .filter(|name| query.is_empty() || matches_query(name, query, request.fuzzy))
        .map(|name| (name, String::new()))
        .collect();
    matched.sort_by(|left, right| crate::query::cmp_ignore_ascii_case(&left.0, &right.0));
    matched
}

fn first_token_cached_listing(request: &CompleteRequest) -> Vec<String> {
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;
    struct Cache {
        key: String,
        fetched_at: Instant,
        names: Vec<String>,
    }
    static CACHE: OnceLock<Mutex<Option<Cache>>> = OnceLock::new();
    let key = format!(
        "{}|{}",
        request.current_shell.as_deref().unwrap_or(""),
        request.current_process.as_deref().unwrap_or("")
    );
    if let Ok(cache) = CACHE.get_or_init(|| Mutex::new(None)).lock() {
        if let Some(entry) = cache.as_ref() {
            if entry.key == key && entry.fetched_at.elapsed() < FIRST_TOKEN_CACHE_TTL {
                return entry.names.clone();
            }
        }
    }
    let mut names = first_token_login_listing(request);
    if names.is_empty() {
        names = first_token_nologin_listing(request);
    }
    if names.is_empty() {
        names = first_token_path_and_alias_listing(request);
    }
    if let Ok(mut cache) = CACHE.get_or_init(|| Mutex::new(None)).lock() {
        *cache = Some(Cache {
            key,
            fetched_at: Instant::now(),
            names: names.clone(),
        });
    }
    names
}

fn first_token_login_listing(request: &CompleteRequest) -> Vec<String> {
    let process = request
        .current_process
        .as_deref()
        .or(request.current_shell.as_deref())
        .unwrap_or("");
    let (shell, command) = if process.contains("fish") {
        ("fish", "complete -C ''")
    } else if process.contains("bash") {
        ("bash", "compgen -ac")
    } else {
        ("zsh", "print -rl -- ${(k)commands} ${(k)aliases}")
    };
    let Some(output) = crate::process::try_execute_isolated_success(
        shell,
        &["-lc".into(), command.into()],
        "",
        FIRST_TOKEN_LOGIN_TIMEOUT,
    ) else {
        return Vec::new();
    };
    dedupe_command_lines(&output)
}

fn first_token_nologin_listing(request: &CompleteRequest) -> Vec<String> {
    let process = request
        .current_process
        .as_deref()
        .or(request.current_shell.as_deref())
        .unwrap_or("");
    let (shell, command) = if process.contains("fish") {
        ("fish", "complete -C ''")
    } else if process.contains("bash") {
        ("bash", "compgen -ac")
    } else {
        ("zsh", "print -rl -- ${(k)commands} ${(k)aliases}")
    };
    let Some(output) = crate::process::try_execute_isolated_success(
        shell,
        &["-c".into(), command.into()],
        "",
        FIRST_TOKEN_LOGIN_TIMEOUT,
    ) else {
        return Vec::new();
    };
    dedupe_command_lines(&output)
}

fn first_token_path_and_alias_listing(request: &CompleteRequest) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    if let Some(alias) = request.alias.as_deref() {
        for key in parse_alias_map(alias, request.current_shell.as_deref()).into_keys() {
            names.insert(key);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                names.insert(name);
            }
        }
    }
    names.into_iter().collect()
}

fn dedupe_command_lines(output: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut names = Vec::new();
    for line in output.lines() {
        let name = line.split('\t').next().unwrap_or(line).trim();
        if name.is_empty() || !seen.insert(name.to_string()) {
            continue;
        }
        names.push(name.to_string());
    }
    names
}

fn first_token_result(
    request: &CompleteRequest,
    raw_search_term: String,
    normalized_search_term: String,
    settings: &fastab_settings::settings::Settings,
) -> CompleteResult {
    if !request.suggest_first_token {
        return CompleteResult {
            suggestions: Vec::new(),
            fuzzy: request.fuzzy,
            search_term: raw_search_term,
            match_term: normalized_search_term,
            ..CompleteResult::default()
        };
    }
    let mut suggestions = Vec::new();
    for (name, description) in first_token_command_names(&normalized_search_term, request) {
        // The legacy first-token generator returned ordinary `arg` rows
        // with an explicit raw insertValue and no shouldAddSpace flag.
        // Keeping that shape matters: accepting `git` must insert only
        // the missing token, while the optional auto-execute wrapper is
        // the only path that appends a newline.
        suggestions.push(Suggestion::new(name.clone(), description, "arg").with_insert_value(name));
    }
    add_exact_auto_execute(
        &mut suggestions,
        &normalized_search_term,
        settings.get_bool_or("autocomplete.hideAutoExecuteSuggestion", false),
        settings.get_bool_or("autocomplete.onlyShowOnTab", false),
        settings.get_bool_or("autocomplete.immediatelyRunDangerousCommands", false),
    );
    CompleteResult {
        suggestions,
        fuzzy: request.fuzzy,
        search_term: raw_search_term,
        match_term: normalized_search_term,
        ..CompleteResult::default()
    }
}

pub fn complete(registry: &mut Registry, request: &CompleteRequest) -> CompleteResult {
    complete_with_settings(registry, request, &fastab_settings::settings::Settings::new())
}

/// Isolated settings input for deterministic engine baselines; production uses
/// the real settings provider through [`complete`].
pub(crate) fn complete_with_settings(
    registry: &mut Registry,
    request: &CompleteRequest,
    settings: &fastab_settings::settings::Settings,
) -> CompleteResult {
    let raw = buffer_before_cursor(&request.buffer, request.cursor);
    let buffer = completion_buffer(&request.buffer, request.cursor);
    let (mut tokens, ends_with_space) = tokenize(buffer);
    if let Some(alias) = request.alias.as_deref() {
        expand_alias_tokens(&mut tokens, ends_with_space, alias, request.current_shell.as_deref());
    }
    if tokens.is_empty() {
        if is_fresh_command_position(raw) {
            return first_token_result(request, String::new(), String::new(), settings);
        }
        return CompleteResult {
            fuzzy: request.fuzzy,
            ..CompleteResult::default()
        };
    }

    let command = &tokens[0];
    let raw_search_term = if ends_with_space {
        String::new()
    } else {
        current_token_raw(buffer)
    };
    let normalized_search_term = if ends_with_space {
        String::new()
    } else {
        tokens.last().cloned().unwrap_or_default()
    };
    let prefer_verbose = settings.get_bool_or("autocomplete.preferVerboseSuggestions", false);

    if command_is_disabled(settings, command) {
        return CompleteResult {
            suggestions: Vec::new(),
            fuzzy: request.fuzzy,
            search_term: raw_search_term,
            match_term: normalized_search_term,
            ..CompleteResult::default()
        };
    }

    if tokens.len() == 1 && !ends_with_space && !command.contains('/') {
        return first_token_result(request, raw_search_term, normalized_search_term, settings);
    }

    let query = normalized_search_term.clone();

    let root = if tokens.len() == 1 && !ends_with_space {
        // Fig `parseArguments`, single-token branch: a first token with a
        // `/` is a script path being typed, so it completes against
        // `dotslash` (files) regardless of what its basename happens to
        // match. `./git` lists `./git*`, not git's subcommands filtered to
        // nothing. `bin/console` is the one special case.
        single_slash_token_spec(registry, command)
    } else {
        root_spec_for_command(registry, request, command, settings)
    };
    let Some(root) = root else {
        return CompleteResult {
            suggestions: Vec::new(),
            fuzzy: request.fuzzy,
            search_term: raw_search_term,
            match_term: query,
            ..CompleteResult::default()
        };
    };

    let context = resolve_context(
        root.clone(),
        &mut tokens,
        ends_with_space,
        &query,
        &raw_search_term,
        Some(registry),
    );
    let only_suggest_args = context.end_of_options
        || context
            .active_arg
            .as_ref()
            .is_some_and(|active| active.only_suggest_args);
    let public_ai_context = crate::public_ai::context(
        registry,
        request,
        &root,
        context.spec.as_ref(),
        &tokens,
        crate::public_ai::QueryState {
            subcommands_allowed: context.subcommands_allowed,
            options_allowed: context.options_allowed,
            end_of_options: context.end_of_options,
            active_arg_source: context.active_arg.as_ref().map(|active| active.source),
            active_arg_only_suggest_args: only_suggest_args,
            crossed_loaded_spec: context.crossed_loaded_spec,
        },
    );
    let fuzzy = effective_fuzzy(
        request.fuzzy,
        Some(context.spec.as_ref()),
        context.active_arg.as_ref().map(|active| &active.arg),
    );
    let current = context.spec.as_ref();
    let persistent_refs: Vec<&OptionSpec> = context.persistent_options.iter().map(Arc::as_ref).collect();
    let option_chain = (!ends_with_space)
        .then(|| {
            parse_short_option_chain(
                current,
                &persistent_refs,
                &normalized_search_term,
                &context.parser_directives,
            )
        })
        .flatten();
    let open_option_chain = option_chain.filter(|chain| chain.attached_value.is_none());
    let (query, search_term) = context.active_arg.as_ref().map_or_else(
        || (query.clone(), raw_search_term.clone()),
        |active| (active.query.clone(), active.search_term.clone()),
    );
    let current_arg = context.active_arg.as_ref().map(|active| CurrentArg {
        name: active.arg.name.clone(),
        description: active.arg.description.clone(),
    });

    // Fig `getResultFromState`: args are always eligible, additional
    // suggestions are always pushed, and subcommands/options are added only
    // when their consume flags are set. `onlySuggestArgs` (a finished `--`,
    // or the caret inside `--opt=value` / `-ovalue`) suppresses both.
    // An open short-option chain is filtered down to options and args by
    // `getAllSuggestions`, which is the chain branch below.
    let suggest_subcommands = context.subcommands_allowed && !only_suggest_args && open_option_chain.is_none();
    let mut suggestions = if suggest_subcommands {
        let subcommands = sorted_named_refs(&current.subcommands, |spec| spec.names.as_slice());
        collect_named(
            &subcommands,
            |spec| spec.names.as_slice(),
            |spec| spec.description.as_str(),
            |spec| args_hint(&spec.args),
            |spec| &spec.meta,
            |spec| spec.meta.priority,
            |spec| {
                spec.meta.should_add_space.unwrap_or_else(|| {
                    should_add_space(&spec.args, spec.requires_subcommand, !spec.subcommands.is_empty())
                })
            },
            |spec| spec.meta.separator_to_add.clone(),
            |spec| spec.args.first().is_some_and(|arg| !arg.is_optional),
            "subcommand",
            &query,
            &search_term,
            fuzzy,
            prefer_verbose,
            |spec| {
                public_ai_context.is_some()
                    && !spec.meta.ai_resolved_reference
                    && !spec.meta.ai_generated
                    && spec.load_spec.is_none()
                    && spec.js_load_spec.is_none()
            },
        )
    } else {
        Vec::new()
    };
    // Fig presents argument/generator results before additional shortcuts and
    // options. Keep that ordering so a generated git alias does not jump
    // behind the static option list while ranking is still settling.
    if let Some(active) = context.active_arg.as_ref() {
        let mut active_suggestions = crate::generate::generate_for_arg_with_history(
            &active.arg,
            &tokens,
            &query,
            &search_term,
            &request.cwd,
            fuzzy,
            || {
                crate::generate::history_store().arg_values(
                    registry,
                    request.alias.as_deref(),
                    request.current_shell.as_deref(),
                    &context.history_slots,
                )
            },
        );
        if active.arg.templates.contains(&Template::Help) {
            active_suggestions.extend(help_template_suggestions(
                context.help_parent.as_ref(),
                current,
                &query,
                fuzzy,
            ));
        }
        let acceptance_scope = active
            .slot
            .as_ref()
            .and_then(|slot| crate::rank::argument_scope(&request.cwd, slot));
        for suggestion in &mut active_suggestions {
            suggestion.argument_value = true;
            suggestion.acceptance_scope = acceptance_scope.clone();
        }
        if open_option_chain.is_some() {
            for suggestion in &mut active_suggestions {
                suggestion.query_term = Some(String::new());
            }
        }
        suggestions.extend(active_suggestions);
    }
    let additional_items = sorted_named_refs(&current.additional_suggestions, |seed| seed.names.as_slice());
    let mut additional = collect_named(
        &additional_items,
        |seed| seed.names.as_slice(),
        |seed| seed.description.as_str(),
        |seed| seed.args_hint.clone(),
        |seed| &seed.meta,
        |seed| seed.meta.priority,
        |seed| seed.meta.should_add_space.unwrap_or(false),
        |seed| seed.meta.separator_to_add.clone(),
        |_| false,
        "arg",
        if open_option_chain.is_some() { "" } else { &query },
        if open_option_chain.is_some() { "" } else { &search_term },
        fuzzy,
        prefer_verbose,
        |_| false,
    );
    if open_option_chain.is_some() {
        for suggestion in &mut additional {
            suggestion.query_term = Some(String::new());
        }
    }
    // `getStaticSuggestions` gives untyped additional suggestions the same
    // template tile used by the WebView, with a right-arrow badge. Preserve
    // that affordance while leaving explicitly typed/iconed rows untouched.
    for suggestion in &mut additional {
        let untyped_seed = current.additional_suggestions.iter().any(|seed| {
            seed.meta.suggestion_type.is_none()
                && seed.meta.icon.is_none()
                && seed.names.iter().any(|name| name == &suggestion.name)
        });
        if untyped_seed && suggestion.icon.is_none() {
            suggestion.icon = Some("fig://template?color=628dad&badge=➡️".into());
        }
    }
    suggestions.extend(additional);
    let include_options = context.options_allowed && !only_suggest_args;
    if !only_suggest_args && let Some(chain) = open_option_chain {
        suggestions.extend(option_chain_suggestions(
            current,
            &persistent_refs,
            chain,
            &context.passed_options,
            &context.parser_directives,
        ));
    } else if include_options {
        suggestions.extend(collect_option_suggestions(
            current,
            &persistent_refs,
            &context.passed_options,
            &query,
            &search_term,
            fuzzy,
            prefer_verbose,
            &context.parser_directives,
            public_ai_context.is_some(),
        ));
    }

    add_exact_auto_execute(
        &mut suggestions,
        &query,
        settings.get_bool_or("autocomplete.hideAutoExecuteSuggestion", false),
        settings.get_bool_or("autocomplete.onlyShowOnTab", false),
        settings.get_bool_or("autocomplete.immediatelyRunDangerousCommands", false),
    );
    add_current_token_auto_execute(
        &mut suggestions,
        &normalized_search_term,
        suggest_current_token_for(
            context.active_arg.as_ref(),
            settings.get_bool_or("autocomplete.alwaysSuggestCurrentToken", false),
        ),
        settings.get_bool_or("autocomplete.hideAutoExecuteSuggestion", false),
        settings.get_bool_or("autocomplete.onlyShowOnTab", false),
        settings.get_bool_or("autocomplete.immediatelyRunDangerousCommands", false),
        context.active_arg.is_some(),
    );
    add_space_auto_execute(
        &mut suggestions,
        &normalized_search_term,
        settings.get_bool_or("autocomplete.immediatelyExecuteAfterSpace", false),
        settings.get_bool_or("autocomplete.hideAutoExecuteSuggestion", false),
        settings.get_bool_or("autocomplete.onlyShowOnTab", false),
    );

    let (pending_generators, debounce_ms) = crate::generate::take_pending_generators();
    let mut result = CompleteResult {
        public_ai_context,
        suggestions,
        fuzzy,
        search_term,
        match_term: query,
        current_arg,
        pending_generators,
        debounce_ms,
    };
    crate::public_ai::validate_provenance(&mut result);
    result
}

fn root_spec_for_command(
    registry: &mut Registry,
    request: &CompleteRequest,
    command: &str,
    settings: &fastab_settings::settings::Settings,
) -> Option<Arc<Spec>> {
    let shortcuts_token = settings.get_string_or("autocomplete.personalShortcutsToken", "+".into());
    if command == "?" {
        return crate::ir::load_project_fig_spec(&request.cwd, "_shortcuts")
            .or_else(|| crate::ir::load_home_fig_spec("_shortcuts"))
            .map(Arc::new)
            .or_else(|| registry.get_arc("_shortcuts"));
    }
    if command == shortcuts_token {
        return crate::ir::load_home_fig_spec(command)
            .map(Arc::new)
            .or_else(|| registry.get_arc(command));
    }
    if command.contains('/') {
        if let Some(local) = crate::ir::load_script_local_spec(&request.cwd, command) {
            return Some(Arc::new(local));
        }
        let basename = local_spec_name(command);
        if let Some(bundled) = versioned_or_bundled(registry, request, basename, settings) {
            return Some(bundled);
        }
        return versioned_or_bundled(registry, request, slash_command_spec_name(command), settings);
    }
    versioned_or_bundled(registry, request, command, settings)
}

fn versioned_or_bundled(
    registry: &mut Registry,
    request: &CompleteRequest,
    command: &str,
    settings: &fastab_settings::settings::Settings,
) -> Option<Arc<Spec>> {
    if registry.versioned_command(command).is_some() {
        let timeout = dynamic_hook_timeout_from_settings(settings);
        if let Some(spec) = registry.get_versioned_arc(command, &request.cwd, timeout) {
            return Some(spec);
        }
    }
    registry.get_arc(command)
}

pub(crate) fn local_spec_name(command: &str) -> &str {
    if command == "?" {
        return "_shortcuts";
    }
    command
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(command)
}

pub fn buffer_before_cursor(buffer: &str, cursor: Option<u32>) -> &str {
    let Some(cursor) = cursor else {
        return buffer;
    };
    let mut end = (cursor as usize).min(buffer.len());
    while end > 0 && !buffer.is_char_boundary(end) {
        end -= 1;
    }
    &buffer[..end]
}

pub fn args_hint(args: &[ArgSpec]) -> String {
    args.iter()
        .filter(|arg| !arg.name.is_empty())
        .map(|arg| {
            let mut base = arg.name.clone();
            if arg.is_variadic {
                base.push_str("...");
            }
            if arg.is_optional {
                format!("[{base}]")
            } else {
                format!("<{base}>")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn select_named_candidate<'a>(
    item_names: &'a [String],
    display_name: Option<&str>,
    query: &str,
    fuzzy: bool,
    prefer_long_name: bool,
) -> Option<&'a String> {
    let matching_names = item_names
        .iter()
        .filter(|candidate| matches_query(candidate, query, fuzzy))
        .collect::<Vec<_>>();
    if matching_names.is_empty() {
        display_name
            .filter(|display| matches_query(display, query, fuzzy))
            .and_then(|_| {
                if prefer_long_name {
                    item_names.iter().max_by_key(|candidate| candidate.len())
                } else {
                    item_names.first()
                }
            })
    } else if prefer_long_name {
        matching_names.into_iter().max_by_key(|candidate| candidate.len())
    } else {
        matching_names.into_iter().next()
    }
}

fn hidden_item_is_visible(hidden: bool, item_names: &[String], query: &str) -> bool {
    !hidden || item_names.iter().any(|candidate| candidate.eq_ignore_ascii_case(query))
}

fn suggest_current_token_for(active_arg: Option<&ActiveArg>, global: bool) -> bool {
    active_arg
        .and_then(|active| active.arg.suggest_current_token)
        .unwrap_or(global)
}

/// Resolve the filtering mode with the same precedence as the WebView. An
/// active argument owns the decision: an omitted argument strategy falls back
/// directly to the user's setting and must not inherit its parent spec's
/// strategy. Only when there is no active argument does the current spec's
/// strategy apply.
pub(crate) fn effective_fuzzy(user_fuzzy: bool, spec: Option<&Spec>, active_arg: Option<&ArgSpec>) -> bool {
    if let Some(strategy) = active_arg.and_then(|arg| arg.filter_strategy) {
        return strategy.effective_fuzzy(user_fuzzy);
    }
    if active_arg.is_some() {
        return user_fuzzy;
    }
    spec.and_then(|spec| spec.filter_strategy)
        .map_or(user_fuzzy, |strategy| strategy.effective_fuzzy(user_fuzzy))
}

pub(crate) fn effective_fuzzy_for_tokens(
    registry: &mut Registry,
    user_fuzzy: bool,
    tokens: &[String],
    ends_with_space: bool,
    query: &str,
    raw_query: &str,
) -> bool {
    let Some(command) = tokens.first() else {
        return user_fuzzy;
    };
    let Some(root) = registry.get_arc(command) else {
        return user_fuzzy;
    };
    let mut walk_tokens = tokens.to_vec();
    let context = resolve_context(
        root,
        &mut walk_tokens,
        ends_with_space,
        query,
        raw_query,
        Some(registry),
    );
    effective_fuzzy(
        user_fuzzy,
        Some(context.spec.as_ref()),
        context.active_arg.as_ref().map(|active| &active.arg),
    )
}

#[allow(clippy::too_many_arguments)]
fn collect_named<T>(
    items: &[T],
    names: impl Fn(&T) -> &[String],
    description: impl Fn(&T) -> &str,
    hint: impl Fn(&T) -> String,
    meta: impl Fn(&T) -> &SuggestionMeta,
    priority: impl Fn(&T) -> Option<i64>,
    should_add_space: impl Fn(&T) -> bool,
    separator: impl Fn(&T) -> Option<String>,
    requires_arg: impl Fn(&T) -> bool,
    kind: &str,
    query: &str,
    search_term: &str,
    fuzzy: bool,
    prefer_verbose: bool,
    public_source: impl Fn(&T) -> bool,
) -> Vec<Suggestion> {
    let mut out = Vec::new();
    for item in items {
        let item_names = names(item);
        let metadata = meta(item);
        let suggestion_kind = metadata.suggestion_type.as_deref().unwrap_or(kind);
        let (item_query, item_query_term) = crate::runtime::suggestion_query_term_with_hook(
            suggestion_kind,
            metadata.get_query_term.as_deref(),
            metadata.js_get_query_term.as_deref(),
            query,
            search_term,
        );
        let prefer_long_name = prefer_verbose && matches!(suggestion_kind, "option" | "subcommand");
        let name = select_named_candidate(
            item_names,
            metadata.display_name.as_deref(),
            &item_query,
            fuzzy,
            prefer_long_name,
        );
        let Some(name) = name else {
            continue;
        };
        // Hidden rows are omitted for empty/partial searches, but the old
        // filter deliberately revealed them when an alias was typed exactly.
        // `displayName` never counted as that exact-name exception.
        if !hidden_item_is_visible(metadata.hidden, item_names, &item_query) {
            continue;
        }
        let display_name = metadata
            .display_name
            .clone()
            .or_else(|| (item_names.len() > 1).then(|| item_names.join(", ")));
        let mut suggestion = Suggestion::new(name.clone(), description(item), suggestion_kind)
            .with_args_hint(hint(item))
            .with_meta(
                metadata.insert_value.clone(),
                display_name,
                separator(item),
                should_add_space(item),
                metadata.hidden,
                priority(item),
                metadata.icon.clone(),
            )
            .with_primary_name(item_names.first().cloned())
            .with_dangerous(metadata.is_dangerous)
            .with_original_type(metadata.original_type.clone())
            .with_query_term(item_query_term)
            .with_alias_names(item_names.to_vec());
        suggestion.requires_arg = requires_arg(item);
        if public_source(item) && suggestion.name.starts_with(query) {
            crate::public_ai::mark_candidate(&mut suggestion, metadata);
        }
        out.push(suggestion);
    }
    out
}

fn cmp_named_names(left: &[String], right: &[String]) -> std::cmp::Ordering {
    let left = left.first().map(String::as_str).unwrap_or_default();
    let right = right.first().map(String::as_str).unwrap_or_default();
    crate::query::cmp_ignore_ascii_case(left, right).then_with(|| left.cmp(right))
}

/// Sort indexes, then hand out references in that order. Equal names keep
/// their original relative order because `sort_by` is stable.
fn sorted_named_refs<T>(items: &[T], names: impl Fn(&T) -> &[String]) -> Vec<&T> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&left, &right| cmp_named_names(names(&items[left]), names(&items[right])));
    order.into_iter().map(|index| &items[index]).collect()
}

fn option_repetition_limit(option: &OptionSpec) -> Option<f64> {
    match option.is_repeatable.as_ref() {
        None | Some(serde_json::Value::Null | serde_json::Value::Bool(false)) => Some(1.0),
        Some(serde_json::Value::Bool(true)) => None,
        Some(serde_json::Value::Number(number)) => {
            let value = number.as_f64()?;
            // Fig treats numeric zero as falsy and therefore as the default
            // one repetition. Negative values are truthy but cannot admit a
            // passed row, which is the natural count < limit result.
            Some(if value == 0.0 { 1.0 } else { value })
        },
        Some(_) => Some(1.0),
    }
}

fn option_repetition_count(option: &OptionSpec, passed: &[OptionSpec]) -> usize {
    passed
        .iter()
        .filter(|candidate| options_are_equal(option, candidate))
        .count()
}

fn option_is_excluded(option: &OptionSpec, passed: &[OptionSpec]) -> bool {
    passed.iter().any(|candidate| {
        candidate
            .exclusive_on
            .iter()
            .any(|excluded| option.names.iter().any(|name| name == excluded))
    })
}

fn option_priority(option: &OptionSpec, passed: &[OptionSpec]) -> Option<i64> {
    let mut unmet = HashSet::new();
    for candidate in passed {
        unmet.extend(candidate.depends_on.iter().cloned());
    }
    for candidate in passed {
        for name in &candidate.names {
            unmet.remove(name);
        }
    }
    option
        .names
        .iter()
        .any(|name| unmet.contains(name))
        .then_some(75)
        .or(option.meta.priority)
}

#[allow(clippy::too_many_arguments)]
fn collect_option_suggestions(
    current: &Spec,
    persistent: &[&OptionSpec],
    passed: &[OptionSpec],
    query: &str,
    search_term: &str,
    fuzzy: bool,
    prefer_verbose: bool,
    directives: &ParserDirectives,
    public_source: bool,
) -> Vec<Suggestion> {
    let mut options: Vec<&OptionSpec> = current
        .options
        .iter()
        .map(Arc::as_ref)
        .chain(persistent.iter().copied())
        .collect();
    options.sort_by(|left, right| cmp_named_names(&left.names, &right.names));
    options.retain(|option| {
        !option_is_excluded(option, passed)
            && !option_repetition_limit(option)
                .is_some_and(|limit| option_repetition_count(option, passed) as f64 >= limit)
    });
    collect_named(
        &options,
        |opt| opt.names.as_slice(),
        |opt| opt.description.as_str(),
        |opt| args_hint(&opt.args),
        |opt| &opt.meta,
        // Depends-on promotion is a suggestion-row override. The cached option keeps its own priority.
        |opt| option_priority(opt, passed),
        |opt| {
            opt.meta
                .should_add_space
                .unwrap_or_else(|| should_add_space(&opt.args, opt.requires_subcommand, false))
        },
        |opt| option_separator(opt, directives),
        |opt| opt.args.first().is_some_and(|arg| !arg.is_optional),
        "option",
        query,
        search_term,
        fuzzy,
        prefer_verbose,
        |option| {
            public_source
                && !option.meta.ai_resolved_reference
                && !option.meta.ai_generated
                && option.load_spec.is_none()
        },
    )
}

fn option_separator(option: &crate::ir::OptionSpec, directives: &ParserDirectives) -> Option<String> {
    if let Some(separator) = option.meta.separator_to_add.clone() {
        return Some(separator);
    }
    match option.requires_separator.as_ref() {
        Some(serde_json::Value::String(separator)) => Some(separator.clone()),
        Some(serde_json::Value::Bool(true)) if !option.args.first().is_some_and(|arg| arg.is_optional) => {
            match &directives.option_arg_separators {
                Some(separators) => separators.first().cloned(),
                None => Some("=".into()),
            }
        },
        _ if option.requires_equals && !option.args.first().is_some_and(|arg| arg.is_optional) => Some("=".into()),
        _ => None,
    }
}

/// Recreate the WebView's composite short-option rows. With `-ab` already in
/// the buffer, the current `-b` row is displayed as `-ab`; every other short
/// option becomes `-ab<letter>` and inserts only that extra letter. If the
/// last option requires an argument, keep only that current option alongside
/// the argument rows generated above.
fn option_chain_suggestions(
    spec: &Spec,
    persistent: &[&OptionSpec],
    chain: ShortOptionChain<'_, '_>,
    passed: &[OptionSpec],
    directives: &ParserDirectives,
) -> Vec<Suggestion> {
    let mandatory_arg = chain.option.args.first().is_some_and(|arg| !arg.is_optional);
    let mut suggestions = Vec::new();

    for option in spec.options.iter().map(Arc::as_ref).chain(persistent.iter().copied()) {
        if option_is_excluded(option, passed)
            || option_repetition_limit(option)
                .is_some_and(|limit| option_repetition_count(option, passed) as f64 >= limit)
        {
            continue;
        }
        if mandatory_arg && !std::ptr::eq(option, chain.option) {
            continue;
        }
        let Some(short_name) = short_option_name(option) else {
            continue;
        };
        if option.meta.hidden {
            continue;
        }

        let is_current = std::ptr::eq(option, chain.option);
        let name = if is_current {
            chain.prefix.to_string()
        } else {
            format!("{}{}", chain.prefix, short_name.trim_start_matches('-'))
        };
        let display_name = if is_current {
            option.meta.display_name.clone().or_else(|| {
                (option.names.len() > 1).then(|| {
                    option
                        .names
                        .iter()
                        .map(|candidate| {
                            if candidate == short_name {
                                chain.prefix
                            } else {
                                candidate.as_str()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
            })
        } else {
            None
        };
        let should_add_space = option
            .meta
            .should_add_space
            .unwrap_or_else(|| should_add_space(&option.args, option.requires_subcommand, false));
        let kind = option.meta.suggestion_type.as_deref().unwrap_or("option");
        let mut suggestion = Suggestion::new(name.clone(), option.description.clone(), kind)
            .with_args_hint(args_hint(&option.args))
            .with_meta(
                if is_current {
                    option.meta.insert_value.clone()
                } else {
                    Some(name.clone())
                },
                display_name,
                option_separator(option, directives),
                should_add_space,
                option.meta.hidden,
                option_priority(option, passed),
                option.meta.icon.clone(),
            )
            .with_primary_name(if is_current {
                Some(chain.prefix.to_string())
            } else {
                Some(name.clone())
            })
            .with_dangerous(option.meta.is_dangerous)
            .with_original_type(option.meta.original_type.clone())
            .with_query_term(Some(chain.prefix.to_string()));
        suggestion.requires_arg = option.args.first().is_some_and(|arg| !arg.is_optional);
        suggestions.push(suggestion);
    }

    suggestions
}

fn should_add_space(args: &[ArgSpec], requires_subcommand: Option<bool>, has_subcommands: bool) -> bool {
    if args.first().is_some_and(|arg| !arg.is_optional) {
        return true;
    }
    requires_subcommand.unwrap_or(has_subcommands)
}

/// The WebView puts an auto-execute row immediately before an exact, safe
/// suggestion.  This is what makes Enter on `git status` execute the already
/// typed command instead of inserting an empty suffix.
fn add_exact_auto_execute(
    suggestions: &mut Vec<Suggestion>,
    query: &str,
    hide: bool,
    only_show_on_tab: bool,
    allow_dangerous: bool,
) {
    if query.is_empty() || hide || only_show_on_tab {
        return;
    }
    let Some(index) = suggestions.iter().position(|suggestion| {
        let row_query = suggestion.query_term.as_deref().unwrap_or(query);
        let exact_name = suggestion.name.eq_ignore_ascii_case(row_query)
            // File generators retain the trailing slash in the accepted name,
            // while the old WebView matched a folder query without it.
            || (suggestion.kind == "folder"
                && suggestion
                    .name
                    .strip_suffix('/')
                    .is_some_and(|name| name.eq_ignore_ascii_case(row_query)));
        suggestion.kind != "auto-execute"
            && exact_name
            && !suggestion.requires_arg
            && (allow_dangerous || !suggestion.is_dangerous)
            && suggestion
                .insert_value
                .as_deref()
                .is_none_or(|value| suggestion.primary_name.as_deref().unwrap_or(&suggestion.name) == value)
    }) else {
        return;
    };
    let original = suggestions[index].clone();
    let is_folder = original.kind == "folder";
    let auto = Suggestion {
        public_ai_candidate: None,
        argument_value: original.argument_value,
        acceptance_scope: None,
        name: if is_folder {
            original.name.strip_suffix('/').unwrap_or(&original.name).to_string()
        } else {
            original.name.clone()
        },
        description: if is_folder {
            "folder".into()
        } else {
            original.description.clone()
        },
        kind: "auto-execute".into(),
        args_hint: original.args_hint,
        insert_value: Some("\n".into()),
        display_name: original.display_name,
        primary_name: original.primary_name,
        separator_to_add: None,
        should_add_space: false,
        hidden: false,
        // Keep this ahead of normal rows even after the native frecency sort.
        priority: i64::MAX,
        icon: Some("fig://icon?type=carrot".into()),
        original_type: original.original_type.clone().or_else(|| Some(original.kind.clone())),
        query_term: original.query_term,
        is_dangerous: false,
        requires_arg: false,
        alias_names: original.alias_names,
    };
    suggestions.insert(0, auto);
}

fn add_space_auto_execute(
    suggestions: &mut Vec<Suggestion>,
    query: &str,
    execute_after_space: bool,
    hide: bool,
    only_show_on_tab: bool,
) {
    if !query.is_empty() || !execute_after_space || hide || only_show_on_tab || suggestions.is_empty() {
        return;
    }
    suggestions.insert(
        0,
        Suggestion {
            name: "↪".into(),
            public_ai_candidate: None,
            argument_value: false,
            acceptance_scope: None,
            description: "Immediately execute".into(),
            kind: "auto-execute".into(),
            args_hint: String::new(),
            insert_value: Some("\n".into()),
            display_name: None,
            primary_name: None,
            separator_to_add: None,
            should_add_space: false,
            hidden: false,
            priority: i64::MAX,
            icon: Some("fig://icon?type=carrot".into()),
            original_type: None,
            query_term: None,
            is_dangerous: true,
            requires_arg: false,
            alias_names: Vec::new(),
        },
    );
}

#[allow(clippy::fn_params_excessive_bools)]
fn add_current_token_auto_execute(
    suggestions: &mut Vec<Suggestion>,
    query: &str,
    always_suggest_current_token: bool,
    hide: bool,
    only_show_on_tab: bool,
    allow_dangerous: bool,
    argument_value: bool,
) {
    if query.is_empty()
        || hide
        || only_show_on_tab
        || suggestions.is_empty()
        || suggestions.iter().any(|suggestion| suggestion.kind == "auto-execute")
    {
        return;
    }

    // `alwaysSuggestCurrentToken` is independent from the dangerous-command
    // setting for partial matches (for example `fo` -> `foo`).  The old
    // filter only blocked this fallback when the current token itself was an
    // exact dangerous command, so that Enter could not silently bypass the
    // safety guard.
    let dangerous_exact = !allow_dangerous
        && suggestions.iter().any(|suggestion| {
            let row_query = suggestion.query_term.as_deref().unwrap_or(query);
            let exact_name = suggestion.name.eq_ignore_ascii_case(row_query)
                || (suggestion.kind == "folder"
                    && suggestion
                        .name
                        .strip_suffix('/')
                        .is_some_and(|name| name.eq_ignore_ascii_case(row_query)));
            exact_name && suggestion.is_dangerous
        });
    if dangerous_exact {
        return;
    }

    // A directory query has a dedicated action in the old UI.  `.` is
    // special even when the generator returned files, while a trailing slash
    // is special only when a folder row is present (the Rust file generator
    // exposes that fact through `kind == "folder"`).
    let folder_query =
        query == "." || (query.ends_with('/') && suggestions.iter().any(|suggestion| suggestion.kind == "folder"));
    if folder_query {
        suggestions.insert(
            0,
            Suggestion {
                name: if query == "." { query.to_string() } else { "↪".into() },
                public_ai_candidate: None,
                argument_value,
                acceptance_scope: None,
                description: "Enter the current directory".into(),
                kind: "auto-execute".into(),
                args_hint: String::new(),
                insert_value: Some("\n".into()),
                display_name: None,
                primary_name: None,
                separator_to_add: None,
                should_add_space: false,
                hidden: false,
                priority: i64::MAX,
                icon: Some("fig://icon?type=carrot".into()),
                original_type: Some("folder".into()),
                query_term: None,
                is_dangerous: false,
                requires_arg: false,
                alias_names: Vec::new(),
            },
        );
        return;
    }

    if !always_suggest_current_token {
        return;
    }

    suggestions.insert(
        0,
        Suggestion {
            name: query.to_string(),
            public_ai_candidate: None,
            argument_value,
            acceptance_scope: None,
            description: "Enter the current argument".into(),
            kind: "auto-execute".into(),
            args_hint: String::new(),
            insert_value: Some("\n".into()),
            display_name: None,
            primary_name: None,
            separator_to_add: None,
            should_add_space: false,
            hidden: false,
            priority: i64::MAX,
            icon: Some("fig://icon?type=carrot".into()),
            original_type: None,
            query_term: None,
            is_dangerous: false,
            requires_arg: false,
            alias_names: Vec::new(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Registry;
    use crate::process;
    use std::fs;

    #[test]
    fn heroku_8_3_0_uses_the_8_0_0_file_and_8_11_1_diff() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("heroku")).unwrap();
        fs::write(
            dir.path().join("heroku/8.0.0.json"),
            r#"{"names":["heroku"],"subcommands":[{"names":["old"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("heroku/8.0.0+8.11.1.json"),
            r#"{"names":["heroku"],"subcommands":[{"names":["old"]},{"names":["domains"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("heroku/8.6.0.json"),
            r#"{"names":["heroku"],"subcommands":[{"names":["new"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("index.json"),
            r#"{
              "completions":["heroku"],
              "files":{"heroku":"heroku/8.6.0.json"},
              "versioned":{
                "heroku":{
                  "command":["heroku","--version"],
                  "parse":"regex",
                  "regex":"heroku\\/([0-9]+\\.[0-9]+\\.[0.9]+)",
                  "regexGroup":1,
                  "parseFallback":"8.0.0",
                  "fallback":"8.6.0",
                  "files":{"8.0.0":"heroku/8.0.0.json","8.6.0":"heroku/8.6.0.json"},
                  "applied":{"8.0.0":{"8.11.1":"heroku/8.0.0+8.11.1.json"}}
                }
              }
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let entry = registry
            .versioned_command("heroku")
            .expect("heroku versioned index")
            .clone();
        let _guard = process::mock::install(vec![process::mock::ExecRule {
            command: Some("heroku".into()),
            args: Some(vec!["--version".into()]),
            stdout: "heroku/8.3.0 darwin-arm64".into(),
            ..process::mock::ExecRule::default()
        }]);
        assert_eq!(
            crate::versioned::detect_cli_version(&entry, "/", std::time::Duration::from_secs(5)).as_deref(),
            Some("8.3.0"),
            "calls={:?}",
            process::mock::calls()
        );
        let settings =
            fastab_settings::settings::Settings::from_slice(&[("autocomplete.scriptTimeout", serde_json::json!(5000))]);
        let result = complete_with_settings(
            &mut registry,
            &CompleteRequest {
                buffer: "heroku ".into(),
                cwd: "/".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
            &settings,
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.contains(&"domains"), "{names:?}");
        assert!(names.contains(&"old"), "{names:?}");
        assert!(!names.contains(&"new"), "{names:?}");
    }

    #[test]
    fn fig_2_16_0_includes_diff_subcommands() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("fig")).unwrap();
        fs::write(
            dir.path().join("fig/2.0.0.json"),
            r#"{"names":["fig"],"subcommands":[{"names":["app"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("fig/2.0.0+2.16.0.json"),
            r#"{"names":["fig"],"subcommands":[{"names":["app"]},{"names":["export"]},{"names":["help"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("index.json"),
            r#"{
              "completions":["fig"],
              "files":{"fig":"fig/2.0.0.json"},
              "versioned":{
                "fig":{
                  "command":["fig","--version"],
                  "parse":"after-first-space",
                  "fallback":"2.0.0",
                  "files":{"2.0.0":"fig/2.0.0.json"},
                  "applied":{"2.0.0":{"2.16.0":"fig/2.0.0+2.16.0.json"}}
                }
              }
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let _guard = process::mock::install(vec![process::mock::ExecRule {
            command: Some("fig".into()),
            args: Some(vec!["--version".into()]),
            stdout: "fig 2.16.0".into(),
            ..process::mock::ExecRule::default()
        }]);
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "fig ".into(),
                cwd: "/".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.contains(&"export"), "{names:?}");
        assert!(names.contains(&"help"), "{names:?}");
        assert!(names.contains(&"app"), "{names:?}");
    }

    #[test]
    fn heroku_version_detect_failure_uses_the_highest_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("heroku")).unwrap();
        fs::write(
            dir.path().join("heroku/8.0.0.json"),
            r#"{"names":["heroku"],"subcommands":[{"names":["old"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("heroku/8.6.0.json"),
            r#"{"names":["heroku"],"subcommands":[{"names":["new"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("index.json"),
            r#"{
              "completions":["heroku"],
              "files":{"heroku":"heroku/8.6.0.json"},
              "versioned":{
                "heroku":{
                  "command":["heroku","--version"],
                  "parse":"regex",
                  "regex":"heroku\\/([0-9]+\\.[0-9]+\\.[0.9]+)",
                  "regexGroup":1,
                  "parseFallback":"8.0.0",
                  "fallback":"8.6.0",
                  "files":{"8.0.0":"heroku/8.0.0.json","8.6.0":"heroku/8.6.0.json"}
                }
              }
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let _guard = process::mock::install(vec![]);
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "heroku ".into(),
                cwd: "/".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.contains(&"new"), "{names:?}");
        assert!(!names.contains(&"old"), "{names:?}");
    }

    fn load_git() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names": ["git"],
              "subcommands": [
                {"names": ["checkout"], "description": "Switch branches"},
                {"names": ["commit"], "description": "Record changes"},
                {"names": ["cherry-pick"], "description": "Apply commits"},
                {"names": ["status"], "description": "Show status"}
              ],
              "options": [{"names": ["--help"], "description": "Show help"}]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    fn public_ai_request(buffer: &str, cwd: &str) -> CompleteRequest {
        CompleteRequest {
            buffer: buffer.to_string(),
            cwd: cwd.to_string(),
            include_history: false,
            include_public_ai: true,
            ..CompleteRequest::default()
        }
    }

    fn trusted_fixture(registry: &mut Registry) {
        // Fixture contents are synthetic, so test provenance at the actual
        // lookup path without weakening the production snapshot-pin check.
        registry.trust_public_ai_fixture_for_test();
    }

    #[test]
    fn public_ai_keeps_the_production_pin_gate_closed_for_unpinned_fixture() {
        let (dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &public_ai_request("git ch", &dir.path().display().to_string()),
        );

        assert!(result.public_ai_context.is_none());
        assert!(
            result
                .suggestions
                .iter()
                .all(|suggestion| suggestion.public_ai_candidate.is_none())
        );
    }

    #[test]
    fn public_ai_excludes_unentered_load_spec_stubs_and_entered_replacements() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("gcloud")).unwrap();
        fs::write(
            dir.path().join("gcloud.json"),
            r#"{
              "names":["gcloud"],
              "subcommands":[
                {"names":["compute"],"description":"Compute","loadSpec":"gcloud/compute"},
                {"names":["sql"],"description":"SQL","loadSpec":"gcloud/sql"},
                {"names":["config"],"description":"Config","subcommands":[
                  {"names":["list"],"description":"List"},
                  {"names":["get"],"description":"Get"}
                ]},
                {"names":["auth"],"description":"Auth","subcommands":[
                  {"names":["login"],"description":"Login"},
                  {"names":["logout"],"description":"Logout"}
                ]}
              ]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("gcloud/compute.json"),
            r#"{
              "names":["compute"],
              "subcommands":[
                {"names":["instances"],"description":"Instances"},
                {"names":["disks"],"description":"Disks"}
              ]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("gcloud/sql.json"),
            r#"{"names":["sql"],"subcommands":[{"names":["backups"]}]}"#,
        )
        .unwrap();
        fs::write(dir.path().join("index.json"), r#"{"files":{"gcloud":"gcloud.json"}}"#).unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        trusted_fixture(&mut registry);
        let cwd = dir.path().display().to_string();

        let menu = complete(&mut registry, &public_ai_request("gcloud ", &cwd));
        assert!(menu.public_ai_context.is_some(), "{menu:?}");
        for name in ["config", "auth"] {
            assert!(
                menu.suggestions
                    .iter()
                    .find(|suggestion| suggestion.name == name)
                    .is_some_and(|suggestion| suggestion.public_ai_candidate.is_some()),
                "{name} was not marked: {:?}",
                menu.suggestions
            );
        }
        for name in ["compute", "sql"] {
            assert!(
                menu.suggestions
                    .iter()
                    .find(|suggestion| suggestion.name == name)
                    .is_some_and(|suggestion| suggestion.public_ai_candidate.is_none()),
                "{name} was marked from a loadSpec stub: {:?}",
                menu.suggestions
            );
        }
        assert!(registry.cached_load_spec("gcloud/compute").is_none());
        assert!(registry.cached_load_spec("gcloud/sql").is_none());

        let typing = complete(&mut registry, &public_ai_request("gcloud comp", &cwd));
        assert!(
            typing
                .suggestions
                .iter()
                .find(|suggestion| suggestion.name == "compute")
                .is_some_and(|suggestion| suggestion.public_ai_candidate.is_none())
        );
        assert!(registry.cached_load_spec("gcloud/compute").is_none());

        let config = complete(&mut registry, &public_ai_request("gcloud config ", &cwd));
        assert!(config.public_ai_context.is_some(), "{config:?}");
        for name in ["list", "get"] {
            assert!(
                config
                    .suggestions
                    .iter()
                    .find(|suggestion| suggestion.name == name)
                    .is_some_and(|suggestion| suggestion.public_ai_candidate.is_some()),
                "{name} was not marked: {:?}",
                config.suggestions
            );
        }
        assert!(registry.cached_load_spec("gcloud/compute").is_none());

        let entered = complete(&mut registry, &public_ai_request("gcloud compute ", &cwd));
        assert!(entered.public_ai_context.is_none(), "{entered:?}");
        for name in ["instances", "disks"] {
            assert!(
                entered
                    .suggestions
                    .iter()
                    .find(|suggestion| suggestion.name == name)
                    .is_some_and(|suggestion| suggestion.public_ai_candidate.is_none()),
                "{name} was marked after a loadSpec: {:?}",
                entered.suggestions
            );
        }
        let cached = registry.cached_load_spec("gcloud/compute").expect("compute file");
        assert!(!cached.meta.ai_resolved_reference);
        assert!(registry.cached_load_spec("gcloud/sql").is_none());

        let root = registry.get_arc("gcloud").expect("gcloud");
        let mut tokens = vec!["gcloud".to_string(), "compute".to_string()];
        let walked = resolve_context(root, &mut tokens, true, "", "", Some(&mut registry));
        assert_eq!(walked.spec.names, vec!["compute"]);
        assert!(walked.spec.meta.ai_resolved_reference);
        assert!(!walked.crossed_loaded_spec);
        assert!(!std::sync::Arc::ptr_eq(&cached, &walked.spec));
        let gcloud = registry.get("gcloud").expect("gcloud");
        let stub = gcloud.find_subcommand("compute").expect("stub");
        assert!(!stub.meta.ai_resolved_reference);
        assert!(matches!(stub.load_spec, Some(crate::ir::LoadSpec::Path(ref path)) if path == "gcloud/compute"));
    }

    #[test]
    fn public_ai_marks_static_siblings_beside_generated_subcommands() {
        let dir = tempfile::tempdir().unwrap();
        let (hook_id, hook_entry) = crate::hook_backend::test_typed_entry(
            "git#generate#0",
            "generateSpec",
            serde_json::json!({
                "op": "spec-object",
                "fields": [
                    {"key": "name", "value": {"op": "string", "value": "git"}},
                    {"key": "subcommands", "value": {"op": "array", "items": [{
                        "op": "spec-object",
                        "fields": [
                            {"key": "name", "value": {"op": "string", "value": "changelog"}},
                            {"key": "description", "value": {"op": "string", "value": "Generated entry"}}
                        ]
                    }]}},
                    {"key": "options", "value": {"op": "array", "items": [{
                        "op": "object",
                        "fields": [{"key": "name", "value": {"op": "string", "value": "--generated"}}]
                    }]}}
                ]
            }),
        );
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names":["git"],
              "jsGenerateSpec":"git#generate#0",
              "args":[{"name":"target"}],
              "options":[
                {"names":["--color"],"args":[{"name":"when","isOptional":true}]},
                {"names":["--config"],"args":[{"name":"file","isOptional":true}]}
              ],
              "subcommands":[
                {"names":["checkout"],"description":"Switch branches"},
                {"names":["cherry-pick"],"description":"Apply commits"}
              ]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        trusted_fixture(&mut registry);
        let native = crate::hook_backend::test_native_hooks(vec![(hook_id, hook_entry)]);
        let _bound = crate::hook_backend::bind_native(Arc::new(native));
        let cwd = dir.path().display().to_string();
        let request = public_ai_request("git ch", &cwd);
        let result = crate::hook_backend::enter_context(&cwd, &crate::hook_types::ShellContext::default(), || {
            complete(&mut registry, &request)
        });

        assert!(result.public_ai_context.is_some(), "{result:?}");
        for static_name in ["checkout", "cherry-pick"] {
            assert!(
                result
                    .suggestions
                    .iter()
                    .find(|suggestion| suggestion.name == static_name)
                    .is_some_and(|suggestion| suggestion.public_ai_candidate.is_some()),
                "{static_name} was not marked as public: {:?}",
                result.suggestions
            );
        }
        assert!(
            result
                .suggestions
                .iter()
                .find(|suggestion| suggestion.name == "changelog")
                .is_some_and(|suggestion| suggestion.public_ai_candidate.is_none()),
            "generated-only suggestion was marked as public: {:?}",
            result.suggestions
        );

        let option_request = public_ai_request("git --", &cwd);
        let option_result =
            crate::hook_backend::enter_context(&cwd, &crate::hook_types::ShellContext::default(), || {
                complete(&mut registry, &option_request)
            });
        assert!(option_result.public_ai_context.is_some(), "{option_result:?}");
        for static_name in ["--color", "--config"] {
            assert!(
                option_result
                    .suggestions
                    .iter()
                    .find(|suggestion| suggestion.name == static_name)
                    .is_some_and(|suggestion| suggestion.public_ai_candidate.is_some()),
                "optional-argument option {static_name} was not marked: {:?}",
                option_result.suggestions
            );
        }
        assert!(
            option_result
                .suggestions
                .iter()
                .find(|suggestion| suggestion.name == "--generated")
                .is_some_and(|suggestion| suggestion.public_ai_candidate.is_none()),
            "generated-only option was marked as public: {:?}",
            option_result.suggestions
        );
    }

    #[test]
    fn public_ai_rejects_a_loaded_argument_spec_that_reuses_a_pinned_path_name() {
        let dir = tempfile::tempdir().unwrap();
        let (generate_id, generate_entry) = crate::hook_backend::test_typed_entry(
            "git#generate#0",
            "generateSpec",
            serde_json::json!({
                "op": "spec-object",
                "fields": [
                    {"key": "name", "value": {"op": "string", "value": "git"}},
                    {"key": "subcommands", "value": {"op": "array", "items": [{
                        "op": "spec-object",
                        "fields": [
                            {"key": "name", "value": {"op": "string", "value": "parent"}},
                            {"key": "args", "value": {"op": "array", "items": [{
                                "op": "object",
                                "fields": [
                                    {"key": "name", "value": {"op": "string", "value": "target"}},
                                    {"key": "jsLoadSpec", "value": {"op": "string", "value": "git#load#0"}}
                                ]
                            }]}}
                        ]
                    }]}}
                ]
            }),
        );
        let (load_id, load_entry) = crate::hook_backend::test_typed_entry(
            "git#load#0",
            "loadSpec",
            serde_json::json!({
                "op": "spec-object",
                "fields": [
                    {"key": "name", "value": {"op": "string", "value": "child"}},
                    {"key": "subcommands", "value": {"op": "array", "items": [
                        {"op": "spec-object", "fields": [{"key": "name", "value": {"op": "string", "value": "private-one"}}]},
                        {"op": "spec-object", "fields": [{"key": "name", "value": {"op": "string", "value": "private-two"}}]}
                    ]}}
                ]
            }),
        );
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names":["git"],
              "jsGenerateSpec":"git#generate#0",
              "subcommands":[{"names":["parent"],"subcommands":[{"names":["child"]}]}]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        trusted_fixture(&mut registry);
        let native = crate::hook_backend::test_native_hooks(vec![(generate_id, generate_entry), (load_id, load_entry)]);
        let _bound = crate::hook_backend::bind_native(Arc::new(native));
        let cwd = dir.path().display().to_string();
        let request = public_ai_request("git parent child ", &cwd);
        let result = crate::hook_backend::enter_context(&cwd, &crate::hook_types::ShellContext::default(), || {
            complete(&mut registry, &request)
        });

        assert!(
            result
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "private-one")
        );
        assert!(
            result
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "private-two")
        );
        assert!(
            result.public_ai_context.is_none(),
            "loaded spec reused a pinned path: {result:?}"
        );
        assert!(
            result
                .suggestions
                .iter()
                .all(|suggestion| suggestion.public_ai_candidate.is_none())
        );
    }

    #[test]
    fn current_command_slice_follows_shell_separators_and_assignments() {
        assert_eq!(current_command_slice("git ch"), "git ch");
        assert_eq!(current_command_slice("echo x && git ch"), "git ch");
        assert_eq!(current_command_slice("echo x&&git ch"), "git ch");
        assert_eq!(current_command_slice("FOO+=1 git ch"), "git ch");
        assert_eq!(current_command_slice("echo x || git ch"), "git ch");
        assert_eq!(current_command_slice("echo x | grep --"), "grep --");
        assert_eq!(current_command_slice("echo x |& grep --"), "grep --");
        assert_eq!(current_command_slice("echo x; git ch"), "git ch");
        assert_eq!(current_command_slice("echo x & git ch"), "git ch");
        assert_eq!(current_command_slice("FOO=1 git ch"), "git ch");
        assert_eq!(current_command_slice("FOO=1 BAR=2 git ch"), "git ch");
        assert_eq!(current_command_slice("echo x && FOO=1 git ch"), "git ch");
        assert_eq!(current_command_slice(r#"echo "x && git" ch"#), r#"echo "x && git" ch"#);
        assert_eq!(current_command_slice("echo $(git ch"), "git ch");
        assert_eq!(current_command_slice("echo `git ch"), "git ch");
        assert_eq!(current_command_slice("echo x && (cd /tmp && git ch"), "git ch");
        assert_eq!(current_command_slice("{ echo x; git ch"), "git ch");
        assert_eq!(current_command_slice("echo foo 2>&1 && git ch"), "git ch");
        assert_eq!(current_command_slice("echo x && 2>&1 git ch"), "git ch");
        assert_eq!(current_command_slice("FOO=1"), "");
        assert_eq!(current_command_slice("echo x && "), "");
        assert_eq!(current_command_slice("'FOO=1' git ch"), "'FOO=1' git ch");
        assert_eq!(current_command_slice(r#""FOO=1" git ch"#), r#""FOO=1" git ch"#);
        assert_eq!(current_command_slice("$'FOO=1' git ch"), "$'FOO=1' git ch");
        assert_eq!(current_command_slice("FOO=1 (git ch"), "git ch");
        assert_eq!(current_command_slice("echo `foo)` && git ch"), "git ch");
        assert_eq!(current_command_slice("echo x\ngit ch"), "git ch");
        assert_eq!(current_command_slice("echo $((git ch"), "echo $((git ch");
        assert_eq!(current_command_slice("cat <(git ch"), "cat <(git ch");
        assert!(is_fresh_command_position("echo x && "));
        assert!(!is_fresh_command_position("echo x &&"));
        assert!(!is_fresh_command_position(""));
        assert!(!is_fresh_command_position("FOO=1 "));
        assert!(!is_fresh_command_position("echo x && FOO=1 "));
    }

    #[test]
    fn git_ch_matches_checkout_and_cherry_pick() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git ch".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"checkout"), "{names:?}");
        assert!(names.contains(&"cherry-pick"), "{names:?}");
        assert!(!names.contains(&"status"), "{names:?}");
        assert_eq!(result.search_term, "ch");
    }

    #[test]
    fn chained_command_completes_the_current_command_not_the_first() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "echo x && git ch".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"checkout"), "{names:?}");
        assert!(names.contains(&"cherry-pick"), "{names:?}");
        assert_eq!(result.search_term, "ch");
    }

    #[test]
    fn chained_quoted_token_keeps_the_raw_search_term() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "echo x && git 'ch".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"checkout"), "{names:?}");
        assert_eq!(result.search_term, "'ch");
    }

    #[test]
    fn assignment_prefix_does_not_hide_the_root_command() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "FOO=1 git ch".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"checkout"), "{names:?}");
        assert_eq!(result.search_term, "ch");
    }

    #[test]
    fn fuzzy_matches_non_prefix_subsequence() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git ckt".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: true,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"checkout"), "{names:?}");
        assert!(!names.contains(&"commit"), "{names:?}");
    }

    #[test]
    fn trailing_space_lists_all_subcommands() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git ".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        assert!(result.suggestions.iter().any(|s| s.name == "status"));
        assert!(result.suggestions.iter().any(|s| s.kind == "option"));
        assert_eq!(result.search_term, "");
    }

    #[test]
    fn first_token_without_space_matches_commands_not_subcommands() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "gi".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"git"), "{names:?}");
        assert!(!names.contains(&"checkout"), "{names:?}");
        assert_eq!(result.search_term, "gi");
    }

    #[test]
    fn separator_with_no_command_uses_first_token_completion() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "echo x && ".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"git"), "{names:?}");
        assert_eq!(result.search_term, "");
    }

    #[test]
    fn first_token_can_be_disabled() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "gi".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: false,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        assert!(result.suggestions.is_empty(), "{:?}", result.suggestions);
        assert_eq!(result.search_term, "gi");
    }

    #[test]
    fn trailing_space_expands_a_shell_alias_before_spec_lookup() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "g ".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: false,
                suggest_first_token: false,
                current_shell: Some("/bin/zsh".into()),
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: Some("alias g=git\n".into()),
            },
        );
        assert!(
            result.suggestions.iter().any(|item| item.name == "checkout"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn a_single_alias_token_without_space_is_not_expanded() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "g".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: false,
                suggest_first_token: false,
                current_shell: Some("/bin/zsh".into()),
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: Some("alias g=git\n".into()),
            },
        );
        assert!(result.suggestions.is_empty(), "{:?}", result.suggestions);
    }

    #[test]
    fn history_template_offers_values_from_the_same_spec_slot_across_aliases_and_options() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{
              "names":["git"],
              "options":[{"names":["-C"],"args":[{"name":"path"}]}],
              "subcommands":[{
                "names":["add"],
                "options":[{"names":["-v"]}],
                "args":[{"name":"pathspec","templates":["history"]}]
              }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        crate::generate::set_history(Arc::new(crate::history::HistoryStore::from_lines(vec![
            "g add file".into(),
            "git -C /repo add -v other".into(),
            "git add -v".into(),
        ])));
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "g add ".into(),
                cwd: dir.path().display().to_string(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: false,
                suggest_first_token: false,
                current_shell: Some("/bin/zsh".into()),
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: Some("alias g=git\n".into()),
            },
        );
        crate::generate::set_history(Arc::default());
        let history: Vec<&str> = result
            .suggestions
            .iter()
            .filter(|item| item.kind == "arg")
            .map(|item| item.name.as_str())
            .collect();
        // Most recent first; `-v` is an option, not a pathspec value.
        assert_eq!(history, vec!["other", "file"]);
    }

    #[test]
    fn cached_history_generator_does_not_rebuild_values_from_the_store() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("wscat.json"),
            r#"{"names":["wscat"],"options":[{"names":["-c"],"args":[{
                "name":"URL","templates":["history"]
            }]}]}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let store = Arc::new(crate::history::HistoryStore::from_lines(vec![
            "wc -c wss://old".into(),
            "wscat -c ws://plain".into(),
            "wc -c wss://recent".into(),
            "wc -c wss://old".into(),
        ]));
        crate::generate::set_history(Arc::clone(&store));
        crate::generate::install_session(crate::generate::GeneratorSession::default());
        let mut request = CompleteRequest {
            buffer: "wc -c w".into(),
            cwd: dir.path().display().to_string(),
            current_shell: Some("/usr/local/bin/fish".into()),
            alias: Some("alias wc 'wscat'\n".into()),
            include_history: false,
            ..CompleteRequest::default()
        };
        let first = complete(&mut registry, &request);
        let names = |result: &CompleteResult| {
            result
                .suggestions
                .iter()
                .filter(|row| row.kind == "arg")
                .map(|row| row.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&first), ["wss://old", "wss://recent", "ws://plain"]);
        assert_eq!(store.arg_values_calls(), 1);
        request.buffer = "wc -c ws".into();
        let cached = complete(&mut registry, &request);
        assert_eq!(names(&cached), names(&first));
        assert_eq!(store.arg_values_calls(), 1, "cache hit must not even call arg_values");
        crate::generate::set_history(Arc::default());
        crate::generate::install_session(crate::generate::GeneratorSession::default());
    }

    #[test]
    fn help_template_lists_sibling_subcommands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "subcommands":[
                {"names":["help"],"args":[{"name":"command","templates":["help"]}]},
                {"names":["build"],"description":"Build it"},
                {"names":["test"],"description":"Test it"}
              ]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "tool help ".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: false,
                suggest_first_token: false,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"build"), "{names:?}");
        assert!(names.contains(&"test"), "{names:?}");
        assert!(!names.contains(&"help"), "{names:?}");
        let help_rows: Vec<_> = result
            .suggestions
            .iter()
            .filter(|row| matches!(row.name.as_str(), "build" | "test"))
            .collect();
        assert!(help_rows.iter().all(|row| row.argument_value));
        let scope = help_rows[0].acceptance_scope.as_deref().expect("help parser slot");
        assert!(
            help_rows
                .iter()
                .all(|row| row.acceptance_scope.as_deref() == Some(scope))
        );
        assert!(
            result.suggestions.iter().any(|item| item.kind == "special"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn unknown_commands_do_not_run_cobra_complete() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::load(dir.path()).expect("load");
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "definitely-not-a-spec-binary ".into(),
                cwd: dir.path().display().to_string(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: false,
                suggest_first_token: false,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        assert!(result.suggestions.is_empty(), "{:?}", result.suggestions);
    }

    #[test]
    fn disable_for_commands_matches_the_configured_command_array() {
        let disabled = vec!["git".to_string(), "kubectl".to_string()];
        assert!(command_is_disabled_from(&disabled, "git"));
        assert!(!command_is_disabled_from(&disabled, "GIT"));
        assert!(!command_is_disabled_from(&disabled, " kubectl "));
        assert!(!command_is_disabled_from(&disabled, "cargo"));
        assert!(!command_is_disabled_from(&disabled, ""));
        assert!(command_is_disabled_from(&[" kubectl ".to_string()], " kubectl "));
    }

    #[test]
    fn exact_first_token_does_not_list_subcommands() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        assert!(
            result.suggestions.iter().all(|s| s.name != "checkout"),
            "{:?}",
            result.suggestions
        );
        assert_eq!(result.search_term, "git");
    }

    #[test]
    fn cursor_slices_buffer_so_mid_line_completes_the_token_before_caret() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git checkout".into(),
                cwd: "/".into(),
                cursor: Some(4),
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        assert!(
            result.suggestions.iter().any(|s| s.name == "status"),
            "{:?}",
            result.suggestions
        );
        assert_eq!(result.search_term, "");
    }

    #[test]
    fn args_hint_formats_optional_and_variadic() {
        assert_eq!(
            args_hint(&[ArgSpec {
                name: "path".into(),
                is_optional: true,
                is_variadic: true,
                ..ArgSpec::default()
            }]),
            "[path...]"
        );
        assert_eq!(
            args_hint(&[ArgSpec {
                name: "branch".into(),
                ..ArgSpec::default()
            }]),
            "<branch>"
        );
    }

    fn load_context_spec() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names":["git"],
              "args":[{"name":"root path","description":"Root positional"}],
              "options":[
                {"names":["--message","-m"],"description":"Commit message","args":[{"name":"message","description":"Message value"}]},
                {"names":["--optional"],"args":[{"name":"maybe","description":"Optional value","isOptional":true}]}
              ],
              "subcommands":[
                {"names":["checkout"],"args":[
                  {"name":"branch","description":"Branch value"},
                  {"name":"path","description":"Path value","isOptional":true}
                ]}
              ]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    fn load_option_state_spec() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "options":[
                {"names":["--one","-o"],"description":"One","exclusiveOn":["--two","-t"],"dependsOn":["--needed"]},
                {"names":["--two","-t"],"description":"Two"},
                {"names":["--needed"],"description":"Needed"},
                {"names":["--once"],"description":"Once"},
                {"names":["--twice"],"description":"Twice","isRepeatable":2},
                {"names":["--many"],"description":"Many","isRepeatable":true}
              ],
              "persistentOptions":[
                {"names":["--global","-g"],"description":"Global","isPersistent":true}
              ],
              "subcommands":[
                {"names":["child"],"options":[{"names":["--child"],"description":"Child"}]}
              ]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    fn load_parser_directive_spec() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("strict.json"),
            r#"{
              "names":["strict"],
              "parserDirectives":{
                "optionsMustPrecedeArguments":true,
                "flagsArePosixNoncompliant":true,
                "optionArgSeparators":[":","="]
              },
              "args":[{"name":"value"}],
              "options":[
                {"names":["--flag"],"description":"Flag"},
                {"names":["-word"],"description":"Non-POSIX long","requiresSeparator":":","args":[{"name":"value"}]},
                {"names":["--required"],"description":"Required separator","requiresSeparator":true,"args":[{"name":"value"}]},
                {"names":["--files"],"description":"Files","args":[{"name":"file","isVariadic":true,"optionsCanBreakVariadicArg":false}]}
              ],
              "subcommands":[
                {"names":["child"],"parserDirectives":{"optionsMustPrecedeArguments":false},"args":[{"name":"values","isVariadic":true,"optionsCanBreakVariadicArg":false}],"options":[{"names":["--child-flag"],"description":"Child flag"}]},
                {"names":["break"],"parserDirectives":{"optionsMustPrecedeArguments":false},"args":[{"name":"values","isVariadic":true,"optionsCanBreakVariadicArg":true}],"options":[{"names":["--child-flag"],"description":"Child flag"}]},
                {"names":["posix"],"parserDirectives":{"flagsArePosixNoncompliant":false},"options":[{"names":["-word"],"args":[{"name":"long-value"}]},{"names":["-w"]},{"names":["-o"]},{"names":["-r"]},{"names":["-d"]}]}
              ]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    #[test]
    fn parser_directives_stop_options_after_positional_arguments() {
        let (_dir, mut registry) = load_parser_directive_spec();
        let result = context_result(&mut registry, "strict value ");
        assert!(!result.suggestions.iter().any(|s| s.name == "--flag"));
    }

    #[test]
    fn parser_directives_control_variadic_option_breaks_and_current_arg() {
        let (_dir, mut registry) = load_parser_directive_spec();
        let result = context_result(&mut registry, "strict --files one ");
        assert_eq!(result.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("file"));
        assert!(!result.suggestions.iter().any(|s| s.name == "--flag"));

        let result = context_result(&mut registry, "strict break one ");
        assert!(result.suggestions.iter().any(|s| s.name == "--child-flag"));

        // A known option-looking token is still a variadic value when the
        // positional arg explicitly disallows option breaks, both while the
        // token is being edited and after it has been completed.
        let result = context_result(&mut registry, "strict child one --child-flag");
        assert_eq!(result.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("values"));
        assert!(!result.suggestions.iter().any(|s| s.name == "--child-flag"));
        let result = context_result(&mut registry, "strict child one --child-flag ");
        assert_eq!(result.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("values"));
        assert!(!result.suggestions.iter().any(|s| s.name == "--child-flag"));

        // POSIX mode must parse a multi-letter single-dash token as a short
        // chain before considering an exact `-word` option.
        let result = context_result(&mut registry, "strict posix -word ");
        assert!(result.current_arg.is_none());
    }

    #[test]
    fn parser_directives_parse_custom_attached_separator_and_non_posix_flags() {
        let (_dir, mut registry) = load_parser_directive_spec();
        for buffer in ["strict -word:abc", "strict -word=abc"] {
            let result = context_result(&mut registry, buffer);
            assert_eq!(result.search_term, "abc", "buffer={buffer}");
            assert_eq!(result.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("value"));
            assert!(result.suggestions.is_empty());
        }

        let result = context_result(&mut registry, "strict -wo");
        let word = result
            .suggestions
            .iter()
            .find(|s| s.name == "-word")
            .expect("non-POSIX option");
        assert_eq!(word.separator_to_add.as_deref(), Some(":"));
        let result = context_result(&mut registry, "strict --req");
        let required = result
            .suggestions
            .iter()
            .find(|s| s.name == "--required")
            .expect("boolean requiresSeparator option");
        assert_eq!(required.separator_to_add.as_deref(), Some(":"));
    }

    #[test]
    fn filter_strategy_uses_spec_only_without_an_active_argument() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "filterStrategy":"fuzzy",
              "subcommands":[{"names":["checkout"]},{"names":["cherry-pick"]}],
              "additionalSuggestions":[
                {"names":["deploy"],"type":"shortcut"},
                {"names":["bar"],"type":"shortcut","getQueryTerm":"/"}
              ]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();

        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "tool ce".into(),
                fuzzy: false,
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        assert!(result.fuzzy);
        assert!(result.suggestions.iter().any(|item| item.name == "checkout"));

        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "tool ?de".into(),
                fuzzy: false,
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let deploy = result
            .suggestions
            .iter()
            .find(|item| item.name == "deploy")
            .expect("shortcut query strips the leading question mark");
        assert_eq!(deploy.query_term.as_deref(), Some("de"));

        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "tool ?foo/bar".into(),
                fuzzy: false,
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let explicit = result
            .suggestions
            .iter()
            .find(|item| item.name == "bar")
            .expect("explicit getQueryTerm wins over shortcut handling");
        assert_eq!(explicit.query_term.as_deref(), Some("bar"));
    }

    #[test]
    fn active_argument_filter_strategy_does_not_inherit_parent() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("argtool.json"),
            r#"{
              "names":["argtool"],
              "filterStrategy":"fuzzy",
              "args":[{"name":"value","suggestions":[{"names":["target"]}]}]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "argtool tr".into(),
                fuzzy: false,
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        assert!(!result.fuzzy);
        assert!(!result.suggestions.iter().any(|item| item.name == "target"));

        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "argtool tr".into(),
                fuzzy: true,
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        assert!(result.fuzzy);
        assert!(
            result.suggestions.iter().any(|item| item.name == "target"),
            "fuzzy={} current_arg={:?} suggestions={:?}",
            result.fuzzy,
            result.current_arg.as_ref().map(|arg| &arg.name),
            result.suggestions.iter().map(|item| &item.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn static_named_categories_sort_without_reordering_positional_rows() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("sortme.json"),
            r#"{
              "names":["sortme"],
              "args":[{"name":"value","suggestions":[{"names":["zulu"]},{"names":["alpha"]}]}],
              "subcommands":[{"names":["sub-zulu"]},{"names":["sub-alpha"]}],
              "additionalSuggestions":[{"names":["add-zulu"]},{"names":["add-alpha"]}],
              "options":[{"names":["--zulu"]},{"names":["--alpha"]}]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = context_result(&mut registry, "sortme ");
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        let sub_start = names.iter().position(|name| *name == "sub-alpha").unwrap();
        assert_eq!(&names[sub_start..sub_start + 2], ["sub-alpha", "sub-zulu"]);
        let positional_start = names
            .iter()
            .position(|name| *name == "zulu")
            .unwrap_or_else(|| panic!("names={names:?}"));
        assert_eq!(&names[positional_start..positional_start + 2], ["zulu", "alpha"]);
        let additional_start = names.iter().position(|name| *name == "add-alpha").unwrap();
        assert_eq!(
            &names[additional_start..additional_start + 2],
            ["add-alpha", "add-zulu"]
        );
        let option_start = names.iter().position(|name| *name == "--alpha").unwrap();
        assert_eq!(&names[option_start..option_start + 2], ["--alpha", "--zulu"]);
    }

    #[test]
    fn named_lists_keep_name_order_when_hidden_and_passed_rows_drop() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("sortopt.json"),
            r#"{
              "names":["sortopt"],
              "subcommands":[
                {"names":["same"],"description":"second"},
                {"names":["same"],"description":"first"},
                {"names":["sub-zulu"],"hidden":true},
                {"names":["sub-beta"]},
                {"names":["sub-mike"],"hidden":true},
                {"names":["sub-Beta"]},
                {"names":["sub-alpha"]}
              ],
              "additionalSuggestions":[
                {"names":["add-zulu"],"hidden":true},
                {"names":["add-beta"]},
                {"names":["add-mike"],"hidden":true},
                {"names":["add-Beta"]},
                {"names":["add-alpha"]}
              ],
              "options":[
                {"names":["--zulu"],"hidden":true},
                {"names":["--beta"]},
                {"names":["--mike"],"hidden":true},
                {"names":["--Beta"]},
                {"names":["--alpha"]},
                {"names":["--one"]}
              ],
              "persistentOptions":[{"names":["--delta"]}]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let menu = context_result(&mut registry, "sortopt ");
        let names: Vec<_> = menu.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "same",
                "same",
                "sub-alpha",
                "sub-Beta",
                "sub-beta",
                "add-alpha",
                "add-Beta",
                "add-beta",
                "--alpha",
                "--Beta",
                "--beta",
                "--delta",
                "--one",
            ]
        );
        let same_descriptions: Vec<_> = menu
            .suggestions
            .iter()
            .filter(|item| item.name == "same")
            .map(|item| item.description.as_str())
            .collect();
        assert_eq!(same_descriptions, ["second", "first"]);

        let passed = context_result(&mut registry, "sortopt --one ");
        let names: Vec<_> = passed.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "same",
                "same",
                "sub-alpha",
                "sub-Beta",
                "sub-beta",
                "add-alpha",
                "add-Beta",
                "add-beta",
                "--alpha",
                "--Beta",
                "--beta",
                "--delta",
            ]
        );

        let revealed = context_result(&mut registry, "sortopt sub-zulu");
        assert!(
            revealed
                .suggestions
                .iter()
                .any(|item| item.name == "sub-zulu" && item.kind == "subcommand")
        );
        assert!(
            revealed
                .suggestions
                .iter()
                .all(|item| item.name == "sub-zulu" || item.kind == "auto-execute")
        );
        let revealed_option = context_result(&mut registry, "sortopt --zulu");
        assert!(
            revealed_option
                .suggestions
                .iter()
                .any(|item| item.name == "--zulu" && item.kind == "option")
        );
    }

    #[test]
    fn promoted_option_priority_stays_off_the_cached_spec() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "options":[
                {"names":["--one","-o"],"dependsOn":["--needed"]},
                {"names":["--needed"],"priority":40},
                {"names":["--alpha"],"priority":40}
              ]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let before = registry.get_arc("tool").expect("tool");
        let needed_before = before
            .options
            .iter()
            .find(|option| option.names.iter().any(|name| name == "--needed"))
            .unwrap()
            .clone();
        let alpha_before = before
            .options
            .iter()
            .find(|option| option.names.iter().any(|name| name == "--alpha"))
            .unwrap()
            .clone();
        assert_eq!(needed_before.meta.priority, Some(40));

        let menu = context_result(&mut registry, "tool ");
        assert_eq!(
            menu.suggestions
                .iter()
                .find(|item| item.name == "--needed")
                .map(|item| item.priority),
            Some(40)
        );
        assert_eq!(
            menu.suggestions
                .iter()
                .find(|item| item.name == "--alpha")
                .map(|item| item.priority),
            Some(40)
        );

        let result = context_result(&mut registry, "tool -o ");
        assert_eq!(
            result
                .suggestions
                .iter()
                .find(|item| item.name == "--needed")
                .map(|item| item.priority),
            Some(75)
        );
        assert_eq!(
            result
                .suggestions
                .iter()
                .find(|item| item.name == "--alpha")
                .map(|item| item.priority),
            Some(40)
        );

        let after = registry.get_arc("tool").expect("tool");
        assert!(Arc::ptr_eq(&before, &after));
        let needed_after = after
            .options
            .iter()
            .find(|option| option.names.iter().any(|name| name == "--needed"))
            .unwrap();
        let alpha_after = after
            .options
            .iter()
            .find(|option| option.names.iter().any(|name| name == "--alpha"))
            .unwrap();
        assert!(Arc::ptr_eq(&needed_before, needed_after));
        assert!(Arc::ptr_eq(&alpha_before, alpha_after));
        assert_eq!(needed_after.meta.priority, Some(40));
        assert_eq!(alpha_after.meta.priority, Some(40));
    }

    #[test]
    fn plain_subcommand_descent_shares_the_cached_child_arc() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names":["git"],
              "subcommands":[
                {"names":["checkout","co"],"description":"Switch","subcommands":[{"names":["orphan"]}]},
                {"names":["status"]}
              ]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let git = registry.get_arc("git").expect("git");
        let checkout = Arc::clone(git.find_subcommand("checkout").expect("checkout"));
        let orphan = Arc::clone(checkout.find_subcommand("orphan").expect("orphan"));

        let (mut tokens, trailing) = tokenize("git checkout orphan ");
        let context = resolve_context(Arc::clone(&git), &mut tokens, trailing, "", "", Some(&mut registry));
        assert!(Arc::ptr_eq(&orphan, &context.spec));
        assert!(Arc::ptr_eq(&checkout, &context.help_parent));

        let git_after = registry.get_arc("git").expect("git");
        assert!(Arc::ptr_eq(&git, &git_after));
        assert!(Arc::ptr_eq(
            &checkout,
            git_after.find_subcommand("checkout").expect("checkout")
        ));
        assert!(Arc::ptr_eq(
            &orphan,
            checkout.find_subcommand("orphan").expect("orphan")
        ));
    }

    #[test]
    fn path_load_spec_descent_writes_a_new_arc_and_shares_loaded_children() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "subcommands":[
                {"names":["child"],"description":"wrapper","loadSpec":"child"},
                {"names":["keep"],"description":"sibling"}
              ]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("child.json"),
            r#"{
              "names":["child-target"],
              "description":"loaded",
              "subcommands":[{"names":["inside"],"description":"nested"}]
            }"#,
        )
        .unwrap();
        fs::write(dir.path().join("index.json"), r#"{"files":{"tool":"tool.json"}}"#).unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let tool = registry.get_arc("tool").expect("tool");
        let stub = Arc::clone(tool.find_subcommand("child").expect("stub"));
        let keep = Arc::clone(tool.find_subcommand("keep").expect("keep"));

        let (mut tokens, trailing) = tokenize("tool child ");
        let context = resolve_context(Arc::clone(&tool), &mut tokens, trailing, "", "", Some(&mut registry));
        let cached = registry.cached_load_spec("child").expect("child file");
        let inside = Arc::clone(cached.find_subcommand("inside").expect("inside"));

        assert!(!Arc::ptr_eq(&stub, &context.spec));
        assert!(!Arc::ptr_eq(&cached, &context.spec));
        assert!(Arc::ptr_eq(&tool, &context.help_parent));
        assert_eq!(context.spec.names, vec!["child"]);
        assert_eq!(context.spec.description, "loaded");
        assert!(context.spec.meta.ai_resolved_reference);
        assert!(context.spec.load_spec.is_none());
        assert!(Arc::ptr_eq(
            &inside,
            context.spec.find_subcommand("inside").expect("walk inside")
        ));
        assert!(!inside.meta.ai_resolved_reference);

        let tool_after = registry.get_arc("tool").expect("tool");
        let cached_after = registry.cached_load_spec("child").expect("child file");
        assert!(Arc::ptr_eq(&tool, &tool_after));
        assert!(Arc::ptr_eq(&stub, tool_after.find_subcommand("child").expect("stub")));
        assert!(Arc::ptr_eq(&keep, tool_after.find_subcommand("keep").expect("keep")));
        assert!(Arc::ptr_eq(&cached, &cached_after));
        assert_eq!(stub.description, "wrapper");
        assert!(stub.find_subcommand("inside").is_none());
        assert!(!stub.meta.ai_resolved_reference);
        assert!(matches!(stub.load_spec, Some(crate::ir::LoadSpec::Path(ref path)) if path == "child"));
        assert_eq!(cached.names, vec!["child-target"]);
        assert_eq!(cached.description, "loaded");
        assert!(!cached.meta.ai_resolved_reference);
        assert!(Arc::ptr_eq(
            &inside,
            cached_after.find_subcommand("inside").expect("cached inside")
        ));
        assert!(registry.cached_load_spec("keep").is_none());
    }

    #[test]
    fn argument_load_spec_holds_the_cached_arc() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "args":[{"name":"command","loadSpec":"curl"}]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("curl.json"),
            r#"{
              "names":["curl"],
              "description":"transfer",
              "subcommands":[{"names":["inside"],"description":"nested"}]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"tool":"tool.json","curl":"curl.json"}}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let tool = registry.get_arc("tool").expect("tool");
        assert!(tool.args[0].resolved_spec.is_none());
        assert!(!registry.is_cached("curl"));

        let (mut tokens, trailing) = tokenize("tool curl ");
        let context = resolve_context(Arc::clone(&tool), &mut tokens, trailing, "", "", Some(&mut registry));
        let cached = registry.get_arc("curl").expect("curl");
        let inside = Arc::clone(cached.find_subcommand("inside").expect("inside"));

        assert!(context.crossed_loaded_spec);
        assert!(Arc::ptr_eq(&cached, &context.spec));
        assert!(Arc::ptr_eq(&tool, &context.help_parent));
        assert_eq!(context.spec.names, vec!["curl"]);
        assert_eq!(context.spec.description, "transfer");
        assert!(!context.spec.meta.ai_resolved_reference);
        assert!(Arc::ptr_eq(
            &inside,
            context.spec.find_subcommand("inside").expect("walk inside")
        ));
        assert!(registry.cached_load_spec("curl").is_none());
        assert!(registry.is_cached("curl"));

        let tool_after = registry.get_arc("tool").expect("tool");
        let cached_after = registry.get_arc("curl").expect("curl");
        assert!(Arc::ptr_eq(&tool, &tool_after));
        assert!(Arc::ptr_eq(&cached, &cached_after));
        assert!(tool_after.args[0].resolved_spec.is_none());
        assert!(matches!(
            tool_after.args[0].load_spec,
            Some(crate::ir::LoadSpec::Path(ref path)) if path == "curl"
        ));
        assert_eq!(cached_after.description, "transfer");
        assert!(!cached_after.meta.ai_resolved_reference);
        assert!(Arc::ptr_eq(
            &inside,
            cached_after.find_subcommand("inside").expect("cached inside")
        ));
    }

    #[test]
    fn generate_spec_replaces_the_walk_root_and_shares_static_children() {
        let dir = tempfile::tempdir().unwrap();
        let (hook_id, hook_entry) = crate::hook_backend::test_typed_entry(
            "git#generate#0",
            "generateSpec",
            serde_json::json!({
                "op": "spec-object",
                "fields": [
                    {"key": "name", "value": {"op": "string", "value": "git"}},
                    {"key": "subcommands", "value": {"op": "array", "items": [{
                        "op": "spec-object",
                        "fields": [
                            {"key": "name", "value": {"op": "string", "value": "changelog"}},
                            {"key": "description", "value": {"op": "string", "value": "Generated entry"}}
                        ]
                    }]}}
                ]
            }),
        );
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names":["git"],
              "description":"source",
              "jsGenerateSpec":"git#generate#0",
              "subcommands":[
                {"names":["checkout"],"description":"Switch branches"},
                {"names":["cherry-pick"],"description":"Apply commits"}
              ]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let git = registry.get_arc("git").expect("git");
        let checkout = Arc::clone(git.find_subcommand("checkout").expect("checkout"));
        let cherry_pick = Arc::clone(git.find_subcommand("cherry-pick").expect("cherry-pick"));
        assert!(!checkout.meta.ai_generated);
        let native = crate::hook_backend::test_native_hooks(vec![(hook_id, hook_entry)]);
        let _bound = crate::hook_backend::bind_native(Arc::new(native));
        let cwd = dir.path().display().to_string();

        let (mut tokens, trailing) = tokenize("git ");
        let context = crate::hook_backend::enter_context(&cwd, &crate::hook_types::ShellContext::default(), || {
            resolve_context(Arc::clone(&git), &mut tokens, trailing, "", "", Some(&mut registry))
        });

        assert!(!Arc::ptr_eq(&git, &context.spec));
        assert_eq!(context.spec.names, vec!["git"]);
        assert!(context.spec.find_subcommand("changelog").is_some());
        assert!(context.spec.find_subcommand("changelog").unwrap().meta.ai_generated);
        assert!(Arc::ptr_eq(
            &checkout,
            context.spec.find_subcommand("checkout").expect("walk checkout")
        ));
        assert!(Arc::ptr_eq(
            &cherry_pick,
            context.spec.find_subcommand("cherry-pick").expect("walk cherry-pick")
        ));
        assert!(!checkout.meta.ai_generated);
        assert!(!context.spec.meta.ai_generated);

        let (mut tokens, trailing) = tokenize("git checkout ");
        let entered = crate::hook_backend::enter_context(&cwd, &crate::hook_types::ShellContext::default(), || {
            resolve_context(Arc::clone(&git), &mut tokens, trailing, "", "", Some(&mut registry))
        });
        assert!(Arc::ptr_eq(&checkout, &entered.spec));
        assert!(!Arc::ptr_eq(&git, &entered.help_parent));
        assert!(entered.help_parent.find_subcommand("changelog").is_some());
        assert!(!checkout.meta.ai_generated);

        let git_after = registry.get_arc("git").expect("git");
        assert!(Arc::ptr_eq(&git, &git_after));
        assert!(git_after.find_subcommand("changelog").is_none());
        assert_eq!(git_after.description, "source");
        assert_eq!(git_after.js_generate_spec.as_deref(), Some("git#generate#0"));
        assert!(Arc::ptr_eq(
            &checkout,
            git_after.find_subcommand("checkout").expect("cached checkout")
        ));
        assert_eq!(checkout.description, "Switch branches");
        assert!(!checkout.meta.ai_generated);
    }

    #[test]
    fn js_load_spec_on_a_subcommand_replaces_only_the_walk_arc() {
        let dir = tempfile::tempdir().unwrap();
        let (hook_id, hook_entry) = crate::hook_backend::test_typed_entry(
            "tool#load#0",
            "loadSpec",
            serde_json::json!({
                "op": "spec-object",
                "fields": [
                    {"key": "name", "value": {"op": "string", "value": "loaded"}},
                    {"key": "description", "value": {"op": "string", "value": "from hook"}},
                    {"key": "subcommands", "value": {"op": "array", "items": [{
                        "op": "spec-object",
                        "fields": [
                            {"key": "name", "value": {"op": "string", "value": "from-hook"}},
                            {"key": "description", "value": {"op": "string", "value": "hooked"}}
                        ]
                    }]}}
                ]
            }),
        );
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "subcommands":[{
                "names":["child"],
                "description":"stub",
                "jsLoadSpec":"tool#load#0",
                "subcommands":[{"names":["static-child"],"description":"stays"}]
              }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let tool = registry.get_arc("tool").expect("tool");
        let stub = Arc::clone(tool.find_subcommand("child").expect("stub"));
        let static_child = Arc::clone(stub.find_subcommand("static-child").expect("static-child"));
        let native = crate::hook_backend::test_native_hooks(vec![(hook_id, hook_entry)]);
        let _bound = crate::hook_backend::bind_native(Arc::new(native));
        let cwd = dir.path().display().to_string();

        let (mut tokens, trailing) = tokenize("tool child ");
        let context = crate::hook_backend::enter_context(&cwd, &crate::hook_types::ShellContext::default(), || {
            resolve_context(Arc::clone(&tool), &mut tokens, trailing, "", "", Some(&mut registry))
        });

        assert!(!Arc::ptr_eq(&stub, &context.spec));
        assert!(Arc::ptr_eq(&tool, &context.help_parent));
        assert_eq!(context.spec.names, vec!["child"]);
        assert_eq!(context.spec.description, "from hook");
        assert!(context.spec.find_subcommand("from-hook").unwrap().meta.ai_generated);
        assert!(Arc::ptr_eq(
            &static_child,
            context.spec.find_subcommand("static-child").expect("walk static")
        ));
        assert!(!static_child.meta.ai_generated);

        let tool_after = registry.get_arc("tool").expect("tool");
        assert!(Arc::ptr_eq(&tool, &tool_after));
        assert!(Arc::ptr_eq(&stub, tool_after.find_subcommand("child").expect("stub")));
        assert_eq!(stub.description, "stub");
        assert_eq!(stub.js_load_spec.as_deref(), Some("tool#load#0"));
        assert!(stub.find_subcommand("from-hook").is_none());
        assert!(Arc::ptr_eq(
            &static_child,
            stub.find_subcommand("static-child").expect("cached static")
        ));
        assert_eq!(static_child.description, "stays");
        assert!(!static_child.meta.ai_generated);
    }

    #[test]
    fn option_state_filters_exclusive_repeatable_and_promotes_dependencies() {
        let (_dir, mut registry) = load_option_state_spec();
        let result = context_result(&mut registry, "tool -o ");
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"--two"), "{names:?}");
        assert!(!names.contains(&"-t"), "{names:?}");
        assert_eq!(
            result
                .suggestions
                .iter()
                .find(|s| s.name == "--needed")
                .map(|s| s.priority),
            Some(75)
        );

        let result = context_result(&mut registry, "tool --once --once ");
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"--once"), "{names:?}");

        let result = context_result(&mut registry, "tool --twice --twice ");
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"--twice"), "{names:?}");

        let result = context_result(&mut registry, "tool --many --many ");
        assert!(result.suggestions.iter().any(|s| s.name == "--many"));
    }

    #[test]
    fn persistent_options_follow_subcommands_and_aliases_count_as_one_option() {
        let (_dir, mut registry) = load_option_state_spec();
        let result = context_result(&mut registry, "tool child ");
        assert!(result.suggestions.iter().any(|s| s.name == "--global"));

        let result = context_result(&mut registry, "tool -o ");
        assert!(result.suggestions.iter().any(|s| s.name == "--needed"));
        let result = context_result(&mut registry, "tool --one ");
        assert!(!result.suggestions.iter().any(|s| s.name == "--one"));
    }

    #[test]
    fn entered_load_spec_keeps_wrapper_names_and_merges_persistent_options_and_directives() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "parserDirectives":{
                "optionsMustPrecedeArguments":true,
                "flagsArePosixNoncompliant":true
              },
              "persistentOptions":[{"names":["--global"],"description":"Global"}],
              "subcommands":[
                {"names":["child"],"description":"wrapper","loadSpec":"child"},
                {"names":["keep"],"description":"wrapper","loadSpec":"keep"}
              ]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("child.json"),
            r#"{
              "names":["child-target"],
              "description":"loaded",
              "parserDirectives":{"optionArgSeparators":[":"]},
              "persistentOptions":[{"names":["--inner"],"description":"Inner"}],
              "options":[{"names":["--local"],"description":"Local","args":[{"name":"local-value"}]}],
              "args":[{"name":"value"}],
              "subcommands":[{"names":["deeper"]}]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("keep.json"),
            r#"{
              "names":["keep-target"],
              "description":"loaded-keep",
              "options":[{"names":["--local"],"description":"Local"}],
              "args":[{"name":"value"}]
            }"#,
        )
        .unwrap();
        fs::write(dir.path().join("index.json"), r#"{"files":{"tool":"tool.json"}}"#).unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();

        let tool = registry.get_arc("tool").expect("tool");
        let mut tokens = vec!["tool".to_string(), "child".to_string()];
        let walked = resolve_context(tool, &mut tokens, true, "", "", Some(&mut registry));
        assert_eq!(walked.spec.names, vec!["child"]);
        assert_eq!(walked.spec.description, "loaded");
        assert!(walked.spec.find_subcommand("deeper").is_some());
        assert!(walked.parser_directives.options_must_precede_arguments.is_none());
        assert!(walked.parser_directives.flags_are_posix_noncompliant.is_none());
        assert_eq!(
            walked.parser_directives.option_arg_separators.as_deref(),
            Some(&[":".to_string()][..])
        );
        let persistent: Vec<_> = walked
            .persistent_options
            .iter()
            .filter_map(|option| option.names.first().map(String::as_str))
            .collect();
        assert!(persistent.contains(&"--global"), "{persistent:?}");
        assert!(persistent.contains(&"--inner"), "{persistent:?}");
        assert!(registry.cached_load_spec("child").is_some());
        assert!(registry.cached_load_spec("keep").is_none());

        let entered = context_result(&mut registry, "tool child ");
        for name in ["--global", "--inner", "--local", "deeper"] {
            assert!(
                entered.suggestions.iter().any(|suggestion| suggestion.name == name),
                "{name} missing from {:?}",
                entered
                    .suggestions
                    .iter()
                    .map(|suggestion| suggestion.name.as_str())
                    .collect::<Vec<_>>()
            );
        }
        let after_value = context_result(&mut registry, "tool child value ");
        for name in ["--global", "--inner", "--local"] {
            assert!(
                after_value.suggestions.iter().any(|suggestion| suggestion.name == name),
                "{name} missing after the positional"
            );
        }
        assert!(
            !after_value
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "deeper")
        );
        let attached = context_result(&mut registry, "tool child --local:abc");
        assert_eq!(attached.search_term, "abc");
        assert_eq!(
            attached.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("local-value")
        );

        let tool = registry.get("tool").expect("tool");
        let stub = tool.find_subcommand("child").expect("stub");
        assert_eq!(stub.description, "wrapper");
        assert!(stub.find_subcommand("deeper").is_none());
        assert!(matches!(stub.load_spec, Some(crate::ir::LoadSpec::Path(ref path)) if path == "child"));

        let tool = registry.get_arc("tool").expect("tool");
        let mut tokens = vec!["tool".to_string(), "keep".to_string()];
        let walked = resolve_context(tool, &mut tokens, true, "", "", Some(&mut registry));
        assert_eq!(walked.spec.names, vec!["keep"]);
        assert_eq!(walked.spec.description, "loaded-keep");
        assert_eq!(walked.parser_directives.options_must_precede_arguments, Some(true));
        assert_eq!(walked.parser_directives.flags_are_posix_noncompliant, Some(true));
        assert!(walked.parser_directives.option_arg_separators.is_none());
        assert!(
            walked
                .persistent_options
                .iter()
                .any(|option| option.names.iter().any(|name| name == "--global"))
        );

        let keep = context_result(&mut registry, "tool keep ");
        assert!(keep.suggestions.iter().any(|suggestion| suggestion.name == "--global"));
        assert!(keep.suggestions.iter().any(|suggestion| suggestion.name == "--local"));
        let keep_after = context_result(&mut registry, "tool keep value ");
        assert!(
            !keep_after
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "--global")
        );
        assert!(
            !keep_after
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "--local")
        );
        let keep_attached = context_result(&mut registry, "tool keep --local:abc");
        assert_ne!(keep_attached.search_term, "abc");
    }

    fn context_result(registry: &mut Registry, buffer: &str) -> CompleteResult {
        complete(
            registry,
            &CompleteRequest {
                buffer: buffer.into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        )
    }

    fn load_gcloud_load_spec() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("gcloud")).unwrap();
        fs::write(
            dir.path().join("gcloud.json"),
            r#"{
              "names":["gcloud"],
              "subcommands":[
                {"names":["compute"],"description":"Compute Engine","loadSpec":"gcloud/compute"},
                {"names":["sql"],"description":"Cloud SQL","loadSpec":"gcloud/sql"}
              ]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("gcloud/compute.json"),
            r#"{"names":["compute"],"subcommands":[{"names":["instances"]},{"names":["disks"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("gcloud/sql.json"),
            r#"{"names":["sql"],"subcommands":[{"names":["backups"]}]}"#,
        )
        .unwrap();
        fs::write(dir.path().join("index.json"), r#"{"files":{"gcloud":"gcloud.json"}}"#).unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    #[test]
    fn subcommand_load_spec_loads_only_the_entered_file() {
        let (_dir, mut registry) = load_gcloud_load_spec();

        let menu = context_result(&mut registry, "gcloud ");
        let menu_names: Vec<_> = menu.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(menu_names.contains(&"compute"), "{menu_names:?}");
        assert!(menu_names.contains(&"sql"), "{menu_names:?}");
        assert!(registry.cached_load_spec("gcloud/compute").is_none());
        assert!(registry.cached_load_spec("gcloud/sql").is_none());

        let typing = context_result(&mut registry, "gcloud comp");
        assert!(
            typing.suggestions.iter().any(|item| item.name == "compute"),
            "{:?}",
            typing.suggestions
        );
        assert!(registry.cached_load_spec("gcloud/compute").is_none());

        let entered = context_result(&mut registry, "gcloud compute ");
        let entered_names: Vec<_> = entered.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(entered_names.contains(&"instances"), "{entered_names:?}");
        assert!(entered_names.contains(&"disks"), "{entered_names:?}");
        assert!(!entered_names.contains(&"backups"), "{entered_names:?}");
        assert!(!entered_names.contains(&"sql"), "{entered_names:?}");
        let compute = registry.cached_load_spec("gcloud/compute").expect("compute cached");
        assert!(registry.cached_load_spec("gcloud/sql").is_none());

        let again = context_result(&mut registry, "gcloud compute ");
        assert!(again.suggestions.iter().any(|item| item.name == "instances"));
        let compute_again = registry.cached_load_spec("gcloud/compute").expect("cache hit");
        assert!(std::sync::Arc::ptr_eq(&compute, &compute_again));

        let gcloud = registry.get("gcloud").expect("gcloud");
        let stub = gcloud.find_subcommand("compute").expect("stub");
        assert!(stub.find_subcommand("instances").is_none());
        assert!(matches!(stub.load_spec, Some(crate::ir::LoadSpec::Path(ref path)) if path == "gcloud/compute"));
        let sql = gcloud.find_subcommand("sql").expect("sql stub");
        assert!(matches!(sql.load_spec, Some(crate::ir::LoadSpec::Path(ref path)) if path == "gcloud/sql"));
    }

    #[test]
    fn dynamic_hook_timeout_uses_the_configured_milliseconds() {
        let defaults = fastab_settings::settings::Settings::new_fake();
        assert_eq!(dynamic_hook_timeout_from_settings(&defaults), Duration::from_secs(5));

        let settings =
            fastab_settings::settings::Settings::from_slice(&[("autocomplete.scriptTimeout", serde_json::json!(250))]);
        assert_eq!(
            dynamic_hook_timeout_from_settings(&settings),
            Duration::from_millis(250)
        );
        settings.set_value("autocomplete.scriptTimeout", 15_000).unwrap();
        assert_eq!(dynamic_hook_timeout_from_settings(&settings), Duration::from_secs(15));

        assert_eq!(
            duration_from_script_timeout_ms(crate::generate::DEFAULT_SCRIPT_TIMEOUT_MS),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn dynamic_hook_timeout_clamps_negative_settings_to_zero() {
        let settings =
            fastab_settings::settings::Settings::from_slice(&[("autocomplete.scriptTimeout", serde_json::json!(-1))]);
        assert_eq!(dynamic_hook_timeout_from_settings(&settings), Duration::ZERO);
        assert_eq!(duration_from_script_timeout_ms(-1), Duration::ZERO);
        assert_eq!(duration_from_script_timeout_ms(i64::MIN), Duration::ZERO);
    }

    fn load_option_chain_spec() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "additionalSuggestions":[{"names":["shortcut"],"description":"Shortcut"}],
              "options":[
                {"names":["-a","--all"],"description":"All"},
                {"names":["-b","--brief"],"description":"Brief"},
                {"names":["-m","--message"],"description":"Message","args":[{
                  "name":"message",
                  "description":"Message value",
                  "suggestions":[{"names":["foo"]},{"names":["foobar"]}]
                }]},
                {"names":["--long"],"description":"Long only"}
              ]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    fn load_argument_load_spec() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "args":[{"name":"input","loadSpec":"positional-target"}],
              "options":[{"names":["--profile","-p"],"args":[{"name":"profile","loadSpec":"profile-target"}]}]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("positional-target.json"),
            r#"{
              "names":["positional-target"],
              "args":[{"name":"next input"}],
              "subcommands":[{"names":["child"]}],
              "options":[{"names":["--target-only"]}]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("profile-target.json"),
            r#"{
              "names":["profile-target"],
              "args":[{"name":"next profile"}],
              "subcommands":[{"names":["list"]}],
              "options":[{"names":["--profile-only"]}]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    #[test]
    fn current_arg_tracks_positional_query_without_consuming_it() {
        let (_dir, mut registry) = load_context_spec();
        let result = context_result(&mut registry, "git checkout fe");
        assert_eq!(result.search_term, "fe");
        assert_eq!(result.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("branch"));
        assert_eq!(
            result.current_arg.as_ref().map(|arg| arg.description.as_str()),
            Some("Branch value")
        );
    }

    #[test]
    fn current_arg_advances_after_a_completed_positional_token() {
        let (_dir, mut registry) = load_context_spec();
        let result = context_result(&mut registry, "git checkout feature ");
        assert_eq!(result.search_term, "");
        assert_eq!(result.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("path"));
        assert_eq!(
            result.current_arg.as_ref().map(|arg| arg.description.as_str()),
            Some("Path value")
        );
    }

    #[test]
    fn completed_positional_load_spec_replaces_context_but_not_the_editing_token() {
        let (_dir, mut registry) = load_argument_load_spec();

        let editing = context_result(&mut registry, "tool value");
        assert_eq!(editing.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("input"));
        assert!(!editing.suggestions.iter().any(|suggestion| suggestion.name == "child"));
        assert!(!registry.is_cached("positional-target"));
        assert!(!registry.is_cached("profile-target"));

        let completed = context_result(&mut registry, "tool value ");
        assert_eq!(
            completed.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("next input")
        );
        assert!(registry.is_cached("positional-target"));
        assert!(!registry.is_cached("profile-target"));
        assert!(
            completed
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "child")
        );
        assert!(
            completed
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "--target-only")
        );
    }

    #[test]
    fn completed_separated_option_value_loads_the_option_spec() {
        let (_dir, mut registry) = load_argument_load_spec();

        let editing = context_result(&mut registry, "tool --profile value");
        assert_eq!(
            editing.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("profile")
        );
        assert!(!editing.suggestions.iter().any(|suggestion| suggestion.name == "list"));
        assert!(!registry.is_cached("profile-target"));
        assert!(!registry.is_cached("positional-target"));

        let completed = context_result(&mut registry, "tool --profile value ");
        assert!(registry.is_cached("profile-target"));
        assert!(!registry.is_cached("positional-target"));
        assert_eq!(
            completed.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("next profile")
        );
        assert!(completed.suggestions.iter().any(|suggestion| suggestion.name == "list"));
        assert!(
            completed
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "--profile-only")
        );
    }

    #[test]
    fn completed_equals_and_short_attached_values_load_the_option_spec() {
        let (_dir, mut registry) = load_argument_load_spec();

        for buffer in ["tool --profile=value", "tool -pvalue"] {
            let editing = context_result(&mut registry, buffer);
            assert_eq!(
                editing.current_arg.as_ref().map(|arg| arg.name.as_str()),
                Some("profile"),
                "buffer={buffer}"
            );
            assert!(
                editing.suggestions.iter().all(|suggestion| suggestion.name != "list"),
                "buffer={buffer}, suggestions={:?}",
                editing.suggestions
            );
            assert!(!registry.is_cached("profile-target"), "buffer={buffer}");
            assert!(!registry.is_cached("positional-target"), "buffer={buffer}");
        }

        for buffer in ["tool --profile=value ", "tool -pvalue "] {
            let completed = context_result(&mut registry, buffer);
            assert_eq!(
                completed.current_arg.as_ref().map(|arg| arg.name.as_str()),
                Some("next profile"),
                "buffer={buffer}"
            );
            assert!(
                completed.suggestions.iter().any(|suggestion| suggestion.name == "list"),
                "buffer={buffer}, suggestions={:?}",
                completed.suggestions
            );
            assert!(registry.is_cached("profile-target"), "buffer={buffer}");
            assert!(!registry.is_cached("positional-target"), "buffer={buffer}");
        }
    }

    #[test]
    fn positional_load_spec_after_double_dash_still_switches_context() {
        let (_dir, mut registry) = load_argument_load_spec();
        let completed = context_result(&mut registry, "tool -- value ");
        assert_eq!(
            completed.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("next input")
        );
        assert!(
            completed
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "child")
        );
        assert!(
            completed
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "--target-only"),
            "a path loadSpec restarts the parser, so -- no longer suppresses the loaded spec"
        );
        assert!(
            completed
                .suggestions
                .iter()
                .all(|suggestion| suggestion.name != "--profile-only")
        );
    }

    #[test]
    fn current_arg_handles_option_value_and_equals_form() {
        let (_dir, mut registry) = load_context_spec();
        for (buffer, search) in [
            ("git --message foo", "foo"),
            ("git --message ", ""),
            ("git --message=foo", "foo"),
        ] {
            let result = context_result(&mut registry, buffer);
            assert_eq!(result.search_term, search, "buffer={buffer}");
            assert_eq!(
                result.current_arg.as_ref().map(|arg| arg.name.as_str()),
                Some("message"),
                "buffer={buffer}"
            );
            assert!(
                result.suggestions.is_empty(),
                "an option value must not fall back to sibling options: buffer={buffer}, suggestions={:?}",
                result.suggestions
            );
        }
        assert!(context_result(&mut registry, "git --message").current_arg.is_none());
    }

    #[test]
    fn short_option_chain_extends_each_short_option_and_keeps_the_current_one() {
        let (_dir, mut registry) = load_option_chain_spec();
        let result = context_result(&mut registry, "tool -ab");
        let option_names = result
            .suggestions
            .iter()
            .filter(|suggestion| suggestion.kind == "option")
            .map(|suggestion| suggestion.name.as_str())
            .collect::<Vec<_>>();
        assert!(option_names.contains(&"-ab"), "{option_names:?}");
        assert!(option_names.contains(&"-aba"), "{option_names:?}");
        assert!(option_names.contains(&"-abm"), "{option_names:?}");
        assert!(!option_names.contains(&"--long"), "{option_names:?}");
        assert!(
            result
                .suggestions
                .iter()
                .find(|suggestion| suggestion.name == "shortcut")
                .is_some_and(|suggestion| suggestion.query_term.as_deref() == Some(""))
        );
    }

    #[test]
    fn mandatory_last_chain_option_shows_its_args_without_deleting_the_chain() {
        let (_dir, mut registry) = load_option_chain_spec();
        let result = context_result(&mut registry, "tool -am");
        assert_eq!(
            result.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("message")
        );
        assert!(
            result
                .suggestions
                .iter()
                .any(|suggestion| { suggestion.name == "foo" && suggestion.query_term.as_deref() == Some("") })
        );
        assert!(result.suggestions.iter().any(|suggestion| {
            suggestion.kind == "option" && suggestion.name == "-am" && suggestion.query_term.as_deref() == Some("-am")
        }));
        assert!(
            result
                .suggestions
                .iter()
                .all(|suggestion| suggestion.name != "-ama" && suggestion.name != "-amb")
        );
    }

    #[test]
    fn attached_short_chain_argument_replaces_only_its_value() {
        let (_dir, mut registry) = load_option_chain_spec();
        let result = context_result(&mut registry, "tool -amfoo");
        assert_eq!(result.search_term, "foo");
        assert_eq!(
            result.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("message")
        );
        assert!(result.suggestions.iter().any(|suggestion| suggestion.name == "foobar"));
        assert!(result.suggestions.iter().all(|suggestion| suggestion.kind != "option"));
    }

    #[test]
    fn completed_short_chain_is_consumed_before_the_argument_context() {
        let (_dir, mut registry) = load_option_chain_spec();
        let result = context_result(&mut registry, "tool -am ");
        assert_eq!(result.search_term, "");
        assert_eq!(
            result.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("message")
        );
        assert!(result.suggestions.iter().any(|suggestion| suggestion.name == "foo"));
        assert!(result.suggestions.iter().all(|suggestion| suggestion.kind != "option"));
    }

    #[test]
    fn unknown_chain_and_long_options_keep_the_normal_path() {
        let (_dir, mut registry) = load_option_chain_spec();
        let unknown = context_result(&mut registry, "tool -az");
        assert!(unknown.suggestions.iter().all(|suggestion| suggestion.kind != "option"));

        let long = context_result(&mut registry, "tool --long");
        assert!(
            long.suggestions
                .iter()
                .any(|suggestion| suggestion.kind == "option" && suggestion.name == "--long")
        );
    }

    fn names_of(result: &CompleteResult) -> Vec<&str> {
        result
            .suggestions
            .iter()
            .map(|suggestion| suggestion.name.as_str())
            .collect()
    }

    fn complete_fuzzy(registry: &mut Registry, buffer: &str, fuzzy: bool) -> CompleteResult {
        complete(
            registry,
            &CompleteRequest {
                buffer: buffer.into(),
                fuzzy,
                include_history: false,
                ..CompleteRequest::default()
            },
        )
    }

    /// Fig keeps subcommands in the list only while `canConsumeSubcommands` is
    /// set and `onlySuggestArgs` is clear. Fuzzy search then filters that
    /// list; it does not invent a second candidate set. `git check m` used to
    /// list `merge` / `mv` / `commit` because the consumed alias argument
    /// never cleared the subcommand flag.
    #[test]
    fn suggestion_flags_follow_fig_after_a_consumed_argument() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names":["git"],
              "args":[{"name":"alias","isOptional":true,"suggestions":[{"names":["mine"]}]}],
              "additionalSuggestions":[{"names":["ship"],"description":"Ship it"}],
              "options":[
                {"names":["--namespace"],"description":"Namespace"},
                {"names":["--version"],"description":"Version"}
              ],
              "subcommands":[
                {"names":["checkout"],"description":"Switch","args":[
                  {"name":"branch","isOptional":true,"suggestions":[{"names":["master"]},{"names":["main"]}]}
                ]},
                {"names":["merge"]},
                {"names":["mv"]},
                {"names":["commit"]},
                {"names":["maintenance"]}
              ]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();

        let typing = complete_fuzzy(&mut registry, "git che", false);
        let typing_names = names_of(&typing);
        assert!(typing_names.contains(&"checkout"), "{typing_names:?}");
        assert!(!typing_names.contains(&"merge"), "{typing_names:?}");

        let fuzzy_slot = complete_fuzzy(&mut registry, "git m", true);
        let fuzzy_slot_names = names_of(&fuzzy_slot);
        assert!(fuzzy_slot_names.contains(&"merge"), "{fuzzy_slot_names:?}");
        assert!(fuzzy_slot_names.contains(&"mv"), "{fuzzy_slot_names:?}");
        assert!(fuzzy_slot_names.contains(&"commit"), "{fuzzy_slot_names:?}");
        assert!(fuzzy_slot_names.contains(&"maintenance"), "{fuzzy_slot_names:?}");
        assert!(
            fuzzy_slot
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "merge" && suggestion.kind == "subcommand"),
            "{fuzzy_slot_names:?}"
        );

        let consumed = complete_fuzzy(&mut registry, "git check m", true);
        let consumed_names = names_of(&consumed);
        for hidden in ["checkout", "merge", "mv", "commit", "maintenance"] {
            assert!(!consumed_names.contains(&hidden), "{hidden} in {consumed_names:?}");
        }
        assert!(
            consumed
                .suggestions
                .iter()
                .all(|suggestion| suggestion.kind != "subcommand"),
            "{consumed_names:?}"
        );
        assert!(
            consumed_names.contains(&"--namespace"),
            "options stay legal after a positional, and fuzzy still filters them: {consumed_names:?}"
        );
        assert!(!consumed_names.contains(&"mine"), "{consumed_names:?}");

        let checkout = complete_fuzzy(&mut registry, "git checkout m", true);
        let checkout_names = names_of(&checkout);
        assert!(checkout_names.contains(&"master"), "{checkout_names:?}");
        assert!(checkout_names.contains(&"main"), "{checkout_names:?}");
        assert!(!checkout_names.contains(&"merge"), "{checkout_names:?}");

        let after_arg = complete_fuzzy(&mut registry, "git check ", false);
        let after_arg_names = names_of(&after_arg);
        assert!(after_arg_names.contains(&"ship"), "{after_arg_names:?}");
        assert!(after_arg_names.contains(&"--namespace"), "{after_arg_names:?}");
        assert!(!after_arg_names.contains(&"merge"), "{after_arg_names:?}");
        assert!(!after_arg_names.contains(&"checkout"), "{after_arg_names:?}");
    }

    #[test]
    fn bundled_git_check_m_does_not_list_sibling_subcommands() {
        let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bundle/specs-ir/git.json");
        let dir = tempfile::tempdir().unwrap();
        fs::copy(&source, dir.path().join("git.json")).unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();

        let typing = complete_fuzzy(&mut registry, "git che", true);
        assert!(names_of(&typing).contains(&"checkout"), "{:?}", names_of(&typing));

        let fuzzy_slot = complete_fuzzy(&mut registry, "git m", true);
        assert!(
            names_of(&fuzzy_slot).contains(&"merge"),
            "fuzzy still matches subcommands before any argument is consumed: {:?}",
            names_of(&fuzzy_slot)
        );

        let consumed = complete_fuzzy(&mut registry, "git check m", true);
        let names = names_of(&consumed);
        for hidden in [
            "checkout",
            "merge",
            "mv",
            "commit",
            "maintenance",
            "remote",
            "rm",
            "submodule",
        ] {
            assert!(!names.contains(&hidden), "{hidden} in {names:?}");
        }
        assert!(
            consumed
                .suggestions
                .iter()
                .all(|suggestion| suggestion.kind != "subcommand"),
            "{names:?}"
        );

        let checkout = complete_fuzzy(&mut registry, "git checkout m", false);
        assert!(
            checkout.current_arg.is_some(),
            "checkout must consume the exact subcommand"
        );
        assert!(!names_of(&checkout).contains(&"merge"));
    }

    #[test]
    fn end_of_options_suppresses_subcommands_and_options() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names":["git"],
              "args":[{"name":"alias","isOptional":true,"suggestions":[{"names":["mine"]}]}],
              "additionalSuggestions":[{"names":["ship"]}],
              "options":[{"names":["--namespace"]}],
              "subcommands":[{"names":["merge"]},{"names":["checkout"]}]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();

        let result = complete_fuzzy(&mut registry, "git -- m", true);
        let names = names_of(&result);
        assert!(names.contains(&"mine"), "{names:?}");
        assert!(!names.contains(&"merge"), "{names:?}");
        assert!(!names.contains(&"checkout"), "{names:?}");
        assert!(!names.contains(&"--namespace"), "{names:?}");
        assert!(!names.contains(&"ship"), "{names:?}");

        let spaced = complete_fuzzy(&mut registry, "git -- ", false);
        let spaced_names = names_of(&spaced);
        assert!(spaced_names.contains(&"mine"), "{spaced_names:?}");
        assert!(spaced_names.contains(&"ship"), "{spaced_names:?}");
        assert!(!spaced_names.contains(&"merge"), "{spaced_names:?}");
        assert!(!spaced_names.contains(&"--namespace"), "{spaced_names:?}");
    }

    #[test]
    fn variadic_option_argument_breaks_for_options_only() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("pack.json"),
            r#"{
              "names":["pack"],
              "subcommands":[{"names":["child"]}],
              "options":[
                {"names":["--files"],"args":[{"name":"file","isVariadic":true,"suggestions":[{"names":["a.txt"]}]}]},
                {"names":["--flag"],"description":"Flag"},
                {"names":["--locked"],"args":[{"name":"file","isVariadic":true,"optionsCanBreakVariadicArg":false}]}
              ]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();

        let open = complete_fuzzy(&mut registry, "pack --files one ", false);
        let open_names = names_of(&open);
        assert!(open_names.contains(&"--flag"), "{open_names:?}");
        assert!(open_names.contains(&"a.txt"), "{open_names:?}");
        assert!(!open_names.contains(&"child"), "{open_names:?}");

        let locked = complete_fuzzy(&mut registry, "pack --locked one ", false);
        let locked_names = names_of(&locked);
        assert!(!locked_names.contains(&"--flag"), "{locked_names:?}");
        assert!(!locked_names.contains(&"child"), "{locked_names:?}");

        let typing = complete_fuzzy(&mut registry, "pack --files o", false);
        let typing_names = names_of(&typing);
        assert!(!typing_names.contains(&"--flag"), "{typing_names:?}");
        assert!(!typing_names.contains(&"child"), "{typing_names:?}");
    }

    #[test]
    fn inline_load_spec_keeps_the_end_of_options_flag() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "args":[{"name":"input","loadSpec":{
                "names":["inner"],
                "subcommands":[{"names":["child"]}],
                "options":[{"names":["--inner"]}],
                "args":[{"name":"next","suggestions":[{"names":["kept"]}]}]
              }}]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let completed = complete_fuzzy(&mut registry, "tool -- value ", false);
        let names = names_of(&completed);
        assert_eq!(
            completed.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("next")
        );
        assert!(names.contains(&"kept"), "{names:?}");
        assert!(!names.contains(&"child"), "{names:?}");
        assert!(!names.contains(&"--inner"), "{names:?}");
    }

    #[test]
    fn current_arg_honors_double_dash_and_shell_quoting() {
        let (_dir, mut registry) = load_context_spec();
        let result = context_result(&mut registry, "git -- fe\\ bar");
        assert_eq!(result.search_term, "fe\\ bar");
        assert_eq!(
            result.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("root path")
        );
        let (tokens, trailing) = tokenize("git 'fe bar' ");
        assert_eq!(tokens, vec!["git", "fe bar"]);
        assert!(trailing);
        let (tokens, trailing) = tokenize("git $'ch");
        assert_eq!(tokens, vec!["git", "ch"]);
        assert!(!trailing);
        let (tokens, trailing) = tokenize("git\t");
        assert_eq!(tokens, vec!["git"]);
        assert!(!trailing);
        let (tokens, trailing) = tokenize(r#"git "c\x"#);
        assert_eq!(tokens, vec!["git", r"c\x"]);
        assert!(!trailing);
        let (tokens, trailing) = tokenize("git $'foo\\'bar'");
        assert_eq!(tokens, vec!["git", r"foo\'bar"]);
        assert!(!trailing);
        let result = context_result(&mut registry, r"git -- foo\");
        assert_eq!(result.search_term, r"foo\");
        assert_eq!(result.match_term, "foo");
    }

    #[test]
    fn parser_inner_text_strips_shell_syntax_for_generators() {
        for (raw, expected) in [
            ("'src/fo", "src/fo"),
            (r#""src/fo"#, "src/fo"),
            ("$'src/fo", "src/fo"),
            (r"my\ dir/fo", "my dir/fo"),
            ("my file", "my file"),
            // ANSI-C quoting follows the existing shell-parser innerText
            // contract: its C escapes are retained verbatim.  Only the
            // quote delimiters are removed here; decoding `\\n`/`\\xNN`
            // would diverge from that parser and from the shell token model.
            ("$'my\\ dir'", r"my\ dir"),
        ] {
            assert_eq!(parser_inner_text(raw), expected, "raw={raw:?}");
        }
        assert_eq!(tokenize(r"git foo\").0, vec!["git", "foo"]);
        assert_eq!(tokenize(r"git \").0, vec!["git", ""]);
        assert_eq!(tokenize(r#"git "foo\"#).0, vec!["git", "foo"]);
        assert_eq!(tokenize(r"git $'foo\").0, vec!["git", r"foo\"]);
        assert_eq!(parser_inner_text(r"foo\"), "foo");
        assert_eq!(parser_inner_text(r#""foo\"#), "foo");
        assert_eq!(parser_inner_text(r"$'foo\"), r"foo\");
        assert_eq!(current_token_raw(r"git foo\"), r"foo\");
        assert_eq!(current_token_raw("git $'foo\\''"), "$'foo\\''");
    }

    #[test]
    fn ansi_c_escaped_quotes_stay_inside_commands_and_first_tokens() {
        assert_eq!(current_command_slice("echo $'x\\'' && git ch"), "git ch");
        assert_eq!(current_command_slice("$'value\\'' git ch"), "$'value\\'' git ch");
        assert_eq!(current_token_raw("echo $'x\\'' git ch"), "ch");
        let raw = "$'value\\'' git ch";
        assert_eq!(raw_first_token_end(raw), "$'value\\''".len());
    }

    #[test]
    fn subcommand_args_hint_comes_from_ir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names": ["git"],
              "subcommands": [
                {
                  "names": ["checkout"],
                  "description": "Switch branches",
                  "args": [{"name": "branch", "isOptional": true}]
                }
              ]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git ".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
                include_public_ai: false,
                defer_slow_generators: false,
                alias: None,
            },
        );
        let checkout = result
            .suggestions
            .iter()
            .find(|s| s.name == "checkout")
            .expect("checkout");
        assert_eq!(checkout.args_hint, "[branch]");
    }

    #[test]
    fn subcommand_with_children_adds_a_trailing_space_by_default() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["remote"],"subcommands":[{"names":["-v"]}]}]}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git ".into(),
                ..CompleteRequest::default()
            },
        );
        let remote = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "remote")
            .expect("remote");
        assert!(remote.should_add_space);
    }

    #[test]
    fn additional_suggestions_are_available_after_a_trailing_space() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names": ["git"],
              "subcommands": [{"names": ["commit"]}],
              "additionalSuggestions": [{"names": ["commit -m 'msg'"], "description": "Git commit shortcut"}]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git ".into(),
                cwd: "/".into(),
                ..CompleteRequest::default()
            },
        );
        let shortcut = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "commit -m 'msg'")
            .expect("additional suggestion");
        assert_eq!(shortcut.kind, "arg");
        assert_eq!(shortcut.description, "Git commit shortcut");
        assert_eq!(shortcut.icon.as_deref(), Some("fig://template?color=628dad&badge=➡️"));
    }

    #[test]
    fn static_suggestion_type_query_term_and_original_type_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo.json"),
            r#"{
              "names": ["demo"],
              "args": [{
                "name": "target",
                "suggestions": [{
                  "names": ["file.txt"],
                  "description": "A file",
                  "type": "file",
                  "originalType": "folder",
                  "getQueryTerm": "/"
                }]
              }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "demo dir/fi".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let suggestion = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "file.txt")
            .expect("static suggestion");
        assert_eq!(suggestion.kind, "file");
        assert_eq!(suggestion.original_type.as_deref(), Some("folder"));
        assert_eq!(suggestion.query_term.as_deref(), Some("fi"));
        // Keep the raw shell token for deletion; the per-row query term tells
        // insertion to delete only the suffix after the slash.
        assert_eq!(result.search_term, "dir/fi");
        assert_eq!(result.match_term, "dir/fi");
    }

    #[test]
    fn argument_query_term_changes_match_term_for_static_rows() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo.json"),
            r#"{
              "names": ["demo"],
              "args": [{
                "name": "target",
                "getQueryTerm": "/",
                "suggestions": [{"names": ["file.txt"], "type": "file"}]
              }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "demo dir/fi".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let suggestion = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "file.txt")
            .expect("static suggestion");
        assert_eq!(suggestion.query_term.as_deref(), Some("fi"));
        assert_eq!(result.search_term, "dir/fi");
        assert_eq!(result.match_term, "dir/fi");
    }

    #[test]
    fn nameless_generator_arg_is_still_the_active_slot() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo.json"),
            r#"{
              "names": ["demo"],
              "args": [{ "generators": [{ "script": ["printf", "alpha\n"], "splitOn": "\n" }] }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "demo ".into(),
                cwd: "/".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "{names:?}");
    }

    #[test]
    fn exact_safe_item_gets_an_auto_execute_row_when_enabled() {
        let mut suggestions = vec![Suggestion::new("status", "Show status", "subcommand")];
        add_exact_auto_execute(&mut suggestions, "status", false, false, false);
        assert_eq!(suggestions[0].kind, "auto-execute");
        assert_eq!(suggestions[0].insert_value.as_deref(), Some("\n"));
        assert_eq!(suggestions[0].original_type.as_deref(), Some("subcommand"));
        assert_eq!(suggestions[1].name, "status");
    }

    #[test]
    fn display_name_alone_does_not_create_an_exact_auto_execute_row() {
        let mut suggestion = Suggestion::new("actual", "", "arg");
        suggestion.display_name = Some("Pretty Label".into());
        let mut suggestions = vec![suggestion];
        add_exact_auto_execute(&mut suggestions, "Pretty Label", false, false, false);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].kind, "arg");
    }

    #[test]
    fn exact_alias_uses_the_source_primary_name_for_auto_execute() {
        // The WebView checks names[0] against insertValue, not the alias that
        // happened to match the query.  This is common for canonical names
        // with a short alias (e.g. `checkout` / `co`).
        let suggestion = Suggestion::new("co", "Checkout", "arg")
            .with_insert_value("checkout")
            .with_primary_name(Some("checkout".into()));
        let mut suggestions = vec![suggestion];
        add_exact_auto_execute(&mut suggestions, "co", false, false, false);
        assert_eq!(suggestions[0].kind, "auto-execute");
        assert_eq!(suggestions[1].name, "co");
    }

    #[test]
    fn verbose_alias_selection_uses_the_longest_matching_name() {
        let names = vec!["-h".into(), "--help".into()];
        assert_eq!(
            select_named_candidate(&names, None, "-", false, false).map(String::as_str),
            Some("-h")
        );
        assert_eq!(
            select_named_candidate(&names, None, "-", false, true).map(String::as_str),
            Some("--help")
        );
    }

    #[test]
    fn hidden_names_are_revealed_only_by_an_exact_alias() {
        let names = vec!["secret".into(), "hidden-alias".into()];
        assert!(!hidden_item_is_visible(true, &names, "sec"));
        assert!(!hidden_item_is_visible(true, &names, "Friendly"));
        assert!(hidden_item_is_visible(true, &names, "SECRET"));
        assert!(hidden_item_is_visible(true, &names, "hidden-alias"));
        assert!(hidden_item_is_visible(false, &names, "sec"));
    }

    #[test]
    fn hidden_static_positional_rows_reappear_only_for_case_insensitive_exact_matches() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo.json"),
            r#"{
              "names": ["demo"],
              "args": [{
                "name": "value",
                "suggestCurrentToken": true,
                "suggestions": [
                  {"names": ["SecretValue"], "description": "Hidden value", "hidden": true},
                  {"names": ["visible"], "description": "Visible value"}
                ]
              }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();

        let root = registry.get_arc("demo").expect("demo spec");
        let (tokens, trailing) = tokenize("demo vis");
        let mut tokens = tokens;
        let context = resolve_context(root, &mut tokens, trailing, "vis", "vis", None);
        assert_eq!(
            context
                .active_arg
                .as_ref()
                .and_then(|active| active.arg.suggest_current_token),
            Some(true)
        );

        let exact = context_result(&mut registry, "demo secretvalue");
        assert!(
            exact
                .suggestions
                .iter()
                .any(|suggestion| { suggestion.name == "SecretValue" && suggestion.description == "Hidden value" })
        );

        let partial = context_result(&mut registry, "demo secret");
        assert!(partial.suggestions.is_empty(), "{:?}", partial.suggestions);

        let empty = context_result(&mut registry, "demo ");
        assert!(
            empty
                .suggestions
                .iter()
                .all(|suggestion| suggestion.name != "SecretValue")
        );
    }

    #[test]
    fn argument_suggest_current_token_override_wins_over_global_setting() {
        let disabled = ArgSpec {
            suggest_current_token: Some(false),
            ..ArgSpec::default()
        };
        let enabled = ArgSpec {
            suggest_current_token: Some(true),
            ..ArgSpec::default()
        };
        let inherited = ArgSpec::default();
        fn active(arg: &ArgSpec) -> ActiveArg {
            ActiveArg {
                arg: arg.clone(),
                slot: None,
                query: "value".into(),
                search_term: "value".into(),
                only_suggest_args: false,
                source: ActiveArgSource::PositionalValue,
            }
        }

        let disabled_active = active(&disabled);
        let enabled_active = active(&enabled);
        let inherited_active = active(&inherited);
        assert!(!suggest_current_token_for(Some(&disabled_active), true));
        assert!(suggest_current_token_for(Some(&enabled_active), false));
        assert!(suggest_current_token_for(Some(&inherited_active), true));
        assert!(!suggest_current_token_for(Some(&inherited_active), false));
        assert!(suggest_current_token_for(None, true));
    }

    #[test]
    fn exact_item_with_required_argument_does_not_auto_execute() {
        let mut required = Suggestion::new("commit", "Commit", "subcommand");
        required.requires_arg = true;
        // Mandatory unnamed args have no visible hint, but must still prevent
        // Enter from executing an incomplete command.
        assert!(required.args_hint.is_empty());
        let mut suggestions = vec![required];
        add_exact_auto_execute(&mut suggestions, "commit", false, false, false);
        assert_eq!(suggestions.len(), 1);
    }

    #[test]
    fn dangerous_exact_items_need_the_explicit_run_setting() {
        let mut suggestions = vec![Suggestion::new("clean", "Remove files", "subcommand").with_dangerous(true)];
        add_exact_auto_execute(&mut suggestions, "clean", false, false, false);
        assert_eq!(suggestions.len(), 1);
        add_exact_auto_execute(&mut suggestions, "clean", false, false, true);
        assert_eq!(suggestions[0].kind, "auto-execute");
    }

    #[test]
    fn partial_dangerous_match_still_allows_current_token_fallback() {
        let mut suggestions = vec![Suggestion::new("clean", "Remove files", "subcommand").with_dangerous(true)];
        add_current_token_auto_execute(&mut suggestions, "cl", true, false, false, false, false);
        assert_eq!(suggestions[0].kind, "auto-execute");
        assert_eq!(suggestions[0].name, "cl");
    }

    #[test]
    fn exact_dangerous_match_blocks_current_token_fallback() {
        let mut suggestions = vec![Suggestion::new("clean", "Remove files", "subcommand").with_dangerous(true)];
        add_current_token_auto_execute(&mut suggestions, "clean", true, false, false, false, false);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].kind, "subcommand");
    }

    #[test]
    fn directory_auto_execute_special_cases_do_not_require_current_token_setting() {
        let mut dot = vec![Suggestion::new(".gitignore", "File", "file")];
        add_current_token_auto_execute(&mut dot, ".", false, false, false, false, false);
        assert_eq!(dot[0].name, ".");
        assert_eq!(dot[0].description, "Enter the current directory");
        assert_eq!(dot[0].original_type.as_deref(), Some("folder"));

        let mut folder = vec![Suggestion::new("src/", "Folder", "folder")];
        add_current_token_auto_execute(&mut folder, "src/", false, false, false, false, false);
        assert_eq!(folder[0].name, "↪");
        assert_eq!(folder[0].description, "Enter the current directory");
    }

    #[test]
    fn auto_execute_rows_still_obey_visibility_configuration() {
        let mut exact_hidden = vec![Suggestion::new("status", "Show status", "subcommand")];
        add_exact_auto_execute(&mut exact_hidden, "status", true, false, false);
        assert!(exact_hidden.iter().all(|item| item.kind != "auto-execute"));

        let mut dot_hidden = vec![Suggestion::new(".gitignore", "File", "file")];
        add_current_token_auto_execute(&mut dot_hidden, ".", false, true, false, false, false);
        assert!(dot_hidden.iter().all(|item| item.kind != "auto-execute"));

        let mut exact_tab_only = vec![Suggestion::new("status", "Show status", "subcommand")];
        add_exact_auto_execute(&mut exact_tab_only, "status", false, true, false);
        assert!(exact_tab_only.iter().all(|item| item.kind != "auto-execute"));
    }

    #[test]
    fn exact_folder_match_uses_the_directory_action_name() {
        let mut suggestions = vec![Suggestion::new("src/", "Folder", "folder")];
        add_exact_auto_execute(&mut suggestions, "src", false, false, false);
        assert_eq!(suggestions[0].name, "src");
        assert_eq!(suggestions[0].description, "folder");
        assert_eq!(suggestions[0].original_type.as_deref(), Some("folder"));
    }

    #[test]
    fn trailing_space_can_add_immediate_execute_row() {
        let mut suggestions = vec![Suggestion::new("status", "Show status", "subcommand")];
        add_space_auto_execute(&mut suggestions, "", true, false, false);
        assert_eq!(suggestions[0].name, "↪");
        assert_eq!(suggestions[0].insert_value.as_deref(), Some("\n"));
    }

    #[test]
    fn always_suggest_current_token_adds_an_action_row() {
        let mut suggestions = vec![Suggestion::new("checkout", "", "subcommand")];
        add_current_token_auto_execute(&mut suggestions, "ch", true, false, false, true, false);
        assert_eq!(suggestions[0].name, "ch");
        assert_eq!(suggestions[0].description, "Enter the current argument");
        assert_eq!(suggestions[0].insert_value.as_deref(), Some("\n"));
    }

    fn load_is_command_registry() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("wrapper.json"),
            r#"{
              "names": ["wrapper"],
              "args": [{ "name": "command", "isCommand": true }],
              "options": [{
                "names": ["-u", "--user"],
                "args": [{ "name": "user" }]
              }]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("target.json"),
            r#"{
              "names": ["target"],
              "subcommands": [
                { "names": ["build"], "description": "Build the project" },
                { "names": ["test"], "description": "Run tests" }
              ]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("eslint.json"),
            r#"{
              "names": ["eslint"],
              "options": [{ "names": ["--fix"], "description": "Fix problems" }]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("modhost.json"),
            r#"{
              "names": ["modhost"],
              "args": [{ "name": "module", "isModule": "lang/" }]
            }"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("lang")).unwrap();
        fs::write(
            dir.path().join("lang/http.json"),
            r#"{
              "names": ["lang/http"],
              "subcommands": [{ "names": ["get"], "description": "GET" }]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("dockerish.json"),
            r#"{
              "names": ["dockerish"],
              "subcommands": [{
                "names": ["exec"],
                "args": [
                  { "name": "container" },
                  { "name": "command", "isCommand": true }
                ]
              }]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    #[test]
    fn is_command_token_does_not_list_bundled_commands() {
        let (_dir, mut registry) = load_is_command_registry();
        let result = context_result(&mut registry, "wrapper ta");
        assert!(
            result.suggestions.iter().all(|item| item.name != "target"),
            "{:?}",
            result.suggestions
        );
        let empty = context_result(&mut registry, "wrapper ");
        assert!(
            empty.suggestions.iter().all(|item| item.name != "target"),
            "{:?}",
            empty.suggestions
        );
        assert_eq!(empty.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("command"));
    }

    #[test]
    fn is_command_switches_to_the_named_spec() {
        let (_dir, mut registry) = load_is_command_registry();
        let result = context_result(&mut registry, "wrapper target b");
        assert!(
            result.suggestions.iter().any(|item| item.name == "build"),
            "{:?}",
            result.suggestions
        );
        let after_space = context_result(&mut registry, "wrapper target ");
        assert!(
            after_space.suggestions.iter().any(|item| item.name == "test"),
            "{:?}",
            after_space.suggestions
        );
    }

    #[test]
    fn is_command_follows_an_option_value() {
        let (_dir, mut registry) = load_is_command_registry();
        let result = context_result(&mut registry, "wrapper -u root target te");
        assert!(
            result.suggestions.iter().any(|item| item.name == "test"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn nested_is_command_keeps_switching() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("sudoish.json"),
            r#"{
              "names": ["sudoish"],
              "args": [{ "name": "command", "isCommand": true }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = context_result(&mut registry, "sudoish sudoish sudoish ");
        assert_eq!(
            result.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("command"),
            "{result:?}"
        );
        assert!(
            result.suggestions.iter().all(|item| item.name != "sudoish"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn is_command_on_a_later_positional_arg() {
        let (_dir, mut registry) = load_is_command_registry();
        let result = context_result(&mut registry, "dockerish exec box eslint --");
        assert!(
            result.suggestions.iter().any(|item| item.name == "--fix"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn is_module_loads_the_prefixed_spec() {
        let (_dir, mut registry) = load_is_command_registry();
        let typing = context_result(&mut registry, "modhost ht");
        assert!(
            typing.suggestions.iter().all(|item| item.name != "http"),
            "{:?}",
            typing.suggestions
        );
        assert_eq!(typing.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("module"));
        let switched = context_result(&mut registry, "modhost http g");
        assert!(
            switched.suggestions.iter().any(|item| item.name == "get"),
            "{:?}",
            switched.suggestions
        );
    }

    #[test]
    fn load_spec_wins_over_is_command() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("host.json"),
            r#"{
              "names": ["host"],
              "args": [{
                "name": "command",
                "isCommand": true,
                "loadSpec": {
                  "names": ["forced"],
                  "subcommands": [{ "names": ["only"], "description": "From loadSpec" }]
                }
              }]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("other.json"),
            r#"{
              "names": ["other"],
              "subcommands": [{ "names": ["nope"], "description": "Should not load" }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = context_result(&mut registry, "host other ");
        assert!(
            result.suggestions.iter().any(|item| item.name == "only"),
            "{:?}",
            result.suggestions
        );
        assert!(result.suggestions.iter().all(|item| item.name != "nope"));
    }

    #[test]
    fn command_lookup_name_uses_basename_for_paths() {
        assert_eq!(command_lookup_name("git", false), "git");
        assert_eq!(command_lookup_name("foo/bar", false), "foo/bar");
        assert_eq!(command_lookup_name("./bin/git", false), "git");
        assert_eq!(command_lookup_name("/usr/bin/git", false), "git");
        assert_eq!(command_lookup_name("~/bin/git", true), "git");
    }

    #[test]
    fn parser_directives_alias_rewrites_the_token_and_walks_the_subcommand() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names": ["git"],
              "subcommands": [
                {"names": ["status"], "description": "Show status", "options": [{"names": ["-s"], "description": "Short"}]}
              ],
              "args": [{
                "name": "alias",
                "isOptional": true,
                "parserDirectives": { "alias": "status" }
              }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git st -".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.contains(&"-s"), "{names:?}");
    }

    #[test]
    fn multi_token_parser_directives_alias_moves_the_typed_token_boundary() {
        // `co = checkout -b`: after expansion the token being typed is the
        // argument of `-b`, not `checkout`'s first positional. Fig re-slices
        // `tokens` from the substituted command and keeps walking; the Rust
        // walker grows `limit` in place, and `active_arg` has to use that
        // grown boundary rather than the one computed from the raw buffer.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names": ["git"],
              "subcommands": [{
                "names": ["checkout"],
                "options": [{"names": ["-b"], "args": [{"name": "new-branch", "suggestions": [{"names": ["fresh"]}]}]}],
                "args": [{"name": "branch", "suggestions": [{"names": ["existing"]}]}]
              }],
              "args": [{
                "name": "alias",
                "isOptional": true,
                "parserDirectives": { "alias": "checkout -b" }
              }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git co ".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.contains(&"fresh"), "{names:?}");
        assert!(!names.contains(&"existing"), "{names:?}");

        let typed = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git co fr".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let names: Vec<_> = typed.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec!["fresh"], "{names:?}");
    }

    #[test]
    fn slash_command_after_space_walks_the_dotslash_spec() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("dotslash.json"),
            r#"{
              "names": ["dotslash"],
              "options": [{"names": ["--bar"], "description": "A flag"}]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "./script --".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.contains(&"--bar"), "{names:?}");
    }

    #[test]
    fn slash_first_token_runs_dotslash_filepaths() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("dotslash.json"),
            r#"{"names":["dotslash"],"args":[{"name":"path","templates":["filepaths","folders"]}]}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "./s".into(),
                cwd: dir.path().display().to_string(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.iter().any(|name| name.contains("src")), "{names:?}");
    }

    #[test]
    fn quoted_path_completion_uses_inner_text_but_keeps_raw_insertion_term() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("main.rs"), "fn").unwrap();
        fs::create_dir(dir.path().join("src").join("more")).unwrap();
        fs::create_dir(dir.path().join("my dir")).unwrap();
        fs::write(
            dir.path().join("cat.json"),
            r#"{"names":["cat"],"args":[{"name":"path","templates":["filepaths"],"getQueryTerm":"/"}]}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        for (buffer, raw_search_term) in [
            ("cat 'src/m", "'src/m"),
            (r#"cat "src/m"#, r#""src/m"#),
            ("cat $'src/m", "$'src/m"),
        ] {
            let result = complete(
                &mut registry,
                &CompleteRequest {
                    buffer: buffer.into(),
                    cwd: dir.path().display().to_string(),
                    include_history: false,
                    ..CompleteRequest::default()
                },
            );
            let row = result
                .suggestions
                .iter()
                .find(|suggestion| suggestion.name == "src/main.rs")
                .unwrap_or_else(|| panic!("buffer={buffer:?}, suggestions={:?}", result.suggestions));
            assert_eq!(result.search_term, raw_search_term, "buffer={buffer:?}");
            assert_eq!(result.match_term, "src/m", "buffer={buffer:?}");
            assert_eq!(row.query_term, None, "buffer={buffer:?}");
        }
        for (buffer, raw_search_term) in [
            ("cat 'src/mo", "'src/mo"),
            (r#"cat "src/mo"#, r#""src/mo"#),
            ("cat $'src/mo", "$'src/mo"),
        ] {
            let result = complete(
                &mut registry,
                &CompleteRequest {
                    buffer: buffer.into(),
                    cwd: dir.path().display().to_string(),
                    include_history: false,
                    ..CompleteRequest::default()
                },
            );
            let row = result
                .suggestions
                .iter()
                .find(|suggestion| suggestion.name == "src/more/")
                .unwrap_or_else(|| panic!("buffer={buffer:?}, suggestions={:?}", result.suggestions));
            assert_eq!(result.search_term, raw_search_term, "buffer={buffer:?}");
            assert_eq!(result.match_term, "src/mo", "buffer={buffer:?}");
            assert_eq!(row.query_term, None, "buffer={buffer:?}");
        }
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "cat my\\ d".into(),
                cwd: dir.path().display().to_string(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let row = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "my dir/")
            .unwrap_or_else(|| panic!("escaped path suggestions={:?}", result.suggestions));
        assert_eq!(result.search_term, r"my\ d");
        assert_eq!(result.match_term, "my d");
        assert_eq!(row.query_term.as_deref(), Some("my d"));
    }

    #[test]
    fn slash_first_token_ignores_a_bundled_spec_with_the_same_basename() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("gitk"), "").unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["checkout"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("dotslash.json"),
            r#"{"names":["dotslash"],"args":[{"name":"path","templates":["filepaths","folders"]}]}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        // Still typing `./git`: Fig's single-token branch is `dotslash`, so
        // the row is the `./gitk` file and not git's subcommands filtered to
        // an empty list.
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "./git".into(),
                cwd: dir.path().display().to_string(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.iter().any(|name| name.ends_with("gitk")), "{names:?}");
        assert!(!names.contains(&"checkout"), "{names:?}");
    }

    #[test]
    fn question_mark_after_space_loads_project_shortcuts() {
        let dir = tempfile::tempdir().unwrap();
        let fig = dir.path().join(".fig/autocomplete/build");
        fs::create_dir_all(&fig).unwrap();
        fs::write(
            fig.join("_shortcuts.json"),
            r#"{"names":["_shortcuts"],"subcommands":[{"names":["deploy"],"description":"Ship it"}]}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "? ".into(),
                cwd: dir.path().display().to_string(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.contains(&"deploy"), "{names:?}");
    }

    #[test]
    fn absolute_path_after_space_uses_the_bundled_basename_spec() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names": ["git"],
              "subcommands": [{"names": ["checkout"], "description": "Switch branches"}]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("dotslash.json"),
            r#"{
              "names": ["dotslash"],
              "subcommands": [{"names": ["not-git"], "description": "Wrong spec"}]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "/usr/bin/git ch".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.contains(&"checkout"), "{names:?}");
        assert!(!names.contains(&"not-git"), "{names:?}");
    }

    #[test]
    fn help_template_lists_the_parent_subcommands_not_the_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("docker.json"),
            r#"{
              "names": ["docker"],
              "subcommands": [
                {"names": ["run"], "description": "Run a container"},
                {
                  "names": ["compose"],
                  "subcommands": [
                    {"names": ["up"], "description": "Create and start"},
                    {"names": ["help"], "args": [{"name": "command", "templates": ["help"]}]}
                  ]
                }
              ]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "docker compose help ".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.contains(&"up"), "{names:?}");
        assert!(!names.contains(&"run"), "{names:?}");
        assert!(!names.contains(&"compose"), "{names:?}");
    }

    #[test]
    fn parser_directives_alias_can_rewrite_the_same_text_twice() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names": ["git"],
              "subcommands": [
                {"names": ["status"], "description": "Show status", "options": [{"names": ["-s"], "description": "Short"}]}
              ],
              "args": [{
                "name": "alias",
                "isOptional": true,
                "isVariadic": true,
                "parserDirectives": { "alias": "status" }
              }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git st st -".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        assert!(names.contains(&"-s"), "{names:?}");
    }

    #[test]
    fn fish_first_token_lines_keep_the_command_name_before_the_tab() {
        assert_eq!(
            dedupe_command_lines("git\tGit version control\ngit\nls\tlist files\n"),
            vec!["git", "ls"]
        );
    }

    #[test]
    fn named_rows_carry_every_alias_spelling() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names": ["git"],
              "subcommands": [
                {"names": ["checkout", "co", "switch"], "description": "Switch branches"}
              ]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git c".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let checkout = result
            .suggestions
            .iter()
            .find(|item| item.alias_names.iter().any(|name| name == "checkout"))
            .expect("checkout");
        assert!(checkout.alias_names.contains(&"co".to_string()));
        assert!(checkout.alias_names.contains(&"switch".to_string()));
    }
}
