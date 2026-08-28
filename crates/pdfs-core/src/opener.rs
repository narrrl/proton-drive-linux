//! How a file chosen in the prompt (or the browser) is actually opened.
//!
//! The default is `xdg-open`, which routes through the desktop's MIME
//! associations. That is the right answer for a GNOME session and the wrong one
//! for a tiling-WM setup where "open a text file" means "run `nvim` inside my
//! terminal" — there is no desktop file for that, and pointing `xdg-open` at a
//! terminal editor globally breaks every graphical caller.
//!
//! So the rules live in `config.json` under `open_with`: an ordered list of
//! name/class patterns mapped to argv, each optionally wrapped in a terminal.
//! First match wins; anything unmatched falls through to `default` (itself
//! `xdg-open` unless overridden). Nothing here is required — an absent
//! `open_with` reproduces the previous behaviour exactly.
//!
//! ```json
//! "open_with": {
//!   "terminal": ["alacritty", "-e"],
//!   "rules": [
//!     { "match": ["@text", "*.rs", "*.toml"], "command": ["nvim"], "terminal": true },
//!     { "match": ["*.png"], "command": ["imv"] }
//!   ]
//! }
//! ```

use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// Environment override for the terminal used by `"terminal": true` rules.
/// Checked before `$TERMINAL` so a support case can redirect it without editing
/// config.
pub const TERMINAL_ENV: &str = "PDFS_TERMINAL";

/// Terminals we know how to hand a command line to, most-preferred first, with
/// the argument that means "run this, don't parse it as options". `kitty` and
/// `wezterm start --` take the command positionally.
const KNOWN_TERMINALS: [(&str, &[&str]); 11] = [
    ("alacritty", &["-e"]),
    ("foot", &["-e"]),
    ("kitty", &[]),
    ("ghostty", &["-e"]),
    ("wezterm", &["start", "--"]),
    ("gnome-terminal", &["--"]),
    ("konsole", &["-e"]),
    ("xfce4-terminal", &["-x"]),
    ("urxvt", &["-e"]),
    ("st", &["-e"]),
    ("xterm", &["-e"]),
];

/// The opener policy as it appears in `config.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenWith {
    /// Terminal argv used by rules with `"terminal": true`, e.g.
    /// `["alacritty", "-e"]`. Unset means "resolve one" — see
    /// [`resolve_terminal`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<Vec<String>>,
    /// What to run when no rule matches. Unset means `xdg-open`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Vec<String>>,
    /// Ordered; first match wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<OpenRule>,
}

/// One `pattern set -> command` mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRule {
    /// Patterns this rule answers to. Either a shell-style glob against the file
    /// name (`*.md`, `notes-*.txt`) or one of the classes `@dir`, `@text`,
    /// `@document`, `@image`, `@media`, `@any`. Matching is case-insensitive.
    #[serde(rename = "match", default)]
    pub patterns: Vec<String>,
    /// argv to run. A `{}` token is replaced by the path; without one the path
    /// is appended. `$VAR`/`${VAR}` tokens expand from the environment, so
    /// `["$EDITOR"]` follows the user's editor.
    #[serde(default)]
    pub command: Vec<String>,
    /// Wrap [`command`](Self::command) in the terminal emulator.
    #[serde(default)]
    pub terminal: bool,
}

impl OpenWith {
    /// The argv that opens `path`, terminal wrapping included. Never fails: an
    /// unmatched path — or a rule whose terminal cannot be resolved — degrades
    /// to the default opener rather than doing nothing.
    pub fn command_for(&self, path: &Path, is_dir: bool) -> Vec<String> {
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_string();
        self.command_for_named(path, &name, is_dir)
    }

    /// [`command_for`](Self::command_for) with the rules matched against `name`
    /// instead of the last component of `path`.
    ///
    /// A materialised Drive file lives in the content cache under its content
    /// hash — no extension, no original name — so matching the on-disk path
    /// classifies every download as "unknown" and hands it to `xdg-open`, which
    /// sniffs the bytes and picks whatever claims `text/plain` (LibreOffice, for
    /// a Markdown file). The rules are about *what the user opened*, so the
    /// caller passes that name and the argv still gets the real path.
    pub fn command_for_named(&self, path: &Path, name: &str, is_dir: bool) -> Vec<String> {
        for rule in &self.rules {
            if rule.command.is_empty() {
                continue;
            }
            if !rule.patterns.iter().any(|p| matches(p, name, is_dir)) {
                continue;
            }
            let argv = expand(&rule.command, path);
            if !rule.terminal {
                return argv;
            }
            match self.terminal_argv() {
                Some(mut terminal) => {
                    terminal.extend(argv);
                    return terminal;
                }
                // A terminal rule with no terminal to run it in would silently
                // launch a TUI with no tty. Fall through to the desktop instead.
                None => {
                    tracing::warn!(
                        "no terminal found for a terminal open rule; using the default opener"
                    );
                    break;
                }
            }
        }

        let fallback = self
            .default
            .clone()
            .unwrap_or_else(|| vec!["xdg-open".to_string()]);
        expand(&fallback, path)
    }

    /// The configured terminal, or a resolved one.
    fn terminal_argv(&self) -> Option<Vec<String>> {
        match &self.terminal {
            Some(argv) if !argv.is_empty() => Some(argv.clone()),
            _ => resolve_terminal(),
        }
    }
}

/// Launch `path` under `policy`. Fire-and-forget: the child outlives us, and a
/// spawn failure is logged rather than propagated — every caller is a UI action
/// with nowhere useful to report to.
pub fn open(policy: &OpenWith, path: &Path, is_dir: bool) {
    let argv = policy.command_for(path, is_dir);
    spawn(&argv, path);
}

/// [`open`] for a path whose own file name says nothing about its type — see
/// [`OpenWith::command_for_named`].
pub fn open_named(policy: &OpenWith, path: &Path, name: &str, is_dir: bool) {
    let argv = policy.command_for_named(path, name, is_dir);
    spawn(&argv, path);
}

/// Launch `argv`, detached from whatever is opening it.
///
/// The child gets its own process group and no inherited stdio, because the
/// caller may be about to exit and take its terminal with it: `pdfs-prompt
/// --fzf` runs inside a terminal it spawned for itself, and that terminal
/// closes the moment the selection is made. A child left in the terminal's
/// foreground process group is sent `SIGHUP` when the pty hangs up, so
/// "open this folder in a terminal" would open a window that dies immediately
/// — which is exactly what it did. Nothing here needs the caller's stdio in the
/// first place: every `argv` is another application, and a terminal one is
/// wrapped in its own emulator by the `terminal` rule flag.
fn spawn(argv: &[String], path: &Path) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    tracing::info!(path = %path.display(), command = ?argv, "opening");
    let spawned = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    if let Err(e) = spawned {
        tracing::error!(command = ?argv, "open failed: {e}");
    }
}

/// [`open`] with the policy from the user's `config.json`, read once per
/// process. For callers that open the occasional path and have no reason to
/// carry a policy around; long-lived windows should hold their own copy.
pub fn open_default(path: &Path, is_dir: bool) {
    open(default_policy(), path, is_dir);
}

/// [`open_default`] matching the rules against `name` — see
/// [`OpenWith::command_for_named`].
pub fn open_default_named(path: &Path, name: &str, is_dir: bool) {
    open_named(default_policy(), path, name, is_dir);
}

fn default_policy() -> &'static OpenWith {
    static POLICY: std::sync::OnceLock<OpenWith> = std::sync::OnceLock::new();
    POLICY.get_or_init(|| {
        crate::config::AppDirs::new()
            .map(|dirs| dirs.load_config().resolved_open_with())
            .unwrap_or_default()
    })
}

/// `$PDFS_TERMINAL`, then `$TERMINAL`, then the first [`KNOWN_TERMINALS`] entry
/// on `PATH`. An env value may be a full command line (`"foot -e"`), in which
/// case it is used verbatim; a bare program name gains that terminal's "run
/// this" argument so `TERMINAL=alacritty` does the obvious thing.
pub fn resolve_terminal() -> Option<Vec<String>> {
    for key in [TERMINAL_ENV, "TERMINAL"] {
        let Ok(value) = std::env::var(key) else {
            continue;
        };
        let argv: Vec<String> = value.split_whitespace().map(str::to_string).collect();
        match argv.len() {
            0 => continue,
            1 => return Some(with_exec_flag(&argv[0])),
            _ => return Some(argv),
        }
    }

    KNOWN_TERMINALS
        .iter()
        .find(|(program, _)| on_path(program))
        .map(|(program, _)| with_exec_flag(program))
}

/// A bare terminal name plus whatever it needs before a command.
fn with_exec_flag(program: &str) -> Vec<String> {
    let base = Path::new(program)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(program);
    let mut argv = vec![program.to_string()];
    let flags = KNOWN_TERMINALS
        .iter()
        .find(|(known, _)| *known == base)
        .map_or(&["-e"][..], |(_, flags)| *flags);
    argv.extend(flags.iter().map(|f| (*f).to_string()));
    argv
}

fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

/// Substitute `{}` (or append) the path, and expand `$VAR` tokens. Tokens that
/// expand to nothing are dropped, so `["$EDITOR"]` with no `EDITOR` set leaves
/// an empty argv and falls back at the call site.
fn expand(argv: &[String], path: &Path) -> Vec<String> {
    let target = path.display().to_string();
    let mut out = Vec::with_capacity(argv.len() + 1);
    let mut substituted = false;
    for token in argv {
        if token.contains("{}") {
            out.push(token.replace("{}", &target));
            substituted = true;
            continue;
        }
        match expand_env(token) {
            Some(value) => out.push(value),
            None => continue,
        }
    }
    if out.is_empty() {
        return vec!["xdg-open".to_string(), target];
    }
    if !substituted {
        out.push(target);
    }
    out
}

/// `$VAR` / `${VAR}` for a whole token, `None` when the variable is unset or
/// empty so the token drops out. A token that is not a variable reference is
/// returned unchanged; this is deliberately not a shell — no word splitting, no
/// quoting rules.
fn expand_env(token: &str) -> Option<String> {
    let Some(name) = token
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .or_else(|| token.strip_prefix('$'))
    else {
        return Some(token.to_string());
    };
    if name.is_empty() {
        return Some(token.to_string());
    }
    // Follow `$EDITOR`'s convention of preferring the graphical-capable one.
    let value = if name == "EDITOR" {
        std::env::var("VISUAL")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var("EDITOR").ok())
    } else {
        std::env::var(name).ok()
    };
    value.filter(|v| !v.is_empty())
}

/// Whether `pattern` — a glob or an `@class` — matches this name.
fn matches(pattern: &str, name: &str, is_dir: bool) -> bool {
    let name_lower = name.to_lowercase();
    match pattern.to_lowercase().as_str() {
        "@any" => true,
        "@dir" => is_dir,
        "@text" => !is_dir && class_text(&name_lower),
        "@document" => !is_dir && class_document(&name_lower),
        "@image" => !is_dir && class_image(&name_lower),
        "@media" => !is_dir && class_media(&name_lower),
        other if other.starts_with('@') => false,
        other => glob_match(other, &name_lower),
    }
}

fn extension(name: &str) -> &str {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
}

/// Plain-text-ish files a terminal editor is a sensible default for. Extensions
/// only: the daemon's hits may name files that are not on this machine yet, so
/// there is nothing to sniff.
fn class_text(name: &str) -> bool {
    matches!(
        extension(name),
        "txt"
            | "md"
            | "markdown"
            | "rst"
            | "org"
            | "log"
            | "conf"
            | "cfg"
            | "ini"
            | "toml"
            | "yaml"
            | "yml"
            | "json"
            | "xml"
            | "csv"
            | "tsv"
            | "env"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "rs"
            | "py"
            | "go"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "js"
            | "ts"
            | "tsx"
            | "jsx"
            | "css"
            | "html"
            | "sql"
            | "lua"
            | "vim"
            | "nix"
            | "patch"
            | "diff"
    ) || name == "makefile"
        || name == "dockerfile"
}

fn class_document(name: &str) -> bool {
    matches!(
        extension(name),
        "pdf"
            | "doc"
            | "docx"
            | "odt"
            | "rtf"
            | "xls"
            | "xlsx"
            | "ods"
            | "ppt"
            | "pptx"
            | "odp"
            | "epub"
    ) || class_text(name)
}

fn class_image(name: &str) -> bool {
    matches!(
        extension(name),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "svg" | "avif" | "heic" | "tiff"
    )
}

fn class_media(name: &str) -> bool {
    matches!(
        extension(name),
        "mp4"
            | "mkv"
            | "webm"
            | "mov"
            | "avi"
            | "mp3"
            | "flac"
            | "wav"
            | "ogg"
            | "opus"
            | "m4a"
            | "aac"
    )
}

/// `*` and `?` globbing, no character classes and no path semantics — patterns
/// are matched against a bare file name. Iterative with backtracking so a
/// pathological pattern cannot blow the stack.
fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0usize, 0usize);
    // Where to resume if the current `*` turns out to have matched too little.
    let (mut star, mut resume) = (None, 0usize);

    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some(p);
                resume = n;
                p += 1;
            }
            Some('?') => {
                p += 1;
                n += 1;
            }
            Some(c) if *c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some(s) => {
                    p = s + 1;
                    resume += 1;
                    n = resume;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(patterns: &[&str], command: &[&str], terminal: bool) -> OpenRule {
        OpenRule {
            patterns: patterns.iter().map(|p| (*p).to_string()).collect(),
            command: command.iter().map(|c| (*c).to_string()).collect(),
            terminal,
        }
    }

    #[test]
    fn a_cache_blob_is_classified_by_the_name_it_was_opened_as() {
        let policy = OpenWith {
            terminal: Some(vec!["alacritty".into(), "-e".into()]),
            default: None,
            rules: vec![rule(&["@text"], &["nvim"], true)],
        };
        // What the daemon hands back: content-hash name, no extension. Matched
        // on its own it is "unknown" and would land in xdg-open's lap.
        let blob = Path::new("/home/me/.cache/proton-drive-linux/content/364d1d8cc8e6");
        assert_eq!(
            policy.command_for(blob, false),
            vec![
                "xdg-open",
                "/home/me/.cache/proton-drive-linux/content/364d1d8cc8e6"
            ]
        );
        assert_eq!(
            policy.command_for_named(blob, "ONBOARDING.md", false),
            vec![
                "alacritty",
                "-e",
                "nvim",
                "/home/me/.cache/proton-drive-linux/content/364d1d8cc8e6"
            ]
        );
    }

    #[test]
    fn an_empty_policy_is_xdg_open() {
        let policy = OpenWith::default();
        assert_eq!(
            policy.command_for(Path::new("/tmp/a.md"), false),
            vec!["xdg-open".to_string(), "/tmp/a.md".to_string()]
        );
    }

    #[test]
    fn a_terminal_rule_wraps_the_command() {
        let policy = OpenWith {
            terminal: Some(vec!["alacritty".into(), "-e".into()]),
            default: None,
            rules: vec![rule(&["@text"], &["nvim"], true)],
        };
        assert_eq!(
            policy.command_for(Path::new("/tmp/notes.md"), false),
            vec!["alacritty", "-e", "nvim", "/tmp/notes.md"]
        );
    }

    #[test]
    fn the_first_matching_rule_wins_and_globs_are_case_insensitive() {
        let policy = OpenWith {
            terminal: None,
            default: None,
            rules: vec![
                rule(&["*.PNG"], &["imv"], false),
                rule(&["@image"], &["eog"], false),
            ],
        };
        assert_eq!(
            policy.command_for(Path::new("/tmp/shot.png"), false),
            vec!["imv", "/tmp/shot.png"]
        );
    }

    #[test]
    fn a_placeholder_positions_the_path_instead_of_appending_it() {
        let policy = OpenWith {
            terminal: None,
            default: None,
            rules: vec![rule(&["@any"], &["sh", "-c", "cat {} | less"], false)],
        };
        assert_eq!(
            policy.command_for(Path::new("/tmp/a.log"), false),
            vec!["sh", "-c", "cat /tmp/a.log | less"]
        );
    }

    #[test]
    fn directories_only_match_the_dir_class() {
        let policy = OpenWith {
            terminal: None,
            default: None,
            rules: vec![
                rule(&["@text"], &["nvim"], false),
                rule(&["@dir"], &["nautilus"], false),
            ],
        };
        assert_eq!(
            policy.command_for(Path::new("/tmp/notes.md"), true),
            vec!["nautilus", "/tmp/notes.md"]
        );
    }

    #[test]
    fn a_rule_that_cannot_find_a_terminal_falls_back_to_the_default() {
        // A terminal that is spelled out in config is always honoured; this is
        // the other path — resolution must not invent one from the test host.
        let policy = OpenWith {
            terminal: Some(Vec::new()),
            default: Some(vec!["gio".into(), "open".into()]),
            rules: vec![rule(&["@text"], &["nvim"], true)],
        };
        let argv = policy.command_for(Path::new("/tmp/a.md"), false);
        // Either a terminal exists on this machine (wrapped nvim) or it does
        // not (the default). Both are correct; what must not happen is a bare
        // `nvim` with no tty.
        assert!(
            argv == vec!["gio", "open", "/tmp/a.md"]
                || argv.ends_with(&["nvim".to_string(), "/tmp/a.md".to_string()])
        );
    }

    #[test]
    fn globs_backtrack() {
        assert!(glob_match("*.md", "notes.md"));
        assert!(glob_match("*notes*.md", "my-notes-2024.md"));
        assert!(glob_match("a?c.txt", "abc.txt"));
        assert!(!glob_match("*.md", "notes.mdx"));
        assert!(!glob_match("a?c.txt", "ac.txt"));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn a_bare_terminal_name_gains_its_exec_flag() {
        assert_eq!(with_exec_flag("alacritty"), vec!["alacritty", "-e"]);
        assert_eq!(with_exec_flag("kitty"), vec!["kitty"]);
        assert_eq!(
            with_exec_flag("/usr/bin/wezterm"),
            vec!["/usr/bin/wezterm", "start", "--"]
        );
        // Unknown terminals get the near-universal `-e`.
        assert_eq!(with_exec_flag("mystery-term"), vec!["mystery-term", "-e"]);
    }
}
