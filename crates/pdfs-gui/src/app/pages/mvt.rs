//! A reader for Mapbox Vector Tiles, the format the online Places map's tiles
//! come in: protobuf, one message per tile, holding named layers of point,
//! line and polygon features with key/value tags.
//!
//! Only what drawing needs is read — layer names, tags and geometry — and a
//! tile that doesn't parse is `None` rather than an error: the map then shows
//! the outline underneath it. The format is at
//! <https://github.com/mapbox/vector-tile-spec>.

/// One layer of a tile, such as `water` or `transportation`.
pub(crate) struct Layer {
    pub(crate) name: String,
    /// The tile's width in geometry units; 4096 almost everywhere.
    pub(crate) extent: f64,
    keys: Vec<String>,
    values: Vec<Value>,
    pub(crate) features: Vec<Feature>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Text(String),
    Number(f64),
    Bool(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Point,
    Line,
    Polygon,
}

pub(crate) struct Feature {
    pub(crate) kind: Kind,
    /// Pairs of indices into the layer's keys and values.
    tags: Vec<(u32, u32)>,
    /// Points, lines or polygon rings, in tile units. A point feature holds
    /// one part with all its points.
    pub(crate) parts: Vec<Vec<(f64, f64)>>,
}

impl Layer {
    /// The value `feature` carries for `key`.
    pub(crate) fn tag(&self, feature: &Feature, key: &str) -> Option<&Value> {
        feature.tags.iter().find_map(|&(k, v)| {
            (self.keys.get(k as usize)? == key).then(|| self.values.get(v as usize))?
        })
    }

    pub(crate) fn text(&self, feature: &Feature, key: &str) -> Option<&str> {
        match self.tag(feature, key)? {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn number(&self, feature: &Feature, key: &str) -> Option<f64> {
        match self.tag(feature, key)? {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }
}

/// Every layer of the tile in `data`.
pub(crate) fn decode(data: &[u8]) -> Option<Vec<Layer>> {
    let mut tile = Reader::new(data);
    let mut layers = Vec::new();
    while let Some((field, wire)) = tile.field()? {
        match field {
            3 => layers.push(decode_layer(tile.bytes()?)?),
            _ => tile.skip(wire)?,
        }
    }
    Some(layers)
}

fn decode_layer(data: &[u8]) -> Option<Layer> {
    let mut layer = Layer {
        name: String::new(),
        extent: 4096.0,
        keys: Vec::new(),
        values: Vec::new(),
        features: Vec::new(),
    };
    let mut reader = Reader::new(data);
    while let Some((field, wire)) = reader.field()? {
        match field {
            1 => layer.name = reader.text()?,
            2 => {
                if let Some(feature) = decode_feature(reader.bytes()?) {
                    layer.features.push(feature);
                }
            }
            3 => layer.keys.push(reader.text()?),
            4 => layer.values.push(decode_value(reader.bytes()?)?),
            5 => layer.extent = reader.varint()? as f64,
            _ => reader.skip(wire)?,
        }
    }
    Some(layer)
}

fn decode_value(data: &[u8]) -> Option<Value> {
    let mut reader = Reader::new(data);
    let mut value = Value::Bool(false);
    while let Some((field, wire)) = reader.field()? {
        value = match field {
            1 => Value::Text(reader.text()?),
            2 => Value::Number(f64::from(f32::from_bits(reader.fixed32()?))),
            3 => Value::Number(f64::from_bits(reader.fixed64()?)),
            4 | 5 => Value::Number(reader.varint()? as i64 as f64),
            6 => Value::Number(zigzag(reader.varint()?) as f64),
            7 => Value::Bool(reader.varint()? != 0),
            _ => {
                reader.skip(wire)?;
                continue;
            }
        };
    }
    Some(value)
}

/// A feature, or `None` for one this map can't draw (unknown geometry type,
/// or broken).
fn decode_feature(data: &[u8]) -> Option<Feature> {
    let mut reader = Reader::new(data);
    let mut kind = None;
    let mut tags = Vec::new();
    let mut geometry = Vec::new();
    while let Some((field, wire)) = reader.field()? {
        match field {
            2 => {
                let packed = reader.packed()?;
                tags = packed
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&[k, v]| (k as u32, v as u32))
                    .collect();
            }
            3 => {
                kind = match reader.varint()? {
                    1 => Some(Kind::Point),
                    2 => Some(Kind::Line),
                    3 => Some(Kind::Polygon),
                    _ => None,
                }
            }
            4 => geometry = reader.packed()?,
            _ => reader.skip(wire)?,
        }
    }
    let kind = kind?;
    Some(Feature {
        kind,
        tags,
        parts: decode_geometry(kind, &geometry)?,
    })
}

/// Turn the command stream of a feature's geometry into its parts. Positions
/// are deltas from the previous one, carried across parts.
fn decode_geometry(kind: Kind, commands: &[u64]) -> Option<Vec<Vec<(f64, f64)>>> {
    let mut parts: Vec<Vec<(f64, f64)>> = Vec::new();
    let (mut x, mut y) = (0i64, 0i64);
    let mut i = 0;
    while i < commands.len() {
        let command = commands[i] & 7;
        let count = (commands[i] >> 3) as usize;
        i += 1;
        match command {
            // MoveTo starts a part — except for points, which all share one.
            1 | 2 => {
                if i + 2 * count > commands.len() {
                    return None;
                }
                for n in 0..count {
                    x += zigzag(commands[i + 2 * n]);
                    y += zigzag(commands[i + 2 * n + 1]);
                    let starts = command == 1 && (kind != Kind::Point || parts.is_empty());
                    if starts {
                        parts.push(Vec::new());
                    }
                    parts.last_mut()?.push((x as f64, y as f64));
                }
                i += 2 * count;
            }
            // ClosePath: the ring's end joins its start, which drawing does.
            7 => {}
            _ => return None,
        }
    }
    Some(parts)
}

fn zigzag(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

/// The protobuf wire format, as far as vector tiles use it.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    /// The next field's number and wire type; `Some(None)` at the end, `None`
    /// on a broken message.
    fn field(&mut self) -> Option<Option<(u64, u8)>> {
        if self.at >= self.data.len() {
            return Some(None);
        }
        let key = self.varint()?;
        Some(Some((key >> 3, (key & 7) as u8)))
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.data.get(self.at)?;
            self.at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let bytes = self.data.get(self.at..end)?;
        self.at = end;
        Some(bytes)
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = usize::try_from(self.varint()?).ok()?;
        self.take(len)
    }

    fn text(&mut self) -> Option<String> {
        Some(String::from_utf8_lossy(self.bytes()?).into_owned())
    }

    fn fixed32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn fixed64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn packed(&mut self) -> Option<Vec<u64>> {
        let mut inner = Reader::new(self.bytes()?);
        let mut values = Vec::new();
        while inner.at < inner.data.len() {
            values.push(inner.varint()?);
        }
        Some(values)
    }

    fn skip(&mut self, wire: u8) -> Option<()> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => {
                self.take(8)?;
            }
            2 => {
                self.bytes()?;
            }
            5 => {
                self.take(4)?;
            }
            _ => return None,
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut n: u64, out: &mut Vec<u8>) {
        while n >= 0x80 {
            out.push((n as u8) | 0x80);
            n >>= 7;
        }
        out.push(n as u8);
    }

    fn field(number: u64, bytes: &[u8], out: &mut Vec<u8>) {
        varint(number << 3 | 2, out);
        varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }

    fn packed(values: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        for &v in values {
            varint(v, &mut out);
        }
        out
    }

    fn zz(n: i64) -> u64 {
        ((n << 1) ^ (n >> 63)) as u64
    }

    /// A tile with one `water` layer holding one square tagged class=lake.
    fn lake_tile() -> Vec<u8> {
        let mut feature = Vec::new();
        field(2, &packed(&[0, 0]), &mut feature);
        varint(3 << 3, &mut feature);
        varint(3, &mut feature);
        let square = [
            1 | 1 << 3,
            zz(10),
            zz(10),
            2 | 3 << 3,
            zz(20),
            zz(0),
            zz(0),
            zz(20),
            zz(-20),
            zz(0),
            7 | 1 << 3,
        ];
        field(4, &packed(&square), &mut feature);

        let mut value = Vec::new();
        field(1, b"lake", &mut value);

        let mut layer = Vec::new();
        field(1, b"water", &mut layer);
        field(2, &feature, &mut layer);
        field(3, b"class", &mut layer);
        field(4, &value, &mut layer);
        varint(5 << 3, &mut layer);
        varint(4096, &mut layer);

        let mut tile = Vec::new();
        field(3, &layer, &mut tile);
        tile
    }

    #[test]
    fn a_polygon_decodes_with_its_tags() {
        let layers = decode(&lake_tile()).unwrap();
        let [water] = &layers[..] else {
            panic!("one layer expected");
        };
        assert_eq!(water.name, "water");
        assert_eq!(water.extent, 4096.0);
        let feature = &water.features[0];
        assert_eq!(feature.kind, Kind::Polygon);
        assert_eq!(water.text(feature, "class"), Some("lake"));
        assert_eq!(water.text(feature, "name"), None);
        assert_eq!(
            feature.parts,
            vec![vec![(10.0, 10.0), (30.0, 10.0), (30.0, 30.0), (10.0, 30.0)]]
        );
    }

    #[test]
    fn a_cut_off_tile_is_rejected() {
        let tile = lake_tile();
        assert!(decode(&tile[..tile.len() - 3]).is_none());
    }

    #[test]
    fn points_of_one_feature_share_a_part() {
        let commands = [1 | 2 << 3, zz(5), zz(5), zz(1), zz(-1)];
        let parts = decode_geometry(Kind::Point, &commands).unwrap();
        assert_eq!(parts, vec![vec![(5.0, 5.0), (6.0, 4.0)]]);
    }
}
