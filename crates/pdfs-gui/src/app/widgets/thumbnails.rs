//! Shared thumbnails for ordinary Drive image files.
//!
//! The Photos gallery has timeline-specific batching and aspect-ratio state.
//! File listings need a smaller, reusable surface: one loader backs My files,
//! search, Shared and Trash, with modification-time cache keys
//! so a replaced image never flashes its previous contents.

use crate::*;
use pdfs_core::control::{FileThumbRequest, is_thumbnail_image_name};

const THUMB_BATCH: usize = 32;
const THUMB_CACHE_MAX: usize = 256;
const THUMB_MISSING_MAX: usize = 2048;
const THUMB_DEBOUNCE: Duration = Duration::from_millis(40);
const THUMB_RETRY: Duration = Duration::from_secs(2);

pub(crate) struct FileThumbnailState {
    textures: RefCell<HashMap<String, gtk4::gdk::Texture>>,
    texture_order: RefCell<VecDeque<String>>,
    missing: RefCell<HashSet<String>>,
    missing_order: RefCell<VecDeque<String>>,
    wanted: RefCell<HashMap<String, Vec<gtk4::Picture>>>,
    queue: RefCell<VecDeque<FileThumbRequest>>,
    inflight: Cell<bool>,
    source: RefCell<Option<glib::SourceId>>,
    generation: Cell<u64>,
    generation_loading: Cell<bool>,
}

#[derive(Debug, PartialEq, Eq)]
enum ReplyDisposition {
    Ignore,
    Load,
    Retry,
    Missing,
}

fn reply_disposition(wanted: bool, has_path: bool, pending: bool) -> ReplyDisposition {
    if !wanted {
        ReplyDisposition::Ignore
    } else if has_path {
        ReplyDisposition::Load
    } else if pending {
        ReplyDisposition::Retry
    } else {
        ReplyDisposition::Missing
    }
}

impl FileThumbnailState {
    pub(crate) fn new() -> Self {
        Self {
            textures: RefCell::new(HashMap::new()),
            texture_order: RefCell::new(VecDeque::new()),
            missing: RefCell::new(HashSet::new()),
            missing_order: RefCell::new(VecDeque::new()),
            wanted: RefCell::new(HashMap::new()),
            queue: RefCell::new(VecDeque::new()),
            inflight: Cell::new(false),
            source: RefCell::new(None),
            generation: Cell::new(0),
            generation_loading: Cell::new(false),
        }
    }

    fn store_texture(&self, key: &str, texture: gtk4::gdk::Texture) {
        let mut textures = self.textures.borrow_mut();
        let mut order = self.texture_order.borrow_mut();
        if textures.insert(key.to_string(), texture).is_none() {
            order.push_back(key.to_string());
        }
        while order.len() > THUMB_CACHE_MAX {
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
        while order.len() > THUMB_MISSING_MAX {
            if let Some(old) = order.pop_front() {
                missing.remove(&old);
            }
        }
    }
}

/// Abandon every opportunistic ordinary-file thumbnail associated with the
/// listing being left. Advancing a wire generation lets the daemon cancel work
/// even when an older request reaches it after this one.
pub(crate) fn cancel_file_thumbnails(ui: &Rc<Ui>) {
    if let Some(source) = ui.file_thumbs.source.borrow_mut().take() {
        source.remove();
    }
    ui.file_thumbs.queue.borrow_mut().clear();
    ui.file_thumbs.wanted.borrow_mut().clear();
    let generation = ui.file_thumbs.generation.replace(0);
    // The request is useful even when its reply is not. `spawn_request` owns the
    // socket round-trip on a worker thread, so dropping the receiver is safe.
    if generation != 0 {
        drop(spawn_request(
            ui.dirs.control_socket(),
            Request::CancelFileThumbs {
                generation: generation.saturating_add(1),
            },
        ));
    }
}

/// A square thumbnail surface with a generic file icon underneath the picture.
/// Call [`bind_file_thumbnail`] whenever a recycled row is bound to an entry.
pub(crate) fn file_thumbnail_widget(size: i32, fallback_size: i32) -> gtk4::Overlay {
    let fallback = gtk4::Image::builder().pixel_size(fallback_size).build();
    let picture = gtk4::Picture::builder()
        .width_request(size)
        .height_request(size)
        .content_fit(gtk4::ContentFit::Cover)
        .can_shrink(true)
        .visible(false)
        .build();
    picture.add_css_class("file-thumbnail");

    let overlay = gtk4::Overlay::new();
    overlay.set_width_request(size);
    overlay.set_height_request(size);
    overlay.set_halign(gtk4::Align::Center);
    overlay.set_valign(gtk4::Align::Center);
    overlay.set_overflow(gtk4::Overflow::Hidden);
    overlay.set_child(Some(&fallback));
    overlay.add_overlay(&picture);
    overlay
}

/// Resize an existing thumbnail surface. Grid rows are recycled, so changing
/// these requests in place and rebinding the model is enough to implement the
/// status-bar zoom without downloading or decoding an image again.
pub(crate) fn resize_file_thumbnail(widget: &gtk4::Overlay, size: i32, fallback_size: i32) {
    widget.set_width_request(size);
    widget.set_height_request(size);
    let Some(fallback) = widget.child().and_downcast::<gtk4::Image>() else {
        return;
    };
    fallback.set_pixel_size(fallback_size);
    if let Some(picture) = fallback.next_sibling().and_downcast::<gtk4::Picture>() {
        picture.set_width_request(size);
        picture.set_height_request(size);
    }
}

/// Bind a thumbnail surface to one entry, painting a cached texture immediately
/// or queueing an on-demand daemon request. Non-images retain the generic icon.
pub(crate) fn bind_file_thumbnail(
    ui: &Rc<Ui>,
    widget: &gtk4::Overlay,
    entry: &DirEntry,
    symbolic_fallback: bool,
) {
    let Some(fallback) = widget.child().and_downcast::<gtk4::Image>() else {
        return;
    };
    // The browser may add a status badge after the picture. Locate the picture
    // immediately after the base fallback instead of assuming it is the last
    // overlay child.
    let Some(picture) = fallback.next_sibling().and_downcast::<gtk4::Picture>() else {
        return;
    };

    let fallback_name = if symbolic_fallback {
        format!("{}-symbolic", icon_base_for(entry))
    } else {
        icon_base_for(entry).to_string()
    };
    fallback.set_icon_name(Some(&fallback_name));
    fallback.set_visible(true);
    picture.set_visible(false);
    picture.set_paintable(gtk4::gdk::Paintable::NONE);

    // The widget name is a cheap binding token. List factories recycle their
    // children; a reply for the previous entry must not paint into the new one.
    let key = thumbnail_key(&entry.uid, entry.modified);
    picture.set_widget_name(&key);
    remove_waiter(ui, &picture);

    if entry.is_dir || !is_image_name(&entry.name) || entry.uid.is_empty() {
        return;
    }
    if let Some(texture) = ui.file_thumbs.textures.borrow().get(&key) {
        paint(&picture, &key, texture);
        return;
    }
    if ui.file_thumbs.missing.borrow().contains(&key) {
        return;
    }

    ui.file_thumbs
        .wanted
        .borrow_mut()
        .entry(key)
        .or_default()
        .push(picture);
    let mut queue = ui.file_thumbs.queue.borrow_mut();
    queue.retain(|item| item.uid != entry.uid);
    queue.push_back(FileThumbRequest {
        uid: entry.uid.clone(),
        modified: entry.modified,
        name: entry.name.clone(),
    });
    drop(queue);
    schedule_file_thumbs(ui);
}

/// Create and immediately bind a thumbnail for a non-recycled row.
pub(crate) fn file_thumbnail(
    ui: &Rc<Ui>,
    entry: &DirEntry,
    size: i32,
    fallback_size: i32,
    symbolic_fallback: bool,
) -> gtk4::Overlay {
    let widget = file_thumbnail_widget(size, fallback_size);
    bind_file_thumbnail(ui, &widget, entry, symbolic_fallback);
    widget
}

/// Whether the file name denotes an image format for which Proton or the local
/// decoder can reasonably provide a thumbnail.
pub(crate) fn is_image_name(name: &str) -> bool {
    is_thumbnail_image_name(name)
}

fn thumbnail_key(uid: &str, modified: i64) -> String {
    format!("{uid}:{modified}")
}

fn remove_waiter(ui: &Rc<Ui>, picture: &gtk4::Picture) {
    let mut wanted = ui.file_thumbs.wanted.borrow_mut();
    wanted.retain(|_, pictures| {
        pictures.retain(|candidate| candidate != picture);
        !pictures.is_empty()
    });
}

fn paint(picture: &gtk4::Picture, key: &str, texture: &gtk4::gdk::Texture) {
    if picture.widget_name() != key {
        return;
    }
    picture.set_paintable(Some(texture));
    picture.set_visible(true);
    if let Some(overlay) = picture.parent().and_downcast::<gtk4::Overlay>()
        && let Some(fallback) = overlay.child().and_downcast::<gtk4::Image>()
    {
        fallback.set_visible(false);
    }
}

fn schedule_file_thumbs(ui: &Rc<Ui>) {
    if ui.file_thumbs.queue.borrow().is_empty() || ui.file_thumbs.inflight.get() {
        return;
    }
    if ui.file_thumbs.generation.get() == 0 {
        reserve_file_thumb_generation(ui);
        return;
    }
    if let Some(source) = ui.file_thumbs.source.borrow_mut().take() {
        source.remove();
    }
    let ui_flush = ui.clone();
    let source = glib::timeout_add_local_once(THUMB_DEBOUNCE, move || {
        ui_flush.file_thumbs.source.borrow_mut().take();
        flush_file_thumbs(&ui_flush);
    });
    *ui.file_thumbs.source.borrow_mut() = Some(source);
}

fn reserve_file_thumb_generation(ui: &Rc<Ui>) {
    if ui.file_thumbs.generation_loading.replace(true) {
        return;
    }
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::ReserveFileThumbGeneration,
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.file_thumbs.generation_loading.set(false);
        match result {
            Ok(Ok(Response::FileThumbGeneration { generation })) if generation != 0 => {
                ui.file_thumbs.generation.set(generation);
                schedule_file_thumbs(&ui);
            }
            _ => retry_file_thumb_generation(&ui),
        }
    });
}

fn retry_file_thumb_generation(ui: &Rc<Ui>) {
    if let Some(source) = ui.file_thumbs.source.borrow_mut().take() {
        source.remove();
    }
    let ui_retry = ui.clone();
    let source = glib::timeout_add_local_once(THUMB_RETRY, move || {
        ui_retry.file_thumbs.source.borrow_mut().take();
        schedule_file_thumbs(&ui_retry);
    });
    *ui.file_thumbs.source.borrow_mut() = Some(source);
}

fn flush_file_thumbs(ui: &Rc<Ui>) {
    if ui.file_thumbs.inflight.get() {
        return;
    }
    let items: Vec<FileThumbRequest> = {
        let mut queue = ui.file_thumbs.queue.borrow_mut();
        let wanted = ui.file_thumbs.wanted.borrow();
        let mut batch = Vec::new();
        while batch.len() < THUMB_BATCH {
            let Some(item) = queue.pop_front() else {
                break;
            };
            if wanted.contains_key(&thumbnail_key(&item.uid, item.modified)) {
                batch.push(item);
            }
        }
        batch
    };
    if items.is_empty() {
        return;
    }

    ui.file_thumbs.inflight.set(true);
    let rx = spawn_request(
        ui.dirs.control_socket(),
        Request::FileThumbs {
            items: items.clone(),
            generation: ui.file_thumbs.generation.get(),
        },
    );
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let result = rx.recv().await;
        ui.file_thumbs.inflight.set(false);
        match result {
            Ok(Ok(Response::Thumbs { items: replies })) => {
                let mut requested: HashMap<String, FileThumbRequest> = items
                    .iter()
                    .cloned()
                    .map(|item| (item.uid.clone(), item))
                    .collect();
                let mut pending = Vec::new();
                for reply in replies {
                    let Some(request) = requested.remove(&reply.uid) else {
                        continue;
                    };
                    let key = thumbnail_key(&request.uid, request.modified);
                    let wanted = ui.file_thumbs.wanted.borrow().contains_key(&key);
                    match reply_disposition(wanted, reply.path.is_some(), reply.pending) {
                        ReplyDisposition::Ignore => continue,
                        ReplyDisposition::Load => {
                            let Some(path) = reply.path else { continue };
                            match gtk4::gdk::Texture::from_filename(&path) {
                                Ok(texture) => {
                                    ui.file_thumbs.store_texture(&key, texture.clone());
                                    if let Some(pictures) =
                                        ui.file_thumbs.wanted.borrow_mut().remove(&key)
                                    {
                                        for picture in pictures {
                                            paint(&picture, &key, &texture);
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!("cannot decode file thumbnail {path}: {e}");
                                    ui.file_thumbs.store_missing(key.clone());
                                    ui.file_thumbs.wanted.borrow_mut().remove(&key);
                                }
                            }
                        }
                        ReplyDisposition::Retry => pending.push(request),
                        ReplyDisposition::Missing => {
                            ui.file_thumbs.store_missing(key.clone());
                            ui.file_thumbs.wanted.borrow_mut().remove(&key);
                        }
                    }
                }
                pending.extend(requested.into_values());
                if !pending.is_empty() {
                    retry_file_thumbs(&ui, pending);
                }
            }
            Ok(Ok(Response::FileThumbsStale)) => {
                ui.file_thumbs.generation.set(0);
                retry_file_thumbs(&ui, items);
            }
            Ok(Ok(Response::Error { message, .. })) => {
                tracing::debug!("file thumbnails failed: {message}");
                retry_file_thumbs(&ui, items);
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {
                tracing::debug!("file thumbnails: no reply");
                retry_file_thumbs(&ui, items);
            }
        }
        schedule_file_thumbs(&ui);
    });
}

fn retry_file_thumbs(ui: &Rc<Ui>, items: Vec<FileThumbRequest>) {
    let ui = ui.clone();
    glib::timeout_add_local_once(THUMB_RETRY, move || {
        let wanted = ui.file_thumbs.wanted.borrow();
        let retry: Vec<FileThumbRequest> = items
            .into_iter()
            .filter(|item| wanted.contains_key(&thumbnail_key(&item.uid, item.modified)))
            .collect();
        drop(wanted);
        ui.file_thumbs.queue.borrow_mut().extend(retry);
        schedule_file_thumbs(&ui);
    });
}

#[cfg(test)]
mod tests {
    use super::{ReplyDisposition, is_image_name, reply_disposition};

    #[test]
    fn image_extensions_are_case_insensitive_and_specific() {
        for name in [
            "photo.jpg",
            "SCAN.PNG",
            "animation.Gif",
            "archive.tiff",
            "camera.nef",
            "negative.CR3",
            "camera.RAW",
        ] {
            assert!(is_image_name(name), "{name}");
        }
        for name in [
            "notes.txt",
            "movie.mp4",
            "image.jpg.zip",
            "jpg",
            "camera.heic",
            "frame.avif",
            "graphic.svg",
            "camera.rw",
        ] {
            assert!(!is_image_name(name), "{name}");
        }
    }

    #[test]
    fn cancelled_listing_replies_cannot_become_permanent_misses() {
        assert_eq!(
            reply_disposition(false, false, false),
            ReplyDisposition::Ignore
        );
        assert_eq!(
            reply_disposition(true, false, false),
            ReplyDisposition::Missing
        );
    }
}
