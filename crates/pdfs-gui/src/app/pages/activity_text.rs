//! Activity entries as the feed words them.
//!
//! The daemon logs each entry in plain English for its own log and the CLI:
//! a target ("report.pdf", "3 item(s)", "sync conflict in Docs") and a detail
//! ("to /Documents", "was notes.txt", "2 uploaded, 1 deleted"). This module
//! reads the shapes the daemon writes and says them again in the user's
//! language. A shape it does not know, which is mostly an error message,
//! shows as the daemon wrote it.

use crate::*;

/// What a feed row says about one entry.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Described {
    pub(crate) title: String,
    /// Subtitle parts, shown joined by " · ".
    pub(crate) details: Vec<String>,
}

/// The title and subtitle parts for `entry`.
pub(crate) fn describe(entry: &ActivityEntry) -> Described {
    let title = entry_title(entry);
    let mut details = Vec::new();
    if entry.kind == ActivityKind::Conflict
        && let Some(stamp) = conflict_stamp(&entry.target)
    {
        let when = dates::relative(stamp);
        if !when.is_empty() {
            // Translators: {time} is when a conflicting copy was made, such as
            // "Sep 21" or "5 min ago".
            details.push(gettext_f("Made {time}", &[("time", &when)]));
        }
    }
    if let Some(detail) = detail_label(entry) {
        details.insert(0, detail);
    }
    Described { title, details }
}

/// The row title: what happened to what, one sentence per case so a
/// translator can place the name where their language needs it.
fn entry_title(entry: &ActivityEntry) -> String {
    use ActivityKind::*;
    let target = entry.target.as_str();
    let ok = entry.ok;

    // Entries whose target is a count or a fixed phrase rather than a name.
    match (entry.kind, target) {
        (Upload, "Google Photos import") => {
            return if ok {
                gettext("Imported from Google Photos")
            } else {
                gettext("Couldn't import from Google Photos")
            };
        }
        (Upload, "bulk upload") => return gettext("Couldn't upload files"),
        (Upload, "add sync folder") => return gettext("Couldn't add a sync folder"),
        (Download, "restore folders") => return gettext("Couldn't restore folders"),
        (Unshare, "shared item") => return gettext("Left a shared item"),
        _ => {}
    }
    if matches!(entry.kind, Restore | DeleteForever | EmptyTrash)
        && let Some(n) = leading_count(target, &["item(s)", "item", "items"])
    {
        return match entry.kind {
            Restore => ngettext_f("Restored {n} item", "Restored {n} items", n, &[]),
            DeleteForever => ngettext_f(
                "Deleted {n} item forever",
                "Deleted {n} items forever",
                n,
                &[],
            ),
            _ => ngettext_f(
                "Emptied the Trash ({n} item)",
                "Emptied the Trash ({n} items)",
                n,
                &[],
            ),
        };
    }
    if entry.kind == Download
        && let Some(rest) = target.strip_prefix("restored ")
        && let Some(n) = leading_count(rest, &["folder(s)"])
    {
        return ngettext_f("Restored {n} folder", "Restored {n} folders", n, &[]);
    }
    if entry.kind == Sync
        && let Some(title) = sync_title(target)
    {
        return title;
    }

    let Some(name) = target_name(target) else {
        return match entry.kind {
            Share => gettext("Shared an item"),
            PublicLink => gettext("Created a link to an item"),
            Unshare if entry.detail == "link removed" => gettext("Removed a link to an item"),
            Unshare => gettext("Stopped sharing an item"),
            _ => activity_title(entry.kind, ok, target),
        };
    };
    if entry.kind == Unshare && entry.detail == "link removed" {
        // Translators: {name} is a file or folder name.
        return gettext_f("Removed the link to {name}", &[("name", &name)]);
    }
    if entry.kind == Conflict
        && let Some(base) = conflict_base(&name)
    {
        // Translators: {name} is the file the conflicting copy was made from.
        return gettext_f("Conflicting copy of {name}", &[("name", &base)]);
    }
    activity_title(entry.kind, ok, &name)
}

/// The title of a sync pass or mode switch, whose target is a phrase.
fn sync_title(target: &str) -> Option<String> {
    if let Some(name) = target.strip_prefix("sync conflict in ") {
        // Translators: {name} is a synced folder's name.
        return Some(gettext_f(
            "Conflicting changes in {name}",
            &[("name", name)],
        ));
    }
    if let Some(name) = target.strip_prefix("sync incomplete for ") {
        // Translators: {name} is a synced folder's name.
        return Some(gettext_f(
            "Some items in {name} didn't sync",
            &[("name", name)],
        ));
    }
    if let Some(name) = target.strip_prefix("sync failed for ") {
        // Translators: {name} is a synced folder's name.
        return Some(gettext_f("Couldn't sync {name}", &[("name", name)]));
    }
    if let Some(name) = target.strip_prefix("couldn't switch ") {
        // Translators: {name} is a synced folder's name.
        return Some(gettext_f(
            "Couldn't change how {name} syncs",
            &[("name", name)],
        ));
    }
    if let Some(path) = target.strip_suffix(" is now on-demand") {
        let name = file_name(path);
        // Translators: {name} is a folder's name.
        return Some(gettext_f("{name} is now online only", &[("name", name)]));
    }
    if let Some(path) = target.strip_suffix(" is mirroring again; downloading") {
        let name = file_name(path);
        // Translators: {name} is a folder's name.
        return Some(gettext_f("{name} is synced again", &[("name", name)]));
    }
    if target.starts_with("already ") {
        return Some(gettext("Sync mode was already set"));
    }
    None
}

/// The row title for an activity on a named item.
pub(crate) fn activity_title(kind: ActivityKind, ok: bool, name: &str) -> String {
    let args = [("name", name)];
    match (kind, ok) {
        // Translators: {name} is a file or folder name.
        (ActivityKind::Upload, true) => gettext_f("Uploaded {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Upload, false) => gettext_f("Couldn't upload {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Download, true) => gettext_f("Downloaded {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Download, false) => gettext_f("Couldn't download {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Sync, true) => gettext_f("Synced {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Sync, false) => gettext_f("Couldn't sync {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Rename, true) => gettext_f("Renamed {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Rename, false) => gettext_f("Couldn't rename {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Move, true) => gettext_f("Moved {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Move, false) => gettext_f("Couldn't move {name}", &args),
        // Translators: {name} is a folder name.
        (ActivityKind::CreateFolder, true) => gettext_f("Created folder {name}", &args),
        // Translators: {name} is a folder name.
        (ActivityKind::CreateFolder, false) => gettext_f("Couldn't create folder {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Trash, true) => gettext_f("Moved {name} to the Trash", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Trash, false) => gettext_f("Couldn't move {name} to the Trash", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Restore, true) => gettext_f("Restored {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Restore, false) => gettext_f("Couldn't restore {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::DeleteForever, true) => gettext_f("Deleted {name} forever", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::DeleteForever, false) => gettext_f("Couldn't delete {name}", &args),
        (ActivityKind::EmptyTrash, true) => gettext("Emptied the Trash"),
        (ActivityKind::EmptyTrash, false) => gettext("Couldn't empty the Trash"),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Share, true) => gettext_f("Shared {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Share, false) => gettext_f("Couldn't share {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::PublicLink, true) => gettext_f("Created a link to {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::PublicLink, false) => gettext_f("Couldn't create a link to {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Unshare, true) => gettext_f("Stopped sharing {name}", &args),
        // Translators: {name} is a file or folder name.
        (ActivityKind::Unshare, false) => gettext_f("Couldn't stop sharing {name}", &args),
        // Translators: {name} is the name of the conflicting copy.
        (ActivityKind::Conflict, _) => gettext_f("Conflicting copy {name}", &args),
    }
}

/// The subtitle's first part: the daemon's detail in the user's language, or
/// `None` when it only repeats the title.
fn detail_label(entry: &ActivityEntry) -> Option<String> {
    let detail = entry.detail.trim();
    let name = |s: &str| file_name(s).to_string();
    match detail {
        ""
        | "uploaded"
        | "created"
        | "trashed"
        | "trashed from the mount"
        | "stopped sharing"
        | "left"
        | "link created"
        | "link removed"
        | "access removed"
        | "removed" => {
            return None;
        }
        "removed on Drive" => return Some(gettext("Removed on Proton Drive")),
        "removed locally" => return Some(gettext("Removed on this computer")),
        "replaced by a rename from the mount" => {
            return Some(gettext("Replaced by a renamed file"));
        }
        "destination folder no longer exists; the file was left in place" => {
            return Some(gettext(
                "The destination folder no longer exists, so the file stayed where it was",
            ));
        }
        _ => {}
    }
    if entry.kind == ActivityKind::Upload
        && entry.target == "Google Photos import"
        && entry.ok
        && let Some(summary) = import_summary(detail)
    {
        return Some(summary);
    }
    if entry.kind == ActivityKind::Sync
        && let Some(summary) = sync_summary(detail)
    {
        return Some(summary);
    }
    if let Some(dest) = detail.strip_prefix("to ") {
        // Translators: {name} is the folder an item was moved into.
        return Some(gettext_f("To {name}", &[("name", &folder_name(dest))]));
    }
    if let Some(old) = detail.strip_prefix("was ") {
        // Translators: {name} is an item's previous name.
        return Some(gettext_f("Previously {name}", &[("name", &name(old))]));
    }
    if detail.starts_with("restored version ") {
        return Some(gettext("Restored an earlier version"));
    }
    if detail.starts_with("deleted version ") {
        return Some(gettext("Deleted an earlier version"));
    }
    if let Some(rest) = detail.strip_prefix("saved version ")
        && let Some((_, path)) = rest.split_once(" to ")
    {
        // Translators: {path} is where an earlier version was saved.
        return Some(gettext_f(
            "Saved an earlier version to {path}",
            &[("path", &tilde_path(path))],
        ));
    }
    if let Some(copy) = detail.strip_prefix("kept both; the copy is now ") {
        // Translators: {name} is the new name of the conflicting copy.
        return Some(gettext_f(
            "Kept both; the copy is now {name}",
            &[("name", copy)],
        ));
    }
    if let Some(base) = detail
        .strip_prefix("kept ")
        .and_then(|rest| rest.strip_suffix("; the copy is in Trash"))
    {
        // Translators: {name} is the file that was kept.
        return Some(gettext_f(
            "Kept {name}; the copy is in the Trash",
            &[("name", base)],
        ));
    }
    if detail.starts_with("kept the conflict copy ") {
        return Some(gettext("Kept the conflicting copy"));
    }
    if let Some(base) = detail.strip_prefix("kept ") {
        // Translators: {name} is the file that was kept.
        return Some(gettext_f("Kept {name}", &[("name", base)]));
    }
    if let Some(base) = detail.strip_prefix("auto-removed duplicate of ") {
        // Translators: {name} is the file the removed copy was identical to.
        return Some(gettext_f(
            "Same as {name}, so it was removed",
            &[("name", base)],
        ));
    }
    if let Some(alt) = detail.strip_prefix("destination already had that name; moved as ") {
        // Translators: {name} is the name the item was given instead.
        return Some(gettext_f(
            "The name was taken, so it was moved as {name}",
            &[("name", alt)],
        ));
    }
    if let Some(alt) = detail.strip_prefix("name was taken remotely; renamed to ") {
        // Translators: {name} is the name the item was given instead.
        return Some(gettext_f(
            "The name was taken on Proton Drive, so it was renamed to {name}",
            &[("name", alt)],
        ));
    }
    if let Some(alt) = detail.strip_prefix("name was taken remotely; created as ") {
        // Translators: {name} is the name the item was given instead.
        return Some(gettext_f(
            "The name was taken on Proton Drive, so it was created as {name}",
            &[("name", alt)],
        ));
    }
    if let Some(alt) =
        detail.strip_prefix("its folder was trashed remotely; created in the root as ")
    {
        // Translators: {name} is the name the item was given.
        return Some(gettext_f(
            "Its folder was moved to the Trash on Proton Drive, so it was created at the top as {name}",
            &[("name", alt)],
        ));
    }
    if let Some(base) = detail.strip_prefix("differs from ") {
        // Translators: {name} is the file the conflicting copy was made from.
        return Some(gettext_f("Differs from {name}", &[("name", base)]));
    }
    if let Some(base) = detail
        .strip_prefix("no original ")
        .and_then(|rest| rest.strip_suffix(" to reconcile against"))
    {
        // Translators: {name} is the file the conflicting copy was made from.
        return Some(gettext_f("{name} no longer exists", &[("name", base)]));
    }
    if let Some(base) = detail
        .strip_prefix("identical to ")
        .and_then(|rest| rest.split_once(';').map(|(base, _)| base))
    {
        // Translators: {name} is the file the conflicting copy was made from.
        return Some(gettext_f("Same as {name}", &[("name", base)]));
    }
    if let Some((n, role)) = detail
        .split_once(" recipient(s) as ")
        .and_then(|(n, role)| Some((n.parse::<u64>().ok()?, role)))
    {
        return Some(match role {
            "viewer" => ngettext_f("{n} person can view", "{n} people can view", n, &[]),
            "editor" => ngettext_f("{n} person can edit", "{n} people can edit", n, &[]),
            _ => ngettext_f("{n} person invited", "{n} people invited", n, &[]),
        });
    }
    if let Some(n) = leading_count(detail, &["file(s) kept as conflict copies"]) {
        return Some(ngettext_f(
            "{n} file kept as a conflicting copy",
            "{n} files kept as conflicting copies",
            n,
            &[],
        ));
    }
    if let Some(n) = leading_count(detail, &["item(s) failed; will retry"]) {
        return Some(ngettext_f(
            "{n} item failed and will be tried again",
            "{n} items failed and will be tried again",
            n,
            &[],
        ));
    }
    Some(capitalize(detail))
}

/// A sync pass summary such as "2 uploaded, 1 folder(s) created", as a
/// list of counts. `None` when a part is not one the daemon writes.
fn sync_summary(detail: &str) -> Option<String> {
    let mut parts = Vec::new();
    for part in detail.split(", ") {
        let (n, label) = part.split_once(' ')?;
        let n = n.parse::<u64>().ok()?;
        parts.push(match label {
            // Translators: part of a list summing up a sync pass.
            "uploaded" => ngettext_f("{n} uploaded", "{n} uploaded", n, &[]),
            // Translators: part of a list summing up a sync pass.
            "downloaded" => ngettext_f("{n} downloaded", "{n} downloaded", n, &[]),
            // Translators: part of a list summing up a sync pass.
            "folder(s) created" => ngettext_f("{n} folder created", "{n} folders created", n, &[]),
            // Translators: part of a list summing up a sync pass.
            "deleted" => ngettext_f("{n} deleted", "{n} deleted", n, &[]),
            // Translators: part of a list summing up a sync pass.
            "conflicted" => ngettext_f("{n} in conflict", "{n} in conflict", n, &[]),
            // Translators: part of a list summing up a sync pass; the files are
            // still open in another app.
            "deferred (open for write)" => ngettext_f("{n} still open", "{n} still open", n, &[]),
            // Translators: part of a list summing up a sync pass.
            "failed" => ngettext_f("{n} failed", "{n} failed", n, &[]),
            _ => return None,
        });
    }
    Some(parts.join(", "))
}

/// A Google Photos import summary such as "40 uploaded · 2 already there".
fn import_summary(detail: &str) -> Option<String> {
    let mut parts = Vec::new();
    for part in detail.split(" · ") {
        if part == "cancelled" {
            parts.push(gettext("Cancelled"));
            continue;
        }
        let (n, label) = part.split_once(' ')?;
        let n = n.parse::<u64>().ok()?;
        parts.push(match label {
            "uploaded" => ngettext_f("{n} uploaded", "{n} uploaded", n, &[]),
            // Translators: photos an import skipped because they were already
            // in Proton Drive.
            "already there" => ngettext_f("{n} already there", "{n} already there", n, &[]),
            "albums" => ngettext_f("{n} album", "{n} albums", n, &[]),
            "failed" => ngettext_f("{n} failed", "{n} failed", n, &[]),
            _ => return None,
        });
    }
    Some(parts.join(" · "))
}

/// The number in front of `text` when the rest is one of `units`, as in
/// "3 item(s)".
fn leading_count(text: &str, units: &[&str]) -> Option<u64> {
    let (n, unit) = text.split_once(' ')?;
    units.contains(&unit).then_some(())?;
    n.parse().ok()
}

/// The name to show for a target: the last part of a path, or `None` when
/// the daemon logged a node id because it had no path.
fn target_name(target: &str) -> Option<String> {
    let looks_like_uid =
        target.len() >= 40 && target.matches('~').count() == 1 && !target.contains(['/', ' ', '.']);
    if target.is_empty() || looks_like_uid {
        return None;
    }
    Some(file_name(target).to_string())
}

/// The folder a destination path names, with the root read as "My files".
fn folder_name(dest: &str) -> String {
    let name = file_name(dest.trim_end_matches('/'));
    if name.is_empty() {
        gettext("My files")
    } else {
        name.to_string()
    }
}

/// The file a `name (sync-conflict <stamp>).ext` copy was made from.
pub(crate) fn conflict_base(name: &str) -> Option<String> {
    let (start, close) = conflict_marker(name)?;
    Some(format!("{}{}", &name[..start], &name[close + 1..]))
}

/// When a conflicting copy was made, from the stamp in its name.
fn conflict_stamp(name: &str) -> Option<i64> {
    let (start, close) = conflict_marker(name)?;
    let inner = &name[start + MARKER.len()..close];
    inner.split('-').next()?.parse().ok()
}

const MARKER: &str = " (sync-conflict ";

/// Where the ` (sync-conflict <stamp>[-<n>])` marker starts and where its
/// closing parenthesis is. The same shape the daemon reads back.
fn conflict_marker(name: &str) -> Option<(usize, usize)> {
    let start = name.find(MARKER)?;
    let after = start + MARKER.len();
    let close = after + name[after..].find(')')?;
    let inner = &name[after..close];
    let mut parts = inner.splitn(2, '-');
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(parts.next()?) || parts.next().is_some_and(|s| !digits(s)) {
        return None;
    }
    Some((start, close))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: ActivityKind, target: &str, detail: &str, ok: bool) -> ActivityEntry {
        ActivityEntry {
            time: 0,
            kind,
            target: target.into(),
            detail: detail.into(),
            ok,
        }
    }

    #[test]
    fn details_that_repeat_the_title_are_dropped() {
        let d = describe(&entry(ActivityKind::Upload, "a.txt", "uploaded", true));
        assert_eq!(d.title, "Uploaded a.txt");
        assert!(d.details.is_empty());
    }

    #[test]
    fn paths_and_counts_read_as_names_and_numbers() {
        let d = describe(&entry(
            ActivityKind::Move,
            "/mnt/a.txt",
            "to /mnt/Docs",
            true,
        ));
        assert_eq!(d.title, "Moved a.txt");
        assert_eq!(d.details, vec!["To Docs"]);
        let d = describe(&entry(ActivityKind::Restore, "3 item(s)", "", true));
        assert_eq!(d.title, "Restored 3 items");
        let d = describe(&entry(ActivityKind::EmptyTrash, "1 item", "", true));
        assert_eq!(d.title, "Emptied the Trash (1 item)");
    }

    #[test]
    fn failures_say_what_could_not_happen() {
        let d = describe(&entry(
            ActivityKind::Upload,
            "a.txt",
            "quota exceeded",
            false,
        ));
        assert_eq!(d.title, "Couldn't upload a.txt");
        assert_eq!(d.details, vec!["Quota exceeded"]);
    }

    #[test]
    fn sync_passes_are_summed_up() {
        let d = describe(&entry(
            ActivityKind::Sync,
            "Docs",
            "2 uploaded, 1 folder(s) created",
            true,
        ));
        assert_eq!(d.title, "Synced Docs");
        assert_eq!(d.details, vec!["2 uploaded, 1 folder created"]);
        let d = describe(&entry(
            ActivityKind::Sync,
            "sync incomplete for Docs",
            "2 item(s) failed; will retry",
            false,
        ));
        assert_eq!(d.title, "Some items in Docs didn't sync");
        assert_eq!(d.details, vec!["2 items failed and will be tried again"]);
    }

    #[test]
    fn a_conflicting_copy_names_its_original() {
        let d = describe(&entry(
            ActivityKind::Conflict,
            "notes (sync-conflict 1700000000).txt",
            "differs from notes.txt",
            false,
        ));
        assert_eq!(d.title, "Conflicting copy of notes.txt");
        assert_eq!(d.details[0], "Differs from notes.txt");
        assert_eq!(d.details.len(), 2);
        assert_eq!(conflict_base("a (sync-conflict 12-2)"), Some("a".into()));
        assert_eq!(conflict_base("a (sync-conflict x).txt"), None);
    }

    #[test]
    fn node_ids_are_not_shown_as_names() {
        let uid = format!("{}~{}", "A".repeat(30), "B".repeat(30));
        let d = describe(&entry(
            ActivityKind::Share,
            &uid,
            "1 recipient(s) as viewer",
            true,
        ));
        assert_eq!(d.title, "Shared an item");
        assert_eq!(d.details, vec!["1 person can view"]);
    }

    #[test]
    fn unknown_details_show_as_written() {
        let d = describe(&entry(
            ActivityKind::Download,
            "a.txt",
            "network timeout",
            false,
        ));
        assert_eq!(d.details, vec!["Network timeout"]);
    }
}
