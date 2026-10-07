//! A secret held in the Linux kernel keyring, for as long as the user is logged in.
//!
//! This is where the key passphrases of a two-password account live by default:
//! the mailbox password is never stored, and the secrets derived from it are
//! kept in kernel memory — not on disk, not in the Secret Service — and vanish
//! when the user's last session ends or the machine reboots. Both the daemon and
//! `pdfs unlock` reach the same entry because it hangs off the *user* keyring.
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

/// Key description. Namespaced so it cannot collide with another program's key.
#[cfg(not(test))]
const DESCRIPTION: &[u8] = b"pdfs:key-passphrases\0";
/// Tests must never touch a real unlock.
#[cfg(test)]
const DESCRIPTION: &[u8] = b"pdfs:key-passphrases:test\0";
const KEY_TYPE: &[u8] = b"user\0";

const KEY_SPEC_USER_KEYRING: i32 = -4;
const KEYCTL_REVOKE: libc::c_long = 3;
const KEYCTL_SEARCH: libc::c_long = 10;
const KEYCTL_READ: libc::c_long = 11;

fn last_error<T>() -> io::Result<T> {
    Err(io::Error::last_os_error())
}

fn find() -> io::Result<Option<libc::c_long>> {
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let id = unsafe {
        libc::syscall(
            libc::SYS_keyctl,
            KEYCTL_SEARCH,
            KEY_SPEC_USER_KEYRING,
            KEY_TYPE.as_ptr(),
            DESCRIPTION.as_ptr(),
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

/// Store `secret`, replacing any earlier one.
pub fn store(secret: &[u8]) -> io::Result<()> {
    // SAFETY: strings are NUL-terminated; the payload pointer/length pair
    // describes `secret`, which outlives the call.
    let id = unsafe {
        libc::syscall(
            libc::SYS_add_key,
            KEY_TYPE.as_ptr(),
            DESCRIPTION.as_ptr(),
            secret.as_ptr(),
            secret.len(),
            KEY_SPEC_USER_KEYRING,
        )
    };
    if id < 0 {
        return last_error();
    }
    Ok(())
}

/// Read the stored secret, or `None` when there is none.
pub fn load() -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    let Some(id) = find()? else {
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
pub fn clear() -> io::Result<()> {
    let Some(id) = find()? else {
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

#[cfg(test)]
mod tests {
    use super::*;

    // The kernel keyring is process-wide state and a sandbox may not offer it,
    // so one test covers the whole round trip and skips when it is unavailable.
    #[test]
    fn stored_secret_round_trips_and_clears() {
        if store(b"probe").is_err() {
            eprintln!("kernel keyring unavailable; skipping");
            return;
        }
        let saved = load().unwrap().expect("stored secret is readable");
        assert_eq!(saved.as_slice(), b"probe");

        store(b"replaced").unwrap();
        assert_eq!(load().unwrap().unwrap().as_slice(), b"replaced");

        clear().unwrap();
        assert!(load().unwrap().is_none());
        clear().unwrap();
    }
}
