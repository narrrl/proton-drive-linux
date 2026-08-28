//! `pdfs-prompt --fzf`: launcher search that queries the daemon per keystroke.
//!
//! A dmenu-protocol launcher (fuzzel, rofi, wofi, …) is handed one static list
//! and filters it itself; it cannot ask for a new one as the user types, which
//! is why [`crate::dmenu`] has to make the search a two-step. `fzf` can:
//! `--bind change:reload(…)` re-runs a command on every keystroke. So this
//! front end re-executes *this binary* in `--feed` mode, and the daemon's own
//! ranking ([`pdfs_core::control::Request::SearchV2`]) decides the order —
//! `--disabled` turns fzf's matcher off so it does not re-sort what came back.
//!
//! Three roles, all the same binary:
//!
//! * **outer** — `--fzf` with no tty (a WM keybinding): spawn a terminal that
//!   runs the inner role, and exit.
//! * **inner** — `--fzf` with a tty: run fzf, open what was chosen.
//! * **feed** — `--feed <TEXT>`: print one line per hit and exit. fzf spawns
//!   this per keystroke and kills the one it superseded.
//!
//! A line is `display\tpayload`, shown through `--with-nth=1` so only the left
//! half is visible. The payload is the JSON [`Hit`], which is what makes the
//! selection unambiguous: unlike the dmenu path, identity does not have to be
//! recovered from the label, so [`crate::dmenu::label_all`]'s uniquifying
//! suffix has no counterpart here. JSON escapes tabs and newlines, so no file
//! name can split a line.

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use pdfs_core::config::AppDirs;
use pdfs_core::menu::{self, PromptConfig};
use pdfs_core::opener::OpenWith;

use crate::query;
use crate::{Hit, SEARCH_DEBOUNCE};

/// Separates the visible half of a feed line from its payload.
const FIELD: char = '\t';

/// What the launcher prompt reads, matching the dmenu front end's wording.
const PROMPT: &str = "Search Drive › ";

pub(crate) struct Options {
    /// Search for this immediately instead of opening on the pinned files.
    pub query: Option<String>,
    /// Set by the terminal we spawned: run fzf here even without a tty rather
    /// than spawning a second terminal, which would recurse forever if the
    /// terminal does not give its child one.
    pub inner: bool,
}

/// Run the fzf flow. Returns an error only for conditions the user must act on
/// (no fzf, no terminal, no daemon); a plain cancel is `Ok`.
pub(crate) fn run(options: Options) -> Result<(), String> {
    if options.inner || std::io::stdin().is_terminal() {
        return interactive(options.query);
    }
    spawn_terminal(options.query)
}

/// Re-launch this binary's inner role inside a terminal.
///
/// Not waited on: a keybinding should return immediately, and the terminal
/// owns the rest of the interaction — including opening the chosen file, which
/// `opener` detaches anyway.
fn spawn_terminal(query: Option<String>) -> Result<(), String> {
    let dirs = AppDirs::new().map_err(|e| format!("cannot resolve app dirs: {e}"))?;
    let prompt: PromptConfig = dirs.load_config().resolved_prompt();
    let terminal = menu::resolve_terminal(prompt.terminal.as_ref()).ok_or_else(|| {
        "no terminal found — install foot/kitty/alacritty, or set prompt.terminal in config.json"
            .to_string()
    })?;

    let argv = menu::with_command(&terminal, &inner_command(self_exe()?, query.as_deref()));
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "no terminal command configured".to_string())?;
    Command::new(program)
        .args(args)
        .spawn()
        .map_err(|e| format!("cannot run terminal {program}: {e}"))?;
    Ok(())
}

/// The argv the spawned terminal runs.
fn inner_command(exe: PathBuf, query: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        exe.display().to_string(),
        "--fzf".to_string(),
        "--inner".to_string(),
    ];
    if let Some(text) = query.filter(|text| !text.trim().is_empty()) {
        argv.push("--query".to_string());
        argv.push(text.to_string());
    }
    argv
}

/// Run fzf here and act on the choice.
fn interactive(query: Option<String>) -> Result<(), String> {
    if !menu::on_path("fzf") {
        return Err("fzf is not installed — install it, or use --dmenu".to_string());
    }
    let dirs = AppDirs::new().map_err(|e| format!("cannot resolve app dirs: {e}"))?;
    let config = dirs.load_config();
    let prompt: PromptConfig = config.resolved_prompt();
    let policy: OpenWith = config.resolved_open_with();
    let socket = dirs.control_socket();

    let query = query
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty());
    // fzf only fires `change` for a *keystroke*, so the first screen — pins, or
    // the results for an initial --query — has to arrive on stdin.
    let initial = hits(&socket, query.as_deref(), prompt.resolved_menu_limit());

    let argv = fzf_argv(&self_exe()?, query.as_deref());
    let (program, args) = argv.split_first().expect("fzf_argv is never empty");
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run fzf: {e}"))?;

    // Written on a worker for the same reason the dmenu path does it: fzf reads
    // stdin only once its UI is up, and a full pipe buffer would deadlock us.
    let payload: String = initial.iter().map(encode_line).collect();
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "fzf stdin unavailable".to_string())?;
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(payload.as_bytes());
        let _ = stdin.flush();
    });

    let mut selected = String::new();
    if let Some(stdout) = child.stdout.take() {
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

    // Escape, or Enter on the "cannot reach the daemon" row, which carries no
    // payload — neither is an error.
    let Some(hit) = decode_line(&selected) else {
        return Ok(());
    };
    let mountpoint = query::mountpoint(&socket, &dirs);
    // Only the no-query screen is the pin list, and only there does a row's
    // `is_dir` mean pin policy rather than node kind.
    query::open(&socket, &mountpoint, &policy, &hit, query.is_none())
}

/// `--feed`: one round-trip, printed for fzf to swallow.
///
/// Always succeeds. A reload command that exits non-zero leaves fzf showing an
/// empty list with no explanation, so a failure becomes a visible row instead.
pub(crate) fn feed(query: Option<&str>) {
    let Ok(dirs) = AppDirs::new() else {
        println!("(cannot resolve app dirs)");
        return;
    };
    let limit = dirs.load_config().resolved_prompt().resolved_menu_limit();
    let socket = dirs.control_socket();
    let query = query.map(str::trim).filter(|text| !text.is_empty());

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in hits(&socket, query, limit).iter().map(encode_line) {
        let _ = out.write_all(line.as_bytes());
    }
}

/// The rows for one screen: pins when there is nothing typed, search results
/// otherwise — the same split the GTK prompt makes on an empty entry.
fn hits(socket: &std::path::Path, query: Option<&str>, limit: usize) -> Vec<Hit> {
    let result = match query {
        Some(text) => query::search(socket, text, limit),
        None => query::pins(socket),
    };
    match result {
        Ok(hits) => hits,
        Err(message) => {
            // Printed as a row rather than to stderr: fzf's alternate screen
            // hides stderr, so this is the only place the user would see it.
            println!("({message})");
            Vec::new()
        }
    }
}

fn fzf_argv(exe: &std::path::Path, query: Option<&str>) -> Vec<String> {
    let feed = format!("{} --feed {{q}}", shell_quote(&exe.display().to_string()));
    // The sleep is the debounce: fzf kills a reload child that a later keystroke
    // superseded, so a burst of typing costs one daemon round-trip, exactly as
    // the GTK prompt's timeout does.
    let debounce = SEARCH_DEBOUNCE.as_secs_f32();
    let mut argv: Vec<String> = [
        "fzf",
        // fzf's own fuzzy matcher would re-order and re-filter what the daemon
        // already ranked, and would hide rows it does not think match.
        "--disabled",
        "--no-sort",
        "--no-multi",
        "--layout=reverse",
        "--info=inline",
        "--delimiter=\t",
        "--with-nth=1",
    ]
    .iter()
    .map(|arg| (*arg).to_string())
    .collect();
    argv.push(format!("--prompt={PROMPT}"));
    // `reload:` rather than `reload(…)`: the unparenthesised form runs to the
    // end of the argument, so an install path containing a bracket cannot
    // truncate the command fzf parses out of it.
    argv.push(format!("--bind=change:reload:sleep {debounce}; {feed}"));
    if let Some(text) = query {
        argv.push(format!("--query={text}"));
    }
    argv
}

/// `display\tpayload\n`. The display half is scrubbed of the delimiter and of
/// newlines; the payload is JSON, which escapes both.
fn encode_line(hit: &Hit) -> String {
    let display =
        format!("{}   ·   {}", hit.name(), hit.location()).replace(['\n', '\r', FIELD], " ");
    let payload = serde_json::to_string(hit).unwrap_or_default();
    format!("{display}{FIELD}{payload}\n")
}

/// The hit a selected line stands for, or `None` for a line that carries no
/// payload — the error/empty row, or an empty selection (Escape).
fn decode_line(line: &str) -> Option<Hit> {
    let (_, payload) = line.trim_end_matches('\n').rsplit_once(FIELD)?;
    serde_json::from_str(payload).ok()
}

fn self_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot locate pdfs-prompt: {e}"))
}

/// POSIX single-quoting, for the one string that goes through fzf's `sh -c`.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfs_core::control::{LocalHit, SearchHit};

    fn drive(name: &str, path: &str) -> Hit {
        Hit::Drive(SearchHit {
            name: name.into(),
            path: path.into(),
            is_dir: false,
            size: 7,
            modified: 9,
            pinned: false,
            uid: "uid~1".into(),
            mounted_path: None,
            score: 42,
        })
    }

    #[test]
    fn a_line_round_trips_through_the_payload() {
        let hit = drive("notes.md", "Docs/notes.md");
        let line = encode_line(&hit);
        assert!(line.ends_with('\n'));
        let back = decode_line(&line).expect("payload decodes");
        assert_eq!(back.name(), "notes.md");
        assert_eq!(back.key(), hit.key());
        assert_eq!(back.score(), 42);
    }

    #[test]
    fn a_local_hit_round_trips_as_itself() {
        let hit = Hit::Local(LocalHit {
            name: "notes.md".into(),
            path: "/home/me/Docs/notes.md".into(),
            is_dir: false,
            size: 1,
            modified: 2,
            score: 3,
        });
        let back = decode_line(&encode_line(&hit)).expect("payload decodes");
        assert!(matches!(back, Hit::Local(_)));
        assert_eq!(back.key(), hit.key());
    }

    #[test]
    fn a_name_with_a_tab_or_newline_stays_one_line() {
        let hit = drive("two\tparts\nfile.md", "Docs/two\tparts\nfile.md");
        let line = encode_line(&hit);
        assert_eq!(line.matches('\n').count(), 1, "{line:?}");
        assert_eq!(line.matches(FIELD).count(), 1, "{line:?}");
        assert_eq!(
            decode_line(&line).expect("payload decodes").name(),
            "two\tparts\nfile.md"
        );
    }

    #[test]
    fn a_row_without_a_payload_is_not_a_hit() {
        assert!(decode_line("").is_none());
        assert!(decode_line("(cannot reach the Proton Drive daemon)").is_none());
        assert!(decode_line("bogus\tnot json").is_none());
    }

    #[test]
    fn fzf_is_told_not_to_do_its_own_matching() {
        let argv = fzf_argv(std::path::Path::new("/usr/bin/pdfs-prompt"), None);
        assert!(argv.contains(&"--disabled".to_string()), "{argv:?}");
        assert!(argv.contains(&"--no-sort".to_string()), "{argv:?}");
        assert!(
            argv.iter().any(|arg| arg.starts_with("--with-nth=")),
            "the payload field must stay hidden: {argv:?}"
        );
    }

    #[test]
    fn every_keystroke_re_runs_the_feed_after_the_debounce() {
        let argv = fzf_argv(std::path::Path::new("/usr/bin/pdfs-prompt"), Some("md"));
        let bind = argv
            .iter()
            .find(|arg| arg.starts_with("--bind=change:"))
            .expect("a change binding");
        assert!(bind.contains("reload"), "{bind}");
        assert!(bind.contains("--feed {q}"), "{bind}");
        assert!(bind.contains(&format!("sleep {}", SEARCH_DEBOUNCE.as_secs_f32())));
        assert!(argv.contains(&"--query=md".to_string()), "{argv:?}");
    }

    #[test]
    fn a_path_with_a_quote_cannot_break_out_of_the_reload_command() {
        let argv = fzf_argv(std::path::Path::new("/home/o'brien/my prompt"), None);
        let bind = argv
            .iter()
            .find(|arg| arg.starts_with("--bind=change:"))
            .expect("a change binding");
        assert!(bind.contains(r"'/home/o'\''brien/my prompt'"), "{bind}");
        // The unparenthesised reload form: a bracket in the path must not end
        // the command as far as fzf's own parser is concerned.
        assert!(bind.starts_with("--bind=change:reload:"), "{bind}");
    }

    #[test]
    fn the_terminal_runs_the_inner_role_so_it_cannot_recurse() {
        let argv = inner_command(PathBuf::from("/usr/bin/pdfs-prompt"), Some("md"));
        assert_eq!(
            argv,
            vec!["/usr/bin/pdfs-prompt", "--fzf", "--inner", "--query", "md"]
        );
        assert_eq!(
            inner_command(PathBuf::from("/usr/bin/pdfs-prompt"), Some("  ")),
            vec!["/usr/bin/pdfs-prompt", "--fzf", "--inner"],
            "a blank query is no query"
        );
    }
}
