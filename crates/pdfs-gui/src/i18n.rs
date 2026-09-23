//! Translation layer shared by `pdfs-app`, `pdfs-tray` and `pdfs-prompt`.
//!
//! Strings go through GNU gettext in the `pdfs` text domain. [`init`] runs first
//! in every binary: it applies the language picked in Preferences (if any) on
//! top of the system locale, then binds the domain to the installed catalogs.
//!
//! Messages with values use named placeholders (`{name}`) filled in by
//! [`gettext_f`] and [`ngettext_f`], so translators can reorder them. `po/`
//! holds the template and the catalogs; see `docs/TRANSLATING.md`.

// Each binary includes this module and uses a different part of it.
#![allow(dead_code)]

use gettextrs::LocaleCategory;
use pdfs_core::config::AppDirs;

/// The gettext text domain, matching `po/pdfs.pot` and the installed `.mo` name.
pub(crate) const DOMAIN: &str = "pdfs";

/// Overrides the catalog directory at run time, for running from a source tree
/// (`PDFS_LOCALEDIR=target/locale`).
const LOCALEDIR_ENV: &str = "PDFS_LOCALEDIR";

/// Where packages install the catalogs unless the build sets `PDFS_LOCALEDIR`.
const DEFAULT_LOCALEDIR: &str = "/usr/share/locale";

/// Languages the app ships a catalog for, as (gettext code, native name). The
/// Preferences picker lists them after "System Default". US English is the
/// source language and needs no catalog. Names stay untranslated so a user can
/// always find their own language.
pub(crate) const LANGUAGES: &[(&str, &str)] = &[
    ("de", "Deutsch"),
    ("en_GB", "English (UK)"),
    ("en_US", "English (US)"),
    ("es", "Español"),
    ("fr", "Français"),
    ("it", "Italiano"),
    ("nl", "Nederlands"),
    ("pl", "Polski"),
    ("pt_BR", "Português (Brasil)"),
];

/// Set up the locale and the text domain. Call first thing in `main`, before
/// any thread starts: an explicit language is applied through the process
/// environment, so that GTK's own strings follow it too.
pub(crate) fn init() {
    let language = AppDirs::new()
        .ok()
        .and_then(|dirs| dirs.load_config().language)
        .filter(|code| LANGUAGES.iter().any(|(known, _)| known == code));
    gettextrs::setlocale(LocaleCategory::LcAll, "");
    if let Some(code) = language {
        apply_language(&code);
    }
    let dir = std::env::var(LOCALEDIR_ENV)
        .ok()
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| {
            option_env!("PDFS_LOCALEDIR")
                .unwrap_or(DEFAULT_LOCALEDIR)
                .to_string()
        });
    if let Err(e) = gettextrs::bindtextdomain(DOMAIN, dir) {
        tracing::warn!("bindtextdomain failed: {e}");
    }
    let _ = gettextrs::bind_textdomain_codeset(DOMAIN, "UTF-8");
    if let Err(e) = gettextrs::textdomain(DOMAIN) {
        tracing::warn!("textdomain failed: {e}");
    }
}

/// Make `code` the message language. gettext reads `LANGUAGE` first, but
/// ignores it while the message locale is "C" or "C.UTF-8", so a system without
/// a configured locale also gets its message locale moved to a real one. Both
/// go into the environment, because GTK calls `setlocale` again during its own
/// start.
fn apply_language(code: &str) {
    // SAFETY: `init` runs at the top of `main`, before any other thread exists.
    unsafe { std::env::set_var("LANGUAGE", code) };
    let current = gettextrs::setlocale(LocaleCategory::LcMessages, "")
        .map(|name| String::from_utf8_lossy(&name).into_owned())
        .unwrap_or_default();
    if !is_c_locale(&current) {
        return;
    }
    for candidate in message_locales(code) {
        if gettextrs::setlocale(LocaleCategory::LcMessages, candidate.as_str()).is_some() {
            // SAFETY: as above, still single-threaded.
            unsafe { std::env::set_var("LC_MESSAGES", &candidate) };
            return;
        }
    }
}

fn is_c_locale(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name);
    matches!(base, "" | "C" | "POSIX")
}

/// Locales to try, in order, as the message locale for `code`: the language's
/// own locale, then US English, which any locale-enabled system has and which
/// is enough for gettext to honour `LANGUAGE`.
fn message_locales(code: &str) -> Vec<String> {
    let full = if code.contains('_') {
        code.to_string()
    } else {
        format!("{code}_{}", code.to_uppercase())
    };
    let mut locales = vec![format!("{full}.UTF-8")];
    if full != "en_US" {
        locales.push("en_US.UTF-8".to_string());
    }
    locales
}

/// Mark `msgid` for extraction without translating it yet, for tables of
/// labels built at compile time. Pass the value through [`gettext`] where it is
/// shown.
pub(crate) const fn gettext_noop(msgid: &'static str) -> &'static str {
    msgid
}

/// Translate `msgid`.
pub(crate) fn gettext(msgid: &str) -> String {
    gettextrs::gettext(msgid)
}

/// Translate `msgid` in context `context`, for short strings whose meaning
/// depends on where they appear ("Open" the verb against "Open" the state).
pub(crate) fn pgettext(context: &str, msgid: &str) -> String {
    gettextrs::pgettext(context, msgid)
}

/// Translate the singular or plural form for `n`.
pub(crate) fn ngettext(singular: &str, plural: &str, n: u64) -> String {
    gettextrs::ngettext(singular, plural, plural_count(n))
}

/// Translate `msgid`, then fill its `{name}` placeholders from `args`.
pub(crate) fn gettext_f(msgid: &str, args: &[(&str, &str)]) -> String {
    fill(&gettext(msgid), args)
}

/// Translate the form for `n`, then fill `{n}` with the count and the other
/// placeholders from `args`.
pub(crate) fn ngettext_f(singular: &str, plural: &str, n: u64, args: &[(&str, &str)]) -> String {
    let count = n.to_string();
    let filled = fill(&ngettext(singular, plural, n), args);
    fill(&filled, &[("n", &count)])
}

/// The translated form of [`pdfs_core::control::pending_summary`]: what is
/// still waiting to go up, or `None` when nothing is.
pub(crate) fn pending_summary(uploads: u64, changes: u64) -> Option<String> {
    match (uploads, changes) {
        (0, 0) => None,
        (uploads, 0) => Some(ngettext_f(
            "{n} upload queued",
            "{n} uploads queued",
            uploads,
            &[],
        )),
        (0, changes) => Some(ngettext_f(
            "{n} change queued",
            "{n} changes queued",
            changes,
            &[],
        )),
        (uploads, changes) => {
            let uploads = ngettext_f("{n} upload", "{n} uploads", uploads, &[]);
            let changes = ngettext_f("{n} change", "{n} changes", changes, &[]);
            // Translators: {uploads} is "3 uploads", {changes} is "2 changes".
            Some(gettext_f(
                "{uploads}, {changes} queued",
                &[("uploads", &uploads), ("changes", &changes)],
            ))
        }
    }
}

/// gettext takes the count as `u32`. Counts past that pick the same form as
/// the largest one, which is the plural in every shipped language.
fn plural_count(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Replace each `{key}` in `template` with its value. Unknown placeholders are
/// left as they are, so a translation mistake shows up rather than vanishing.
fn fill(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (key, value) in args {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_replaces_named_placeholders_in_any_order() {
        assert_eq!(
            fill("{b} before {a}", &[("a", "x"), ("b", "y")]),
            "y before x"
        );
    }

    #[test]
    fn fill_keeps_unknown_placeholders() {
        assert_eq!(fill("{missing} {a}", &[("a", "1")]), "{missing} 1");
    }

    #[test]
    fn ngettext_f_fills_the_count() {
        assert_eq!(ngettext_f("{n} file", "{n} files", 1, &[]), "1 file");
        assert_eq!(
            ngettext_f(
                "{n} file in {dir}",
                "{n} files in {dir}",
                3,
                &[("dir", "A")]
            ),
            "3 files in A"
        );
    }

    #[test]
    fn pending_summary_matches_the_core_wording() {
        for (uploads, changes) in [(0, 0), (1, 0), (3, 0), (0, 1), (2, 4)] {
            assert_eq!(
                pending_summary(uploads, changes),
                pdfs_core::control::pending_summary(uploads, changes)
            );
        }
    }

    #[test]
    fn plural_count_saturates() {
        assert_eq!(plural_count(7), 7);
        assert_eq!(plural_count(u64::MAX), u32::MAX);
    }

    #[test]
    fn language_codes_are_unique_and_sorted() {
        let codes: Vec<&str> = LANGUAGES.iter().map(|(code, _)| *code).collect();
        let mut sorted = codes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(codes, sorted);
    }

    fn repo_file(path: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }

    fn linguas() -> Vec<String> {
        std::fs::read_to_string(repo_file("po/LINGUAS"))
            .unwrap()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn every_offered_language_has_a_catalog() {
        let linguas = linguas();
        for (code, _) in LANGUAGES {
            // US English is the source language.
            if *code != "en_US" {
                assert!(
                    linguas.iter().any(|lang| lang == code),
                    "{code} is not in po/LINGUAS"
                );
            }
        }
        for lang in &linguas {
            assert!(
                repo_file(&format!("po/{lang}.po")).exists(),
                "po/{lang}.po is missing"
            );
        }
    }

    #[test]
    fn potfiles_lists_every_source_that_translates() {
        let listed = std::fs::read_to_string(repo_file("po/POTFILES.in")).unwrap();
        let listed: Vec<&str> = listed.lines().map(str::trim).collect();
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                if !text.contains("gettext") {
                    continue;
                }
                let relative = path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap();
                let name = format!("crates/pdfs-gui/{}", relative.display());
                assert!(
                    listed.contains(&name.as_str()),
                    "{name} is not in po/POTFILES.in"
                );
            }
        }
    }

    /// `msgfmt --check` rejects a translation whose `{placeholders}` differ from
    /// the English, besides syntax errors. Skipped where gettext is not installed.
    #[test]
    fn catalogs_compile_with_matching_placeholders() {
        let msgfmt = std::process::Command::new("msgfmt")
            .arg("--version")
            .output();
        if msgfmt.is_err() {
            eprintln!("msgfmt not installed; skipping the catalog check");
            return;
        }
        for lang in linguas() {
            let output = std::process::Command::new("msgfmt")
                .args(["--check", "--output-file=/dev/null"])
                .arg(repo_file(&format!("po/{lang}.po")))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "po/{lang}.po: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn c_locales_are_detected() {
        assert!(is_c_locale("C"));
        assert!(is_c_locale("POSIX"));
        assert!(is_c_locale("C.UTF-8"));
        assert!(is_c_locale("C.utf8"));
        assert!(!is_c_locale("de_DE.UTF-8"));
    }

    #[test]
    fn message_locales_start_with_the_language_region() {
        assert_eq!(message_locales("de"), ["de_DE.UTF-8", "en_US.UTF-8"]);
        assert_eq!(message_locales("pt_BR"), ["pt_BR.UTF-8", "en_US.UTF-8"]);
        assert_eq!(message_locales("en_US"), ["en_US.UTF-8"]);
    }
}
