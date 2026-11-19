//! Host Proton Gallery's account and catalog services in the Drive daemon.

mod video;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
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
use tokio::sync::{Mutex, OnceCell};

use super::{Core, NodeUid, parse_uid};

pub(super) struct Gallery {
    root: PathBuf,
    session: ProtonApiSession,
    cancel: CancellationToken,
    app: OnceCell<App>,
    checked: Mutex<Option<(Instant, Option<Arc<Visibility>>)>>,
    unlocked: AtomicBool,
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
            unlocked: AtomicBool::new(false),
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
            && let Some((instant, Some(visibility))) = checked.as_ref()
            && instant.elapsed() < Duration::from_secs(60)
        {
            return Ok(visibility.clone());
        }
        let previous = checked.as_ref().map(|(_, visibility)| visibility.as_ref());
        let visibility = async {
            let account = self.account()?;
            app.refresh_gallery_configuration(false)
                .await
                .map_err(gallery_error)?;
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
                    NodeUid::new(album.volume.as_str().into(), album.node.as_str().into())
                        .to_string(),
                );
            }
            Ok::<_, CoreError>(Arc::new(visibility))
        }
        .await;
        let current = visibility.as_ref().ok();
        if previous.is_some_and(|previous| previous != current) {
            self.video.close_all();
            core.events
                .publish(&[pdfs_core::control::Topic::GalleryVisibilitySynced]);
        }
        *checked = Some((Instant::now(), current.cloned()));
        visibility
    }

    fn account(&self) -> CoreResult<pg_core::model::AccountId> {
        pg_core::model::AccountId::new(self.session.user_id().as_str()).map_err(gallery_error)
    }

    fn lock(&self) {
        self.unlocked.store(false, Ordering::SeqCst);
        self.video.close_all();
    }

    fn unlocked(&self) -> bool {
        self.unlocked.load(Ordering::SeqCst)
    }

    pub(super) async fn poll(self: Arc<Self>, core: Core) {
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            }
            if let Some(app) = self.app.get() {
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

impl Core {
    pub(super) fn gallery_visibility(&self) -> CoreResult<Arc<Visibility>> {
        if self.gallery.is_none() {
            return Ok(Arc::default());
        }
        let gallery = self.gallery_host()?;
        self.rt.block_on(async {
            let app = gallery.app(self).await?;
            let visibility = gallery.ensure_visibility(self, app, false).await?;
            Ok(if gallery.unlocked() {
                Arc::default()
            } else {
                visibility
            })
        })
    }

    fn gallery_host(&self) -> CoreResult<&Gallery> {
        self.gallery
            .as_deref()
            .filter(|gallery| !gallery.cancel.is_cancelled())
            .ok_or_else(|| CoreError::remote("The Gallery session is unavailable"))
    }

    pub(super) fn drive_gallery(&self, action: GalleryAction) -> CoreResult<Response> {
        let gallery = self.gallery_host()?;
        let action = match action {
            GalleryAction::Unlock { passphrase } => {
                let passphrase = zeroize::Zeroizing::new(passphrase);
                pdfs_core::auth::unlock_gallery(gallery.session.username(), &passphrase)
                    .map_err(gallery_error)?;
                gallery.unlocked.store(true, Ordering::SeqCst);
                return Ok(done());
            }
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
            let unlocked = gallery.unlocked();
            match action {
                GalleryAction::Unlock { .. } | GalleryAction::Lock => unreachable!(),
                GalleryAction::List {
                    album,
                    offset,
                    limit,
                } => {
                    let parent = album
                        .as_ref()
                        .map(|a| (a.gallery.as_str(), a.album.as_str()));
                    let albums = if offset == 0 {
                        app.drive_albums(parent, unlocked)
                            .await
                            .map_err(gallery_error)?
                            .into_iter()
                            .map(|entry| GalleryAlbum {
                                place: GalleryPlace {
                                    gallery: entry.gallery.to_string(),
                                    album: entry.source.id.to_string(),
                                },
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
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let (items, total) = if let Some(GalleryPlace { gallery, album }) = album {
                        let view = app
                            .outline_with_visibility(
                                View::DriveAlbum { gallery, album },
                                false,
                                unlocked,
                            )
                            .await
                            .map_err(gallery_error)?;
                        let total = view.len();
                        let items = app
                            .outline_items(&view, offset.min(total), limit.clamp(1, 250))
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
                    Ok(Response::GalleryPage {
                        albums,
                        items,
                        total,
                    })
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
                    app.outline(View::DriveAlbum {
                        gallery: album.gallery,
                        album: album.album,
                    })
                    .await
                    .map_err(gallery_error)?;
                    let mut settings = app
                        .catalog
                        .album_settings(id.clone())
                        .await
                        .map_err(gallery_error)?;
                    settings.cover = Some(gallery.asset(&uid(&node)?)?);
                    app.catalog
                        .set_album_settings(id, settings)
                        .await
                        .map_err(gallery_error)?;
                    gallery.ensure_visibility(self, app, true).await?;
                    Ok(done())
                }
            }
        })
    }

    pub(super) fn open_gallery_video(&self, node: &str) -> CoreResult<Response> {
        let gallery = self.gallery_host()?;
        let uid = uid(node)?;
        self.rt.block_on(async {
            let app = gallery.app(self).await?;
            let visibility = gallery.ensure_visibility(self, app, false).await?;
            if !gallery.unlocked() {
                visibility.permits(node)?;
            }
            let stream = app
                .stream_video(&gallery.asset(&uid)?)
                .await
                .map_err(gallery_error)?;
            gallery.video.open(stream, &gallery.cancel).await
        })
    }

    pub(super) fn close_gallery_video(&self, token: &str) -> CoreResult<Response> {
        self.gallery_host()?.video.close(token);
        Ok(done())
    }

    pub(super) fn gallery_thumbnail(&self, node: &str) -> CoreResult<Response> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let gallery = self.gallery_host()?;
        let asset = gallery.asset(&uid(node)?)?;
        self.rt.block_on(async {
            let app = gallery.app(self).await?;
            let visibility = gallery.ensure_visibility(self, app, false).await?;
            if !gallery.unlocked() {
                visibility.permits(node)?;
            }
            let data = app
                .thumbnail(&asset)
                .await
                .map_err(gallery_error)?
                .map(|bytes| STANDARD.encode(bytes));
            Ok(Response::GalleryThumbnail { data })
        })
    }
}
