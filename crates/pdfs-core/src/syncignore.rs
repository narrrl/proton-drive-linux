//! Selective-sync ignore rules for mirror folders.
//!
//! A mirror folder otherwise uploads every path beneath its root, which for a
//! project directory means `node_modules/`, `.git/`, and `target/` — quota
//! spent on files nobody wants on Drive, and enough API traffic to invite rate
//! limiting.
//!
//! Rules come from two places, unioned:
//!
//! * `.pdfsignore` (or `.protonignore`) at the root of the synced folder,
//!   gitignore syntax.
//! * A global pattern list from the config, applied to every synced folder.
//!
//! Both are read fresh at the start of each reconcile pass, so editing the file
//! takes effect on the next pass without restarting the daemon.
//!
//! # Ignoring is not deleting
//!
//! A path that becomes ignored after it was already synced keeps its remote
//! copy and its baseline row. The sync engine drops ignored paths from
//! classification entirely rather than treating them as locally deleted —
//! see the union filter in `pdfs-fuse`'s `do_reconcile`. Editing a text file
//! must never trash data on Drive.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Patterns applied to every synced folder when the config does not override
/// them. Version-control metadata, dependency and build trees, editor
/// leftovers, and OS turds — none of which belong on Drive, all of which are
/// large or rewritten constantly.
///
/// The second half is what Nextcloud, Dropbox and Syncthing leave out as well:
/// files a desktop, a file system or a program in the middle of a write leaves
/// behind, never anything a person made. Every one is in a test, next to a real
/// file of a similar name that must stay synced.
pub const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    ".git/",
    ".hg/",
    ".svn/",
    "node_modules/",
    "target/",
    ".venv/",
    "__pycache__/",
    "*~",
    "*.swp",
    "*.tmp",
    ".DS_Store",
    "Thumbs.db",
    // Folder settings of Windows and KDE's Dolphin.
    "desktop.ini",
    ".directory",
    // Trash folders and file-system leftovers: a file deleted while open on a
    // FUSE or NFS mount lives on under one of these names until it is closed.
    ".Trash-*/",
    ".Trashes/",
    ".Spotlight-V100/",
    ".fuse_hidden*",
    ".nfs*",
    // Downloads and saves still being written.
    "*.part",
    "*.crdownload",
    "*.kate-swp",
    // Office lock files: LibreOffice's `.~lock.<name>#`, Microsoft Office's `~$<name>`.
    ".~lock.*#",
    "~$*",
];

/// Suffixes that mean a file is incomplete *by construction*: only the program
/// writing it gives it this name, only while the write is in flight, and the
/// finished article always arrives under a different one. On an on-demand mount
/// these should never seal a remote revision — the app renames the finished file
/// to its real name, and only that belongs on Drive. See [`is_transient_name`]
/// and docs/BUGS.md B70.
///
/// `.tmp`/`.temp` are deliberately **not** here, though they are in
/// [`DEFAULT_IGNORE_PATTERNS`]. The two lists answer different questions. Skipping
/// a `report.tmp` during a mirror reconcile costs nothing — the file stays on
/// disk, and a later pass picks it up if the rules change. Parking one on the
/// on-demand mount is irreversible in practice: the only exit is a rename to a
/// finished name, and a file that is simply *called* `index.tmp` — a static-site
/// template, a tool's output — never gets one. It then never reaches Drive at
/// all, silently, while its bytes pin `staging/`. That is what a real Takeout
/// tree of `index.tmp`/`preview.tmp` gallery templates did: 43 creates parked
/// forever. The generic-scratch case they were added for renames within
/// milliseconds, so it never needed the park; the download suffixes below, which
/// stay in flight for minutes, are what B70 was actually about.
const TRANSIENT_SUFFIXES: &[&str] = &[
    ".crdownload", // Chromium/Brave/Edge
    ".part",       // Firefox, wget, aria2c
    ".partial",    // some downloaders / older IE
    ".download",   // Safari / macOS
    ".swp",        // vim swap
    ".swx",        // vim swap (secondary)
];

/// Whether `name` is a transient scratch/in-flight file whose bytes should not
/// be uploaded on an on-demand mount until it is renamed to its finished name.
///
/// Recognises browser download temps, editor swap/backup files, and lock files.
/// Case-insensitive on the suffix so `FOO.CRDOWNLOAD` counts too. This is the
/// on-demand-write counterpart to [`DEFAULT_IGNORE_PATTERNS`] (which only covers
/// mirror-folder reconcile); keep the two conceptually in step.
pub fn is_transient_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if TRANSIENT_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        return true;
    }
    // Emacs/gedit/kate backup copies.
    if name.ends_with('~') {
        return true;
    }
    // LibreOffice lock files (`.~lock.<doc>#`) and MS Office temps (`~$doc`).
    if name.starts_with(".~lock.") || name.starts_with("~$") {
        return true;
    }
    false
}

/// Whether `name` is scratch another program writes and throws away: the
/// [`is_transient_name`] set, plus the save-by-rename temporaries of GTK, Qt,
/// Chromium and Electron, generic `.tmp` files and Emacs lock files. None of
/// them is a file the user made, so a front-end can hide them from a history
/// of what happened to the user's files.
pub fn is_scratch_name(name: &str) -> bool {
    if is_transient_name(name) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tmp")
        || lower.ends_with(".temp")
        || name.starts_with(".goutputstream-")
        || name.starts_with(".org.chromium.")
        || name.starts_with(".com.google.chrome.")
        || name.starts_with(".#")
        || (name.starts_with('#') && name.ends_with('#') && name.len() > 1)
        || (name.starts_with('.') && name.contains(".tmp."))
}

/// Filenames accepted as a folder's ignore file, in precedence order.
const IGNORE_FILE_NAMES: &[&str] = &[".pdfsignore", ".protonignore"];

/// Whether `line` is a rule [`IgnoreRules::load`] accepts: a pattern, a
/// comment or a blank line. The error says what is wrong with it.
pub fn check_pattern(line: &str) -> Result<(), String> {
    GitignoreBuilder::new("/")
        .add_line(None, line)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The lines of an ignore file that [`IgnoreRules::load`] would skip, by line
/// number (from 1), with what is wrong with each.
pub fn check_rules(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| check_pattern(line).err().map(|e| (i + 1, e)))
        .collect()
}

/// A synced folder's own ignore file, read so it can be edited and written
/// back. See [`FolderRules::read`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRules {
    /// The file the rules are in: the one in use, or `.pdfsignore` when there
    /// is none yet.
    pub path: std::path::PathBuf,
    /// The rules as read; empty when there is no file. `None` means there was
    /// no file, which [`write`](Self::write) checks is still so.
    pub text: Option<String>,
}

/// Why [`FolderRules::write`] did not write.
#[derive(Debug)]
pub enum WriteRulesError {
    /// The file changed after it was read, for example by an edit on another
    /// computer that sync brought in. The rules on disk are the new ones.
    Changed,
    /// Reading or writing failed.
    Io(std::io::Error),
}

impl std::fmt::Display for WriteRulesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Changed => f.write_str("the file changed since it was opened"),
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for WriteRulesError {}

impl From<std::io::Error> for WriteRulesError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl FolderRules {
    /// Read the ignore file of the folder at `root`, the same one
    /// [`IgnoreRules::load`] uses.
    pub fn read(root: &Path) -> std::io::Result<Self> {
        for name in IGNORE_FILE_NAMES {
            let path = root.join(name);
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    return Ok(Self {
                        path,
                        text: Some(text),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            }
        }
        // Like `load`, a missing folder reads as one without rules; the write
        // is what fails then.
        Ok(Self {
            path: root.join(IGNORE_FILE_NAMES[0]),
            text: None,
        })
    }

    /// Replace the rules with `text`, unless the file changed since it was
    /// read. Rules that are all blank remove the file, so a folder never keeps
    /// an empty one around.
    ///
    /// Written to a temporary file and renamed over the old one, so the sync
    /// pass reading the rules at the same moment sees the old ones or the new
    /// ones, never half of them.
    pub fn write(&self, text: &str) -> Result<(), WriteRulesError> {
        let now = match std::fs::read_to_string(&self.path) {
            Ok(now) => Some(now),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if now != self.text {
            return Err(WriteRulesError::Changed);
        }
        if text.trim().is_empty() {
            return match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            };
        }
        let Some(name) = self.path.file_name() else {
            return Err(std::io::Error::other("the rules path has no file name").into());
        };
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(name);
        tmp_name.push(format!(".tmp-{}", std::process::id()));
        let tmp = self.path.with_file_name(tmp_name);
        let mut body = text.to_string();
        if !body.ends_with('\n') {
            body.push('\n');
        }
        let written = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, &self.path));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(written?)
    }

    /// The patterns in the rules: every line that is not blank or a comment,
    /// trimmed of surrounding white space.
    pub fn patterns(&self) -> Vec<&str> {
        self.text
            .as_deref()
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    }
}

/// How often [`add_folder_pattern`] and [`remove_folder_pattern`] read the
/// rules again when they changed between the read and the write.
const RULES_RETRIES: usize = 3;

/// Add `pattern` at the end of the own rules of the folder at `root`, keeping
/// every other line and comment. Says whether it added it: a pattern already
/// there is left alone.
pub fn add_folder_pattern(root: &Path, pattern: &str) -> std::io::Result<bool> {
    let pattern = pattern.trim();
    edit_folder_rules(root, |rules| {
        if rules.patterns().contains(&pattern) {
            return None;
        }
        let mut text = rules.text.clone().unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(pattern);
        text.push('\n');
        Some(text)
    })
}

/// Remove every line that is `pattern` from the own rules of the folder at
/// `root`. Says whether there was one.
pub fn remove_folder_pattern(root: &Path, pattern: &str) -> std::io::Result<bool> {
    let pattern = pattern.trim();
    edit_folder_rules(root, |rules| {
        if !rules.patterns().contains(&pattern) {
            return None;
        }
        let text = rules.text.as_deref().unwrap_or_default();
        Some(
            text.lines()
                .filter(|line| line.trim() != pattern)
                .map(|line| format!("{line}\n"))
                .collect(),
        )
    })
}

/// Read the folder's rules, let `edit` return their new text (or `None` to
/// leave them), and write it. Rules that changed in between, such as by sync
/// bringing in an edit from another computer, are read again and edited anew.
fn edit_folder_rules(
    root: &Path,
    edit: impl Fn(&FolderRules) -> Option<String>,
) -> std::io::Result<bool> {
    for _ in 0..RULES_RETRIES {
        let rules = FolderRules::read(root)?;
        let Some(text) = edit(&rules) else {
            return Ok(false);
        };
        match rules.write(&text) {
            Ok(()) => return Ok(true),
            Err(WriteRulesError::Changed) => continue,
            Err(WriteRulesError::Io(e)) => return Err(e),
        }
    }
    Err(std::io::Error::other(
        "the rules kept changing while they were being saved",
    ))
}

/// Where a rule that decided about a path came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleSource {
    /// The rules for every synced folder (`ignore_patterns`).
    AllFolders,
    /// The folder's own ignore file.
    ThisFolder,
}

/// What the rules say about one path, and which rule says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// No rule matches: the path is synced.
    Synced,
    /// Left out by `pattern`. `pattern` may name a folder the path is in.
    Excluded { pattern: String, source: RuleSource },
    /// A rule excluded it, and a later `!pattern` brought it back.
    Included { pattern: String, source: RuleSource },
}

/// Compiled ignore rules for one synced folder.
pub struct IgnoreRules {
    matcher: Option<Gitignore>,
}

impl IgnoreRules {
    /// Build the rules for a folder rooted at `root`: `globals` first, then any
    /// ignore file found at the root, so a folder-local rule can negate a
    /// global one with `!pattern`.
    ///
    /// Malformed patterns are skipped individually rather than failing the
    /// load — a typo in `.pdfsignore` must not take the folder's sync down.
    pub fn load(root: &Path, globals: &[String]) -> Self {
        let mut builder = GitignoreBuilder::new(root);
        let mut any = false;

        for pattern in globals {
            if builder.add_line(None, pattern).is_ok() {
                any = true;
            }
        }

        for name in IGNORE_FILE_NAMES {
            let path = root.join(name);
            if !path.is_file() {
                continue;
            }
            // `add` returns the first parse error but still takes the lines it
            // could read, so a bad line costs that line and nothing more.
            let _ = builder.add(&path);
            any = true;
            break;
        }

        if !any {
            return Self { matcher: None };
        }
        Self {
            matcher: builder.build().ok(),
        }
    }

    /// [`load`](Self::load) with `text` standing in for the folder's ignore
    /// file, for showing what rules being edited would do before they are
    /// saved.
    pub fn with_folder_text(root: &Path, globals: &[String], text: &str) -> Self {
        let mut builder = GitignoreBuilder::new(root);
        let mut any = false;
        for pattern in globals {
            any |= builder.add_line(None, pattern).is_ok();
        }
        let file = root.join(IGNORE_FILE_NAMES[0]);
        for line in text.lines() {
            any |= builder.add_line(Some(file.clone()), line).is_ok();
        }
        if !any {
            return Self { matcher: None };
        }
        Self {
            matcher: builder.build().ok(),
        }
    }

    /// Rules matching nothing, for callers with no folder context.
    pub fn empty() -> Self {
        Self { matcher: None }
    }

    /// Whether any rule is loaded. When false, [`is_ignored`](Self::is_ignored)
    /// is always false and callers can skip filtering entirely.
    pub fn is_empty(&self) -> bool {
        self.matcher.is_none()
    }

    /// Whether `rel` — a `/`-joined path relative to the folder root — is
    /// ignored.
    ///
    /// A path under an ignored directory is ignored too. `Gitignore` matches
    /// one path against one rule set and does not walk parents itself, so each
    /// ancestor is tested as a directory. This is what makes filtering the
    /// classification union sound: the engine can ask about any path in
    /// isolation, in any order, without having walked its parents first.
    pub fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        let Some(matcher) = &self.matcher else {
            return false;
        };
        if rel.is_empty() {
            return false;
        }

        let mut prefix = String::new();
        let mut parts = rel.split('/').peekable();
        while let Some(part) = parts.next() {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let last = parts.peek().is_none();
            // Every ancestor is a directory; only the final component's kind
            // is the caller's to say.
            let as_dir = if last { is_dir } else { true };
            if matcher.matched(&prefix, as_dir).is_ignore() {
                return true;
            }
        }
        false
    }
}

impl IgnoreRules {
    /// What the rules say about `rel`, and which rule decides, the way
    /// [`is_ignored`](Self::is_ignored) decides it.
    pub fn explain(&self, rel: &str, is_dir: bool) -> Verdict {
        let Some(matcher) = &self.matcher else {
            return Verdict::Synced;
        };
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            return Verdict::Synced;
        }
        let source = |glob: &ignore::gitignore::Glob| match glob.from() {
            Some(_) => RuleSource::ThisFolder,
            None => RuleSource::AllFolders,
        };
        let mut prefix = String::new();
        let mut parts = rel.split('/').peekable();
        while let Some(part) = parts.next() {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let last = parts.peek().is_none();
            let as_dir = if last { is_dir } else { true };
            match matcher.matched(&prefix, as_dir) {
                ignore::Match::Ignore(glob) => {
                    return Verdict::Excluded {
                        pattern: glob.original().to_string(),
                        source: source(glob),
                    };
                }
                ignore::Match::Whitelist(glob) if last => {
                    return Verdict::Included {
                        pattern: glob.original().to_string(),
                        source: source(glob),
                    };
                }
                _ => {}
            }
        }
        Verdict::Synced
    }
}

impl std::fmt::Debug for IgnoreRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IgnoreRules")
            .field("active", &!self.is_empty())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp directory removed on drop; avoids a dev-dependency, as in
    /// `pdfs_store::cache`'s tests.
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "pdfs-ignore-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn globals() -> Vec<String> {
        DEFAULT_IGNORE_PATTERNS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    fn write(root: &Path, name: &str, body: &str) {
        std::fs::write(root.join(name), body).unwrap();
    }

    #[test]
    fn the_added_defaults_leave_out_clutter_but_not_look_alikes() {
        let dir = TempDir::new();
        let rules = IgnoreRules::load(dir.path(), &globals());
        for (clutter, is_dir) in [
            ("desktop.ini", false),
            ("Photos/.directory", false),
            (".Trash-1000", true),
            (".Trashes", true),
            (".Spotlight-V100", true),
            (".fuse_hidden0000001a00000002", false),
            (".nfs000000000123abcd", false),
            ("movie.mkv.part", false),
            ("setup.exe.crdownload", false),
            ("notes.txt.kate-swp", false),
            (".~lock.report.odt#", false),
            ("~$budget.xlsx", false),
        ] {
            assert!(rules.is_ignored(clutter, is_dir), "{clutter}");
        }
        for kept in [
            "Desktop/notes.txt",
            "directory.txt",
            "Trash-talk.md",
            "nfs-setup.md",
            "parts.csv",
            "part.txt",
            "report.odt",
            "budget.xlsx",
        ] {
            assert!(!rules.is_ignored(kept, false), "{kept}");
        }
    }

    #[test]
    fn a_bad_pattern_is_reported_with_its_line() {
        assert!(check_pattern("*.db").is_ok());
        assert!(check_pattern("# a comment").is_ok());
        assert!(check_pattern("").is_ok());
        assert!(check_pattern("*.{jpg,png").is_err());
        assert!(check_pattern("[z-a]").is_err());
        let bad = check_rules("*.log\n{oops\nbuild/\n");
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].0, 2);
    }

    #[test]
    fn explain_names_the_rule_and_where_it_came_from() {
        let dir = TempDir::new();
        write(dir.path(), ".pdfsignore", "*.db\n!keep.tmp\n");
        let rules = IgnoreRules::load(dir.path(), &globals());
        assert_eq!(
            rules.explain("data/cache.db", false),
            Verdict::Excluded {
                pattern: "*.db".into(),
                source: RuleSource::ThisFolder
            }
        );
        assert_eq!(
            rules.explain("app/node_modules/x/index.js", false),
            Verdict::Excluded {
                pattern: "node_modules/".into(),
                source: RuleSource::AllFolders
            }
        );
        assert_eq!(
            rules.explain("keep.tmp", false),
            Verdict::Included {
                pattern: "!keep.tmp".into(),
                source: RuleSource::ThisFolder
            }
        );
        assert_eq!(rules.explain("src/main.rs", false), Verdict::Synced);
        // `explain` and `is_ignored` never disagree.
        for (path, is_dir) in [
            ("data/cache.db", false),
            ("keep.tmp", false),
            ("x.tmp", false),
        ] {
            assert_eq!(
                rules.is_ignored(path, is_dir),
                matches!(rules.explain(path, is_dir), Verdict::Excluded { .. }),
                "{path}"
            );
        }
    }

    #[test]
    fn rules_being_edited_explain_like_the_saved_ones() {
        let dir = TempDir::new();
        let text = "*.db\n!keep.db\n";
        write(dir.path(), ".pdfsignore", text);
        let saved = IgnoreRules::load(dir.path(), &globals());
        let edited = IgnoreRules::with_folder_text(dir.path(), &globals(), text);
        for path in ["a.db", "keep.db", "node_modules/x", "src/x.rs"] {
            assert_eq!(
                saved.explain(path, false),
                edited.explain(path, false),
                "{path}"
            );
        }
    }

    #[test]
    fn folder_rules_are_written_back_and_removed_when_blank() {
        let dir = TempDir::new();
        let rules = FolderRules::read(dir.path()).unwrap();
        assert_eq!(rules.text, None);
        assert_eq!(rules.path, dir.path().join(".pdfsignore"));
        rules.write("*.db").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".pdfsignore")).unwrap(),
            "*.db\n"
        );
        assert!(IgnoreRules::load(dir.path(), &[]).is_ignored("x.db", false));

        let rules = FolderRules::read(dir.path()).unwrap();
        rules.write("  \n").unwrap();
        assert!(!dir.path().join(".pdfsignore").exists());
    }

    #[test]
    fn folder_rules_keep_the_file_name_in_use() {
        let dir = TempDir::new();
        write(dir.path(), ".protonignore", "old/\n");
        let rules = FolderRules::read(dir.path()).unwrap();
        assert_eq!(rules.path, dir.path().join(".protonignore"));
        rules.write("new/\n").unwrap();
        assert!(!dir.path().join(".pdfsignore").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".protonignore")).unwrap(),
            "new/\n"
        );
    }

    #[test]
    fn folder_rules_changed_elsewhere_are_not_written_over() {
        let dir = TempDir::new();
        write(dir.path(), ".pdfsignore", "a/\n");
        let rules = FolderRules::read(dir.path()).unwrap();
        write(dir.path(), ".pdfsignore", "b/\n");
        assert!(matches!(rules.write("c/"), Err(WriteRulesError::Changed)));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".pdfsignore")).unwrap(),
            "b/\n"
        );

        // A file that appeared after an empty read counts as a change too.
        let dir = TempDir::new();
        let rules = FolderRules::read(dir.path()).unwrap();
        write(dir.path(), ".pdfsignore", "b/\n");
        assert!(matches!(rules.write("c/"), Err(WriteRulesError::Changed)));
    }

    #[test]
    fn a_pattern_added_to_a_folder_keeps_its_other_lines() {
        let dir = TempDir::new();
        assert!(add_folder_pattern(dir.path(), " *.db ").unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".pdfsignore")).unwrap(),
            "*.db\n"
        );

        write(dir.path(), ".pdfsignore", "# my notes\nbuild/");
        assert!(add_folder_pattern(dir.path(), "*.db").unwrap());
        assert!(!add_folder_pattern(dir.path(), "build/").unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".pdfsignore")).unwrap(),
            "# my notes\nbuild/\n*.db\n"
        );
        assert_eq!(
            FolderRules::read(dir.path()).unwrap().patterns(),
            ["build/", "*.db"]
        );
    }

    #[test]
    fn removing_a_folder_pattern_keeps_comments_and_drops_an_empty_file() {
        let dir = TempDir::new();
        write(dir.path(), ".pdfsignore", "# keep\nbuild/\n*.db\n");
        assert!(remove_folder_pattern(dir.path(), "build/").unwrap());
        assert!(!remove_folder_pattern(dir.path(), "build/").unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".pdfsignore")).unwrap(),
            "# keep\n*.db\n"
        );

        write(dir.path(), ".pdfsignore", "*.db\n");
        assert!(remove_folder_pattern(dir.path(), "*.db").unwrap());
        assert!(!dir.path().join(".pdfsignore").exists());
    }

    #[test]
    fn no_rules_at_all_ignores_nothing() {
        let dir = TempDir::new();
        let rules = IgnoreRules::load(dir.path(), &[]);
        assert!(rules.is_empty());
        assert!(!rules.is_ignored("anything", false));
        assert!(!rules.is_ignored("deep/nested/path.rs", false));
    }

    #[test]
    fn global_defaults_match_without_an_ignore_file() {
        let dir = TempDir::new();
        let rules = IgnoreRules::load(dir.path(), &globals());
        assert!(rules.is_ignored("node_modules", true));
        assert!(rules.is_ignored("target", true));
        assert!(rules.is_ignored("notes.txt~", false));
        assert!(!rules.is_ignored("src/main.rs", false));
    }

    #[test]
    fn a_path_under_an_ignored_directory_is_ignored() {
        let dir = TempDir::new();
        let rules = IgnoreRules::load(dir.path(), &globals());
        // The engine asks about deep paths in isolation, so each must answer
        // correctly without its parents having been tested first.
        assert!(rules.is_ignored("node_modules/left-pad/index.js", false));
        assert!(rules.is_ignored("node_modules/left-pad", true));
        assert!(rules.is_ignored("target/debug/build/x/out", false));
    }

    #[test]
    fn a_folder_ignore_file_adds_to_the_globals() {
        let dir = TempDir::new();
        write(dir.path(), ".pdfsignore", "secrets/\n*.log\n");
        let rules = IgnoreRules::load(dir.path(), &globals());
        assert!(rules.is_ignored("secrets/key.pem", false));
        assert!(rules.is_ignored("run.log", false));
        // Globals still apply alongside the file.
        assert!(rules.is_ignored("node_modules", true));
        assert!(!rules.is_ignored("run.txt", false));
    }

    #[test]
    fn a_folder_rule_can_negate_a_global_one() {
        let dir = TempDir::new();
        write(dir.path(), ".pdfsignore", "!keep.tmp\n");
        let rules = IgnoreRules::load(dir.path(), &globals());
        assert!(rules.is_ignored("scratch.tmp", false));
        assert!(!rules.is_ignored("keep.tmp", false));
    }

    #[test]
    fn protonignore_is_accepted_as_an_alias() {
        let dir = TempDir::new();
        write(dir.path(), ".protonignore", "build/\n");
        let rules = IgnoreRules::load(dir.path(), &[]);
        assert!(rules.is_ignored("build/out.o", false));
    }

    #[test]
    fn pdfsignore_wins_when_both_files_exist() {
        let dir = TempDir::new();
        write(dir.path(), ".pdfsignore", "from-pdfs/\n");
        write(dir.path(), ".protonignore", "from-proton/\n");
        let rules = IgnoreRules::load(dir.path(), &[]);
        assert!(rules.is_ignored("from-pdfs/x", false));
        assert!(!rules.is_ignored("from-proton/x", false));
    }

    #[test]
    fn a_malformed_pattern_does_not_take_the_rest_down() {
        let dir = TempDir::new();
        // An unclosed character class is not a valid glob.
        write(dir.path(), ".pdfsignore", "[unclosed\nvalid/\n");
        let rules = IgnoreRules::load(dir.path(), &globals());
        assert!(rules.is_ignored("valid/x", false));
        assert!(rules.is_ignored("node_modules", true));
    }

    #[test]
    fn the_root_itself_is_never_ignored() {
        let dir = TempDir::new();
        write(dir.path(), ".pdfsignore", "*\n");
        let rules = IgnoreRules::load(dir.path(), &[]);
        // A blanket rule must not classify the folder root, or the pass would
        // have nothing to reconcile against.
        assert!(!rules.is_ignored("", true));
    }

    #[test]
    fn transient_download_and_editor_names_are_recognised() {
        for name in [
            "Unconfirmed 540477.crdownload",
            "movie.mkv.part",
            "big.iso.partial",
            "photo.jpg.download",
            ".notes.txt.swp",
            "doc.txt~",
            ".~lock.report.odt#",
            "~$budget.xlsx",
            "FOO.CRDOWNLOAD",
        ] {
            assert!(is_transient_name(name), "{name} should be transient");
        }
    }

    #[test]
    fn save_by_rename_temporaries_are_scratch() {
        for name in [
            ".goutputstream-4XK2B2",
            ".org.chromium.Chromium.aBc123",
            "~$report.docx",
            ".~lock.budget.ods#",
            ".#notes.org",
            "#notes.org#",
            "cache.tmp",
            "page.html.crdownload",
        ] {
            assert!(is_scratch_name(name), "{name}");
        }
        for name in ["report.docx", "notes.org", "#1 hits.txt", ".bashrc"] {
            assert!(!is_scratch_name(name), "{name}");
        }
    }

    #[test]
    fn finished_names_are_not_transient() {
        for name in [
            "teamspeak.tar.gz",
            "nils.zip",
            "report.xml",
            "archive.partly.zip", // ".partly" is not ".part"
            "template.dotx",
            "readme",
            // A file that is *named* `.tmp` and never renamed: parking it would
            // keep it off Drive for good.
            "index.tmp",
            "notes.TEMP",
        ] {
            assert!(!is_transient_name(name), "{name} should not be transient");
        }
    }
}
