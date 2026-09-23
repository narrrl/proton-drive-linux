//! `pdfs-prompt --dmenu`: the same search, presented in the user's own launcher.
//!
//! Everything here is blocking and GTK-free — the process exists for one
//! round-trip and then exits, so there is no main loop to keep off. Talking to
//! the daemon and opening a chosen file are [`crate::query`]'s, shared with the
//! fzf front end and built on the GTK prompt's own [`Hit`](crate::Hit) model
//! and activation policy; this module is only presentation.
//!
//! Launchers filter a static list; they cannot call back for a new one per
//! keystroke. So the interaction is a loop: the first menu shows pinned files
//! and accepts free text, and any text that matches no entry becomes the next
//! search. Escape (an empty selection) ends it. `--fzf` ([`crate::fzf`]) is the
//! front end for users who want results to appear while they type instead.

use pdfs_core::config::AppDirs;
use pdfs_core::menu::{self, MenuChoice, MenuItem, PromptConfig};
use pdfs_core::opener::OpenWith;

use crate::query;
use crate::{Hit, gettext, gettext_f};

/// Guard against a launcher that keeps handing back text we cannot satisfy —
/// without it a scripted (non-interactive) menu could spin forever.
const MAX_ROUNDS: usize = 64;

pub(crate) struct Options {
    /// Launcher argv from `--menu`, overriding the configured/detected one.
    pub menu: Option<Vec<String>>,
    /// Skip the pinned-files round and search for this immediately.
    pub query: Option<String>,
}

/// Run the launcher flow. Returns an error only for conditions the user must
/// act on (no daemon, no launcher); a plain cancel is `Ok`.
pub(crate) fn run(options: Options) -> Result<(), String> {
    let dirs = AppDirs::new().map_err(|e| format!("cannot resolve app dirs: {e}"))?;
    let config = dirs.load_config();
    let prompt: PromptConfig = config.resolved_prompt();
    let policy: OpenWith = config.resolved_open_with();
    let socket = dirs.control_socket();

    let menu_argv =
        menu::resolve_menu(options.menu.as_ref().or(prompt.menu.as_ref())).ok_or_else(|| {
            "no launcher found — install fuzzel/rofi/wofi, or set prompt.menu in config.json"
                .to_string()
        })?;

    let mountpoint = query::mountpoint(&socket, &dirs);

    let mut query = options
        .query
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty());
    for _ in 0..MAX_ROUNDS {
        let hits = match &query {
            Some(text) => query::search(&socket, text, prompt.resolved_menu_limit())?,
            None => query::pins(&socket)?,
        };
        let items: Vec<MenuItem> = if hits.is_empty() {
            vec![MenuItem::new(placeholder(query.as_deref()), None)]
        } else {
            label_all(&hits)
        };

        match menu::run(&menu_argv, &title(query.as_deref()), &items).map_err(|e| e.to_string())? {
            MenuChoice::Item(index) => {
                let Some(hit) = hits.get(index) else {
                    // The placeholder row. Selecting it is not a cancel — the
                    // user pressed Enter on "type to search", so start over
                    // with the pinned list rather than quitting on them.
                    query = None;
                    continue;
                };
                return query::open(&socket, &mountpoint, &policy, hit, query.is_none());
            }
            // No entry matched what was typed: treat it as the next query.
            MenuChoice::Custom(text) => query = Some(text),
            MenuChoice::Cancelled => return Ok(()),
        }
    }
    Ok(())
}

/// The launcher's prompt string.
///
/// A launcher renders this immediately left of the input, so it is the only
/// place to say what typing here does. "Drive" alone read as a label and left
/// the two-step nature of the flow — type, Enter, *then* results — invisible;
/// the verb and the chevron make it a search box, and echoing the current query
/// shows which round you are in.
fn title(query: Option<&str>) -> String {
    match query {
        // Translators: the launcher prompt while refining a search; {query} is the text searched for. Keep the trailing space.
        Some(text) => gettext_f("Drive: {query} › ", &[("query", text)]),
        // Translators: the launcher prompt; keep the trailing space, the input follows it directly.
        None => gettext("Search Drive › "),
    }
}

/// Text for the one row shown when there is nothing to list.
///
/// A launcher handed an empty list shows a bare prompt, and some refuse to
/// return anything at all from an empty menu — which would strand the user on
/// the very first round, since a fresh account has no pins. A single row keeps
/// the menu non-empty and says why it is empty.
///
/// A fuzzy launcher can match this row against what the user types, and the
/// selection comes back as a label with no way to tell the two apart — so the
/// wording is parenthesised and avoids words that read like a filename query.
fn placeholder(query: Option<&str>) -> String {
    match query {
        // Translators: the only row of an empty launcher list; {query} is the text searched for. Keep the parentheses.
        Some(text) => gettext_f("(no matches for “{query}”)", &[("query", text)]),
        // Translators: the only row of an empty launcher list. Keep the parentheses, and avoid words that look like a file name.
        None => gettext("(no pinned files — type to search)"),
    }
}

/// One launcher line per hit. Labels must be unique: they are how the selection
/// is matched back, and two files of the same name in the same folder listing
/// would otherwise be indistinguishable.
fn label_all(hits: &[Hit]) -> Vec<MenuItem> {
    let mut items: Vec<MenuItem> = Vec::with_capacity(hits.len());
    for hit in hits {
        let mut label = format!("{}   ·   {}", hit.name(), hit.location());
        let mut suffix = 2;
        while items.iter().any(|item| item.label == label) {
            label = format!("{}   ·   {} ({suffix})", hit.name(), hit.location());
            suffix += 1;
        }
        items.push(MenuItem::new(label, Some(query::icon_name(hit))));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfs_core::control::{LocalHit, SearchHit};

    fn drive(name: &str, path: &str, is_dir: bool) -> Hit {
        Hit::Drive(SearchHit {
            name: name.into(),
            path: path.into(),
            is_dir,
            size: 0,
            modified: 0,
            pinned: false,
            cached: false,
            uid: "uid".into(),
            mounted_path: None,
            score: 0,
        })
    }

    #[test]
    fn labels_are_unique_so_a_selection_maps_back_to_one_hit() {
        let hits = vec![
            drive("notes.md", "Docs/notes.md", false),
            Hit::Local(LocalHit {
                name: "notes.md".into(),
                path: "/home/me/Docs/notes.md".into(),
                is_dir: false,
                size: 0,
                modified: 0,
                score: 0,
            }),
            drive("notes.md", "Docs/notes.md", false),
        ];
        let items = label_all(&hits);
        assert_eq!(items.len(), 3);
        assert_ne!(items[0].label, items[2].label);
        let unique: std::collections::HashSet<_> =
            items.iter().map(|item| item.label.clone()).collect();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn the_prompt_says_a_search_is_what_enter_does() {
        let first = title(None);
        assert!(first.to_lowercase().contains("search"), "{first}");
        // The launcher renders the input immediately after the prompt, so the
        // prompt has to carry its own separator or it runs into what is typed.
        assert!(first.ends_with(' '), "{first:?}");

        let refined = title(Some("md"));
        assert!(refined.contains("md"), "{refined}");
        assert!(refined.ends_with(' '), "{refined:?}");
    }

    #[test]
    fn an_empty_round_still_has_a_row_explaining_itself() {
        assert!(placeholder(None).contains("type to search"));
        assert!(placeholder(Some("md")).contains("md"));
    }
}
