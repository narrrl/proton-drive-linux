//! Selective-sync ignore rules for mirror folders (features.md 1.1).
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
];

/// Filenames accepted as a folder's ignore file, in precedence order.
const IGNORE_FILE_NAMES: &[&str] = &[".pdfsignore", ".protonignore"];

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
    /// [`crate::cache`]'s tests.
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
}
