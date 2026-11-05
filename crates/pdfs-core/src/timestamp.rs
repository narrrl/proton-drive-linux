//! Convert the client's stored epoch seconds at the SDK boundary.

use proton_drive_rs::ModificationTime;

/// A UTC modification time, or `None` when outside the SDK's supported range.
pub fn modification_time(seconds: i64) -> Option<ModificationTime> {
    chrono::DateTime::from_timestamp(seconds, 0).map(|time| time.fixed_offset())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_seconds_preserve_the_instant_including_before_1970() {
        for seconds in [-1, 0, 1_709_208_000, 1_782_482_460] {
            let time = modification_time(seconds).unwrap();
            assert_eq!(time.timestamp(), seconds);
            assert_eq!(time.offset().local_minus_utc(), 0);
            assert_eq!(time.timestamp_subsec_nanos(), 0);
        }
        assert_eq!(
            modification_time(-1).unwrap().to_rfc3339(),
            "1969-12-31T23:59:59+00:00"
        );
    }

    #[test]
    fn out_of_range_seconds_omit_the_timestamp() {
        assert!(modification_time(i64::MIN).is_none());
        assert!(modification_time(i64::MAX).is_none());
    }
}
