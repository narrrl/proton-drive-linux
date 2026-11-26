//! The cells that belong to one mount's FUSE session (`SessionCells`).

use super::*;

/// What belongs to one mount's FUSE session rather than to the daemon: whether
/// it is the primary My Files mount, the tree inode it is rooted at, its
/// kernel channel and whether the session is up.
///
/// Every other part of `Core` is shared between the primary mount and its
/// on-demand forks. A fork gets fresh cells from [`SessionCells::fork`].
#[derive(Clone)]
pub(crate) struct SessionCells {
    /// False only for a forked on-demand device mount. The synthetic shared
    /// directory belongs exclusively to the primary My Files mount.
    pub(crate) primary: bool,
    /// The tree inode this mount's session is rooted at, set when the session
    /// is built ([`ProtonFs::new`]). Each fork has its own cell.
    pub(crate) root: Arc<OnceLock<u64>>,
    /// This mount's kernel notification channel, for telling the kernel to drop
    /// metadata it has cached. Set once the session exists — which is *after*
    /// the `Core` it is built from, hence the cell.
    ///
    /// Per mount, not per daemon: each session calls its own root inode 1.
    /// [`Core::fork_state`] gives each fork an empty cell of its own. Work that
    /// changes the tree for every mount tells every session
    /// ([`Core::for_each_mount`]).
    pub(crate) notifier: Arc<OnceLock<KernelChannel>>,
    /// True only after this mount's FUSE session spawned successfully.
    /// Registration happens earlier for the primary drain, so registration
    /// alone cannot answer whether the location is mounted.
    pub(crate) session_live: Arc<AtomicBool>,
}

impl SessionCells {
    pub(crate) fn new() -> Self {
        Self {
            primary: true,
            root: Arc::new(OnceLock::new()),
            notifier: Arc::new(OnceLock::new()),
            session_live: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Fresh cells for an on-demand fork: not the primary mount, and no root,
    /// channel or session until the fork's own session is built.
    pub(crate) fn fork(&self) -> Self {
        Self {
            primary: false,
            ..Self::new()
        }
    }
}
