//! The online street map under the Places map: vector tiles from OpenFreeMap,
//! fetched and drawn on a worker thread, and handed to the map as finished
//! images.
//!
//! It is off unless turned on in Preferences, since every tile fetched tells
//! OpenFreeMap which part of the world is being looked at. OpenFreeMap needs
//! no account or key, and its tiles are OpenStreetMap data in the
//! OpenMapTiles schema; both have to be credited wherever the map shows.
//!
//! Tiles are kept on disk under the cache directory, per build of the tile
//! set. A build never changes, so a cached tile is good until OpenFreeMap
//! publishes the next one, and then the old build's directory goes. Town and
//! country names are not baked into the images: they come back as [`Label`]s
//! for the map to set on top, where it can keep them upright, sharp and apart
//! from each other.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gtk4::cairo;

use super::mvt::{self, Kind, Layer};

/// Where OpenFreeMap describes its current tile set.
const TILEJSON_URL: &str = "https://tiles.openfreemap.org/planet";

/// The deepest zoom OpenFreeMap has tiles for. Closer in, a tile is drawn
/// from part of its ancestor at this zoom.
pub(crate) const DATA_MAX_ZOOM: u8 = 14;

/// A tile's edge in logical px at the zoom it was made for.
pub(crate) const TILE_EDGE: f64 = 256.0;

/// How many tiles are fetched at once.
const PARALLEL_FETCHES: usize = 6;

/// How long one fetch may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Past this size the tile cache is emptied rather than left to grow.
const DISK_BUDGET: u64 = 256 * 1024 * 1024;

/// One tile of the Web Mercator grid: at zoom `z` the world is `2^z` tiles
/// wide and tall, counted from the north-west.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TileKey {
    pub(crate) z: u8,
    pub(crate) x: u32,
    pub(crate) y: u32,
}

impl TileKey {
    pub(crate) fn parent(self) -> Option<Self> {
        (self.z > 0).then(|| Self {
            z: self.z - 1,
            x: self.x / 2,
            y: self.y / 2,
        })
    }

    /// The tile whose data this one is drawn from.
    fn data(self) -> Self {
        let mut key = self;
        while key.z > DATA_MAX_ZOOM {
            key = key.parent().unwrap_or(key);
        }
        key
    }
}

/// The colours of the map, for a light or a dark window.
pub(crate) struct Palette {
    pub(crate) land: u32,
    pub(crate) water: u32,
    wood: u32,
    grass: u32,
    ice: u32,
    sand: u32,
    residential: u32,
    industrial: u32,
    building: u32,
    motorway: u32,
    primary: u32,
    road: u32,
    casing: u32,
    rail: u32,
    path: u32,
    pub(crate) border: u32,
    state_border: u32,
    pub(crate) coast: u32,
    pub(crate) text: u32,
    pub(crate) halo: u32,
    pub(crate) water_text: u32,
}

pub(crate) const LIGHT: Palette = Palette {
    land: 0xf4f2ee,
    water: 0xb3cde3,
    wood: 0xd3e5c5,
    grass: 0xdcebcf,
    ice: 0xf2f6f9,
    sand: 0xefe7d2,
    residential: 0xebe7e2,
    industrial: 0xebe4e8,
    building: 0xdcd5ce,
    motorway: 0xf2c98f,
    primary: 0xfbe4ad,
    road: 0xffffff,
    casing: 0xd2cbc1,
    rail: 0xb5afa8,
    path: 0xbfae9c,
    border: 0x9d94b5,
    state_border: 0xc2bbd2,
    coast: 0x9fbbd4,
    text: 0x34313d,
    halo: 0xffffff,
    water_text: 0x4f7398,
};

pub(crate) const DARK: Palette = Palette {
    land: 0x24222d,
    water: 0x141c2b,
    wood: 0x212c26,
    grass: 0x232d27,
    ice: 0x2c313a,
    sand: 0x2d2b27,
    residential: 0x292733,
    industrial: 0x2b2833,
    building: 0x322f3c,
    motorway: 0x7a6647,
    primary: 0x625a4b,
    road: 0x46434f,
    casing: 0x1a1822,
    rail: 0x4e4a5a,
    path: 0x3f3b49,
    border: 0x857ca0,
    state_border: 0x5c556f,
    coast: 0x2c3b52,
    text: 0xe7e4ef,
    halo: 0x16141c,
    water_text: 0x86a6cb,
};

pub(crate) fn palette(dark: bool) -> &'static Palette {
    if dark { &DARK } else { &LIGHT }
}

pub(crate) fn set_rgb(cr: &cairo::Context, color: u32, alpha: f64) {
    cr.set_source_rgba(
        f64::from((color >> 16) & 0xff) / 255.0,
        f64::from((color >> 8) & 0xff) / 255.0,
        f64::from(color & 0xff) / 255.0,
        alpha,
    );
}

/// What kind of place a name stands for, which sets how it is written and
/// which names win when they would overlap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum LabelClass {
    Country,
    State,
    City,
    Town,
    Village,
    Suburb,
    Hamlet,
    Water,
}

impl LabelClass {
    fn from_place(class: &str) -> Option<Self> {
        Some(match class {
            "country" => Self::Country,
            "state" | "province" => Self::State,
            "city" => Self::City,
            "town" => Self::Town,
            "village" => Self::Village,
            "suburb" | "quarter" => Self::Suburb,
            "hamlet" | "neighbourhood" | "isolated_dwelling" => Self::Hamlet,
            _ => return None,
        })
    }

    /// The zoom from which such names are shown at all.
    pub(crate) fn min_zoom(self) -> u8 {
        match self {
            Self::Country | Self::Water | Self::City => 0,
            Self::State => 4,
            Self::Town => 8,
            Self::Village => 11,
            Self::Suburb => 12,
            Self::Hamlet => 14,
        }
    }

    /// The zoom past which such names are left out again: a country's name
    /// is no help once its towns are.
    pub(crate) fn max_zoom(self) -> u8 {
        match self {
            Self::Country => 7,
            Self::State => 9,
            _ => u8::MAX,
        }
    }
}

/// A name to set on the map, at its place in world space (0..1 both ways).
#[derive(Clone, Debug)]
pub(crate) struct Label {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) text: String,
    pub(crate) class: LabelClass,
    /// Lower is more important, within a class.
    pub(crate) rank: f64,
}

/// A finished tile: premultiplied ARGB pixels, as cairo keeps them.
pub(crate) struct Raster {
    pub(crate) key: TileKey,
    pub(crate) dark: bool,
    pub(crate) edge: i32,
    pub(crate) pixels: Vec<u8>,
    pub(crate) labels: Vec<Label>,
}

impl Raster {
    pub(crate) fn surface(self) -> Option<cairo::ImageSurface> {
        let stride = self.edge * 4;
        cairo::ImageSurface::create_for_data(
            self.pixels,
            cairo::Format::ARgb32,
            self.edge,
            self.edge,
            stride,
        )
        .ok()
    }
}

pub(crate) enum Done {
    Tile(Raster),
    /// Nothing came: no network, or no such tile. The outline shows there.
    Failed(TileKey),
    /// The tile went off screen before its turn.
    Dropped(TileKey),
}

struct Job {
    key: TileKey,
    dark: bool,
}

/// The worker thread making tiles. Dropping it stops the thread once the
/// fetches under way are done.
pub(crate) struct Tiles {
    jobs: async_channel::Sender<Job>,
    wanted: Arc<Mutex<HashSet<TileKey>>>,
    /// Device pixels per logical px the tiles are made at.
    pub(crate) scale: i32,
}

impl Tiles {
    /// Start the worker, caching under `cache_dir`, drawing names in
    /// `language` where the map has them. Finished tiles arrive on the
    /// returned channel.
    pub(crate) fn start(
        cache_dir: PathBuf,
        language: String,
        scale: i32,
    ) -> (Self, async_channel::Receiver<Done>) {
        let (jobs, job_rx) = async_channel::unbounded();
        let (done_tx, done_rx) = async_channel::unbounded();
        let wanted = Arc::new(Mutex::new(HashSet::new()));
        let worker = Worker {
            dir: cache_dir,
            language,
            scale,
            wanted: wanted.clone(),
            done: done_tx,
        };
        let spawned = std::thread::Builder::new()
            .name("map-tiles".into())
            .spawn(move || worker.run(job_rx));
        if let Err(e) = spawned {
            tracing::warn!("map tiles: cannot start the worker: {e}");
        }
        (
            Self {
                jobs,
                wanted,
                scale,
            },
            done_rx,
        )
    }

    pub(crate) fn request(&self, key: TileKey, dark: bool) {
        let _ = self.jobs.try_send(Job { key, dark });
    }

    /// The tiles on screen now. A queued tile that isn't is skipped.
    pub(crate) fn set_wanted(&self, keys: HashSet<TileKey>) {
        if let Ok(mut wanted) = self.wanted.lock() {
            *wanted = keys;
        }
    }
}

struct Worker {
    dir: PathBuf,
    language: String,
    scale: i32,
    wanted: Arc<Mutex<HashSet<TileKey>>>,
    done: async_channel::Sender<Done>,
}

impl Worker {
    fn run(self, jobs: async_channel::Receiver<Job>) {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                tracing::warn!("map tiles: no runtime: {e}");
                return;
            }
        };
        let client = match reqwest::Client::builder()
            .user_agent(concat!(
                "proton-drive-linux/",
                env!("CARGO_PKG_VERSION"),
                " (",
                env!("CARGO_PKG_REPOSITORY"),
                ")"
            ))
            .timeout(FETCH_TIMEOUT)
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                tracing::warn!("map tiles: no HTTP client: {e}");
                return;
            }
        };
        let worker = Arc::new(self);
        runtime.block_on(async move {
            let source = Arc::new(tokio::sync::Mutex::new(None::<String>));
            let slots = Arc::new(tokio::sync::Semaphore::new(PARALLEL_FETCHES));
            while let Ok(job) = jobs.recv().await {
                let Ok(permit) = slots.clone().acquire_owned().await else {
                    break;
                };
                let (worker, client, source) = (worker.clone(), client.clone(), source.clone());
                tokio::spawn(async move {
                    let done = worker.make(&client, &source, job).await;
                    let _ = worker.done.send(done).await;
                    drop(permit);
                });
            }
            // Let the tiles under way finish before the runtime goes.
            let _ = slots.acquire_many(PARALLEL_FETCHES as u32).await;
        });
    }

    async fn make(
        self: &Arc<Self>,
        client: &reqwest::Client,
        source: &tokio::sync::Mutex<Option<String>>,
        job: Job,
    ) -> Done {
        let key = job.key;
        let still_wanted = self.wanted.lock().map(|w| w.contains(&key)).unwrap_or(true);
        if !still_wanted {
            return Done::Dropped(key);
        }
        let Some(data) = self.load(client, source, key.data()).await else {
            return Done::Failed(key);
        };
        let worker = self.clone();
        let made = tokio::task::spawn_blocking(move || {
            let layers = mvt::decode(&data)?;
            render(&layers, key, job.dark, worker.scale, &worker.language)
        })
        .await;
        match made {
            Ok(Some(raster)) => Done::Tile(raster),
            _ => Done::Failed(key),
        }
    }

    /// The tile's data, from the disk cache or else from OpenFreeMap.
    async fn load(
        &self,
        client: &reqwest::Client,
        source: &tokio::sync::Mutex<Option<String>>,
        key: TileKey,
    ) -> Option<Vec<u8>> {
        let template = {
            let mut source = source.lock().await;
            if source.is_none() {
                *source = self.tile_source(client).await;
            }
            source.clone()?
        };
        let path = self
            .build_dir(&template)?
            .join(key.z.to_string())
            .join(key.x.to_string())
            .join(format!("{}.pbf", key.y));
        if let Ok(data) = tokio::fs::read(&path).await {
            return Some(data);
        }
        let url = template
            .replace("{z}", &key.z.to_string())
            .replace("{x}", &key.x.to_string())
            .replace("{y}", &key.y.to_string());
        let reply = client.get(&url).send().await.ok()?;
        if !reply.status().is_success() {
            tracing::debug!("map tile {url}: {}", reply.status());
            return None;
        }
        let data = reply.bytes().await.ok()?.to_vec();
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::write(&path, &data).await;
        Some(data)
    }

    /// The URL template of OpenFreeMap's current build, remembered on disk so
    /// the cached tiles still show without a network.
    async fn tile_source(&self, client: &reqwest::Client) -> Option<String> {
        let saved = self.dir.join("tiles.json");
        let fetched = async {
            let reply = client.get(TILEJSON_URL).send().await.ok()?;
            if !reply.status().is_success() {
                return None;
            }
            reply.bytes().await.ok()
        }
        .await;
        let (json, fresh) = match fetched {
            Some(json) => (json.to_vec(), true),
            None => (tokio::fs::read(&saved).await.ok()?, false),
        };
        let template = tile_template(&json)?;
        if fresh {
            let _ = tokio::fs::create_dir_all(&self.dir).await;
            let _ = tokio::fs::write(&saved, &json).await;
            let (dir, build) = (self.dir.clone(), build_name(&template)?);
            let _ = tokio::task::spawn_blocking(move || prune(&dir, &build)).await;
        }
        Some(template)
    }

    fn build_dir(&self, template: &str) -> Option<PathBuf> {
        Some(self.dir.join(build_name(template)?))
    }
}

/// The first tile URL template in a TileJSON document.
fn tile_template(json: &[u8]) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_slice(json).ok()?;
    let template = doc.get("tiles")?.get(0)?.as_str()?;
    template
        .starts_with("https://")
        .then(|| template.to_owned())
}

/// The tile set's build, the path segment before `/{z}`: a safe directory
/// name, or `None`.
fn build_name(template: &str) -> Option<String> {
    let head = template.split("/{z}").next()?;
    let name = head.rsplit('/').next()?;
    let safe = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        && name != "."
        && name != "..";
    safe.then(|| name.to_owned())
}

/// Remove the cached builds other than `keep`, and `keep` itself once it
/// holds more than [`DISK_BUDGET`].
fn prune(dir: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let stale = entry.file_name() != keep || dir_size(&path) > DISK_BUDGET;
        if stale && let Err(e) = std::fs::remove_dir_all(&path) {
            tracing::debug!("map tiles: cannot remove {}: {e}", path.display());
        }
    }
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => dir_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Draw tile `key` from the layers of its data tile, at `scale` device px
/// per logical px, and gather the names on it.
fn render(
    layers: &[Layer],
    key: TileKey,
    dark: bool,
    scale: i32,
    language: &str,
) -> Option<Raster> {
    let edge = (TILE_EDGE as i32) * scale.max(1);
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, edge, edge).ok()?;
    let labels = {
        let cr = cairo::Context::new(&surface).ok()?;
        cr.scale(f64::from(scale), f64::from(scale));
        let painter = Painter {
            cr: &cr,
            key,
            data: key.data(),
            colors: palette(dark),
        };
        painter.paint(layers);
        gather_labels(layers, key.data(), language)
    };
    surface.flush();
    let pixels = surface.take_data().ok()?.to_vec();
    Some(Raster {
        key,
        dark,
        edge,
        pixels,
        labels,
    })
}

struct Painter<'a> {
    cr: &'a cairo::Context,
    key: TileKey,
    data: TileKey,
    colors: &'static Palette,
}

impl Painter<'_> {
    fn paint(&self, layers: &[Layer]) {
        let c = self.colors;
        let z = self.key.z;
        set_rgb(self.cr, c.land, 1.0);
        let _ = self.cr.paint();

        let layer = |name: &str| layers.iter().find(|l| l.name == name);
        if let Some(layer) = layer("landcover") {
            self.fill(layer, |class| match class {
                "wood" | "forest" => Some(c.wood),
                "grass" | "wetland" => Some(c.grass),
                "ice" | "glacier" => Some(c.ice),
                "sand" | "beach" => Some(c.sand),
                _ => None,
            });
        }
        if let Some(layer) = layer("landuse") {
            self.fill(layer, |class| match class {
                "residential" | "suburb" | "neighbourhood" => Some(c.residential),
                "industrial" | "commercial" | "retail" | "railway" => Some(c.industrial),
                "stadium" | "pitch" | "playground" | "cemetery" => Some(c.grass),
                _ => None,
            });
        }
        if let Some(layer) = layer("park") {
            self.fill(layer, |_| Some(c.grass));
        }
        if let Some(layer) = layer("water") {
            self.fill(layer, |_| Some(c.water));
        }
        if let Some(layer) = layer("waterway") {
            self.stroke(layer, |class| {
                let (base, from) = match class {
                    "river" => (1.2, 8),
                    "canal" => (1.0, 12),
                    "stream" => (0.6, 13),
                    _ => return None,
                };
                (z >= from).then(|| (c.water, grow(base, z), false))
            });
        }
        if z >= 13
            && let Some(layer) = layer("building")
        {
            self.fill(layer, |_| Some(c.building));
        }
        if let Some(layer) = layer("transportation") {
            self.roads(layer);
        }
        if let Some(layer) = layer("boundary") {
            self.borders(layer);
        }
    }

    /// Map tile units of the data tile onto this tile's logical px.
    fn to_px(&self, layer: &Layer, (u, v): (f64, f64)) -> (f64, f64) {
        let k = f64::from(1u32 << (self.key.z - self.data.z));
        let ox = f64::from(self.key.x) - f64::from(self.data.x) * k;
        let oy = f64::from(self.key.y) - f64::from(self.data.y) * k;
        (
            (u / layer.extent * k - ox) * TILE_EDGE,
            (v / layer.extent * k - oy) * TILE_EDGE,
        )
    }

    fn trace(&self, layer: &Layer, part: &[(f64, f64)], close: bool) {
        for (i, &point) in part.iter().enumerate() {
            let (x, y) = self.to_px(layer, point);
            if i == 0 {
                self.cr.move_to(x, y);
            } else {
                self.cr.line_to(x, y);
            }
        }
        if close {
            self.cr.close_path();
        }
    }

    /// Fill the layer's polygons in the colour `color` picks by class.
    fn fill(&self, layer: &Layer, color: impl Fn(&str) -> Option<u32>) {
        self.cr.set_fill_rule(cairo::FillRule::EvenOdd);
        for feature in &layer.features {
            if feature.kind != Kind::Polygon {
                continue;
            }
            let class = layer.text(feature, "class").unwrap_or("");
            let Some(rgb) = color(class) else {
                continue;
            };
            self.cr.new_path();
            for part in &feature.parts {
                self.trace(layer, part, true);
            }
            set_rgb(self.cr, rgb, 1.0);
            let _ = self.cr.fill();
        }
    }

    /// Stroke the layer's lines as `style` says by class: colour, width in
    /// px, and whether dashed.
    fn stroke(&self, layer: &Layer, style: impl Fn(&str) -> Option<(u32, f64, bool)>) {
        self.cr.set_line_cap(cairo::LineCap::Round);
        self.cr.set_line_join(cairo::LineJoin::Round);
        for feature in &layer.features {
            if feature.kind != Kind::Line {
                continue;
            }
            let class = layer.text(feature, "class").unwrap_or("");
            let Some((rgb, width, dashed)) = style(class) else {
                continue;
            };
            self.cr.new_path();
            for part in &feature.parts {
                self.trace(layer, part, false);
            }
            set_rgb(self.cr, rgb, 1.0);
            self.cr.set_line_width(width);
            let dash = [width * 2.0, width * 2.0];
            self.cr.set_dash(if dashed { &dash } else { &[] }, 0.0);
            let _ = self.cr.stroke();
        }
        self.cr.set_dash(&[], 0.0);
    }

    /// Roads, minor first so the major ones cross over them, each on a
    /// darker casing once they are wide enough for one.
    fn roads(&self, layer: &Layer) {
        let c = self.colors;
        let z = self.key.z;
        let road = |class: &str| -> Option<(u32, f64, u8)> {
            let (color, width, rank) = match class {
                "motorway" if z >= 6 => (c.motorway, grow(2.2, z), 4),
                "trunk" if z >= 7 => (c.motorway, grow(2.2, z), 4),
                "primary" if z >= 8 => (c.primary, grow(1.8, z), 3),
                "secondary" if z >= 10 => (c.road, grow(1.4, z), 2),
                "tertiary" if z >= 11 => (c.road, grow(1.4, z), 2),
                "minor" | "busway" if z >= 12 => (c.road, grow(1.0, z), 1),
                "service" if z >= 14 => (c.road, grow(0.6, z), 0),
                _ => return None,
            };
            Some((color, width, rank))
        };
        for rank in 0..=4u8 {
            let pick = |class: &str| road(class).filter(|r| r.2 == rank);
            if z >= 12 {
                self.stroke(layer, |class| {
                    pick(class).map(|(_, width, _)| (c.casing, width + 1.5, false))
                });
            }
            self.stroke(layer, |class| {
                pick(class).map(|(color, width, _)| (color, width, false))
            });
        }
        self.stroke(layer, |class| match class {
            "rail" | "transit" if z >= 10 => Some((c.rail, grow(0.7, z).min(2.5), false)),
            "path" | "track" if z >= 14 => Some((c.path, 0.8, true)),
            _ => None,
        });
    }

    fn borders(&self, layer: &Layer) {
        let c = self.colors;
        let z = self.key.z;
        self.cr.set_line_cap(cairo::LineCap::Round);
        for feature in &layer.features {
            if feature.kind != Kind::Line || layer.number(feature, "maritime") == Some(1.0) {
                continue;
            }
            let (color, width, dashed) = match layer.number(feature, "admin_level") {
                Some(level) if level <= 2.0 => {
                    (c.border, (0.9 * 1.12f64.powi(i32::from(z))).min(2.5), false)
                }
                Some(level) if level <= 4.0 && z >= 5 => (c.state_border, 0.8, true),
                _ => continue,
            };
            let dashed = dashed || layer.number(feature, "disputed") == Some(1.0);
            self.cr.new_path();
            for part in &feature.parts {
                self.trace(layer, part, false);
            }
            set_rgb(self.cr, color, 1.0);
            self.cr.set_line_width(width);
            self.cr
                .set_dash(if dashed { &[3.0, 2.5] } else { &[] }, 0.0);
            let _ = self.cr.stroke();
        }
        self.cr.set_dash(&[], 0.0);
    }
}

/// A line width that grows with the zoom, as the things it stands for get
/// bigger on screen.
fn grow(base: f64, z: u8) -> f64 {
    (base * 1.4f64.powi(i32::from(z) - 10)).clamp(0.5, 24.0)
}

/// The names of towns, countries and seas on data tile `data`, in world
/// space. Names in the tile's buffer beyond its edge are left to the
/// neighbour they belong to.
fn gather_labels(layers: &[Layer], data: TileKey, language: &str) -> Vec<Label> {
    let n = f64::from(1u32 << data.z);
    let mut labels = Vec::new();
    for layer in layers {
        let water = match layer.name.as_str() {
            "place" => false,
            "water_name" => true,
            _ => continue,
        };
        for feature in &layer.features {
            if feature.kind != Kind::Point {
                continue;
            }
            let class = if water {
                LabelClass::Water
            } else {
                let Some(class) = layer
                    .text(feature, "class")
                    .and_then(LabelClass::from_place)
                else {
                    continue;
                };
                class
            };
            let Some(text) = label_name(layer, feature, language) else {
                continue;
            };
            let rank = layer.number(feature, "rank").unwrap_or(99.0);
            for &(u, v) in feature.parts.iter().flatten() {
                if !(0.0..layer.extent).contains(&u) || !(0.0..layer.extent).contains(&v) {
                    continue;
                }
                labels.push(Label {
                    x: (f64::from(data.x) + u / layer.extent) / n,
                    y: (f64::from(data.y) + v / layer.extent) / n,
                    text: text.clone(),
                    class,
                    rank,
                });
            }
        }
    }
    labels
}

/// A feature's name in `language` where the map has it, else in Latin
/// letters, else as written locally.
fn label_name(layer: &Layer, feature: &mvt::Feature, language: &str) -> Option<String> {
    [
        format!("name:{language}"),
        format!("name_{language}"),
        "name:latin".to_owned(),
        "name".to_owned(),
    ]
    .iter()
    .find_map(|key| layer.text(feature, key).filter(|name| !name.is_empty()))
    .map(|name| name.replace('\n', " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_tiles_draw_from_their_ancestor_at_the_data_zoom() {
        let key = TileKey {
            z: 16,
            x: 34_336,
            y: 22_437,
        };
        assert_eq!(
            key.data(),
            TileKey {
                z: 14,
                x: 8_584,
                y: 5_609
            }
        );
        let shallow = TileKey { z: 3, x: 4, y: 2 };
        assert_eq!(shallow.data(), shallow);
    }

    #[test]
    fn the_build_comes_from_the_tile_template() {
        let json = br#"{"tiles":["https://tiles.openfreemap.org/planet/20260913_164504_pt/{z}/{x}/{y}.pbf"]}"#;
        let template = tile_template(json).unwrap();
        assert_eq!(build_name(&template).as_deref(), Some("20260913_164504_pt"));
        assert_eq!(build_name("https://example.org/../{z}/{x}/{y}.pbf"), None);
        assert_eq!(
            tile_template(br#"{"tiles":["http://plain.example/{z}"]}"#),
            None
        );
    }

    #[test]
    fn an_old_build_is_pruned_and_the_current_one_kept() {
        let dir = std::env::temp_dir().join(format!("pdfs-map-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("old/1/0")).unwrap();
        std::fs::create_dir_all(dir.join("new/1/0")).unwrap();
        std::fs::write(dir.join("new/1/0/0.pbf"), b"tile").unwrap();
        std::fs::write(dir.join("tiles.json"), b"{}").unwrap();
        prune(&dir, "new");
        assert!(!dir.join("old").exists());
        assert!(dir.join("new/1/0/0.pbf").exists());
        assert!(dir.join("tiles.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn roads_widen_as_the_map_zooms_in() {
        assert!(grow(1.4, 14) > grow(1.4, 10));
        assert_eq!(grow(2.2, 30), 24.0);
    }
}
