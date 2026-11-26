//! Sharing: members and roles, public links, what is shared with you,
//! invitations and bookmarks.

use super::*;

pub(super) fn cmd_members(path: PathBuf) -> Result<()> {
    let response = control_request(CtlRequest::ListShare {
        path: path_arg(&path)?,
    })?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Share { entries, link } => {
            if entries.is_empty() {
                println!("No members or pending invitations.");
            }
            for e in entries {
                let kind = match e.kind {
                    ShareEntryKind::Member => "member",
                    ShareEntryKind::ProtonInvite => "invited (proton)",
                    ShareEntryKind::ExternalInvite => "invited (external)",
                };
                println!("{:<18} {:<8} {}  [{}]", kind, e.role, e.email, e.id);
            }
            match link {
                Some(l) => {
                    let url = l.url.as_deref().unwrap_or("(url hidden)");
                    let pw = if l.has_password { " +password" } else { "" };
                    let expiry = l
                        .expires
                        .map(|at| format!(", expires {}", format_epoch_utc(at)))
                        .unwrap_or_default();
                    println!("public link ({}{pw}{expiry}): {url}  [{}]", l.role, l.id);
                }
                None => println!("public link: none"),
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_share_role(
    path: PathBuf,
    id: String,
    kind: ShareKindArg,
    role: String,
) -> Result<()> {
    ok_or_bail(control_request(CtlRequest::UpdateShareRole {
        path: path_arg(&path)?,
        id,
        kind: kind.to_kind(),
        role,
    })?)
}

pub(super) fn cmd_unshare(
    path: PathBuf,
    id: Option<String>,
    kind: Option<ShareKindArg>,
) -> Result<()> {
    let path = path_arg(&path)?;
    let request = match (id, kind) {
        (Some(id), Some(kind)) => CtlRequest::RemoveShareEntry {
            path,
            id,
            kind: kind.to_kind(),
        },
        _ => CtlRequest::StopSharing { path },
    };
    ok_or_bail(control_request(request)?)
}

pub(super) fn cmd_public_link(action: PublicLinkCmd) -> Result<()> {
    match action {
        PublicLinkCmd::Create {
            path,
            role,
            password,
            expires,
        } => match control_request(CtlRequest::CreatePublicLink {
            path: path_arg(&path)?,
            role,
            password,
            expires,
        })? {
            response if emit_json(&response)? => {}
            CtlResponse::PublicLink { link } => {
                println!("{}", link.url.as_deref().unwrap_or("(no url returned)"));
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        },
        PublicLinkCmd::Update {
            path,
            id,
            role,
            password,
            no_password,
            expires,
            no_expiry,
        } => match control_request(CtlRequest::UpdatePublicLink {
            path: path_arg(&path)?,
            id,
            role,
            password: link_change(password, no_password),
            expires: link_change(expires, no_expiry),
        })? {
            response if emit_json(&response)? => {}
            CtlResponse::PublicLink { link } => {
                println!("{}", link.url.as_deref().unwrap_or("(no url returned)"));
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        },
        PublicLinkCmd::Remove { path, id } => {
            ok_or_bail(control_request(CtlRequest::RemovePublicLink {
                path: path_arg(&path)?,
                id,
            })?)?
        }
    }
    Ok(())
}

/// A `--x VALUE` / `--no-x` flag pair as one link option change.
pub(super) fn link_change<T>(value: Option<T>, remove: bool) -> LinkChange<T> {
    match (value, remove) {
        (Some(value), _) => LinkChange::Set(value),
        (None, true) => LinkChange::Remove,
        (None, false) => LinkChange::Keep,
    }
}

pub(super) fn cmd_shared() -> Result<()> {
    match control_request(CtlRequest::ListSharedByMe)? {
        CtlResponse::SharedByMe { items } if items.is_empty() => {
            println!("You haven't shared anything.")
        }
        CtlResponse::SharedByMe { items } => {
            for it in items {
                let kind = if it.is_dir { "d" } else { "-" };
                let mut tags = Vec::new();
                if it.member_count > 0 {
                    tags.push(format!("{} member(s)", it.member_count));
                }
                if it.invite_count > 0 {
                    tags.push(format!("{} pending", it.invite_count));
                }
                if let Some(link) = &it.link {
                    tags.push(match &link.url {
                        Some(url) => format!("link: {url}"),
                        None => "link".to_string(),
                    });
                }
                let tags = if tags.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", tags.join(", "))
                };
                println!("{kind} {}{tags}  [{}]", it.name, it.uid);
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// List what is shared with me, or — given a folder uid — that folder's
/// children. Shared items have no path in the mount, so a uid is the only handle
/// there is: every row prints one, and it is what the next level down (or
/// `shared-get`) is addressed with.
pub(super) fn cmd_shared_with_me(uid: Option<String>) -> Result<()> {
    let request = match uid {
        Some(uid) => CtlRequest::ListSharedFolder { uid },
        None => CtlRequest::ListSharedWithMe,
    };
    let response = control_request(request)?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Entries { entries } if entries.is_empty() => {
            println!("Nothing shared with you.")
        }
        CtlResponse::Entries { entries } => {
            for e in entries {
                let kind = if e.is_dir { "d" } else { "-" };
                // My role decides whether this is writable at all, so it is
                // printed whenever the API reported one; an empty role prints
                // nothing rather than a guess.
                let role = if e.role.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", e.role)
                };
                println!("{kind} {:>12}  {}{role}  [{}]", e.size, e.name, e.uid);
                if let Some(line) = shared_by_line(&e) {
                    println!("{:>15}{line}", "");
                }
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// Who shared an entry with me, and when, as a line printed under it — `None`
/// when the invitation said nothing (owned content, or a node inside a shared
/// folder rather than its root). An inviter whose signature did not verify is
/// said to be claimed, not stated: the name may be forged.
pub(super) fn shared_by_line(entry: &pdfs_core::control::DirEntry) -> Option<String> {
    if entry.shared_by.is_empty() {
        return None;
    }
    let mut line = format!("shared by {}", entry.shared_by);
    if entry.shared_at > 0 {
        line.push_str(&format!(" at {}", format_epoch_utc(entry.shared_at)));
    }
    if entry.shared_by_unverified {
        line.push_str("  WARNING: invitation signature did not verify; the sender may be forged");
    }
    Some(line)
}

/// Download a file shared with me. The daemon puts the plaintext in its content
/// cache and replies with that path; with a `dest` we copy it out, so the caller
/// gets a file it owns rather than one the cache may later evict.
pub(super) fn cmd_shared_get(uid: String, dest: Option<PathBuf>) -> Result<()> {
    let path = match control_request(CtlRequest::OpenSharedFile { uid })? {
        CtlResponse::FilePath { path } => PathBuf::from(path),
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    };
    match dest {
        Some(dest) => {
            std::fs::copy(&path, &dest).with_context(|| format!("writing {}", dest.display()))?;
            println!("{}", dest.display());
        }
        None => println!("{}", path.display()),
    }
    Ok(())
}

pub(super) fn cmd_quota() -> Result<()> {
    let response = control_request(CtlRequest::AccountQuota)?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::AccountQuota {
            max_space,
            used_space,
        } => {
            let used = used_space.max(0) as u64;
            if max_space > 0 {
                let total = max_space as u64;
                let pct = (used as f64 / total as f64 * 100.0).round() as u64;
                println!(
                    "{} of {} used ({pct}%)",
                    human_bytes(used),
                    human_bytes(total)
                );
            } else {
                println!("{} used", human_bytes(used));
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

pub(super) fn cmd_activity(limit: usize) -> Result<()> {
    let response = control_request(CtlRequest::ListActivity { limit })?;
    if emit_json(&response)? {
        return Ok(());
    }
    match response {
        CtlResponse::Activity { items } if items.is_empty() => println!("No recent activity."),
        CtlResponse::Activity { items } => {
            for a in items {
                let when = format_epoch(a.time);
                let verb = activity_verb(a.kind);
                let mark = if a.ok { " " } else { "!" };
                let detail = if a.detail.is_empty() {
                    String::new()
                } else {
                    format!("  {}", a.detail)
                };
                println!("{mark} {when}  {verb:<8} {}{detail}", a.target);
            }
        }
        CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
        other => bail!("unexpected response: {other:?}"),
    }
    Ok(())
}

/// A short verb for an activity kind, for the CLI's one-line-per-event feed.
pub(super) fn activity_verb(kind: pdfs_core::control::ActivityKind) -> &'static str {
    use pdfs_core::control::ActivityKind::*;
    match kind {
        Upload => "upload",
        Download => "download",
        Sync => "sync",
        Rename => "rename",
        Move => "move",
        Copy => "copy",
        CreateFolder => "mkdir",
        Trash => "trash",
        Restore => "restore",
        DeleteForever => "delete",
        EmptyTrash => "empty",
        Share => "share",
        PublicLink => "link",
        Unshare => "unshare",
        Conflict => "conflict",
    }
}

/// The `synced:` column of `sync list` and `devices list`: when the last pass
/// finished, or `never`. The daemon reports 0 for a folder that never synced.
pub(super) fn last_synced(secs: Option<i64>) -> String {
    match secs {
        Some(secs) if secs > 0 => format_epoch(secs),
        _ => "never".to_string(),
    }
}

pub(super) fn cmd_leave(uid: String) -> Result<()> {
    ok_or_bail(control_request(CtlRequest::LeaveShared { uid })?)
}

pub(super) fn cmd_invitations(action: InvitationCmd) -> Result<()> {
    match action {
        InvitationCmd::List => match control_request(CtlRequest::ListInvitations)? {
            CtlResponse::Invitations { items } if items.is_empty() => {
                println!("No pending invitations.")
            }
            CtlResponse::Invitations { items } => {
                for i in items {
                    let kind = if i.is_dir { "folder" } else { "file" };
                    let name = i.name.as_deref().unwrap_or("(name hidden)");
                    println!("{} shared {kind} \"{name}\"  [{}]", i.inviter_email, i.id);
                }
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        },
        InvitationCmd::Accept { id } => {
            ok_or_bail(control_request(CtlRequest::AcceptInvitation { id })?)?
        }
        InvitationCmd::Reject { id } => {
            ok_or_bail(control_request(CtlRequest::RejectInvitation { id })?)?
        }
    }
    Ok(())
}

pub(super) fn cmd_bookmarks(action: BookmarkCmd) -> Result<()> {
    match action {
        BookmarkCmd::List => match control_request(CtlRequest::ListBookmarks)? {
            CtlResponse::Bookmarks { items } if items.is_empty() => println!("No bookmarks."),
            CtlResponse::Bookmarks { items } => {
                for b in items {
                    let name = b.name.as_deref().unwrap_or("(name hidden)");
                    println!("{name}  {}  [{}]", b.url, b.token);
                }
            }
            CtlResponse::Error { message, kind } => bail!("{}", cli_error(kind, &message)),
            other => bail!("unexpected response: {other:?}"),
        },
        BookmarkCmd::Add { url, password } => {
            ok_or_bail(control_request(CtlRequest::CreateBookmark {
                url,
                password,
            })?)?
        }
        BookmarkCmd::Rm { token } => {
            ok_or_bail(control_request(CtlRequest::DeleteBookmark { token })?)?
        }
    }
    Ok(())
}
