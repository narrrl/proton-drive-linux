//! Host Proton Gallery's account and catalog services in the Drive daemon.

mod video;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use pdfs_core::control::{
    GalleryAction, GalleryAlbum, GalleryFolder, GalleryPlace, PhotoItem, PhotoKind, Response,
};
use pdfs_core::{CoreError, CoreResult};
use pg_app::{App, View};
use pg_core::CancellationToken;
use pg_core::model::{AssetKey, SourceId};
use proton_drive_rs::proton_sdk::session::ProtonApiSession;
use tokio::sync::{Mutex, Notify, OnceCell};

use super::{Core, NodeUid, parse_uid};

pub(super) struct Gallery {
    root: PathBuf,
    session: ProtonApiSession,
    cancel: CancellationToken,
    app: OnceCell<App>,
    checked: Mutex<Option<Instant>>,
    visibility: RwLock<Option<Arc<Visibility>>>,
    refresh: Notify,
    access: AtomicU64,
    video: video::Server,
}

#[derive(Default, PartialEq, Eq)]
pub(super) struct Visibility {
    pub(super) media: HashSet<String>,
    pub(super) albums: HashSet<String>,
}

impl Visibility {
    pub(super) fn permits(&self, uid: &str) -> CoreResult<()> {
        if self.media.contains(uid) || self.albums.contains(uid) {
            return Err(CoreError::invalid("Unlock hidden albums to open this item"));
        }
        Ok(())
    }
}

impl Gallery {
    pub(super) fn new(root: PathBuf, session: ProtonApiSession) -> Self {
        Self {
            root,
            session,
            cancel: CancellationToken::new(),
            app: OnceCell::new(),
            checked: Mutex::new(None),
            visibility: RwLock::default(),
            refresh: Notify::new(),
            access: AtomicU64::new(0),
            video: video::Server::default(),
        }
    }

    pub(super) fn cancel(&self) {
        self.cancel.cancel();
        self.video.close_all();
    }

    async fn app(&self, core: &Core) -> CoreResult<&App> {
        self.app
            .get_or_try_init(|| async {
                App::open_hosted(
                    self.root.clone(),
                    self.session.clone(),
                    core.client.clone(),
                    self.cancel.child_token(),
                )
                .await
                .map_err(gallery_error)
            })
            .await
    }

    async fn ensure_visibility(
        &self,
        core: &Core,
        app: &App,
        force: bool,
    ) -> CoreResult<Arc<Visibility>> {
        if self.cancel.is_cancelled() {
            return Err(CoreError::remote("The Gallery session expired"));
        }
        let mut checked = self.checked.lock().await;
        if !force
            && let Some(instant) = *checked
            && instant.elapsed()
                < if self.snapshot().is_some() {
                    Duration::from_secs(60)
                } else {
                    Duration::from_secs(5)
                }
        {
            return self.snapshot().ok_or_else(visibility_loading);
        }
        if self.snapshot().is_none()
            && app
                .gallery_visibility_cached()
                .await
                .map_err(gallery_error)?
        {
            self.publish_visibility(core, self.read_visibility(core, app).await?);
        }
        let started = Instant::now();
        let result = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(CoreError::remote("The Gallery session expired")),
            result = tokio::time::timeout(Duration::from_secs(20), async {
                app.refresh_gallery_configuration(false).await.map_err(gallery_error)?;
                self.read_visibility(core, app).await
            }) => result.unwrap_or_else(|_| Err(CoreError::remote("Gallery visibility check timed out"))),
        };
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            success = result.is_ok(),
            "Gallery visibility refresh"
        );
        *checked = Some(Instant::now());
        if let Ok(visibility) = result {
            self.publish_visibility(core, visibility.clone());
            return Ok(visibility);
        }
        if !force && let Some(visibility) = self.snapshot() {
            return Ok(visibility);
        }
        result
    }

    fn snapshot(&self) -> Option<Arc<Visibility>> {
        self.visibility
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn publish_visibility(&self, core: &Core, visibility: Arc<Visibility>) {
        let mut snapshot = self.visibility.write().unwrap_or_else(|e| e.into_inner());
        if snapshot.as_ref() != Some(&visibility) {
            *snapshot = Some(visibility);
            drop(snapshot);
            self.video.close_all();
            core.events
                .publish(&[pdfs_core::control::Topic::GalleryVisibilitySynced]);
        }
    }

    async fn read_visibility(&self, core: &Core, app: &App) -> CoreResult<Arc<Visibility>> {
        let account = self.account()?;
        let mut visibility = Visibility::default();
        for (volume, node) in app
            .catalog
            .hidden_assets(account.clone())
            .await
            .map_err(gallery_error)?
        {
            visibility
                .media
                .insert(NodeUid::new(volume.into(), node.into()).to_string());
        }
        visibility.media.extend(
            core.db
                .photo_group_members_for(&visibility.media)
                .map_err(CoreError::from)?,
        );
        for album in app
            .catalog
            .hidden_configuration_albums(account)
            .await
            .map_err(gallery_error)?
        {
            visibility.albums.insert(
                NodeUid::new(album.volume.as_str().into(), album.node.as_str().into()).to_string(),
            );
        }
        Ok(Arc::new(visibility))
    }

    fn account(&self) -> CoreResult<pg_core::model::AccountId> {
        pg_core::model::AccountId::new(self.session.user_id().as_str()).map_err(gallery_error)
    }

    async fn page(
        &self,
        app: &App,
        album: Option<GalleryPlace>,
        proton: bool,
        access: Option<u64>,
        offset: usize,
        limit: usize,
    ) -> CoreResult<Response> {
        let private = access.is_some();
        let parent = album
            .as_ref()
            .map(|a| (a.gallery.as_str(), a.album.as_str()));
        let mut albums = Vec::new();
        if offset == 0 && !proton {
            if private && parent.is_none() {
                albums.extend(
                    app.photo_albums_with_hidden(true)
                        .await
                        .map_err(gallery_error)?
                        .into_iter()
                        .filter(|entry| entry.settings.hidden)
                        .map(|entry| gallery_album(entry, true)),
                );
                albums.extend(
                    app.hidden_drive_albums()
                        .await
                        .map_err(gallery_error)?
                        .into_iter()
                        .map(|entry| gallery_album(entry, false)),
                );
            } else {
                albums.extend(
                    app.drive_albums(parent, private)
                        .await
                        .map_err(gallery_error)?
                        .into_iter()
                        .map(|entry| gallery_album(entry, false)),
                );
            }
        }
        let (items, total) = if let Some(GalleryPlace { gallery, album }) = album {
            let view = if proton && private {
                let id = SourceId::new(&album).map_err(gallery_error)?;
                if !app
                    .catalog
                    .album_settings(id)
                    .await
                    .map_err(gallery_error)?
                    .hidden
                {
                    return Err(CoreError::not_found("Hidden album"));
                }
                View::PhotoAlbum(album)
            } else {
                if private
                    && !app
                        .catalog
                        .drive_album_is_private(
                            SourceId::new(&gallery).map_err(gallery_error)?,
                            SourceId::new(&album).map_err(gallery_error)?,
                        )
                        .await
                        .map_err(gallery_error)?
                {
                    return Err(CoreError::not_found("Hidden gallery folder"));
                }
                View::DriveAlbum { gallery, album }
            };
            let outline = app
                .outline_with_visibility(view, false, private)
                .await
                .map_err(gallery_error)?;
            let total = outline.len();
            let items = app
                .outline_items(&outline, offset.min(total), limit.clamp(1, 250))
                .await
                .map_err(gallery_error)?
                .into_iter()
                .flatten()
                .map(|item| PhotoItem {
                    uid: asset_uid(&item.key),
                    capture_time: item.sort_time,
                    thumb_path: None,
                    name: Some(item.name),
                    ratio: item.aspect_ratio,
                    no_thumb: false,
                    kind: if item.is_video {
                        PhotoKind::Video
                    } else {
                        PhotoKind::Photo
                    },
                    favorite: item.favorite,
                    group_size: 1,
                    has_raw: false,
                    place_id: None,
                })
                .collect();
            (items, total)
        } else {
            (Vec::new(), 0)
        };
        self.require_access(access)?;
        Ok(Response::GalleryPage {
            albums,
            items,
            total,
        })
    }

    fn lock(&self) {
        let _ = self
            .access
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                Some((value | 1).wrapping_add(1))
            });
        self.video.close_all();
    }

    fn private_access(&self) -> CoreResult<u64> {
        let access = self.access.load(Ordering::SeqCst);
        if access & 1 == 0 {
            return Err(CoreError::invalid("Unlock hidden albums to open this item"));
        }
        Ok(access)
    }

    fn require_access(&self, access: Option<u64>) -> CoreResult<()> {
        if let Some(access) = access
            && self.private_access()? != access
        {
            return Err(CoreError::invalid("Hidden albums were locked"));
        }
        Ok(())
    }

    pub(super) async fn poll(self: Arc<Self>, core: Core) {
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break,
                _ = self.refresh.notified() => {},
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            }
            if let Ok(Ok(app)) = tokio::time::timeout(Duration::from_secs(5), self.app(&core)).await
            {
                let _ = self.ensure_visibility(&core, app, false).await;
            }
        }
    }

    fn asset(&self, uid: &NodeUid) -> CoreResult<AssetKey> {
        Ok(AssetKey {
            account: self.account()?,
            volume: pg_core::model::VolumeId::new(uid.volume_id.as_str()).map_err(gallery_error)?,
            node: pg_core::model::NodeId::new(uid.link_id.as_str()).map_err(gallery_error)?,
        })
    }
}

fn gallery_album(entry: pg_core::library::Album, proton: bool) -> GalleryAlbum {
    GalleryAlbum {
        place: GalleryPlace {
            gallery: entry.gallery.to_string(),
            album: entry.source.id.to_string(),
        },
        proton,
        folder: GalleryFolder {
            uid: NodeUid::new(
                entry.source.volume.as_str().into(),
                entry.source.root.as_str().into(),
            )
            .to_string(),
            name: entry.settings.title.unwrap_or(entry.source.label),
        },
        cover: entry.cover.map(|asset| asset_uid(&asset.key)),
        items: entry.direct_items,
        folders: entry.child_albums,
        hidden: entry.settings.hidden,
    }
}

fn gallery_error(error: impl std::fmt::Display) -> CoreError {
    CoreError::remote(error.to_string())
}

fn uid(value: &str) -> CoreResult<NodeUid> {
    parse_uid(value).ok_or_else(|| CoreError::invalid("Invalid gallery node"))
}

fn done() -> Response {
    Response::Ok {
        message: "Gallery updated".into(),
    }
}

fn asset_uid(asset: &AssetKey) -> String {
    NodeUid::new(asset.volume.as_str().into(), asset.node.as_str().into()).to_string()
}

fn visibility_loading() -> CoreError {
    CoreError::new(
        pdfs_core::control::ErrorKind::Offline,
        "Gallery visibility is loading. Try again shortly.",
    )
}

impl Core {
    pub(super) fn cached_gallery_visibility(&self) -> CoreResult<Arc<Visibility>> {
        if self.gallery.is_none() {
            return Ok(Arc::default());
        }
        let gallery = self.gallery_host()?;
        match gallery.snapshot() {
            Some(snapshot) => Ok(snapshot),
            None => {
                gallery.refresh.notify_one();
                Err(visibility_loading())
            }
        }
    }

    pub(super) fn gallery_visibility(&self) -> CoreResult<Arc<Visibility>> {
        if self.gallery.is_none() {
            return Ok(Arc::default());
        }
        let gallery = self.gallery_host()?;
        self.rt.block_on(async {
            let app = gallery.app(self).await?;
            gallery.ensure_visibility(self, app, false).await
        })
    }

    fn gallery_host(&self) -> CoreResult<&Gallery> {
        self.gallery
            .as_deref()
            .filter(|gallery| !gallery.cancel.is_cancelled())
            .ok_or_else(|| CoreError::remote("The Gallery session is unavailable"))
    }

    pub(super) fn authenticate_hidden_gallery(
        &self,
        stream: &std::os::unix::net::UnixStream,
    ) -> CoreResult<Response> {
        let gallery = self.gallery_host()?;
        let access = gallery.access.load(Ordering::SeqCst);
        let peer = rustix::net::sockopt::socket_peercred(stream)
            .map_err(|_| CoreError::invalid("Couldn’t identify the authentication requester."))?;
        if peer.uid != rustix::process::geteuid() {
            return Err(CoreError::invalid(
                "Use your Linux sign-in to unlock Hidden albums.",
            ));
        }
        let subject = pg_app::desktop_auth::Subject::new(
            peer.pid.as_raw_nonzero().get() as u32,
            peer.uid.as_raw(),
        )
        .map_err(gallery_error)?;
        self.rt
            .block_on(subject.authenticate("io.narl.proton-drive-linux.unlock-hidden-albums"))
            .map_err(gallery_error)?;
        if gallery.cancel.is_cancelled() {
            return Err(CoreError::invalid("The Gallery session has ended."));
        }
        gallery
            .access
            .compare_exchange(access, access | 1, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| CoreError::invalid("Hidden albums were locked"))?;
        Ok(done())
    }

    pub(super) fn drive_gallery(&self, action: GalleryAction) -> CoreResult<Response> {
        let gallery = self.gallery_host()?;
        let action = match action {
            GalleryAction::Lock => {
                gallery.lock();
                return Ok(done());
            }
            other => other,
        };
        self.rt.block_on(async {
            let app = gallery.app(self).await?;
            gallery
                .ensure_visibility(self, app, matches!(action, GalleryAction::Refresh))
                .await?;
            match action {
                GalleryAction::AuthenticateHidden | GalleryAction::Lock => unreachable!(),
                GalleryAction::List {
                    album,
                    offset,
                    limit,
                } => gallery.page(app, album, false, None, offset, limit).await,
                GalleryAction::PrivateList {
                    album,
                    proton,
                    offset,
                    limit,
                } => {
                    let access = gallery.private_access()?;
                    gallery
                        .page(app, album, proton, Some(access), offset, limit)
                        .await
                }
                GalleryAction::Folders { parent } => {
                    let list = app
                        .browse_drive_folders(parent.as_deref().map(uid).transpose()?)
                        .await
                        .map_err(gallery_error)?;
                    Ok(Response::GalleryFolders {
                        folders: list
                            .children
                            .into_iter()
                            .map(|folder| GalleryFolder {
                                uid: folder.uid.to_string(),
                                name: folder.name,
                            })
                            .collect(),
                    })
                }
                GalleryAction::Add { uid: node } => {
                    app.add_drive_folder(uid(&node)?)
                        .await
                        .map_err(gallery_error)?;
                    gallery.ensure_visibility(self, app, true).await?;
                    Ok(done())
                }
                GalleryAction::Remove { gallery: id } => {
                    app.forget_drive_gallery(SourceId::new(id).map_err(gallery_error)?)
                        .await
                        .map_err(gallery_error)?;
                    gallery.ensure_visibility(self, app, true).await?;
                    Ok(done())
                }
                GalleryAction::Refresh => {
                    for album in app.drive_albums(None, true).await.map_err(gallery_error)? {
                        let root = NodeUid::new(
                            album.source.volume.as_str().into(),
                            album.source.root.as_str().into(),
                        );
                        app.scan_drive_folder(root).await.map_err(gallery_error)?;
                    }
                    gallery.ensure_visibility(self, app, true).await?;
                    Ok(done())
                }
                GalleryAction::Cover { album, uid: node } => {
                    let id = SourceId::new(&album.album).map_err(gallery_error)?;
                    let root = SourceId::new(&album.gallery).map_err(gallery_error)?;
                    app.checked_album(album.gallery, album.album)
                        .await
                        .map_err(gallery_error)?;
                    let access = if app
                        .catalog
                        .drive_album_is_private(root.clone(), id.clone())
                        .await
                        .map_err(gallery_error)?
                    {
                        Some(gallery.private_access()?)
                    } else {
                        None
                    };
                    let mut settings = app
                        .catalog
                        .album_settings(id.clone())
                        .await
                        .map_err(gallery_error)?;
                    gallery.require_access(access)?;
                    settings.cover = Some(gallery.asset(&uid(&node)?)?);
                    app.catalog
                        .set_album_settings_at(Some(root), id, settings)
                        .await
                        .map_err(gallery_error)?;
                    gallery.ensure_visibility(self, app, true).await?;
                    Ok(done())
                }
            }
        })
    }

    pub(super) fn open_gallery_video(&self, node: &str, private: bool) -> CoreResult<Response> {
        let gallery = self.gallery_host()?;
        let uid = uid(node)?;
        self.rt.block_on(async {
            let app = gallery.app(self).await?;
            let visibility = gallery.ensure_visibility(self, app, false).await?;
            let access = if private {
                Some(gallery.private_access()?)
            } else {
                visibility.permits(node)?;
                None
            };
            let stream = app
                .stream_video(&gallery.asset(&uid)?)
                .await
                .map_err(gallery_error)?;
            let response = gallery.video.open(stream, &gallery.cancel).await?;
            if let Err(error) = gallery.require_access(access) {
                if let Response::GalleryVideo { token, .. } = &response {
                    gallery.video.close(token);
                }
                return Err(error);
            }
            Ok(response)
        })
    }

    pub(super) fn gallery_image(&self, node: &str) -> CoreResult<Response> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let gallery = self.gallery_host()?;
        let access = gallery.private_access()?;
        let asset = gallery.asset(&uid(node)?)?;
        self.rt.block_on(async {
            let app = gallery.app(self).await?;
            let bytes = tokio::time::timeout(Duration::from_secs(30), async {
                if let Some(bytes) = app.preview(&asset).await? {
                    Ok(bytes)
                } else {
                    app.original_in_memory(&asset, 8 * 1024 * 1024).await
                }
            })
            .await
            .map_err(|_| CoreError::remote("Proton took too long to open this photo"))?
            .map_err(gallery_error)?;
            gallery.require_access(Some(access))?;
            Ok(Response::GalleryThumbnail {
                data: Some(STANDARD.encode(bytes)),
            })
        })
    }

    pub(super) fn close_gallery_video(&self, token: &str) -> CoreResult<Response> {
        self.gallery_host()?.video.close(token);
        Ok(done())
    }

    pub(super) fn gallery_thumbnail(&self, node: &str, private: bool) -> CoreResult<Response> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let gallery = self.gallery_host()?;
        let asset = gallery.asset(&uid(node)?)?;
        self.rt.block_on(async {
            let app = gallery.app(self).await?;
            let visibility = gallery.ensure_visibility(self, app, false).await?;
            let access = if private {
                Some(gallery.private_access()?)
            } else {
                visibility.permits(node)?;
                None
            };
            let data = app
                .thumbnail(&asset)
                .await
                .map_err(gallery_error)?
                .map(|bytes| STANDARD.encode(bytes));
            gallery.require_access(access)?;
            Ok(Response::GalleryThumbnail { data })
        })
    }
}
