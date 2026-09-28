//! Telling photos apart by what they show rather than by their bytes.
//!
//! The server's content hash only matches byte-identical files. A photo that
//! was resized, re-encoded or sent through a messenger is another file, so the
//! similar-photo finder hashes each thumbnail instead: a 64-bit difference
//! hash ("dHash"), which compares neighbouring brightness in a 9×8 grid. Two
//! copies of one picture land a few bits apart, and different pictures about
//! half of the bits apart.

/// Hashes this many bits apart or fewer show the same picture.
pub const SIMILAR_BITS: u32 = 6;

/// Stored for a photo whose thumbnail was looked at and gave no usable hash
/// (it couldn't be decoded, or is one flat colour), so it isn't looked at
/// again. A real hash is never 0: a picture that would hash to 0 has no
/// brightness changes, which [`difference_hash`] turns down as flat.
pub const NO_HASH: u64 = 0;

/// Samples of the grid that differ by less than this in brightness, all of
/// them, make a flat picture — a black frame, a blank page — whose hash is
/// noise and would match every other flat picture.
const FLAT: u8 = 8;

/// The difference hash of an 8-bit greyscale image `width` px wide, rows
/// stored one after another in `luma`. `None` for an empty or flat image.
pub fn difference_hash(width: usize, height: usize, luma: &[u8]) -> Option<u64> {
    if width == 0 || height == 0 || luma.len() < width * height {
        return None;
    }
    // Box-average down to 9×8, so every source pixel counts once and the hash
    // doesn't depend on which pixels a nearest-neighbour pick happens to hit.
    let mut grid = [[0u8; 9]; 8];
    for (gy, row) in grid.iter_mut().enumerate() {
        let (y0, y1) = span(gy, 8, height);
        for (gx, cell) in row.iter_mut().enumerate() {
            let (x0, x1) = span(gx, 9, width);
            let mut sum = 0u64;
            for y in y0..y1 {
                sum += luma[y * width + x0..y * width + x1]
                    .iter()
                    .map(|&v| u64::from(v))
                    .sum::<u64>();
            }
            *cell = (sum / ((y1 - y0) * (x1 - x0)) as u64) as u8;
        }
    }
    let (min, max) = grid
        .iter()
        .flatten()
        .fold((u8::MAX, 0), |(min, max), &v| (min.min(v), max.max(v)));
    if max - min < FLAT {
        return None;
    }
    let mut hash = 0u64;
    for row in &grid {
        for pair in row.windows(2) {
            hash = hash << 1 | u64::from(pair[0] < pair[1]);
        }
    }
    (hash != NO_HASH).then_some(hash)
}

/// The source pixels `from..to` that cell `i` of `cells` covers along an edge
/// of `len` px; never empty, even for an edge shorter than the grid.
fn span(i: usize, cells: usize, len: usize) -> (usize, usize) {
    let from = (i * len / cells).min(len - 1);
    let to = ((i + 1) * len / cells).max(from + 1);
    (from, to)
}

/// Whether two hashes show the same picture.
pub fn alike(a: u64, b: u64) -> bool {
    (a ^ b).count_ones() <= SIMILAR_BITS
}

/// Group `hashes` into sets of alike ones, by index, each set in index order
/// and the sets by their first index. A hash alike to none other is in no
/// set. Alike is taken as catching: a burst of shots each a little different
/// from the last is one set.
pub fn sets(hashes: &[u64]) -> Vec<Vec<usize>> {
    // Two hashes at most SIMILAR_BITS apart agree on at least one of their
    // eight bytes, as long as SIMILAR_BITS < 8. So only hashes sharing some
    // byte are compared, not every pair of the library.
    const _: () = assert!(SIMILAR_BITS < 8);
    let mut parent: Vec<usize> = (0..hashes.len()).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for byte in 0..8 {
        let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); 256];
        for (i, &hash) in hashes.iter().enumerate() {
            buckets[(hash >> (byte * 8)) as u8 as usize].push(i);
        }
        for bucket in &buckets {
            for (n, &a) in bucket.iter().enumerate() {
                for &b in &bucket[n + 1..] {
                    if alike(hashes[a], hashes[b]) {
                        let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
                        if ra != rb {
                            parent[ra.max(rb)] = ra.min(rb);
                        }
                    }
                }
            }
        }
    }
    let mut by_root: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
    for i in 0..hashes.len() {
        let r = root(&mut parent, i);
        by_root.entry(r).or_default().push(i);
    }
    by_root.into_values().filter(|set| set.len() > 1).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 90×80 picture: a bright disc on a gradient.
    fn picture(width: usize, height: usize) -> Vec<u8> {
        let mut luma = Vec::with_capacity(width * height);
        for y in 0..height {
            for x in 0..width {
                let (fx, fy) = (x as f64 / width as f64, y as f64 / height as f64);
                let disc = ((fx - 0.3).powi(2) + (fy - 0.6).powi(2)).sqrt() < 0.2;
                let v = if disc { 240.0 } else { 40.0 + 120.0 * fx * fy };
                luma.push(v as u8);
            }
        }
        luma
    }

    #[test]
    fn a_resized_copy_hashes_alike() {
        let big = difference_hash(900, 800, &picture(900, 800)).unwrap();
        let small = difference_hash(91, 79, &picture(91, 79)).unwrap();
        assert!(
            alike(big, small),
            "{} bits apart",
            (big ^ small).count_ones()
        );
    }

    #[test]
    fn a_different_picture_hashes_apart() {
        let one = difference_hash(90, 80, &picture(90, 80)).unwrap();
        let mirrored: Vec<u8> = picture(90, 80)
            .chunks(90)
            .flat_map(|row| row.iter().rev().copied().collect::<Vec<_>>())
            .collect();
        let other = difference_hash(90, 80, &mirrored).unwrap();
        assert!(!alike(one, other));
    }

    #[test]
    fn a_flat_picture_has_no_hash() {
        assert_eq!(difference_hash(64, 64, &[17; 64 * 64]), None);
        assert_eq!(difference_hash(0, 0, &[]), None);
        assert_eq!(difference_hash(4, 4, &[0; 3]), None);
    }

    #[test]
    fn a_picture_smaller_than_the_grid_still_hashes() {
        let luma: Vec<u8> = (0..4 * 3).map(|i| (i * 20) as u8).collect();
        assert!(difference_hash(4, 3, &luma).is_some());
    }

    #[test]
    fn alike_hashes_form_sets_and_catch_on() {
        let a = 0x0123_4567_89ab_cdef_u64;
        let hashes = [
            a,
            !a,
            a ^ 0b111,
            a ^ 0b111 ^ (0b1111 << 40),
            0x5555_0000_ffff_aaaa,
        ];
        assert_eq!(sets(&hashes), vec![vec![0, 2, 3]]);
    }

    #[test]
    fn hashes_apart_in_most_bytes_are_still_found() {
        // One bit apart in each of six bytes: six bits in all, and only the
        // top two bytes agree.
        let a = 1u64 << 63;
        let b = (0..6).fold(a, |h, byte| h | 1 << (byte * 8));
        assert_eq!(sets(&[a, b]), vec![vec![0, 1]]);
    }
}
