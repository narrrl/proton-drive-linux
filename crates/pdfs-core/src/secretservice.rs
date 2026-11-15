//! Saving a new secret into an existing Secret Service item by replacing the
//! item, not by editing its secret (`docs/BUGS.md` B291).
//!
//! oo7-daemon before 0.7.0 applies `SetSecret` only in memory: the keyring
//! file keeps the old secret, and it comes back once the daemon restarts.
//! Fedora 45 ships such a build (`0.7.0~beta^1.e91b98a`). Every refreshed
//! sign-in pdfs saved there was gone after a reboot, and the next start found
//! a refresh token Proton had already spent.
//!
//! `CreateItem` with `replace` is written to the file there, and every
//! provider implements it. So a secret that changes is written as a new item
//! that replaces the old one: in the old one's collection, with its label and
//! all of its attributes, so the provider recognises it as the same entry.

use dbus_secret_service::{Collection, EncryptionType, Path, SecretService};
use std::collections::HashMap;

/// The content type the keyring store writes its secrets with.
const CONTENT_TYPE: &str = "application/octet-stream";

/// What [`replace`] did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Replaced {
    /// The secret is saved.
    Saved,
    /// Nothing was changed; save it the usual way. Says why, for the log.
    Skipped(String),
}

/// Save `secret` into the one item for `service`/`user` by replacing it.
///
/// Skips, changing nothing, when there is no such item yet (the store creates
/// it), when there is more than one, when it is locked, or when the Secret
/// Service fails before the new item exists. Never shows a prompt: one would
/// block the caller, so it is dismissed and the write is skipped.
pub(crate) fn replace(service: &str, user: &str, secret: &[u8]) -> Replaced {
    match try_replace(service, user, secret) {
        Ok(replaced) => replaced,
        Err(e) => Replaced::Skipped(e.to_string()),
    }
}

fn try_replace(
    service: &str,
    user: &str,
    secret: &[u8],
) -> Result<Replaced, dbus_secret_service::Error> {
    let ss = SecretService::connect_with_max_prompt_timeout(EncryptionType::Dh, 0)?;
    let lookup = HashMap::from([("service", service), ("username", user)]);
    let found = ss.search_items(lookup.clone())?;
    if !found.locked.is_empty() {
        return Ok(Replaced::Skipped("the item is locked".into()));
    }
    let [old] = found.unlocked.as_slice() else {
        return Ok(Replaced::Skipped(format!(
            "{} items match, not one",
            found.unlocked.len()
        )));
    };
    let Some(collection) = collection_of(&old.path) else {
        return Ok(Replaced::Skipped(format!(
            "no collection in the item path {}",
            old.path
        )));
    };
    let label = old.get_label()?;
    let attributes = old.get_attributes()?;
    let attributes: HashMap<&str, &str> = attributes
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let collection = Collection::new(&ss, collection);
    let new = collection.create_item(&label, attributes, secret, true, CONTENT_TYPE)?;

    // A provider that did not match the old item leaves it next to the new
    // one, and two items make every read ambiguous.
    let after = ss.search_items(lookup)?;
    for item in after.unlocked.iter().chain(&after.locked) {
        if item.path == new.path {
            continue;
        }
        if let Err(e) = item.delete() {
            // The old item still holds the previous secret, so taking the new
            // one out leaves things as they were, for the usual write.
            if let Err(undo) = new.delete() {
                tracing::error!(
                    error = %undo,
                    "could not remove a duplicate keyring item; remove one of the two \
                     `proton-drive-linux` entries in your keyring and sign in again"
                );
            }
            return Ok(Replaced::Skipped(format!(
                "could not remove the replaced item: {e}"
            )));
        }
    }
    Ok(Replaced::Saved)
}

/// The collection an item path belongs to: the path without its last part,
/// as long as it is a Secret Service object path.
fn collection_of(item: &Path<'_>) -> Option<Path<'static>> {
    let (collection, _) = item.rsplit_once('/')?;
    if !collection.starts_with("/org/freedesktop/secrets/") || collection.ends_with('/') {
        return None;
    }
    Path::new(collection.to_owned()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection(item: &str) -> Option<String> {
        collection_of(&Path::new(item).unwrap()).map(|p| p.to_string())
    }

    #[test]
    fn an_item_belongs_to_the_collection_its_path_names() {
        assert_eq!(
            collection("/org/freedesktop/secrets/collection/login/14").as_deref(),
            Some("/org/freedesktop/secrets/collection/login")
        );
        assert_eq!(
            collection("/org/freedesktop/secrets/aliases/default/3").as_deref(),
            Some("/org/freedesktop/secrets/aliases/default")
        );
    }

    #[test]
    fn a_path_outside_the_secret_service_names_no_collection() {
        assert_eq!(collection("/org/example/item/1"), None);
        assert_eq!(collection("/item"), None);
    }
}
