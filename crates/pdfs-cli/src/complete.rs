//! Shell completion for `pdfs`.
//!
//! Built on clap_complete's dynamic engine: the script a shell loads is a thin
//! stub that calls `pdfs` back on every Tab with the words typed so far, and
//! `pdfs` answers from its own clap definition. That lets an argument complete
//! from the running service as well as from the command tree — `pdfs sync rm
//! <Tab>` offers the synced-folder ids with their paths, `pdfs pin Doc<Tab>`
//! walks My files.
//!
//! Every value that comes from the service is best effort. With no service, a
//! slow one or an error, a completer offers nothing: a Tab that hangs or prints
//! an error into the prompt is worse than one that does nothing.
//!
//! The engine sits behind clap_complete's `unstable-dynamic` feature, so all of
//! this crate's use of it is kept to this module.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::CommandFactory;
use clap_complete::engine::{CompletionCandidate, PathCompleter, ValueCompleter};
use clap_complete::env::{Bash, CompleteEnv, Elvish, EnvCompleter, Fish, Powershell, Zsh};
use pdfs_core::control::{
    AlbumInfo, BookmarkInfo, ConflictInfo, DeviceInfo, DirEntry, InvitationInfo, PendingOpInfo,
    PublicLinkInfo, Request, Response, RevisionInfo, ShareEntry, ShareEntryKind, SyncFolderInfo,
};

/// The environment variable that turns a run of `pdfs` into a completion
/// request. Not clap's generic `COMPLETE`: the same binary is the systemd
/// daemon, and an unrelated `COMPLETE` in its environment must not stop it.
pub const VAR: &str = "PDFS_COMPLETE";

/// How long a completer waits for the service. A Tab is interactive, so a
/// listing the service cannot give soon is better skipped. Long enough for the
/// listings that ask Proton (computers take over a second).
const SERVICE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a listing that asks Proton is reused. Every Tab is a new `pdfs`, so
/// without it `pdfs devices rm <Tab>`, a letter and another Tab would wait for
/// Proton twice. A uid that went away in the meantime only fails the command.
const CACHE_TTL: Duration = Duration::from_secs(30);

/// Answer a completion request and exit, if this run is one. Must run before
/// anything writes to stdout.
pub fn handle_request() {
    CompleteEnv::with_factory(crate::Cli::command)
        .var(VAR)
        .complete();
}

/// The shells `pdfs completions` writes a script for.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum ShellArg {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
}

impl ShellArg {
    fn completer(self) -> &'static dyn EnvCompleter {
        match self {
            ShellArg::Bash => &Bash,
            ShellArg::Zsh => &Zsh,
            ShellArg::Fish => &Fish,
            ShellArg::Elvish => &Elvish,
            ShellArg::Powershell => &Powershell,
        }
    }
}

/// Write the script that registers `pdfs` completion with `shell`.
///
/// The script calls `pdfs` from `PATH`, not this binary's path, so one written
/// at packaging time keeps working once installed.
pub fn write_script(shell: ShellArg, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    shell
        .completer()
        .write_registration(VAR, "pdfs", "pdfs", "pdfs", out)?;
    if let ShellArg::Zsh = shell {
        // Installed as `_pdfs` in site-functions, the script is autoloaded as
        // that function's body on the first Tab. The body above only defines
        // and registers the completer, so this line also runs it, or the first
        // Tab would complete nothing. Sourced, `funcstack` names no `_pdfs`.
        writeln!(
            out,
            "if [ \"$funcstack[1]\" = \"_pdfs\" ]; then\n    _clap_dynamic_completer_pdfs \"$@\"\nfi"
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Talking to the service

/// One request to the service, or `None` if there is no answer worth showing.
fn ask(request: Request) -> Option<Response> {
    let socket = pdfs_core::config::AppDirs::new().ok()?.control_socket();
    ask_at(&socket, &request)
}

fn ask_at(socket: &Path, request: &Request) -> Option<Response> {
    match pdfs_core::control::send_with_timeout(socket, request, SERVICE_TIMEOUT) {
        Ok(Response::Error { .. }) | Err(_) => None,
        Ok(response) => Some(response),
    }
}

/// [`ask`] for a listing that comes from Proton rather than the local
/// database, reusing an answer younger than [`CACHE_TTL`]. `key` names the
/// listing's cache file.
fn ask_cached(key: &str, request: Request) -> Option<Response> {
    let file = cache_file(key);
    if let Some(hit) = file.as_deref().and_then(read_fresh) {
        return Some(hit);
    }
    let response = ask(request)?;
    if let Some(file) = &file {
        write_cache(file, &response);
    }
    Some(response)
}

/// In `$XDG_RUNTIME_DIR`: private to the user, in memory and gone at logout,
/// which suits names from the account. With no runtime dir, nothing is cached.
fn cache_file(key: &str) -> Option<PathBuf> {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    Some(
        runtime
            .join("proton-drive-linux")
            .join(format!("completion-{key}.json")),
    )
}

fn read_fresh(file: &Path) -> Option<Response> {
    let age = std::fs::metadata(file)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()?;
    if age > CACHE_TTL {
        return None;
    }
    serde_json::from_slice(&std::fs::read(file).ok()?).ok()
}

/// Best effort: a cache that cannot be written only costs the next Tab a wait.
fn write_cache(file: &Path, response: &Response) {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let (Some(dir), Ok(json)) = (file.parent(), serde_json::to_vec(response)) else {
        return;
    };
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir);
    let staging = file.with_extension(format!("{}.tmp", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &json));
    if written.is_err() || std::fs::rename(&staging, file).is_err() {
        let _ = std::fs::remove_file(&staging);
    }
}

fn candidate(value: impl Into<OsString>, help: impl Into<String>) -> CompletionCandidate {
    let help = help.into();
    let candidate = CompletionCandidate::new(value);
    if help.is_empty() {
        candidate
    } else {
        candidate.help(Some(help.into()))
    }
}

// ---------------------------------------------------------------------------
// Paths in Proton Drive

/// A path in the mount: an absolute path completes from the local filesystem
/// (through the mount), anything else from the service's listing of My files,
/// which is what the commands resolve a relative path against.
pub fn drive_path(current: &OsStr) -> Vec<CompletionCandidate> {
    complete_drive(current, false)
}

/// [`drive_path`] for an argument that must be a folder.
pub fn drive_dir(current: &OsStr) -> Vec<CompletionCandidate> {
    complete_drive(current, true)
}

/// `pdfs refresh`: a folder, or one of the two named caches.
pub fn refresh_target(current: &OsStr) -> Vec<CompletionCandidate> {
    let typed = current.to_string_lossy();
    let mut candidates: Vec<_> = [
        ("trash", "the trash listing"),
        ("photos", "the photos timeline"),
    ]
    .into_iter()
    .filter(|(name, _)| name.starts_with(typed.as_ref()))
    .map(|(name, help)| candidate(name, help))
    .collect();
    candidates.extend(drive_dir(current));
    candidates
}

fn complete_drive(current: &OsStr, dirs_only: bool) -> Vec<CompletionCandidate> {
    let Some(word) = current.to_str() else {
        return Vec::new();
    };
    if word.starts_with('/') || word.starts_with('~') {
        let local = if dirs_only {
            PathCompleter::dir()
        } else {
            PathCompleter::any()
        };
        return local.complete(current);
    }
    let (parent, prefix) = split_drive_word(word);
    let listing = Request::ListDir {
        path: parent.trim_end_matches('/').to_owned(),
    };
    match ask(listing) {
        Some(Response::Entries { entries }) => {
            drive_candidates(parent, prefix, &entries, dirs_only)
        }
        _ => Vec::new(),
    }
}

/// Split a typed drive path into the folder to list (with its trailing `/`)
/// and the start of the name being completed in it.
fn split_drive_word(word: &str) -> (&str, &str) {
    match word.rfind('/') {
        Some(at) => word.split_at(at + 1),
        None => ("", word),
    }
}

/// The entries of `parent` whose name starts with `prefix`. A folder ends in
/// `/`, which tells every shell not to add a space after it, so the next Tab
/// goes on into it.
fn drive_candidates(
    parent: &str,
    prefix: &str,
    entries: &[DirEntry],
    dirs_only: bool,
) -> Vec<CompletionCandidate> {
    entries
        .iter()
        .filter(|e| e.name.starts_with(prefix) && (e.is_dir || !dirs_only))
        .map(|e| {
            let slash = if e.is_dir { "/" } else { "" };
            CompletionCandidate::new(format!("{parent}{}{slash}", e.name))
                .hide(e.name.starts_with('.') && !prefix.starts_with('.'))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Ids and uids from the service's listings

pub fn sync_folder_id() -> Vec<CompletionCandidate> {
    match ask(Request::ListSyncFolders) {
        Some(Response::SyncFolders { items }) => sync_folder_candidates(&items),
        _ => Vec::new(),
    }
}

fn sync_folder_candidates(items: &[SyncFolderInfo]) -> Vec<CompletionCandidate> {
    items
        .iter()
        .map(|f| candidate(f.id.to_string(), format!("{} ({})", f.local_path, f.mode)))
        .collect()
}

pub fn queue_id() -> Vec<CompletionCandidate> {
    match ask(Request::ListPendingOps) {
        Some(Response::PendingOps { items }) => queue_candidates(&items),
        _ => Vec::new(),
    }
}

fn queue_candidates(items: &[PendingOpInfo]) -> Vec<CompletionCandidate> {
    items
        .iter()
        .map(|op| candidate(op.id.to_string(), format!("{} {}", op.kind, op.path)))
        .collect()
}

pub fn device_uid() -> Vec<CompletionCandidate> {
    match ask_cached("devices", Request::ListDevices) {
        Some(Response::Devices { items }) => device_candidates(&items),
        _ => Vec::new(),
    }
}

fn device_candidates(items: &[DeviceInfo]) -> Vec<CompletionCandidate> {
    items
        .iter()
        .map(|d| {
            let this = if d.this_device {
                " (this computer)"
            } else {
                ""
            };
            candidate(d.uid.as_str(), format!("{}{this}", d.name))
        })
        .collect()
}

pub fn trash_uid() -> Vec<CompletionCandidate> {
    match ask_cached("trash", Request::ListTrash) {
        Some(Response::Entries { entries }) => trash_candidates(&entries),
        _ => Vec::new(),
    }
}

fn trash_candidates(entries: &[DirEntry]) -> Vec<CompletionCandidate> {
    entries
        .iter()
        .map(|e| {
            let help = match e.trashed_from.as_deref() {
                Some(from) if !from.is_empty() => format!("{} (from {from})", e.name),
                _ => e.name.clone(),
            };
            candidate(e.uid.as_str(), help)
        })
        .collect()
}

pub fn album_uid() -> Vec<CompletionCandidate> {
    match ask_cached("albums", Request::PhotoAlbums) {
        Some(Response::Albums { items, .. }) => album_candidates(&items),
        _ => Vec::new(),
    }
}

fn album_candidates(items: &[AlbumInfo]) -> Vec<CompletionCandidate> {
    items
        .iter()
        .map(|a| candidate(a.uid.as_str(), a.name.as_str()))
        .collect()
}

pub fn shared_uid() -> Vec<CompletionCandidate> {
    match ask_cached("shared-with-me", Request::ListSharedWithMe) {
        Some(Response::Entries { entries }) => shared_candidates(&entries),
        _ => Vec::new(),
    }
}

fn shared_candidates(entries: &[DirEntry]) -> Vec<CompletionCandidate> {
    entries
        .iter()
        .map(|e| {
            let help = if e.shared_by.is_empty() {
                e.name.clone()
            } else {
                format!("{} (from {})", e.name, e.shared_by)
            };
            candidate(e.uid.as_str(), help)
        })
        .collect()
}

pub fn invitation_id() -> Vec<CompletionCandidate> {
    match ask_cached("invitations", Request::ListInvitations) {
        Some(Response::Invitations { items }) => invitation_candidates(&items),
        _ => Vec::new(),
    }
}

fn invitation_candidates(items: &[InvitationInfo]) -> Vec<CompletionCandidate> {
    items
        .iter()
        .map(|i| {
            let help = match i.name.as_deref() {
                Some(name) => format!("{name} from {}", i.inviter_email),
                None => format!("from {}", i.inviter_email),
            };
            candidate(i.id.as_str(), help)
        })
        .collect()
}

pub fn bookmark_token() -> Vec<CompletionCandidate> {
    match ask_cached("bookmarks", Request::ListBookmarks) {
        Some(Response::Bookmarks { items }) => bookmark_candidates(&items),
        _ => Vec::new(),
    }
}

fn bookmark_candidates(items: &[BookmarkInfo]) -> Vec<CompletionCandidate> {
    items
        .iter()
        .map(|b| candidate(b.token.as_str(), b.name.as_deref().unwrap_or(&b.url)))
        .collect()
}

pub fn conflict_path() -> Vec<CompletionCandidate> {
    match ask(Request::ListConflicts) {
        Some(Response::Conflicts { items }) => conflict_candidates(&items),
        _ => Vec::new(),
    }
}

fn conflict_candidates(items: &[ConflictInfo]) -> Vec<CompletionCandidate> {
    items
        .iter()
        .map(|c| candidate(c.path.as_str(), format!("copy of {}", c.original_path)))
        .collect()
}

// ---------------------------------------------------------------------------
// Ids that belong to the path typed before them

/// A version of the file named earlier on the line (`pdfs versions restore
/// <path> <Tab>`).
pub fn version_id(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(path) = typed_path() else {
        return Vec::new();
    };
    match ask(Request::ListRevisions { path }) {
        Some(Response::Revisions { items }) => starting_with(current, revision_candidates(&items)),
        _ => Vec::new(),
    }
}

fn revision_candidates(items: &[RevisionInfo]) -> Vec<CompletionCandidate> {
    items
        .iter()
        .map(|r| {
            let current = if r.is_active { ", current" } else { "" };
            let size = r.claimed_size.unwrap_or(r.size_on_storage).max(0);
            candidate(
                r.id.as_str(),
                format!(
                    "{}, {}{current}",
                    crate::format_epoch_utc(r.created),
                    crate::human_bytes(size as u64)
                ),
            )
        })
        .collect()
}

/// A member or invitation on the share of the path typed earlier on the line.
pub fn share_entry_id(current: &OsStr) -> Vec<CompletionCandidate> {
    match share_of_typed_path() {
        Some((entries, _)) => starting_with(current, share_entry_candidates(&entries)),
        None => Vec::new(),
    }
}

fn share_entry_candidates(entries: &[ShareEntry]) -> Vec<CompletionCandidate> {
    entries
        .iter()
        .map(|e| {
            // The kind is the next argument, so the help names it in the words
            // that argument takes.
            let kind = match e.kind {
                ShareEntryKind::Member => "member",
                ShareEntryKind::ProtonInvite => "proton",
                ShareEntryKind::ExternalInvite => "external",
            };
            candidate(e.id.as_str(), format!("{} ({}, {kind})", e.email, e.role))
        })
        .collect()
}

/// The public link of the path typed earlier on the line.
pub fn public_link_id(current: &OsStr) -> Vec<CompletionCandidate> {
    match share_of_typed_path() {
        Some((_, Some(link))) => starting_with(current, vec![public_link_candidate(&link)]),
        _ => Vec::new(),
    }
}

/// Described without its URL: the fragment is the link's password, and a Tab
/// menu is on screen for anyone watching.
fn public_link_candidate(link: &PublicLinkInfo) -> CompletionCandidate {
    let help = match link.expires {
        Some(expires) => format!(
            "{} link, expires {}",
            link.role,
            crate::format_epoch_utc(expires)
        ),
        None => format!("{} link", link.role),
    };
    candidate(link.id.as_str(), help)
}

fn share_of_typed_path() -> Option<(Vec<ShareEntry>, Option<PublicLinkInfo>)> {
    match ask(Request::ListShare {
        path: typed_path()?,
    })? {
        Response::Share { entries, link } => Some((entries, link)),
        _ => None,
    }
}

/// [`clap_complete::engine::ArgValueCompleter`] candidates are not filtered by
/// the engine, unlike [`clap_complete::engine::ArgValueCandidates`].
fn starting_with(
    current: &OsStr,
    candidates: Vec<CompletionCandidate>,
) -> Vec<CompletionCandidate> {
    let typed = current.to_string_lossy();
    candidates
        .into_iter()
        .filter(|c| c.get_value().to_string_lossy().starts_with(typed.as_ref()))
        .collect()
}

/// The `path` argument of the command being completed, as the service takes
/// it. A completer only receives the word under the cursor, so this reads the
/// line from the process arguments, where the shell's stub passes it.
fn typed_path() -> Option<String> {
    let args: Vec<OsString> = std::env::args_os().collect();
    let index = std::env::var("_CLAP_COMPLETE_INDEX")
        .ok()
        .and_then(|i| i.parse().ok());
    path_in(words_before_cursor(&args, index))
}

/// The words of the line before the one being completed. The stub runs
/// `pdfs -- <words>`; bash, zsh and elvish say which word the cursor is on,
/// fish and PowerShell pass the line cut at the cursor, so it is the last.
fn words_before_cursor(args: &[OsString], index: Option<usize>) -> Vec<OsString> {
    let start = args
        .iter()
        .position(|a| a == "--")
        .map_or(args.len(), |at| at + 1);
    let mut words = args[start..].to_vec();
    let index = index.unwrap_or(words.len().saturating_sub(1));
    words.truncate(index);
    words
}

/// Parse `words` as far as they go and take the innermost command's `path`.
fn path_in(words: Vec<OsString>) -> Option<String> {
    let matches = crate::Cli::command()
        .ignore_errors(true)
        .try_get_matches_from(words)
        .ok()?;
    let mut leaf = &matches;
    while let Some((_, sub)) = leaf.subcommand() {
        leaf = sub;
    }
    let path = leaf.try_get_one::<PathBuf>("path").ok().flatten()?;
    crate::path_arg(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(candidates: &[CompletionCandidate]) -> Vec<String> {
        candidates
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect()
    }

    fn help(candidate: &CompletionCandidate) -> String {
        candidate
            .get_help()
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    fn entry(name: &str, is_dir: bool) -> DirEntry {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "is_dir": is_dir,
            "size": 0,
            "modified": 0,
            "pinned": false,
            "uid": format!("vol~{name}"),
        }))
        .unwrap()
    }

    fn words(line: &[&str]) -> Vec<OsString> {
        line.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_command_definition_is_valid() {
        crate::Cli::command().debug_assert();
    }

    #[test]
    fn completions_script_is_written_for_every_shell() {
        for shell in [
            ShellArg::Bash,
            ShellArg::Zsh,
            ShellArg::Fish,
            ShellArg::Elvish,
            ShellArg::Powershell,
        ] {
            let mut script = Vec::new();
            write_script(shell, &mut script).unwrap();
            let script = String::from_utf8(script).unwrap();
            assert!(script.contains(VAR), "{script}");
            assert!(script.contains("pdfs"), "{script}");
        }
    }

    #[test]
    fn drive_path_splits_parent_and_prefix() {
        assert_eq!(split_drive_word(""), ("", ""));
        assert_eq!(split_drive_word("Doc"), ("", "Doc"));
        assert_eq!(split_drive_word("Documents/"), ("Documents/", ""));
        assert_eq!(split_drive_word("Documents/Rep"), ("Documents/", "Rep"));
        assert_eq!(split_drive_word("a/b/c"), ("a/b/", "c"));
    }

    #[test]
    fn drive_dirs_end_in_a_slash() {
        let entries = [
            entry("Reports", true),
            entry("Receipt.pdf", false),
            entry("Other", true),
        ];
        let all = drive_candidates("Documents/", "Re", &entries, false);
        assert_eq!(
            values(&all),
            ["Documents/Reports/", "Documents/Receipt.pdf"]
        );
        let dirs = drive_candidates("Documents/", "", &entries, true);
        assert_eq!(values(&dirs), ["Documents/Reports/", "Documents/Other/"]);
    }

    #[test]
    fn hidden_drive_entries_need_a_typed_dot() {
        let entries = [entry(".git", true)];
        assert!(drive_candidates("", "", &entries, false)[0].is_hide_set());
        assert!(!drive_candidates("", ".", &entries, false)[0].is_hide_set());
    }

    #[test]
    fn sync_folder_ids_complete_with_their_local_path() {
        let folders: Vec<SyncFolderInfo> = serde_json::from_value(serde_json::json!([{
            "id": 3,
            "local_path": "/home/me/Documents",
            "remote_uid": "vol~docs",
            "mode": "mirror",
            "state": "idle",
            "last_sync": 0,
        }]))
        .unwrap();
        let candidates = sync_folder_candidates(&folders);
        assert_eq!(values(&candidates), ["3"]);
        assert_eq!(help(&candidates[0]), "/home/me/Documents (mirror)");
    }

    #[test]
    fn trash_uids_complete_with_their_name() {
        let mut from_docs = entry("a.pdf", false);
        from_docs.trashed_from = Some("Documents".into());
        let candidates = trash_candidates(&[from_docs, entry("b", true)]);
        assert_eq!(values(&candidates), ["vol~a.pdf", "vol~b"]);
        assert_eq!(help(&candidates[0]), "a.pdf (from Documents)");
        assert_eq!(help(&candidates[1]), "b");
    }

    #[test]
    fn completer_without_a_service_offers_nothing() {
        let dir = std::env::temp_dir().join(format!("pdfs-complete-{}", std::process::id()));
        let socket = dir.join("control.sock");
        assert!(ask_at(&socket, &Request::ListSyncFolders).is_none());
    }

    #[test]
    fn cached_listing_is_reused_until_it_is_stale() {
        let dir = std::env::temp_dir().join(format!("pdfs-complete-cache-{}", std::process::id()));
        let file = dir.join("completion-devices.json");
        write_cache(&file, &Response::Devices { items: Vec::new() });
        assert!(matches!(read_fresh(&file), Some(Response::Devices { .. })));
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&file).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);

        let stale = std::time::SystemTime::now() - CACHE_TTL - Duration::from_secs(1);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(stale)
            .unwrap();
        assert!(read_fresh(&file).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn words_before_cursor_stop_at_the_cursor() {
        let args = words(&[
            "pdfs", "--", "pdfs", "versions", "restore", "a.txt", "", "x",
        ]);
        assert_eq!(
            words_before_cursor(&args, Some(4)),
            words(&["pdfs", "versions", "restore", "a.txt"])
        );
        // fish: the line arrives cut at the cursor, the current word last.
        let fish = words(&["pdfs", "--", "pdfs", "versions", "restore", "a.txt", "ab"]);
        assert_eq!(
            words_before_cursor(&fish, None),
            words(&["pdfs", "versions", "restore", "a.txt"])
        );
    }

    #[test]
    fn completing_words_finds_the_path_before_the_id() {
        let line = words(&["pdfs", "versions", "restore", "Documents/a.txt"]);
        assert_eq!(path_in(line).as_deref(), Some("Documents/a.txt"));
        let line = words(&["pdfs", "--json", "share-role", "Shared/x"]);
        assert_eq!(path_in(line).as_deref(), Some("Shared/x"));
        let line = words(&["pdfs", "public-link", "update", "--role", "editor", "a"]);
        assert_eq!(path_in(line).as_deref(), Some("a"));
        assert_eq!(path_in(words(&["pdfs", "versions", "restore"])), None);
    }
}
