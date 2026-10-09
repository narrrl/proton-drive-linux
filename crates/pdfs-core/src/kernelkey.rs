//! A secret held in the Linux kernel keyring, for as long as the user is logged in.
//!
//! This is where the key passphrases of a two-password account live by default:
//! the mailbox password is never stored, and the secrets derived from it are
//! kept in kernel memory — not on disk, not in the Secret Service — and vanish
//! when the user's last session ends or the machine reboots. Both the daemon and
//! `pdfs unlock` reach the same entry because it hangs off the *user* keyring.
//! It is also where session tokens wait while the Secret Service refuses them
//! ([`UNSAVED_TOKENS`]), so they outlive the process that holds them.
//!
//! Kernel keys are not paged to swap, are invisible to other users, and are
//! wiped by [`clear`] (`KEYCTL_REVOKE`), which makes them a better home than a
//! process's heap. Processes of the same user can still read them, exactly as
//! they could ptrace the daemon: this protects against disk, backup and other
//! users, not against code already running as you.
//!
//! The keyring may be unavailable (a container, a kernel built without
//! `CONFIG_KEYS`); every operation then reports it as an error and the caller
//! falls back to asking again, never to storing the secret somewhere weaker.

use std::io;

use zeroize::Zeroizing;

/// One secret in the user keyring, by its description. Descriptions are
/// namespaced so they cannot collide with another program's keys.
pub struct Key {
    description: &'static [u8],
}

/// The account's key passphrases (see the module docs).
#[cfg(not(test))]
pub const PASSPHRASES: Key = Key {
    description: b"pdfs:key-passphrases\0",
};
/// Tests must never touch a real unlock.
#[cfg(test)]
pub const PASSPHRASES: Key = Key {
    description: b"pdfs:key-passphrases:test\0",
};

/// Session tokens the Secret Service refused to store, until it takes them.
/// A refresh token held only in a process's memory dies with the process.
#[cfg(not(test))]
pub const UNSAVED_TOKENS: Key = Key {
    description: b"pdfs:unsaved-tokens\0",
};
#[cfg(test)]
pub const UNSAVED_TOKENS: Key = Key {
    description: b"pdfs:unsaved-tokens:test\0",
};

const KEY_TYPE: &[u8] = b"user\0";

const KEY_SPEC_USER_KEYRING: i32 = -4;
const KEYCTL_REVOKE: libc::c_long = 3;
const KEYCTL_SETPERM: libc::c_long = 5;
const KEYCTL_SEARCH: libc::c_long = 10;
const KEYCTL_READ: libc::c_long = 11;

/// Possessor and owner may view, read, write, search, link and set attributes;
/// group and others get nothing.
///
/// A new key's default grants the owner only *view*, and read access to the
/// possessor. A process possesses the user keyring only when its session
/// keyring links to it, which a systemd user service or a container does not
/// guarantee — so with the default, the daemon could store a key it then
/// cannot read back. The owner is the same user that could ptrace the daemon
/// anyway, so granting it read access gives up nothing.
const PERM_OWNER_AND_POSSESSOR: u32 = 0x3f3f_0000;

fn last_error<T>() -> io::Result<T> {
    Err(io::Error::last_os_error())
}

fn find(description: &[u8]) -> io::Result<Option<libc::c_long>> {
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let id = unsafe {
        libc::syscall(
            libc::SYS_keyctl,
            KEYCTL_SEARCH,
            KEY_SPEC_USER_KEYRING,
            KEY_TYPE.as_ptr(),
            description.as_ptr(),
            0,
        )
    };
    if id >= 0 {
        return Ok(Some(id));
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // ENOKEY: not stored. EKEYREVOKED/EKEYEXPIRED: stored once, now gone.
        Some(libc::ENOKEY | libc::EKEYREVOKED | libc::EKEYEXPIRED) => Ok(None),
        _ => Err(err),
    }
}

/// Stop this process from being core-dumped or ptraced by an unprivileged
/// process of the same user, so the secrets in its memory stay out of crash
/// reports. Call once at start of a long-lived process that holds them.
pub fn forbid_dumps() {
    // SAFETY: `PR_SET_DUMPABLE` takes plain integers and only affects this process.
    let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    if rc != 0 {
        tracing::warn!(error = %io::Error::last_os_error(), "could not mark the process non-dumpable");
    }
}

impl Key {
    /// Store `secret`, replacing any earlier one.
    pub fn store(&self, secret: &[u8]) -> io::Result<()> {
        // SAFETY: strings are NUL-terminated; the payload pointer/length pair
        // describes `secret`, which outlives the call.
        let id = unsafe {
            libc::syscall(
                libc::SYS_add_key,
                KEY_TYPE.as_ptr(),
                self.description.as_ptr(),
                secret.as_ptr(),
                secret.len(),
                KEY_SPEC_USER_KEYRING,
            )
        };
        if id < 0 {
            return last_error();
        }
        // SAFETY: plain integer arguments.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_keyctl,
                KEYCTL_SETPERM,
                id,
                PERM_OWNER_AND_POSSESSOR,
            )
        };
        if rc < 0 {
            // A key whose permissions are unknown is not one to leave behind.
            let err = io::Error::last_os_error();
            let _ = self.clear();
            return Err(err);
        }
        Ok(())
    }

    /// Read the stored secret, or `None` when there is none.
    pub fn load(&self) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
        let Some(id) = find(self.description)? else {
            return Ok(None);
        };
        // Size first, then read; retry if the key was replaced in between.
        for _ in 0..3 {
            // SAFETY: a null buffer asks the kernel only for the payload length.
            let len = unsafe {
                libc::syscall(
                    libc::SYS_keyctl,
                    KEYCTL_READ,
                    id,
                    std::ptr::null_mut::<u8>(),
                    0usize,
                )
            };
            if len < 0 {
                return match io::Error::last_os_error().raw_os_error() {
                    Some(libc::ENOKEY | libc::EKEYREVOKED | libc::EKEYEXPIRED) => Ok(None),
                    _ => last_error(),
                };
            }
            let mut buf = Zeroizing::new(vec![0u8; len as usize]);
            // SAFETY: `buf` is `len` writable bytes.
            let read = unsafe {
                libc::syscall(
                    libc::SYS_keyctl,
                    KEYCTL_READ,
                    id,
                    buf.as_mut_ptr(),
                    buf.len(),
                )
            };
            if read < 0 {
                return last_error();
            }
            if read as usize <= buf.len() {
                buf.truncate(read as usize);
                return Ok(Some(buf));
            }
        }
        Err(io::Error::other("the key changed while it was being read"))
    }

    /// Remove the stored secret. Succeeds when there is none.
    pub fn clear(&self) -> io::Result<()> {
        let Some(id) = find(self.description)? else {
            return Ok(());
        };
        // SAFETY: plain integer arguments.
        let rc = unsafe { libc::syscall(libc::SYS_keyctl, KEYCTL_REVOKE, id) };
        if rc < 0 {
            return match io::Error::last_os_error().raw_os_error() {
                Some(libc::ENOKEY | libc::EKEYREVOKED) => Ok(()),
                _ => last_error(),
            };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The kernel keyring is process-wide state and a sandbox may not offer it,
    // so one test covers the whole round trip and skips when it is unavailable.
    #[test]
    fn stored_secret_round_trips_and_clears() {
        if PASSPHRASES.store(b"probe").is_err() {
            eprintln!("kernel keyring unavailable; skipping");
            return;
        }
        let saved = PASSPHRASES
            .load()
            .unwrap()
            .expect("stored secret is readable");
        assert_eq!(saved.as_slice(), b"probe");

        PASSPHRASES.store(b"replaced").unwrap();
        assert_eq!(PASSPHRASES.load().unwrap().unwrap().as_slice(), b"replaced");

        PASSPHRASES.clear().unwrap();
        assert!(PASSPHRASES.load().unwrap().is_none());
        PASSPHRASES.clear().unwrap();
    }
}
