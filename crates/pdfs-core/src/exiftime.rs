//! When a photo was taken, as its own EXIF tags say.
//!
//! The fallback for a photo nothing else dates: a Takeout photo without a
//! sidecar or a date in its name, and a photo uploaded from the app. Without
//! it the first was filed under the moment of the import and the second under
//! the file's modification time, which a copy resets.

use std::io::{BufRead, Seek};
use std::path::Path;

use crate::takeout::days_from_civil;

/// The capture time in the EXIF of the file at `path`, in epoch seconds.
pub fn capture_time_of(path: &Path) -> Option<i64> {
    let file = std::fs::File::open(path).ok()?;
    capture_time(&mut std::io::BufReader::new(file))
}

/// The capture time in the EXIF `reader` holds: `DateTimeOriginal`, then
/// `DateTimeDigitized`, each adjusted by its `OffsetTime…` tag when the camera
/// wrote one. Without an offset the time is read as UTC, as a date in a file
/// name is (`takeout::capture_time_from_name`).
pub fn capture_time(reader: &mut (impl BufRead + Seek)) -> Option<i64> {
    let exif = exif::Reader::new().read_from_container(reader).ok()?;
    let ascii = |tag: exif::Tag| -> Option<String> {
        let field = exif.get_field(tag, exif::In::PRIMARY)?;
        match &field.value {
            exif::Value::Ascii(parts) => parts.first().map(|part| {
                String::from_utf8_lossy(part)
                    .trim_matches('\0')
                    .trim()
                    .to_string()
            }),
            _ => None,
        }
    };
    [
        (exif::Tag::DateTimeOriginal, exif::Tag::OffsetTimeOriginal),
        (exif::Tag::DateTimeDigitized, exif::Tag::OffsetTimeDigitized),
    ]
    .into_iter()
    .find_map(|(time, offset)| {
        let local = parse_exif_time(&ascii(time)?)?;
        let offset = ascii(offset).and_then(|o| parse_offset(&o)).unwrap_or(0);
        Some(local - offset)
    })
}

/// `YYYY:MM:DD HH:MM:SS` as epoch seconds, read as UTC. `None` for the
/// all-zero or blank value cameras write when they do not know.
fn parse_exif_time(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 || bytes[4] != b':' || bytes[7] != b':' || bytes[13] != b':' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1800..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// `+HH:MM` or `-HH:MM` as seconds east of UTC.
fn parse_offset(text: &str) -> Option<i64> {
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = text.get(1..)?.split_once(':')?;
    let (hours, minutes) = (hours.parse::<i64>().ok()?, minutes.parse::<i64>().ok()?);
    (hours <= 14 && minutes < 60).then_some(sign * (hours * 3600 + minutes * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exif_time_reads_as_utc_and_its_offset_moves_it() {
        // 2023-06-01 12:00:00 UTC.
        let noon = 1_685_620_800;
        assert_eq!(parse_exif_time("2023:06:01 12:00:00"), Some(noon));
        assert_eq!(parse_offset("+02:00"), Some(7200));
        assert_eq!(parse_offset("-05:30"), Some(-19_800));
        assert_eq!(
            parse_exif_time("2023:06:01 14:00:00").map(|t| t - parse_offset("+02:00").unwrap()),
            Some(noon)
        );
    }

    #[test]
    fn an_unknown_exif_time_is_no_time() {
        assert_eq!(parse_exif_time("0000:00:00 00:00:00"), None);
        assert_eq!(parse_exif_time("    :  :     :  :  "), None);
        assert_eq!(parse_exif_time(""), None);
        assert_eq!(parse_offset("02:00"), None);
    }

    #[test]
    fn a_photo_is_dated_by_its_original_time_and_offset() {
        let ascii = |tag, text: &str| exif::Field {
            tag,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Ascii(vec![text.as_bytes().to_vec()]),
        };
        let taken = ascii(exif::Tag::DateTimeOriginal, "2023:06:01 14:00:00");
        let offset = ascii(exif::Tag::OffsetTimeOriginal, "+02:00");
        let digitized = ascii(exif::Tag::DateTimeDigitized, "2024:01:01 00:00:00");
        let mut writer = exif::experimental::Writer::new();
        writer.push_field(&taken);
        writer.push_field(&offset);
        writer.push_field(&digitized);
        let mut tiff = std::io::Cursor::new(Vec::new());
        writer.write(&mut tiff, false).unwrap();
        tiff.set_position(0);
        assert_eq!(capture_time(&mut tiff), Some(1_685_620_800));
    }

    #[test]
    fn a_file_without_exif_has_no_capture_time() {
        let mut png = std::io::Cursor::new(b"\x89PNG\r\n\x1a\n".to_vec());
        assert_eq!(capture_time(&mut png), None);
    }
}
