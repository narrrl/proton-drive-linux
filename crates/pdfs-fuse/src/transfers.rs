//! In-memory registry of in-flight transfers, behind [`Request::GetQueueStatus`].
//!
//! The daemon downloads and uploads whole files through the SDK's streaming
//! [`Read`]/[`Write`] variants. Wrapping that reader/writer in a
//! [`CountingReader`]/[`CountingWriter`] lets each block of bytes tick a
//! per-transfer counter without the SDK knowing anything about progress. A
//! [`TransferGuard`] registers a transfer on creation and deregisters it on
//! drop, so the registry always reflects exactly what is in flight — even if a
//! transfer fails partway and unwinds.
//!
//! [`Request::GetQueueStatus`]: pdfs_core::control::Request::GetQueueStatus

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pdfs_core::control::{TransferDirection, TransferItem};

/// One registered transfer. `done` is bumped from the I/O wrapper without the
/// registry lock; the rest is immutable for the transfer's lifetime.
struct Entry {
    name: String,
    uid: String,
    direction: TransferDirection,
    total: u64,
    done: AtomicU64,
    started: Instant,
}

/// The set of transfers currently in flight. Cloned `Arc`-style across the FUSE
/// session and the control-socket task (both share one registry via [`Core`]).
#[derive(Default)]
pub struct TransferRegistry {
    inner: Mutex<HashMap<u64, Arc<Entry>>>,
    next: AtomicU64,
}

impl TransferRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a new transfer of `total` bytes (`0` = unknown), returning a
    /// guard that tracks progress and deregisters the transfer when dropped.
    pub fn begin(
        self: &Arc<Self>,
        name: impl Into<String>,
        uid: impl Into<String>,
        direction: TransferDirection,
        total: u64,
    ) -> TransferGuard {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let entry = Arc::new(Entry {
            name: name.into(),
            uid: uid.into(),
            direction,
            total,
            done: AtomicU64::new(0),
            started: Instant::now(),
        });
        self.inner.lock().unwrap().insert(id, entry.clone());
        TransferGuard {
            reg: self.clone(),
            id,
            entry,
        }
    }

    /// Snapshot every in-flight transfer for [`Response::Transfers`]. Speed is the
    /// running average since the transfer began — simple, and stable enough for a
    /// progress widget without per-tick sampling state.
    ///
    /// [`Response::Transfers`]: pdfs_core::control::Response::Transfers
    pub fn snapshot(&self) -> Vec<TransferItem> {
        let map = self.inner.lock().unwrap();
        map.values()
            .map(|e| {
                let done = e.done.load(Ordering::Relaxed);
                let secs = e.started.elapsed().as_secs_f64();
                let speed = if secs > 0.0 {
                    (done as f64 / secs) as u64
                } else {
                    0
                };
                TransferItem {
                    uid: e.uid.clone(),
                    name: e.name.clone(),
                    direction: e.direction,
                    bytes_completed: done,
                    bytes_total: e.total,
                    speed_bytes_sec: speed,
                }
            })
            .collect()
    }
}

/// Lifetime handle for a registered transfer: tick progress with [`add`], and
/// the transfer leaves the registry when this drops.
///
/// [`add`]: TransferGuard::add
pub struct TransferGuard {
    reg: Arc<TransferRegistry>,
    id: u64,
    entry: Arc<Entry>,
}

impl TransferGuard {
    /// Record `n` more bytes moved.
    pub fn add(&self, n: u64) {
        self.entry.done.fetch_add(n, Ordering::Relaxed);
    }
}

impl Drop for TransferGuard {
    fn drop(&mut self) {
        self.reg.inner.lock().unwrap().remove(&self.id);
    }
}

/// A [`Write`] that tallies bytes written to a [`TransferGuard`] (download path).
pub struct CountingWriter<'a, W> {
    inner: W,
    guard: &'a TransferGuard,
}

impl<'a, W: Write> CountingWriter<'a, W> {
    pub fn new(inner: W, guard: &'a TransferGuard) -> Self {
        Self { inner, guard }
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.guard.add(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// A [`Read`] that tallies bytes read and *owns* its [`TransferGuard`], so the
/// transfer stays registered exactly as long as the reader lives. Used by the
/// concurrent bulk uploader, where each task hands its reader to the SDK and has
/// nowhere separate to park the guard; the transfer deregisters when the SDK
/// drops the reader after sealing the revision. Owning (rather than borrowing)
/// also keeps each upload future `Send + 'static` for [`tokio::task::spawn`].
pub struct OwnedCountingReader<R> {
    inner: R,
    guard: TransferGuard,
}

impl<R: Read> OwnedCountingReader<R> {
    pub fn new(inner: R, guard: TransferGuard) -> Self {
        Self { inner, guard }
    }
}

impl<R: Read> Read for OwnedCountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.guard.add(n as u64);
        Ok(n)
    }
}

/// A [`Read`] that tallies bytes read through a [`TransferGuard`] (upload path).
pub struct CountingReader<'a, R> {
    inner: R,
    guard: &'a TransferGuard,
}

impl<'a, R: Read> CountingReader<'a, R> {
    pub fn new(inner: R, guard: &'a TransferGuard) -> Self {
        Self { inner, guard }
    }
}

impl<R: Read> Read for CountingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.guard.add(n as u64);
        Ok(n)
    }
}
