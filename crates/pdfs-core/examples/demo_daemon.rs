//! A stand-in daemon that serves made-up data, for the README screenshots.
//!
//! `scripts/screenshots.sh` runs it; see `docs/DEVELOPMENT.md`. It answers the
//! control socket the way the real daemon does, for the requests the captured
//! pages send, with an invented account: no Proton login, no FUSE mount and
//! nothing private. Every name, place and number below is made up.
//!
//! ```text
//! demo_daemon serve <photos-dir>   answer the control socket until killed
//! demo_daemon seed-keyring         store a fake session, so the app signs in
//! ```
//!
//! Both refuse to run outside a throwaway environment: `PDFS_DEMO_ROOT` must be
//! set and every XDG directory must lie inside it, so the real daemon's socket
//! is never touched (`HOME` may instead name a folder that does not exist, the
//! demo user's, so paths read as theirs), and `seed-keyring` refuses the session bus the script was
//! started on (`PDFS_DEMO_OUTER_BUS`), so the fake session only ever lands in a
//! private keyring.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, Local, TimeZone};
use pdfs_core::config::{AppDirs, KEYRING_SERVICE};
use pdfs_core::control::Pin;
use pdfs_core::control::{
    ActivityEntry, ActivityKind, AlbumInfo, DeviceInfo, DirEntry, Event, LocalHit, PhotoItem,
    PhotoKind, PhotoLocation, PhotoMonth, PhotoThumb, PlaceInfo, Request, Response, SearchHit,
    SyncFolderInfo, Topic,
};
use pdfs_core::mounts::{MountAccess, MountKind, MountMode, MountSpec};

const USERNAME: &str = "alex.example";
const MOUNTPOINT: &str = "/home/alex/ProtonDrive";
const GIB: u64 = 1 << 30;
const DAY: i64 = 86_400;

/// How many photos the timeline holds. The demo pictures repeat, but not
/// within a screen.
const PHOTOS: usize = 1_840;

fn main() {
    if let Err(e) = run() {
        eprintln!("demo_daemon: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    check_sandbox()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["serve", photos] => serve(Path::new(photos)),
        ["seed-keyring"] => seed_keyring(),
        _ => Err("usage: demo_daemon serve <photos-dir> | seed-keyring".into()),
    }
}

/// Refuse to run anywhere but inside the throwaway root the script made.
fn check_sandbox() -> Result<(), String> {
    let root = std::env::var_os("PDFS_DEMO_ROOT")
        .map(PathBuf::from)
        .ok_or("PDFS_DEMO_ROOT is not set; run scripts/screenshots.sh")?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if home.is_none_or(|home| home.exists() && !home.starts_with(&root)) {
        return Err("HOME must lie inside PDFS_DEMO_ROOT or not exist".into());
    }
    for var in [
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
    ] {
        let inside = std::env::var_os(var)
            .map(PathBuf::from)
            .is_some_and(|dir| dir.starts_with(&root));
        if !inside {
            return Err(format!("{var} must lie inside PDFS_DEMO_ROOT"));
        }
    }
    Ok(())
}

/// Store a made-up session where the app looks for one. Its keys "unlock"
/// without a prompt; nothing ever uses them against Proton.
fn seed_keyring() -> Result<(), String> {
    let bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default();
    let outer =
        std::env::var("PDFS_DEMO_OUTER_BUS").map_err(|_| "PDFS_DEMO_OUTER_BUS is not set")?;
    if bus.is_empty() || bus == outer {
        return Err("refusing to write a fake session to your own keyring".into());
    }
    let session = serde_json::json!({
        "session_id": "demo-session",
        "username": USERNAME,
        "user_id": "demo-user",
        "access_token": "demo",
        "refresh_token": "demo",
        "scopes": ["full", "self", "drive"],
        "password_mode": 1,
        "key_passphrases": { "demo-key": "ZGVtbw==" },
    });
    let store: Arc<keyring_core::CredentialStore> =
        dbus_secret_service_keyring_store::Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    keyring_core::Entry::new(KEYRING_SERVICE, "session")
        .and_then(|entry| entry.set_password(&session.to_string()))
        .map_err(|e| e.to_string())
}

fn serve(photos: &Path) -> Result<(), String> {
    let demo = Arc::new(Demo::new(photos)?);
    let dirs = AppDirs::new().map_err(|e| e.to_string())?;
    dirs.ensure().map_err(|e| e.to_string())?;
    let socket = dirs.control_socket();
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    eprintln!("demo_daemon: serving {}", socket.display());
    for stream in listener.incoming().flatten() {
        let demo = Arc::clone(&demo);
        std::thread::spawn(move || {
            if let Err(e) = connection(&demo, stream) {
                eprintln!("demo_daemon: {e}");
            }
        });
    }
    Ok(())
}

fn connection(demo: &Demo, stream: UnixStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let request: Request = match serde_json::from_str(line.trim()) {
        Ok(request) => request,
        Err(e) => {
            eprintln!("demo_daemon: unreadable request: {e}");
            return Ok(());
        }
    };
    let mut out = stream;
    if matches!(request, Request::Subscribe) {
        return subscription(demo, &mut out);
    }
    send(&mut out, &demo.answer(&request))
}

/// What the real daemon sends a new subscriber, then a heartbeat now and then
/// so the window never counts the daemon as gone.
fn subscription(demo: &Demo, out: &mut UnixStream) -> std::io::Result<()> {
    send(out, &Event::Status(Box::new(demo.status())))?;
    send(
        out,
        &Event::Transfers {
            items: Vec::new(),
            jobs: Vec::new(),
        },
    )?;
    send(
        out,
        &Event::Changed {
            topics: Topic::ALL.to_vec(),
        },
    )?;
    loop {
        std::thread::sleep(Duration::from_secs(10));
        send(out, &Event::Heartbeat)?;
    }
}

fn send(out: &mut UnixStream, value: &impl serde::Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    out.write_all(&line)?;
    out.flush()
}

/// One invented account.
struct Demo {
    now: i64,
    pictures: Vec<PathBuf>,
    photos: Vec<PhotoItem>,
    /// Each photo's place, by index into [`PLACES`].
    place_of: Vec<Option<usize>>,
}

/// Towns the photos were "taken" in: name, ISO country, latitude, longitude.
const PLACES: &[(&str, &str, f64, f64)] = &[
    ("Lisbon", "PT", 38.7223, -9.1393),
    ("Porto", "PT", 41.1579, -8.6291),
    ("Barcelona", "ES", 41.3874, 2.1686),
    ("Paris", "FR", 48.8566, 2.3522),
    ("Amsterdam", "NL", 52.3676, 4.9041),
    ("Copenhagen", "DK", 55.6761, 12.5683),
    ("Berlin", "DE", 52.5200, 13.4050),
    ("Prague", "CZ", 50.0755, 14.4378),
    ("Vienna", "AT", 48.2082, 16.3738),
    ("Florence", "IT", 43.7696, 11.2558),
    ("Rome", "IT", 41.9028, 12.4964),
    ("Thira", "GR", 36.4166, 25.4318),
];

const ALBUMS: &[&str] = &[
    "Summer in Portugal",
    "Family",
    "Hiking",
    "Food",
    "City trips",
    "Best of 2025",
];

impl Demo {
    fn new(dir: &Path) -> Result<Self, String> {
        let mut pictures: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "jpg"))
            .collect();
        pictures.sort();
        if pictures.is_empty() {
            return Err(format!("no .jpg pictures in {}", dir.display()));
        }
        let now = Local::now().timestamp();
        let mut demo = Demo {
            now,
            pictures,
            photos: Vec::new(),
            place_of: Vec::new(),
        };
        demo.make_photos();
        Ok(demo)
    }

    /// The timeline: newest first, a few a week back three years, plus a
    /// photo on this day in each of the last three years.
    fn make_photos(&mut self) {
        let mut seed = 0x5eed_u64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as i64
        };
        let today = Local::now().date_naive();
        let noon = |years_back: i32| {
            let day = today
                .with_year(today.year() - years_back)
                .unwrap_or(today - chrono::Days::new(365 * years_back as u64));
            Local
                .from_local_datetime(&day.and_hms_opt(11, 30, 0).unwrap_or_default())
                .single()
                .map_or(0, |t| t.timestamp())
        };
        // Three years, so a few a day.
        let step = 3 * 365 * DAY / PHOTOS as i64;
        let mut times: Vec<i64> = (0..PHOTOS as i64)
            .map(|i| self.now - 3 * 3600 - i * step - next() % step)
            .collect();
        times.extend([noon(1), noon(2), noon(3)]);
        times.sort_unstable_by(|a, b| b.cmp(a));
        for (i, time) in times.into_iter().enumerate() {
            let picture = &self.pictures[i % self.pictures.len()];
            let kind = match i % 23 {
                7 | 15 => PhotoKind::Video,
                19 => PhotoKind::Raw,
                _ => PhotoKind::Photo,
            };
            let place = (i % 5 != 4).then_some((i / 7) % PLACES.len());
            self.photos.push(PhotoItem {
                uid: format!("demo-photos~{i}"),
                capture_time: time,
                thumb_path: Some(picture.display().to_string()),
                name: Some(format!(
                    "IMG_{}.{}",
                    20_000 + i,
                    if kind == PhotoKind::Video {
                        "MP4"
                    } else {
                        "JPG"
                    }
                )),
                ratio: Some(if picture.to_string_lossy().ends_with("-p.jpg") {
                    2.0 / 3.0
                } else {
                    1.5
                }),
                no_thumb: false,
                kind,
                favorite: i % 11 == 3,
                group_size: 0,
                has_raw: kind == PhotoKind::Raw,
                place_id: place.map(|p| p as u32 + 1),
            });
            self.place_of.push(place);
        }
    }

    fn status(&self) -> Response {
        Response::Status {
            username: USERNAME.into(),
            mountpoint: MOUNTPOINT.into(),
            pinned: 3,
            used: 6 * GIB + 300 * (1 << 20),
            budget: 20 * GIB,
            pins: self.pins(),
            online: true,
            pending_uploads: 0,
            pending_changes: 0,
            parked_uploads: 0,
            failing_ops: 0,
            failing_error: None,
            staged_bytes: 0,
            staged_oldest_secs: 0,
            paused: false,
            paused_until: None,
            session_expired: false,
            tokens_unsaved: false,
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }

    fn answer(&self, request: &Request) -> Response {
        match request {
            Request::Status => self.status(),
            Request::AccountQuota => Response::AccountQuota {
                max_space: 500 * GIB as i64,
                used_space: 137 * GIB as i64,
            },
            Request::ListDir { path } => Response::Entries {
                entries: self.dir(path),
            },
            Request::ListTrash => Response::Entries {
                entries: self.trash(),
            },
            Request::ListTrashedFolder { uid } => Response::Entries {
                entries: self.trashed_folder(uid),
            },
            Request::ReserveFileThumbGeneration => Response::FileThumbGeneration { generation: 1 },
            Request::FileThumbs { items, .. } => Response::Thumbs {
                items: items
                    .iter()
                    .map(|item| PhotoThumb {
                        uid: item.uid.clone(),
                        path: self.file_picture(&item.uid),
                        pending: false,
                    })
                    .collect(),
            },
            Request::PhotosTimeline {
                offset,
                limit,
                kind,
                favorites,
                ..
            } => {
                let items: Vec<PhotoItem> = self
                    .photos
                    .iter()
                    .filter(|p| kind.is_none_or(|k| p.kind == k))
                    .filter(|p| !favorites || p.favorite)
                    .skip(*offset)
                    .take(*limit)
                    .cloned()
                    .collect();
                Response::Photos {
                    available: true,
                    items,
                    counts: Some(self.counts()),
                }
            }
            Request::PhotoMonths { kind } => Response::PhotoMonths {
                months: self.months(*kind),
            },
            Request::PhotosOnThisDay { .. } => {
                let today = Local::now();
                let items = self
                    .photos
                    .iter()
                    .filter(|p| {
                        let at = Local.timestamp_opt(p.capture_time, 0).single();
                        at.is_some_and(|at| {
                            at.year() < today.year()
                                && at.month() == today.month()
                                && at.day() == today.day()
                        })
                    })
                    .cloned()
                    .collect();
                Response::Photos {
                    available: true,
                    items,
                    counts: None,
                }
            }
            Request::PhotoThumbs { uids } => Response::Thumbs {
                items: uids
                    .iter()
                    .map(|uid| PhotoThumb {
                        uid: uid.clone(),
                        path: self
                            .photos
                            .iter()
                            .find(|p| &p.uid == uid)
                            .and_then(|p| p.thumb_path.clone()),
                        pending: false,
                    })
                    .collect(),
            },
            Request::PhotoAlbums => Response::Albums {
                available: true,
                items: self.albums(),
            },
            Request::AlbumPhotos { uid, offset, limit } => {
                let index: usize = uid
                    .rsplit('~')
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                Response::Photos {
                    available: true,
                    items: self
                        .album_photos(index)
                        .skip(*offset)
                        .take(*limit)
                        .cloned()
                        .collect(),
                    counts: None,
                }
            }
            Request::PhotoPlaces => Response::Places {
                items: self.places(),
                mapping: None,
            },
            Request::PlaceLocations { id } => Response::PhotoLocations {
                items: self.place_locations(*id),
            },
            Request::PlacePhotos { id, offset, limit } => Response::Photos {
                available: true,
                items: self
                    .in_place(*id)
                    .map(|(_, p)| p)
                    .skip(*offset)
                    .take(*limit)
                    .cloned()
                    .collect(),
                counts: None,
            },
            Request::ListDevices => Response::Devices {
                items: self.devices(),
            },
            Request::ListSyncFolders => Response::SyncFolders {
                items: self.sync_folders(),
            },
            Request::ListLocations => Response::Locations {
                items: self.locations(),
            },
            // Every conflict in the history is resolved: an open one would
            // badge every page.
            Request::ListConflicts => Response::Conflicts { items: Vec::new() },
            Request::ListPendingOps => Response::PendingOps { items: Vec::new() },
            Request::ListActivity { limit } => Response::Activity {
                items: self.activity().into_iter().take(*limit).collect(),
            },
            Request::ListInvitations => Response::Invitations { items: Vec::new() },
            Request::ListBookmarks => Response::Bookmarks { items: Vec::new() },
            Request::GetQueueStatus => Response::Transfers {
                items: Vec::new(),
                jobs: Vec::new(),
            },
            Request::ListPins => Response::Pins { pins: self.pins() },
            Request::SearchV2 { query, limit, .. } => Response::SearchResultsV2 {
                drive_hits: self.search(query, *limit),
                local_hits: self.local_search(query),
                local_indexing: false,
            },
            Request::Search { query, limit, .. } => Response::SearchResults {
                hits: self.search(query, *limit),
            },
            Request::ImportStatus => Response::ImportStatus {
                running: false,
                summary: None,
                progress: None,
                resumable: Vec::new(),
            },
            Request::PhotosRefreshStatus => Response::PhotosRefresh { running: false },
            other => {
                eprintln!("demo_daemon: no demo answer for {}", request_name(other));
                Response::Ok {
                    message: String::new(),
                }
            }
        }
    }

    fn counts(&self) -> (usize, usize, usize) {
        let of = |kind| self.photos.iter().filter(|p| p.kind == kind).count();
        (
            of(PhotoKind::Photo),
            of(PhotoKind::Video),
            of(PhotoKind::Raw),
        )
    }

    fn months(&self, kind: Option<PhotoKind>) -> Vec<PhotoMonth> {
        let mut months: BTreeMap<(i32, i32), usize> = BTreeMap::new();
        for photo in self
            .photos
            .iter()
            .filter(|p| kind.is_none_or(|k| p.kind == k))
        {
            if let Some(at) = Local.timestamp_opt(photo.capture_time, 0).single() {
                *months.entry((at.year(), at.month() as i32)).or_default() += 1;
            }
        }
        months
            .into_iter()
            .rev()
            .map(|((year, month), count)| PhotoMonth { year, month, count })
            .collect()
    }

    fn albums(&self) -> Vec<AlbumInfo> {
        ALBUMS
            .iter()
            .enumerate()
            .map(|(i, name)| AlbumInfo {
                uid: format!("demo-album~{i}"),
                name: (*name).into(),
                photo_count: self.album_photos(i).count(),
                cover_uid: self.album_photos(i).next().map(|p| p.uid.clone()),
                last_activity: Some(self.now - (i as i64 * 9 + 2) * DAY),
                shared: i == 1,
            })
            .collect()
    }

    fn album_photos(&self, album: usize) -> impl Iterator<Item = &PhotoItem> {
        let stride = 3 + album;
        self.photos
            .iter()
            .skip(album * 2)
            .step_by(stride)
            .take(40 + album * 17)
    }

    fn in_place(&self, id: u32) -> impl Iterator<Item = (usize, &PhotoItem)> {
        self.photos
            .iter()
            .enumerate()
            .filter(move |(i, _)| self.place_of[*i].is_some_and(|p| p as u32 + 1 == id))
    }

    fn places(&self) -> Vec<PlaceInfo> {
        PLACES
            .iter()
            .enumerate()
            .filter_map(|(i, (name, country, latitude, longitude))| {
                let id = i as u32 + 1;
                let count = self.in_place(id).count();
                let cover = self.in_place(id).next()?.1.clone();
                Some(PlaceInfo {
                    id,
                    name: (*name).into(),
                    country: (*country).into(),
                    latitude: *latitude,
                    longitude: *longitude,
                    photo_count: count,
                    cover,
                })
            })
            .collect()
    }

    fn place_locations(&self, id: u32) -> Vec<PhotoLocation> {
        let Some((_, _, latitude, longitude)) = PLACES.get(id as usize - 1) else {
            return Vec::new();
        };
        self.in_place(id)
            .map(|(i, photo)| {
                // Spread around the centre, the same way on every run.
                let angle = i as f64 * 2.399;
                let reach = 0.004 + (i % 9) as f64 * 0.003;
                PhotoLocation {
                    photo: photo.clone(),
                    latitude: latitude + reach * angle.sin(),
                    longitude: longitude + reach * angle.cos(),
                }
            })
            .collect()
    }

    /// The trash: things deleted from a few folders, and a deleted folder with
    /// a file deleted from it before it went.
    fn trash(&self) -> Vec<DirEntry> {
        let item = |name: &str, size: u64, hours: i64, from: &str| DirEntry {
            name: name.into(),
            is_dir: size == 0,
            size,
            modified: self.now - 40 * DAY,
            uid: format!("demo-trash~{from}/{name}"),
            trashed_at: self.now - hours * 3600,
            trashed_from: Some(from.into()),
            ..Default::default()
        };
        let mut inside = item("Draft 3.odt", 48_120, 30, "");
        inside.uid = "demo-trash~Old drafts/Draft 3.odt".into();
        inside.trashed_from = None;
        inside.trashed_parent = Some("demo-trash~Work/Old drafts".into());
        vec![
            item("Budget 2025.ods", 51_200, 2, "Documents"),
            item("Scan 0042.pdf", 1_204_331, 5, "Documents"),
            item("Scan 0043.pdf", 1_198_002, 5, "Documents"),
            item("IMG_2041.jpg", 3_402_112, 20, "Pictures/Lisbon"),
            item("IMG_2042.jpg", 3_311_870, 20, "Pictures/Lisbon"),
            item("IMG_2043.jpg", 3_587_004, 20, "Pictures/Lisbon"),
            item("Old drafts", 0, 26, "Work"),
            item("Meeting notes (copy).md", 6_120, 50, ""),
            item("Invoice 118.pdf", 88_341, 80, "Taxes/2025"),
            inside,
        ]
    }

    /// What the deleted folder in [`Self::trash`] holds.
    fn trashed_folder(&self, uid: &str) -> Vec<DirEntry> {
        if uid != "demo-trash~Work/Old drafts" {
            return Vec::new();
        }
        let file = |name: &str, size: u64| DirEntry {
            name: name.into(),
            size,
            modified: self.now - 50 * DAY,
            uid: format!("demo-trash~Old drafts/{name}"),
            trashed_parent: Some(uid.into()),
            ..Default::default()
        };
        let mut gone = file("Draft 3.odt", 48_120);
        gone.trashed_at = self.now - 30 * 3600;
        vec![
            file("Draft 1.odt", 41_002),
            file("Draft 2.odt", 44_870),
            gone,
        ]
    }

    /// My files: folders first, then a few files, some of them pictures with
    /// thumbnails. Any folder below the root holds the same few documents.
    fn dir(&self, path: &str) -> Vec<DirEntry> {
        let folder = |name: &str, days: i64| DirEntry {
            name: name.into(),
            is_dir: true,
            modified: self.now - days * DAY,
            uid: format!("demo-files~{path}/{name}"),
            path: join(path, name),
            ..Default::default()
        };
        let file = |name: &str, size: u64, days: i64| DirEntry {
            name: name.into(),
            is_dir: false,
            size,
            modified: self.now - days * DAY - 4000,
            uid: format!("demo-files~{path}/{name}"),
            path: join(path, name),
            ..Default::default()
        };
        if !path.trim_matches('/').is_empty() {
            return vec![
                file("Notes.md", 4_812, 1),
                file("Overview.pdf", 912_384, 6),
                file("Budget.ods", 38_112, 12),
            ];
        }
        let mut entries = vec![
            folder("Documents", 1),
            folder("Music", 30),
            folder("Pictures", 2),
            folder("Projects", 4),
            folder("Recipes", 60),
            folder("Taxes", 120),
            folder("Videos", 14),
            folder("Work", 3),
        ];
        let mut pinned = file("Lisbon sunset.jpg", 4_218_112, 20);
        pinned.pinned = true;
        pinned.cached = true;
        entries.extend([
            file("Apartment lease.pdf", 1_482_011, 40),
            file("Budget 2026.ods", 52_340, 2),
            file("Cliffs.jpg", 3_844_201, 33),
            file("Garden.jpg", 2_904_877, 9),
            pinned,
            file("Meeting notes.md", 6_120, 0),
            file("Mountain lake.jpg", 5_120_330, 61),
            file("Packing list.txt", 1_204, 5),
            file("Road trip.jpg", 3_211_008, 75),
            file("Thesis draft.odt", 812_448, 18),
        ]);
        entries
    }

    /// The picture behind a demo file, for its thumbnail.
    fn file_picture(&self, uid: &str) -> Option<String> {
        let name = uid.rsplit('/').next()?;
        if !name.ends_with(".jpg") {
            return None;
        }
        let index = name.bytes().map(usize::from).sum::<usize>() % self.pictures.len();
        Some(self.pictures[index].display().to_string())
    }

    fn pins(&self) -> Vec<Pin> {
        [
            ("Documents", true),
            ("Work/Quarterly report.odt", false),
            ("Lisbon sunset.jpg", false),
        ]
        .into_iter()
        .map(|(path, dir)| Pin {
            uid: format!("demo-files~/{path}"),
            path: (*path).into(),
            recursive: dir,
            is_dir: Some(dir),
            cached: true,
        })
        .collect()
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        vec![
            DeviceInfo {
                uid: "demo-devices~1".into(),
                name: "alex-laptop".into(),
                device_type: "Linux".into(),
                last_sync: Some(self.now - 240),
                this_device: true,
                adopted: true,
            },
            DeviceInfo {
                uid: "demo-devices~2".into(),
                name: "studio-desktop".into(),
                device_type: "Windows".into(),
                last_sync: Some(self.now - 2 * DAY - 3 * 3600),
                this_device: false,
                adopted: false,
            },
            DeviceInfo {
                uid: "demo-devices~3".into(),
                name: "old-macbook".into(),
                device_type: "MacOs".into(),
                last_sync: Some(self.now - 210 * DAY),
                this_device: false,
                adopted: false,
            },
        ]
    }

    /// This computer's synced folders: mirrors, and one kept online only.
    const FOLDERS: &[(&str, &str)] = &[
        ("/home/alex/Documents", "mirror"),
        ("/home/alex/Pictures/Camera", "mirror"),
        ("/home/alex/Music", "ondemand"),
        ("/home/alex/Projects", "mirror"),
    ];

    fn sync_folders(&self) -> Vec<SyncFolderInfo> {
        Self::FOLDERS
            .iter()
            .enumerate()
            .map(|(i, (path, mode))| SyncFolderInfo {
                id: i as i64 + 1,
                kind: "device".into(),
                local_path: (*path).into(),
                remote_uid: format!("demo-sync~{i}"),
                mode: (*mode).into(),
                pending_mode: None,
                state: "idle".into(),
                last_sync: self.now - 240 - i as i64 * 60,
                progress: None,
                paused: false,
            })
            .collect()
    }

    fn locations(&self) -> Vec<MountSpec> {
        let mut items = vec![MountSpec {
            id: 1,
            kind: MountKind::MyFiles,
            local_path: MOUNTPOINT.into(),
            root_uid: "demo-files~".into(),
            root_share_id: String::new(),
            mode: MountMode::OnDemand,
            access: MountAccess::Rw,
            state: "idle".into(),
            last_sync: self.now - 60,
            pending_mode: None,
            mounted: true,
            progress: None,
            paused: false,
        }];
        items.extend(
            Self::FOLDERS
                .iter()
                .enumerate()
                .map(|(i, (path, mode))| MountSpec {
                    id: i as i64 + 2,
                    kind: MountKind::Device {
                        sync_folder_id: i as i64 + 1,
                    },
                    local_path: (*path).into(),
                    root_uid: format!("demo-sync~{i}"),
                    root_share_id: String::new(),
                    mode: MountMode::from(*mode),
                    access: MountAccess::Rw,
                    state: "idle".into(),
                    last_sync: self.now - 240 - i as i64 * 60,
                    pending_mode: None,
                    mounted: *mode == "ondemand",
                    progress: None,
                    paused: false,
                }),
        );
        items
    }

    fn activity(&self) -> Vec<ActivityEntry> {
        let at = |minutes: i64| self.now - minutes * 60;
        let entry = |time, kind, target: &str, detail: &str, ok| ActivityEntry {
            time,
            kind,
            target: target.into(),
            detail: detail.into(),
            ok,
        };
        let conflict = |time, name: &str, ext: &str| {
            entry(
                time,
                ActivityKind::Conflict,
                &format!("{name} (sync-conflict {time}).{ext}"),
                &format!("differs from {name}.{ext}"),
                false,
            )
        };
        vec![
            entry(at(2), ActivityKind::Upload, "Meeting notes.md", "", true),
            entry(
                at(14),
                ActivityKind::Share,
                "Budget 2026.ods",
                "1 recipient(s) as editor",
                true,
            ),
            entry(
                at(38),
                ActivityKind::Upload,
                "Pictures/Camera/IMG_20481.JPG",
                "",
                true,
            ),
            entry(
                at(39),
                ActivityKind::Upload,
                "Pictures/Camera/IMG_20480.JPG",
                "",
                true,
            ),
            entry(
                at(52),
                ActivityKind::Rename,
                "Thesis draft.odt",
                "was thesis-v3.odt",
                true,
            ),
            conflict(at(180), "Quarterly report", "odt"),
            entry(
                at(240),
                ActivityKind::Move,
                "Taxes/2025",
                "from Documents",
                true,
            ),
            entry(
                at(26 * 60),
                ActivityKind::PublicLink,
                "Road trip.jpg",
                "",
                true,
            ),
            conflict(at(26 * 60 + 5), "Packing list", "txt"),
            entry(at(27 * 60), ActivityKind::CreateFolder, "Recipes", "", true),
            conflict(at(28 * 60), "Garden plan", "svg"),
            entry(at(29 * 60), ActivityKind::Trash, "Old invoices", "", true),
            entry(
                at(30 * 60),
                ActivityKind::Restore,
                "Apartment lease.pdf",
                "",
                true,
            ),
            entry(
                at(50 * 60),
                ActivityKind::Upload,
                "Videos/Harbour at dusk.mp4",
                "",
                true,
            ),
        ]
    }

    /// Hits for the search launcher: every demo file whose name holds the
    /// query, or a few to show when nothing does.
    fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        let names = [
            ("Work/Quarterly report.odt", false, 180_912),
            ("Work/Quarterly report 2025.pdf", false, 1_204_334),
            ("Work/Reports", true, 0),
            ("Documents/Expense report March.ods", false, 44_201),
            ("Documents/Lab report.pdf", false, 2_301_442),
            ("Projects/Annual report draft.md", false, 18_400),
            ("Taxes/2025/Tax report.pdf", false, 640_008),
            ("Pictures/Report cover.jpg", false, 2_801_220),
        ];
        let needle = query.to_lowercase();
        names
            .iter()
            .filter(|(path, _, _)| needle.is_empty() || path.to_lowercase().contains(&needle))
            .take(limit)
            .enumerate()
            .map(|(i, (path, is_dir, size))| SearchHit {
                name: path.rsplit('/').next().unwrap_or(path).into(),
                path: (*path).into(),
                is_dir: *is_dir,
                size: *size,
                modified: self.now - (i as i64 * 5 + 1) * DAY,
                pinned: i == 0,
                cached: i == 0,
                uid: format!("demo-files~/{path}"),
                mounted_path: Some(format!("{MOUNTPOINT}/{path}")),
                score: 100 - i as i64,
            })
            .collect()
    }

    fn local_search(&self, query: &str) -> Vec<LocalHit> {
        let needle = query.to_lowercase();
        [("/home/alex/Downloads/report-template.ott", 22_018)]
            .iter()
            .filter(|(path, _)| path.to_lowercase().contains(&needle))
            .map(|(path, size)| LocalHit {
                name: path.rsplit('/').next().unwrap_or(path).into(),
                path: (*path).into(),
                is_dir: false,
                size: *size,
                modified: self.now - 4 * DAY,
                score: 10,
            })
            .collect()
    }
}

fn join(dir: &str, name: &str) -> String {
    let dir = dir.trim_matches('/');
    if dir.is_empty() {
        name.into()
    } else {
        format!("{dir}/{name}")
    }
}

/// The variant name of `request`, for the log of requests the demo ignores.
fn request_name(request: &Request) -> String {
    let json = serde_json::to_string(request).unwrap_or_default();
    json.trim_start_matches(['{', '"'])
        .split('"')
        .next()
        .unwrap_or_default()
        .to_string()
}
