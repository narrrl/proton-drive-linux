//! Dragging photos out of the timeline into a file manager or another app.
//!
//! A photo is not a file anywhere the receiver can read until it has been
//! downloaded, and a drag has to start the moment the pointer moves. So the
//! drag offers a list of files without having them: [`PhotoDrag`] downloads
//! the photos only when the receiver asks for the list, which it does once the
//! photos are dropped on it, and then hands over copies under their own names.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use gtk4::gdk;
use gtk4::gdk::subclass::prelude::*;

use crate::*;

/// The one format a drag of photos offers. GTK turns it into a file list for
/// a receiver in this app, and every file manager reads it.
const URI_LIST: &str = "text/uri-list";

/// How long the copies handed to a receiver are kept. The receiver copies
/// them on drop, so a day is far more than it needs.
const KEEP_DRAGGED: Duration = Duration::from_secs(24 * 60 * 60);

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct PhotoDrag {
        /// The photos dragged, as uid and Drive name.
        pub(super) photos: RefCell<Vec<(String, Option<String>)>>,
        pub(super) socket: RefCell<PathBuf>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PhotoDrag {
        const NAME: &'static str = "PdfsPhotoDrag";
        type Type = super::PhotoDrag;
        type ParentType = gdk::ContentProvider;
    }

    impl ObjectImpl for PhotoDrag {}

    impl ContentProviderImpl for PhotoDrag {
        fn formats(&self) -> gdk::ContentFormats {
            gdk::ContentFormats::new(&[URI_LIST])
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>> + 'static>> {
            let photos = self.photos.borrow().clone();
            let socket = self.socket.borrow().clone();
            let stream = stream.clone();
            let wanted = mime_type == URI_LIST;
            Box::pin(async move {
                if !wanted {
                    return Err(glib::Error::new(
                        gio::IOErrorEnum::NotSupported,
                        "photos are offered as a list of files only",
                    ));
                }
                let files = fetch_dragged(socket, photos).await?;
                let list: String = files
                    .iter()
                    .map(|path| format!("{}\r\n", gio::File::for_path(path).uri()))
                    .collect();
                stream
                    .write_all_future(list.into_bytes(), io_priority)
                    .await
                    .map_err(|(_, e)| e)?;
                Ok(())
            })
        }
    }
}

glib::wrapper! {
    /// The content of a drag of photos: their files, fetched when dropped.
    pub struct PhotoDrag(ObjectSubclass<imp::PhotoDrag>)
        @extends gdk::ContentProvider;
}

impl PhotoDrag {
    pub(crate) fn new(socket: PathBuf, photos: Vec<(String, Option<String>)>) -> Self {
        let drag: Self = glib::Object::new();
        *drag.imp().photos.borrow_mut() = photos;
        *drag.imp().socket.borrow_mut() = socket;
        drag
    }
}

/// Download each dragged photo and put a copy under its Drive name where the
/// receiver can read it. A photo that fails is left out; only when every one
/// fails does the drop fail.
async fn fetch_dragged(
    socket: PathBuf,
    photos: Vec<(String, Option<String>)>,
) -> Result<Vec<PathBuf>, glib::Error> {
    let base = glib::user_cache_dir().join("pdfs-drag");
    let mut files = Vec::new();
    let mut last_error = None;
    for (uid, name) in photos {
        let rx = spawn_request(socket.clone(), Request::OpenPhoto { uid: uid.clone() });
        let source = match rx.recv().await {
            Ok(Ok(Response::FilePath { path })) => path,
            Ok(Ok(Response::Error { message, .. })) => {
                last_error = Some(message);
                continue;
            }
            _ => {
                last_error = Some("the Proton Drive service didn't respond".to_string());
                continue;
            }
        };
        let base = base.clone();
        let copied = gio::spawn_blocking(move || {
            prune_dragged(&base, SystemTime::now());
            let name = name.unwrap_or_else(|| "photo".to_string());
            hand_over(&base, Path::new(&source), &uid, &name)
        })
        .await;
        match copied {
            Ok(Ok(path)) => files.push(path),
            Ok(Err(e)) => last_error = Some(e.to_string()),
            Err(_) => last_error = Some("the copy was interrupted".to_string()),
        }
    }
    if files.is_empty() {
        let message = last_error.unwrap_or_else(|| "nothing to drop".to_string());
        tracing::warn!("dragging photos out failed: {message}");
        return Err(glib::Error::new(gio::IOErrorEnum::Failed, &message));
    }
    Ok(files)
}

/// Put the cached photo `source` at `base/<uid>/<name>`, where a receiver
/// copies it from.
///
/// A hard link when the cache is on the same file system, so a large video is
/// not copied twice; a copy otherwise. Not a symlink: a file manager copies a
/// symlink as a link, which would leave the user a link into the cache.
pub(crate) fn hand_over(
    base: &Path,
    source: &Path,
    uid: &str,
    name: &str,
) -> std::io::Result<PathBuf> {
    let file_name = Path::new(name)
        .file_name()
        .ok_or_else(|| std::io::Error::other("the photo has no usable name"))?;
    let dir = base.join(uid.replace('/', "_"));
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(file_name);
    let _ = std::fs::remove_file(&dest);
    if std::fs::hard_link(source, &dest).is_err() {
        std::fs::copy(source, &dest)?;
    }
    Ok(dest)
}

/// Remove the copies handed over more than [`KEEP_DRAGGED`] before `now`.
pub(crate) fn prune_dragged(base: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| {
                now.duration_since(modified)
                    .is_ok_and(|age| age > KEEP_DRAGGED)
            });
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pdfs-drag-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_dragged_photo_is_handed_over_under_its_own_name() {
        let dir = scratch("name");
        let source = dir.join("0f3a9c");
        std::fs::write(&source, b"jpeg bytes").unwrap();
        let base = dir.join("out");

        let path = hand_over(&base, &source, "vol~link", "IMG_0001.JPG").unwrap();
        assert_eq!(path, base.join("vol~link").join("IMG_0001.JPG"));
        assert_eq!(std::fs::read(&path).unwrap(), b"jpeg bytes");
        assert!(!path.is_symlink());

        // Dragged again, it replaces the copy rather than failing on it.
        std::fs::write(&source, b"new bytes").unwrap();
        let again = hand_over(&base, &source, "vol~link", "IMG_0001.JPG").unwrap();
        assert_eq!(std::fs::read(again).unwrap(), b"new bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_cannot_climb_out_of_its_folder() {
        let dir = scratch("climb");
        let source = dir.join("blob");
        std::fs::write(&source, b"x").unwrap();
        let base = dir.join("out");
        let path = hand_over(&base, &source, "u", "../../escape.jpg").unwrap();
        assert_eq!(path, base.join("u").join("escape.jpg"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_old_handovers_are_pruned() {
        let dir = scratch("prune");
        std::fs::create_dir_all(dir.join("fresh")).unwrap();
        prune_dragged(&dir, SystemTime::now());
        assert!(dir.join("fresh").exists());
        prune_dragged(&dir, SystemTime::now() + KEEP_DRAGGED * 2);
        assert!(!dir.join("fresh").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
