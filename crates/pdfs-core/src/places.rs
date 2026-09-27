//! Offline reverse geocoding: the town a photo was taken in, from its GPS
//! position, without asking any service.
//!
//! The towns are GeoNames' `cities15000` dump (every place with more than
//! 15,000 people), trimmed to id, name, position and country in
//! `data/cities15000.tsv` by `scripts/geonames-cities.py`. The script drops the
//! parts of a city, such as Kreuzberg or "Paris 15 Vaugirard", so that a photo
//! lands in the town people would name. GeoNames publishes the dump under
//! CC BY 4.0, which is why the About dialog credits it.
//!
//! `data/countries.tsv` maps each country code to its ISO 3166-1 English name,
//! as the `iso-codes` project spells it, so a front end can translate it
//! through that project's `iso_3166-1` gettext domain.

use std::collections::HashMap;
use std::sync::OnceLock;

/// How far from the nearest town a photo may be and still count as taken
/// there, in km. Farther out (at sea, in the wilderness) it has no place.
pub const MAX_DISTANCE_KM: f64 = 50.0;

/// Mean Earth radius in km, for the haversine distance.
const EARTH_RADIUS_KM: f64 = 6371.0;

const CITIES: &str = include_str!("../data/cities15000.tsv");
const COUNTRIES: &str = include_str!("../data/countries.tsv");

/// One town of the table.
#[derive(Debug, Clone, PartialEq)]
pub struct City {
    /// The GeoNames id, stable across dumps.
    pub id: u32,
    pub name: &'static str,
    pub latitude: f64,
    pub longitude: f64,
    /// ISO 3166-1 alpha-2 country code.
    pub country: &'static str,
}

/// The towns, and an index of them by whole-degree cell.
struct Table {
    cities: Vec<City>,
    cells: HashMap<(i32, i32), Vec<usize>>,
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        let cities: Vec<City> = CITIES.lines().filter_map(parse_city).collect();
        let mut cells: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        for (index, city) in cities.iter().enumerate() {
            cells
                .entry(cell(city.latitude, city.longitude))
                .or_default()
                .push(index);
        }
        Table { cities, cells }
    })
}

fn parse_city(line: &'static str) -> Option<City> {
    let mut fields = line.split('\t');
    Some(City {
        id: fields.next()?.parse().ok()?,
        name: fields.next()?,
        latitude: fields.next()?.parse().ok()?,
        longitude: fields.next()?.parse().ok()?,
        country: fields.next()?,
    })
}

fn cell(latitude: f64, longitude: f64) -> (i32, i32) {
    (latitude.floor() as i32, longitude.floor() as i32)
}

/// Great-circle distance between two positions, in km.
pub fn distance_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lat2) = (a.0.to_radians(), b.0.to_radians());
    let dlat = lat2 - lat1;
    let dlon = (b.1 - a.1).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * h.sqrt().min(1.0).asin()
}

/// The town nearest to a position, if one lies within [`MAX_DISTANCE_KM`].
pub fn nearest(latitude: f64, longitude: f64) -> Option<&'static City> {
    if !latitude.is_finite() || !longitude.is_finite() {
        return None;
    }
    let table = table();
    // A degree of latitude is about 111 km everywhere; a degree of longitude
    // shrinks towards the poles, so more cells are needed to cover the radius.
    let lat_span = (MAX_DISTANCE_KM / 111.0).ceil() as i32;
    let lon_span = {
        let scale = latitude.to_radians().cos().abs().max(0.01);
        ((MAX_DISTANCE_KM / (111.0 * scale)).ceil() as i32).min(180)
    };
    let (row, column) = cell(latitude, longitude);
    let mut best: Option<(f64, &City)> = None;
    for dlat in -lat_span..=lat_span {
        for dlon in -lon_span..=lon_span {
            // Longitude wraps at the antimeridian: -181 is 179.
            let lon_cell = (column + dlon + 180).rem_euclid(360) - 180;
            let Some(indices) = table.cells.get(&(row + dlat, lon_cell)) else {
                continue;
            };
            for &index in indices {
                let city = &table.cities[index];
                let d = distance_km((latitude, longitude), (city.latitude, city.longitude));
                if d <= MAX_DISTANCE_KM && best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, city));
                }
            }
        }
    }
    best.map(|(_, city)| city)
}

/// The town with this GeoNames id.
pub fn city(id: u32) -> Option<&'static City> {
    let cities = &table().cities;
    // The table is sorted by id.
    cities
        .binary_search_by_key(&id, |city| city.id)
        .ok()
        .map(|index| &cities[index])
}

/// The English ISO 3166-1 name of a country code, such as "Germany" for "DE".
pub fn country_name(code: &str) -> Option<&'static str> {
    COUNTRIES.lines().find_map(|line| {
        let (c, name) = line.split_once('\t')?;
        (c == code).then_some(name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_in_a_town_resolves_to_that_town() {
        let berlin = nearest(52.52, 13.405).expect("Berlin is a town");
        assert_eq!(berlin.name, "Berlin");
        assert_eq!(berlin.country, "DE");
        assert_eq!(city(berlin.id), Some(berlin));
    }

    #[test]
    fn the_open_sea_has_no_town() {
        assert_eq!(nearest(0.0, -30.0), None);
        assert_eq!(nearest(f64::NAN, 0.0), None);
    }

    #[test]
    fn a_town_across_the_antimeridian_is_still_found() {
        // Just east of 180°, the nearest towns in Fiji are west of it.
        let town = nearest(-16.8, 179.99);
        assert!(town.is_none_or(|t| t.country == "FJ"));
        let suva = nearest(-18.14, 178.44).expect("Suva is a town");
        assert_eq!(suva.country, "FJ");
    }

    #[test]
    fn every_town_has_a_named_country() {
        for city in &table().cities {
            assert!(
                country_name(city.country).is_some(),
                "{} has no country name for {}",
                city.name,
                city.country
            );
        }
        assert_eq!(country_name("DE"), Some("Germany"));
    }

    #[test]
    fn distances_are_great_circle_km() {
        let d = distance_km((52.52, 13.405), (48.137, 11.575));
        assert!(
            (d - 504.0).abs() < 5.0,
            "Berlin to Munich is about 504 km, got {d}"
        );
    }
}
