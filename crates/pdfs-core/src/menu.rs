//! Driving an external dmenu-style launcher (fuzzel, rofi, wofi, …).
//!
//! `pdfs-prompt` ships its own GTK HUD, but on a tiling-WM desktop the user
//! already has a launcher bound to a key, themed to match everything else. This
//! module lets the prompt borrow it: entries go out on the child's stdin, the
//! chosen line comes back on stdout.
//!
//! Two things vary between launchers and are the only reasons this is not four
//! lines of `Command`:
//!
//! * the flag that turns on filter mode and the one that sets the prompt text;
//! * icon support, which fuzzel and rofi both spell `text\0icon\x1fNAME` and
//!   everything else would render as literal garbage.
//!
//! Selection is matched back by label rather than by index, because the one
//! feature no launcher agrees on is index output. A line that matches nothing is
//! the user's own typing — [`MenuChoice::Custom`] — which is what makes
//! type-then-search work at all.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Separates the label from its icon name in the fuzzel/rofi protocol.
const ICON_SEPARATOR: &str = "\0icon\x1f";

/// Launchers we know how to invoke, most-preferred first:
/// `(program, filter-mode args, prompt flag, icon protocol)`.
///
/// The prompt flag is written as it is passed: fuzzel wants `--prompt=TEXT` as
/// one argument, the rest take the text as a separate one.
const KNOWN_MENUS: [MenuFlavor; 6] = [
    MenuFlavor {
        program: "fuzzel",
        mode: &["--dmenu"],
        prompt: PromptFlag::Joined("--prompt="),
        icons: true,
    },
    MenuFlavor {
        program: "rofi",
        mode: &["-dmenu"],
        prompt: PromptFlag::Separate("-p"),
        icons: true,
    },
    MenuFlavor {
        program: "wofi",
        mode: &["--dmenu"],
        prompt: PromptFlag::Separate("--prompt"),
        icons: false,
    },
    MenuFlavor {
        program: "tofi",
        mode: &[],
        prompt: PromptFlag::Joined("--prompt-text="),
        icons: false,
    },
    MenuFlavor {
        program: "bemenu",
        mode: &[],
        prompt: PromptFlag::Separate("-p"),
        icons: false,
    },
    MenuFlavor {
        program: "dmenu",
        mode: &[],
        prompt: PromptFlag::Separate("-p"),
        icons: false,
    },
];

/// Terminals that can host `pdfs-prompt --fzf`, most-preferred first.
///
/// The fzf front end is a terminal program, so a keybinding that has no tty
/// needs one spawned for it. Each entry sets a stable app-id/class
/// ([`TERMINAL_APP_ID`]) so a tiling WM can be told to float exactly this
/// window, and ends with the flag after which the command to run follows.
const KNOWN_TERMINALS: [&[&str]; 6] = [
    &["foot", "--app-id=pdfs-prompt", "--"],
    &["ghostty", "--class=pdfs-prompt", "-e"],
    &["kitty", "--class", "pdfs-prompt"],
    &["alacritty", "--class", "pdfs-prompt", "-e"],
    &["wezterm", "start", "--class", "pdfs-prompt", "--"],
    &["xterm", "-class", "pdfs-prompt", "-e"],
];

/// The app-id/class every [`KNOWN_TERMINALS`] entry sets, so the window rule a
/// user writes for one terminal keeps working if they switch.
pub const TERMINAL_APP_ID: &str = "pdfs-prompt";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptFlag {
    /// `--prompt=TEXT`
    Joined(&'static str),
    /// `-p TEXT`
    Separate(&'static str),
}

#[derive(Debug, Clone, Copy)]
struct MenuFlavor {
    program: &'static str,
    mode: &'static [&'static str],
    prompt: PromptFlag,
    icons: bool,
}

/// Prompt behaviour from `config.json`, under `prompt`.
///
/// ```json
/// "prompt": { "menu": ["fuzzel", "--dmenu", "--width=60"], "menu_limit": 40 }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptConfig {
    /// Which front end `pdfs-prompt` uses when invoked with no mode flag, so a
    /// desktop shortcut does not have to be re-bound to switch. `None` is
    /// [`PromptMode::Gtk`], the built-in HUD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<PromptMode>,
    /// Launcher argv for `pdfs-prompt --dmenu`. Unset means "find one" — see
    /// [`resolve_menu`]. A `{prompt}` token anywhere in the argv is replaced by
    /// the prompt text; without one the launcher's own prompt flag is appended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu: Option<Vec<String>>,
    /// Terminal argv for `pdfs-prompt --fzf` when it is started without a tty.
    /// Unset means "find one" — see [`resolve_terminal`]. A `{cmd}` token
    /// anywhere in the argv is replaced by the command to run; without one the
    /// command is appended, which is what every [`KNOWN_TERMINALS`] entry
    /// expects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<Vec<String>>,
    /// How many results to feed the launcher. Unset means
    /// [`DEFAULT_MENU_LIMIT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu_limit: Option<usize>,
    /// Show icons when the launcher understands them. Defaults to true; set
    /// false for a launcher that advertises support but is themed without them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu_icons: Option<bool>,
}

/// Which front end the prompt presents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptMode {
    /// The built-in GTK HUD.
    #[default]
    Gtk,
    /// The user's own launcher, driven over stdin/stdout.
    Dmenu,
    /// `fzf` in a terminal, re-querying the daemon on every keystroke.
    Fzf,
}

/// Rows an external launcher can show without becoming a scroll exercise. Larger
/// than the GTK prompt's cap because filtering happens launcher-side.
pub const DEFAULT_MENU_LIMIT: usize = 50;

impl PromptConfig {
    pub fn resolved_mode(&self) -> PromptMode {
        self.mode.unwrap_or_default()
    }

    pub fn resolved_menu_limit(&self) -> usize {
        self.menu_limit.unwrap_or(DEFAULT_MENU_LIMIT)
    }

    pub fn resolved_menu_icons(&self) -> bool {
        self.menu_icons.unwrap_or(true)
    }
}

/// One line offered to the launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// What the user sees, and what identifies the choice on the way back.
    pub label: String,
    /// Icon theme name, used only by launchers that support the protocol.
    pub icon: Option<String>,
}

impl MenuItem {
    pub fn new(label: impl Into<String>, icon: Option<String>) -> Self {
        Self {
            label: label.into(),
            icon,
        }
    }
}

/// What came back from the launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuChoice {
    /// Index into the items that were passed in.
    Item(usize),
    /// Text the user typed that matched no item — treat it as a query.
    Custom(String),
    /// Escape, or an empty selection.
    Cancelled,
}

/// The launcher to run: the configured one, or the first [`KNOWN_MENUS`] entry
/// on `PATH`. Returns the argv as it should be spawned, filter-mode flags
/// included.
pub fn resolve_menu(configured: Option<&Vec<String>>) -> Option<Vec<String>> {
    if let Some(argv) = configured.filter(|argv| !argv.is_empty()) {
        return Some(argv.clone());
    }
    let flavor = KNOWN_MENUS.iter().find(|f| on_path(f.program))?;
    let mut argv = vec![flavor.program.to_string()];
    argv.extend(flavor.mode.iter().map(|f| (*f).to_string()));
    Some(argv)
}

/// The terminal to host `pdfs-prompt --fzf` in: the configured one, or the
/// first [`KNOWN_TERMINALS`] entry on `PATH`. Returns the argv up to and
/// including the "run this command" flag — the caller appends the command, or
/// substitutes it for a `{cmd}` token, via [`with_command`].
pub fn resolve_terminal(configured: Option<&Vec<String>>) -> Option<Vec<String>> {
    if let Some(argv) = configured.filter(|argv| !argv.is_empty()) {
        return Some(argv.clone());
    }
    let flavor = KNOWN_TERMINALS
        .iter()
        .find(|argv| argv.first().is_some_and(|program| on_path(program)))?;
    Some(flavor.iter().map(|arg| (*arg).to_string()).collect())
}

/// Place `command` into a terminal argv: substituted for a `{cmd}` token if the
/// user wrote one, otherwise appended.
///
/// The token exists for terminals that want the command in the middle of their
/// own arguments; appending is right for every [`KNOWN_TERMINALS`] entry, whose
/// last element is the flag the command follows.
pub fn with_command(argv: &[String], command: &[String]) -> Vec<String> {
    if !argv.iter().any(|arg| arg == "{cmd}") {
        let mut argv = argv.to_vec();
        argv.extend_from_slice(command);
        return argv;
    }
    let mut out = Vec::with_capacity(argv.len() + command.len());
    for arg in argv {
        if arg == "{cmd}" {
            out.extend_from_slice(command);
        } else {
            out.push(arg.clone());
        }
    }
    out
}

/// Show `items` in the launcher named by `argv` and wait for a choice.
///
/// Blocking by design: the caller is a one-shot CLI invocation whose entire job
/// is this round-trip.
pub fn run(argv: &[String], prompt: &str, items: &[MenuItem]) -> Result<MenuChoice> {
    let argv = with_prompt(argv, prompt);
    let Some((program, args)) = argv.split_first() else {
        return Err(Error::Other("no launcher command configured".into()));
    };
    let icons = supports_icons(program);

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Other(format!("cannot run launcher {program}: {e}")))?;

    // Write on a worker: a launcher that starts reading only after it maps its
    // window would otherwise deadlock against a full pipe buffer.
    let payload = render(items, icons);
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Other("launcher stdin unavailable".into()))?;
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(payload.as_bytes());
        let _ = stdin.flush();
    });

    let mut selected = String::new();
    if let Some(stdout) = child.stdout.take() {
        // Only the first line matters; multi-select is not a thing here, and a
        // launcher that emits more must not leave us blocked on a full pipe.
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if selected.is_empty() {
                selected = line.trim_end_matches('\n').to_string();
            }
            line.clear();
        }
    }
    let _ = child.wait();
    let _ = writer.join();

    Ok(interpret(&selected, items))
}

/// Map the launcher's output back to a choice. Exposed for tests; the ambiguity
/// it resolves (label vs. free text) is the whole contract with the launcher.
fn interpret(selected: &str, items: &[MenuItem]) -> MenuChoice {
    // Some launchers echo the icon protocol back verbatim.
    let selected = selected
        .split(ICON_SEPARATOR)
        .next()
        .unwrap_or(selected)
        .trim_end();
    if selected.is_empty() {
        return MenuChoice::Cancelled;
    }
    match items.iter().position(|item| item.label == selected) {
        Some(index) => MenuChoice::Item(index),
        None => MenuChoice::Custom(selected.to_string()),
    }
}

/// The stdin payload. Labels are stripped of newlines rather than escaped —
/// there is no escape in this protocol, and a file name with a newline in it
/// would otherwise become two entries.
fn render(items: &[MenuItem], icons: bool) -> String {
    let mut out = String::new();
    for item in items {
        let label = item.label.replace(['\n', '\r'], " ");
        out.push_str(&label);
        if icons && let Some(icon) = &item.icon {
            out.push_str(ICON_SEPARATOR);
            out.push_str(icon);
        }
        out.push('\n');
    }
    out
}

/// Add the prompt text: substituted for `{prompt}` if the user placed one,
/// otherwise appended using the launcher's own flag. An unrecognised launcher
/// gets no prompt rather than a guessed flag that might not parse.
fn with_prompt(argv: &[String], prompt: &str) -> Vec<String> {
    if argv.iter().any(|arg| arg.contains("{prompt}")) {
        return argv
            .iter()
            .map(|arg| arg.replace("{prompt}", prompt))
            .collect();
    }
    let mut argv = argv.to_vec();
    if prompt.is_empty() {
        return argv;
    }
    if let Some(flavor) = argv.first().and_then(|program| flavor_of(program)) {
        match flavor.prompt {
            PromptFlag::Joined(flag) => argv.push(format!("{flag}{prompt}")),
            PromptFlag::Separate(flag) => {
                argv.push(flag.to_string());
                argv.push(prompt.to_string());
            }
        }
    }
    argv
}

fn supports_icons(program: &str) -> bool {
    flavor_of(program).is_some_and(|flavor| flavor.icons)
}

fn flavor_of(program: &str) -> Option<&'static MenuFlavor> {
    let base = Path::new(program).file_name()?.to_str()?;
    KNOWN_MENUS.iter().find(|flavor| flavor.program == base)
}

/// Whether `program` is an executable file on `PATH`.
pub fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<MenuItem> {
        vec![
            MenuItem::new(
                "notes.md   ·   My files / Docs",
                Some("text-x-generic".into()),
            ),
            MenuItem::new("photo.png   ·   Home / Pictures", None),
        ]
    }

    #[test]
    fn a_matching_line_is_an_item_and_anything_else_is_a_query() {
        let items = items();
        assert_eq!(
            interpret("notes.md   ·   My files / Docs", &items),
            MenuChoice::Item(0)
        );
        assert_eq!(
            interpret("invoice", &items),
            MenuChoice::Custom("invoice".into())
        );
        assert_eq!(interpret("", &items), MenuChoice::Cancelled);
    }

    #[test]
    fn an_echoed_icon_field_does_not_break_the_match() {
        let items = items();
        assert_eq!(
            interpret(
                "notes.md   ·   My files / Docs\0icon\x1ftext-x-generic",
                &items
            ),
            MenuChoice::Item(0)
        );
    }

    #[test]
    fn icons_are_only_rendered_for_launchers_that_understand_them() {
        let items = items();
        assert_eq!(
            render(&items, true),
            "notes.md   ·   My files / Docs\0icon\x1ftext-x-generic\nphoto.png   ·   Home / Pictures\n"
        );
        assert_eq!(
            render(&items, false),
            "notes.md   ·   My files / Docs\nphoto.png   ·   Home / Pictures\n"
        );
        assert!(supports_icons("fuzzel"));
        assert!(supports_icons("/usr/bin/rofi"));
        assert!(!supports_icons("wofi"));
        assert!(!supports_icons("mystery-menu"));
    }

    #[test]
    fn a_label_with_a_newline_stays_one_entry() {
        let sneaky = vec![MenuItem::new("two\nlines.txt", None)];
        assert_eq!(render(&sneaky, false), "two lines.txt\n");
    }

    #[test]
    fn the_prompt_uses_each_launchers_own_flag() {
        assert_eq!(
            with_prompt(&["fuzzel".into(), "--dmenu".into()], "Drive"),
            vec!["fuzzel", "--dmenu", "--prompt=Drive"]
        );
        assert_eq!(
            with_prompt(&["rofi".into(), "-dmenu".into()], "Drive"),
            vec!["rofi", "-dmenu", "-p", "Drive"]
        );
        // Unknown launcher: no guessed flag.
        assert_eq!(
            with_prompt(&["mystery-menu".into()], "Drive"),
            vec!["mystery-menu"]
        );
    }

    #[test]
    fn an_explicit_placeholder_wins_over_the_flag() {
        assert_eq!(
            with_prompt(
                &[
                    "rofi".into(),
                    "-dmenu".into(),
                    "-p".into(),
                    "{prompt} >".into()
                ],
                "Drive"
            ),
            vec!["rofi", "-dmenu", "-p", "Drive >"]
        );
    }

    #[test]
    fn a_configured_terminal_is_used_verbatim() {
        let configured = vec!["my-term".to_string(), "-e".to_string()];
        assert_eq!(
            resolve_terminal(Some(&configured)),
            Some(configured.clone())
        );
        // An empty list is "not configured", not "run nothing".
        assert_eq!(
            resolve_terminal(Some(&Vec::new())),
            resolve_terminal(None),
            "an empty terminal list must fall back to detection"
        );
    }

    #[test]
    fn the_command_is_appended_unless_the_user_placed_it() {
        let command = vec!["pdfs-prompt".to_string(), "--fzf".to_string()];
        assert_eq!(
            with_command(&["foot".into(), "--".into()], &command),
            vec!["foot", "--", "pdfs-prompt", "--fzf"]
        );
        assert_eq!(
            with_command(
                &["term".into(), "-e".into(), "{cmd}".into(), "--hold".into()],
                &command
            ),
            vec!["term", "-e", "pdfs-prompt", "--fzf", "--hold"]
        );
    }

    #[test]
    fn every_known_terminal_sets_the_same_app_id() {
        for argv in KNOWN_TERMINALS {
            assert!(
                argv.iter().any(|arg| arg.contains(TERMINAL_APP_ID)),
                "{argv:?} must be floatable by one window rule"
            );
        }
    }

    #[test]
    fn a_configured_launcher_is_used_verbatim() {
        let configured = vec!["my-menu".to_string(), "--pick".to_string()];
        assert_eq!(resolve_menu(Some(&configured)), Some(configured.clone()));
        // An empty list is "not configured", not "run nothing".
        assert_eq!(
            resolve_menu(Some(&Vec::new())),
            resolve_menu(None),
            "an empty menu list must fall back to detection"
        );
    }
}
