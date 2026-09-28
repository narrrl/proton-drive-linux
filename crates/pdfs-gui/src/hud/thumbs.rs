//! Thumbnails for the launcher's rows and preview pane.
//!
//! The main thread keeps a small LRU of textures, the keys known to have no
//! thumbnail, and the pictures waiting for one (as weak references, so a row
//! that was rebuilt or dropped keeps nothing alive). Everything that touches
//! a file runs on two worker threads, which try in order:
//!
//! 1. the freedesktop thumbnail cache, valid only when its `Thumb::MTime`
//!    matches the file's;
//! 2. for local images, decoding the file itself, and writing the result back
//!    to that cache for the next time and for every other application;
//! 3. for Drive images, the daemon's `FileThumbs`, batched after a short quiet
//!    period.
//!
//! Workers hand back `gdk::Texture`s, never a `Pixbuf`, which may not cross
//! threads. Each job carries the render epoch it was queued in, and workers
//! skip jobs a newer query has made pointless.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use gtk4::gdk_pixbuf::Pixbuf;
use gtk4::prelude::*;
use gtk4::{gdk, glib};

use pdfs_core::control::{FileThumbRequest, Request, Response, is_thumbnail_image_name};

use crate::{Hit, spawn_request};

/// Decoded textures kept in memory. A launcher shows a few dozen rows, so
/// this covers several searches back.
const TEXTURE_CACHE_MAX: usize = 128;
const MISSING_MAX: usize = 1024;
const WORKERS: usize = 2;

/// Edge of the thumbnails this loader decodes and writes: the freedesktop
/// "large" size.
const THUMB_SIZE: i32 = 256;

/// Local images above this size are not decoded here; a thumbnail is not
/// worth reading that much.
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;

/// Quiet period before Drive misses go to the daemon as one batch, so typing
/// a word asks once for the final result set.
const DAEMON_DEBOUNCE: Duration = Duration::from_millis(150);
const DAEMON_RETRY: Duration = Duration::from_secs(1);
const DAEMON_BATCH: usize = 32;

/// Where a job finds its image.
enum Source {
    /// A file on this machine.
    Local(PathBuf),
    /// A Drive file: the thumbnail cache is keyed by its mounted path, and
    /// the daemon is asked on a miss.
    Drive {
        mounted: PathBuf,
        request: FileThumbRequest,
    },
    /// A thumbnail the daemon made, in its own cache.
    Daemon(PathBuf),
}

struct Job {
    epoch: u64,
    key: String,
    source: Source,
}

enum Outcome {
    Texture(gdk::Texture),
    Missing,
    /// Not in the thumbnail cache; the daemon has to make it.
    Daemon(FileThumbRequest),
    /// Queued for an older render; handed back in case it is wanted again.
    Skipped(Source),
}

struct Done {
    key: String,
    outcome: Outcome,
}

pub(crate) struct Thumbs {
    socket: PathBuf,
    epoch: Arc<AtomicU64>,
    jobs: async_channel::Sender<Job>,

    textures: RefCell<HashMap<String, gdk::Texture>>,
    /// Least recently used first.
    order: RefCell<VecDeque<String>>,
    missing: RefCell<HashSet<String>>,
    missing_order: RefCell<VecDeque<String>>,
    waiters: RefCell<HashMap<String, Vec<glib::WeakRef<gtk4::Picture>>>>,
    /// Keys with a worker job or a daemon request outstanding.
    working: RefCell<HashSet<String>>,

    /// Drive misses waiting for the next batch, by key.
    queued: RefCell<HashMap<String, FileThumbRequest>>,
    /// Drive items the daemon is working on under `generation`, by key.
    sent: RefCell<HashMap<String, FileThumbRequest>>,
    generation: Cell<u64>,
    reserving: Cell<bool>,
    inflight: Cell<bool>,
    flush: RefCell<Option<glib::SourceId>>,
}

impl Thumbs {
    pub(crate) fn new(socket: PathBuf) -> Rc<Self> {
        let epoch = Arc::new(AtomicU64::new(0));
        let (jobs, inbox) = async_channel::unbounded::<Job>();
        let (done, results) = async_channel::unbounded::<Done>();
        for n in 0..WORKERS {
            let inbox = inbox.clone();
            let done = done.clone();
            let epoch = epoch.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("prompt-thumbs-{n}"))
                .spawn(move || work(n, &inbox, &done, &epoch));
            if let Err(e) = spawned {
                tracing::warn!("cannot start a thumbnail worker: {e}");
            }
        }

        let thumbs = Rc::new(Self {
            socket,
            epoch,
            jobs,
            textures: RefCell::new(HashMap::new()),
            order: RefCell::new(VecDeque::new()),
            missing: RefCell::new(HashSet::new()),
            missing_order: RefCell::new(VecDeque::new()),
            waiters: RefCell::new(HashMap::new()),
            working: RefCell::new(HashSet::new()),
            queued: RefCell::new(HashMap::new()),
            sent: RefCell::new(HashMap::new()),
            generation: Cell::new(0),
            reserving: Cell::new(false),
            inflight: Cell::new(false),
            flush: RefCell::new(None),
        });
        let weak = Rc::downgrade(&thumbs);
        glib::spawn_future_local(async move {
            while let Ok(done) = results.recv().await {
                let Some(thumbs) = weak.upgrade() else {
                    break;
                };
                thumbs.finish(done);
            }
        });
        thumbs
    }

    /// Paint `hit`'s thumbnail into `picture` now if it is known, or as soon
    /// as it is. Folders and non-images keep the slot's icon.
    pub(crate) fn want(self: &Rc<Self>, hit: &Hit, mountpoint: &Path, picture: &gtk4::Picture) {
        let key = hit.thumb_key();
        // The widget name binds the picture to one key: a late reply for
        // another hit must not paint into it.
        picture.set_widget_name(&key);
        if hit.is_dir() || !is_thumbnail_image_name(hit.name()) {
            return;
        }
        if matches!(hit, Hit::Drive(drive) if drive.uid.is_empty()) {
            return;
        }
        let cached = self.textures.borrow().get(&key).cloned();
        if let Some(texture) = cached {
            self.touch(&key);
            paint(picture, &key, &texture);
            return;
        }
        if self.missing.borrow().contains(&key) {
            return;
        }
        self.waiters
            .borrow_mut()
            .entry(key.clone())
            .or_default()
            .push(picture.downgrade());
        if !self.working.borrow_mut().insert(key.clone()) {
            return;
        }
        let source = match hit {
            Hit::Local(local) => Source::Local(PathBuf::from(&local.path)),
            Hit::Drive(drive) => Source::Drive {
                mounted: hit.fs_path(mountpoint),
                request: FileThumbRequest {
                    uid: drive.uid.clone(),
                    modified: drive.modified,
                    name: drive.name.clone(),
                },
            },
        };
        self.dispatch(key, source);
    }

    /// The rows are about to be rebuilt: forget their pictures, and let the
    /// workers skip what was queued for them.
    pub(crate) fn begin_render(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.waiters.borrow_mut().clear();
    }

    /// The rows are rebuilt and have asked for their thumbnails. Drive work
    /// for hits that are gone is dropped, and cancelled at the daemon when it
    /// had already been sent.
    pub(crate) fn end_render(self: &Rc<Self>) {
        let wanted: HashSet<String> = self.waiters.borrow().keys().cloned().collect();
        let mut dropped = Vec::new();
        self.queued.borrow_mut().retain(|key, _| {
            let keep = wanted.contains(key);
            if !keep {
                dropped.push(key.clone());
            }
            keep
        });
        let stale = self.sent.borrow().keys().any(|key| !wanted.contains(key));
        if stale {
            self.cancel_daemon();
            // Still-wanted items go back into the next batch, under a fresh
            // generation.
            let sent: Vec<(String, FileThumbRequest)> = self.sent.borrow_mut().drain().collect();
            for (key, request) in sent {
                if wanted.contains(&key) {
                    self.queued.borrow_mut().insert(key, request);
                } else {
                    dropped.push(key);
                }
            }
            self.schedule(DAEMON_DEBOUNCE);
        }
        let mut working = self.working.borrow_mut();
        for key in dropped {
            working.remove(&key);
        }
    }

    /// The launcher was dismissed: stop everything.
    pub(crate) fn cancel_all(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.waiters.borrow_mut().clear();
        self.working.borrow_mut().clear();
        self.queued.borrow_mut().clear();
        self.sent.borrow_mut().clear();
        if let Some(source) = self.flush.borrow_mut().take() {
            source.remove();
        }
        self.cancel_daemon();
    }

    fn dispatch(&self, key: String, source: Source) {
        let job = Job {
            epoch: self.epoch.load(Ordering::SeqCst),
            key: key.clone(),
            source,
        };
        if self.jobs.try_send(job).is_err() {
            self.working.borrow_mut().remove(&key);
        }
    }

    fn wanted(&self, key: &str) -> bool {
        self.waiters.borrow().contains_key(key)
    }

    fn finish(self: &Rc<Self>, done: Done) {
        let Done { key, outcome } = done;
        match outcome {
            Outcome::Texture(texture) => {
                self.working.borrow_mut().remove(&key);
                self.store_texture(&key, texture.clone());
                let waiters = self.waiters.borrow_mut().remove(&key);
                for picture in waiters.into_iter().flatten() {
                    if let Some(picture) = picture.upgrade() {
                        paint(&picture, &key, &texture);
                    }
                }
            }
            Outcome::Missing => {
                self.working.borrow_mut().remove(&key);
                self.store_missing(key.clone());
                self.waiters.borrow_mut().remove(&key);
            }
            Outcome::Daemon(request) => {
                if self.wanted(&key) {
                    self.queued.borrow_mut().insert(key, request);
                    self.schedule(DAEMON_DEBOUNCE);
                } else {
                    self.working.borrow_mut().remove(&key);
                }
            }
            Outcome::Skipped(source) => {
                // A newer render may want the same image again.
                if self.wanted(&key) {
                    self.dispatch(key, source);
                } else {
                    self.working.borrow_mut().remove(&key);
                }
            }
        }
    }

    fn touch(&self, key: &str) {
        let mut order = self.order.borrow_mut();
        if let Some(at) = order.iter().position(|held| held == key) {
            order.remove(at);
            order.push_back(key.to_string());
        }
    }

    fn store_texture(&self, key: &str, texture: gdk::Texture) {
        let mut textures = self.textures.borrow_mut();
        if textures.insert(key.to_string(), texture).is_none() {
            self.order.borrow_mut().push_back(key.to_string());
        } else {
            drop(textures);
            self.touch(key);
            return;
        }
        let mut order = self.order.borrow_mut();
        while order.len() > TEXTURE_CACHE_MAX {
            if let Some(old) = order.pop_front() {
                textures.remove(&old);
            }
        }
    }

    fn store_missing(&self, key: String) {
        let mut missing = self.missing.borrow_mut();
        let mut order = self.missing_order.borrow_mut();
        if missing.insert(key.clone()) {
            order.push_back(key);
        }
        while order.len() > MISSING_MAX {
            if let Some(old) = order.pop_front() {
                missing.remove(&old);
            }
        }
    }

    /// Stop the daemon's work for this launcher. The generation is shared
    /// with the app, so this is only sent when something was asked for.
    fn cancel_daemon(&self) {
        let generation = self.generation.replace(0);
        if generation != 0 {
            // The reply is not needed; `spawn_request` owns the round-trip.
            drop(spawn_request(
                self.socket.clone(),
                Request::CancelFileThumbs {
                    generation: generation.saturating_add(1),
                },
            ));
        }
    }

    fn schedule(self: &Rc<Self>, delay: Duration) {
        if let Some(source) = self.flush.borrow_mut().take() {
            source.remove();
        }
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(delay, move || {
            if let Some(thumbs) = weak.upgrade() {
                thumbs.flush.borrow_mut().take();
                thumbs.send_batch();
            }
        });
        *self.flush.borrow_mut() = Some(source);
    }

    fn send_batch(self: &Rc<Self>) {
        if self.inflight.get() || self.queued.borrow().is_empty() {
            return;
        }
        if self.generation.get() == 0 {
            self.reserve();
            return;
        }
        let batch: Vec<(String, FileThumbRequest)> = {
            let mut queued = self.queued.borrow_mut();
            let keys: Vec<String> = queued.keys().take(DAEMON_BATCH).cloned().collect();
            keys.into_iter()
                .filter_map(|key| queued.remove_entry(&key))
                .collect()
        };
        self.sent.borrow_mut().extend(batch.iter().cloned());
        let generation = self.generation.get();
        self.inflight.set(true);
        let rx = spawn_request(
            self.socket.clone(),
            Request::FileThumbs {
                items: batch.iter().map(|(_, request)| request.clone()).collect(),
                generation,
            },
        );
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let reply = rx.recv().await;
            let Some(thumbs) = weak.upgrade() else {
                return;
            };
            thumbs.inflight.set(false);
            thumbs.receive(generation, batch, reply);
            if !thumbs.queued.borrow().is_empty() {
                thumbs.schedule(Duration::ZERO);
            }
        });
    }

    fn receive(
        self: &Rc<Self>,
        generation: u64,
        batch: Vec<(String, FileThumbRequest)>,
        reply: Result<Result<Response, String>, async_channel::RecvError>,
    ) {
        // Cancelled while in flight: `end_render` or `cancel_all` already
        // re-queued what is still wanted. A finished thumbnail is still worth
        // painting.
        let current = generation == self.generation.get();
        match reply {
            Ok(Ok(Response::Thumbs { items })) => {
                let mut by_uid: HashMap<String, (String, FileThumbRequest)> = batch
                    .into_iter()
                    .map(|(key, request)| (request.uid.clone(), (key, request)))
                    .collect();
                let mut again = Vec::new();
                for item in items {
                    let Some((key, request)) = by_uid.remove(&item.uid) else {
                        continue;
                    };
                    if let Some(path) = item.path {
                        if current {
                            self.sent.borrow_mut().remove(&key);
                        }
                        if self.wanted(&key) {
                            self.working.borrow_mut().insert(key.clone());
                            self.dispatch(key, Source::Daemon(PathBuf::from(path)));
                        } else {
                            self.working.borrow_mut().remove(&key);
                        }
                    } else if !current {
                        continue;
                    } else if item.pending {
                        again.push((key, request));
                    } else {
                        self.sent.borrow_mut().remove(&key);
                        self.working.borrow_mut().remove(&key);
                        self.store_missing(key.clone());
                        self.waiters.borrow_mut().remove(&key);
                    }
                }
                if current {
                    again.extend(by_uid.into_values());
                    self.retry(again);
                }
            }
            Ok(Ok(Response::FileThumbsStale)) if current => {
                self.generation.set(0);
                self.retry(batch);
            }
            Ok(Ok(Response::Error { message, .. })) if current => {
                tracing::debug!("launcher thumbnails failed: {message}");
                self.retry(batch);
            }
            _ if current => self.retry(batch),
            _ => {}
        }
    }

    /// Ask again for items the daemon is still making, after a pause.
    fn retry(self: &Rc<Self>, items: Vec<(String, FileThumbRequest)>) {
        if items.is_empty() {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(DAEMON_RETRY, move || {
            let Some(thumbs) = weak.upgrade() else {
                return;
            };
            for (key, request) in items {
                // Gone from `sent` means cancelled or already answered.
                if thumbs.sent.borrow_mut().remove(&key).is_some() && thumbs.wanted(&key) {
                    thumbs.queued.borrow_mut().insert(key, request);
                } else {
                    thumbs.working.borrow_mut().remove(&key);
                }
            }
            thumbs.send_batch();
        });
    }

    fn reserve(self: &Rc<Self>) {
        if self.reserving.replace(true) {
            return;
        }
        let rx = spawn_request(self.socket.clone(), Request::ReserveFileThumbGeneration);
        let weak: Weak<Self> = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let reply = rx.recv().await;
            let Some(thumbs) = weak.upgrade() else {
                return;
            };
            thumbs.reserving.set(false);
            match reply {
                Ok(Ok(Response::FileThumbGeneration { generation })) if generation != 0 => {
                    thumbs.generation.set(generation);
                    thumbs.send_batch();
                }
                _ => thumbs.schedule(DAEMON_RETRY),
            }
        });
    }
}

/// Show `texture` in `picture` if the picture still belongs to `key`, and hide
/// the icon underneath it.
fn paint(picture: &gtk4::Picture, key: &str, texture: &gdk::Texture) {
    if picture.widget_name() != key {
        return;
    }
    picture.set_paintable(Some(texture));
    picture.set_visible(true);
    if let Some(overlay) = picture.parent().and_downcast::<gtk4::Overlay>()
        && let Some(fallback) = overlay.child()
    {
        fallback.set_visible(false);
    }
}

fn work(
    index: usize,
    inbox: &async_channel::Receiver<Job>,
    done: &async_channel::Sender<Done>,
    epoch: &AtomicU64,
) {
    while let Ok(job) = inbox.recv_blocking() {
        // A daemon thumbnail is already made; decoding it is cheap and the
        // texture is cached for the next render.
        let stale =
            !matches!(job.source, Source::Daemon(_)) && job.epoch < epoch.load(Ordering::SeqCst);
        let outcome = if stale {
            Outcome::Skipped(job.source)
        } else {
            load(index, job.source)
        };
        if done
            .send_blocking(Done {
                key: job.key,
                outcome,
            })
            .is_err()
        {
            break;
        }
    }
}

fn load(worker: usize, source: Source) -> Outcome {
    match source {
        Source::Local(path) => {
            let Ok(meta) = std::fs::metadata(&path) else {
                return Outcome::Missing;
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |at| at.as_secs() as i64);
            if let Some(pixbuf) = cached(&path, mtime) {
                return texture(&pixbuf);
            }
            if meta.len() > MAX_SOURCE_BYTES {
                return Outcome::Missing;
            }
            let Some((pixbuf, scaled)) = decode(&path) else {
                return Outcome::Missing;
            };
            // An image that already fits is its own thumbnail.
            if scaled && let Err(e) = write_cached(worker, &path, mtime, &pixbuf) {
                tracing::debug!("cannot cache a thumbnail of {}: {e}", path.display());
            }
            texture(&pixbuf)
        }
        Source::Drive { mounted, request } => match cached(&mounted, request.modified) {
            Some(pixbuf) => texture(&pixbuf),
            None => Outcome::Daemon(request),
        },
        Source::Daemon(path) => match gdk::Texture::from_filename(&path) {
            Ok(texture) => Outcome::Texture(texture),
            Err(e) => {
                tracing::debug!("cannot decode {}: {e}", path.display());
                Outcome::Missing
            }
        },
    }
}

/// `$XDG_CACHE_HOME/thumbnails`.
fn thumbnail_root() -> PathBuf {
    glib::user_cache_dir().join("thumbnails")
}

/// The URI and its MD5, which name a file's entry in the thumbnail cache.
fn cache_name(path: &Path) -> Option<(String, String)> {
    let uri = glib::filename_to_uri(path, None).ok()?.to_string();
    let md5 = glib::compute_checksum_for_data(glib::ChecksumType::Md5, uri.as_bytes())?.to_string();
    Some((uri, md5))
}

/// A thumbnail from the shared cache, if one was made for this version of
/// the file.
fn cached(path: &Path, mtime: i64) -> Option<Pixbuf> {
    let (_, md5) = cache_name(path)?;
    let root = thumbnail_root();
    ["large", "x-large", "normal"].iter().find_map(|size| {
        let pixbuf = Pixbuf::from_file(root.join(size).join(format!("{md5}.png"))).ok()?;
        let stamp = pixbuf.option("tEXt::Thumb::MTime")?;
        (stamp.parse::<i64>().ok()? == mtime).then_some(pixbuf)
    })
}

/// Decode a local image at most [`THUMB_SIZE`] on its longer edge, upright.
/// The bool says whether it had to be scaled down.
fn decode(path: &Path) -> Option<(Pixbuf, bool)> {
    let (_, width, height) = Pixbuf::file_info(path)?;
    let scaled = width > THUMB_SIZE || height > THUMB_SIZE;
    let pixbuf = if scaled {
        Pixbuf::from_file_at_scale(path, THUMB_SIZE, THUMB_SIZE, true)
    } else {
        Pixbuf::from_file(path)
    }
    .ok()?;
    let pixbuf = pixbuf.apply_embedded_orientation().unwrap_or(pixbuf);
    Some((pixbuf, scaled))
}

/// Store a thumbnail as the freedesktop spec asks: in `large/`, with
/// `Thumb::URI` and `Thumb::MTime`, readable only by the user, and written
/// through a temporary file so another reader never sees half of it.
fn write_cached(worker: usize, path: &Path, mtime: i64, pixbuf: &Pixbuf) -> std::io::Result<()> {
    let (uri, md5) = cache_name(path).ok_or_else(|| std::io::Error::other("no file URI"))?;
    let dir = thumbnail_root().join("large");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let mtime = mtime.to_string();
    let png = pixbuf
        .save_to_bufferv(
            "png",
            &[
                ("tEXt::Thumb::URI", uri.as_str()),
                ("tEXt::Thumb::MTime", mtime.as_str()),
            ],
        )
        .map_err(std::io::Error::other)?;
    let tmp = dir.join(format!(
        "{md5}.png.pdfs-{}-{worker}.tmp",
        std::process::id()
    ));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(&png))
        .and_then(|()| std::fs::rename(&tmp, dir.join(format!("{md5}.png"))));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// A `Pixbuf` may not leave its thread; its pixels, as a memory texture, can.
fn texture(pixbuf: &Pixbuf) -> Outcome {
    let format = if pixbuf.has_alpha() {
        gdk::MemoryFormat::R8g8b8a8
    } else {
        gdk::MemoryFormat::R8g8b8
    };
    let texture = gdk::MemoryTexture::new(
        pixbuf.width(),
        pixbuf.height(),
        format,
        &pixbuf.read_pixel_bytes(),
        pixbuf.rowstride() as usize,
    );
    Outcome::Texture(texture.upcast())
}
