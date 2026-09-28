//! Dates and times as the app shows them, so every page words a moment the
//! same way.
//!
//! - [`relative`] for "when did this happen" in lists: "Just now", "5 min
//!   ago", "Yesterday, 14:05", "Tuesday", then a date.
//! - [`short_date`] for date columns: "Sep 23", with the year only when it is
//!   not this year.
//! - [`full`] for tooltips: the weekday, date, year and time.
//!
//! The formats are `strftime` patterns that go through gettext, so they follow
//! the language picked in Preferences, not only the system locale.

use gtk4::glib;

use crate::{gettext, gettext_f, ngettext_f};

/// Seconds in a day, for the "within the last week" checks.
const DAY: i64 = 86_400;

/// A past moment in epoch seconds, relative to now. Empty when the time can't
/// be read.
pub(crate) fn relative(secs: i64) -> String {
    match (
        glib::DateTime::from_unix_local(secs),
        glib::DateTime::now_local(),
    ) {
        (Ok(at), Ok(now)) => relative_to(&at, &now),
        _ => String::new(),
    }
}

/// [`relative`] for a fixed `now`, so it can be tested.
pub(crate) fn relative_to(at: &glib::DateTime, now: &glib::DateTime) -> String {
    let ago = now.to_unix() - at.to_unix();
    // A clock a little ahead of ours still reads as "just now".
    if (-60..60).contains(&ago) {
        return gettext("Just now");
    }
    if (60..3600).contains(&ago) {
        let minutes = (ago / 60) as u64;
        return ngettext_f("{n} min ago", "{n} min ago", minutes, &[]);
    }
    // Translators: strftime format for a time of day, such as "14:05".
    let time = || format(at, &gettext("%H:%M"));
    if at.ymd() == now.ymd() {
        return time();
    }
    if now.add_days(-1).is_ok_and(|y| y.ymd() == at.ymd()) {
        // Translators: {time} is a time of day, such as "14:05".
        return gettext_f("Yesterday, {time}", &[("time", &time())]);
    }
    if (0..6 * DAY).contains(&ago) {
        return format(at, "%A");
    }
    short_date_to(at, now)
}

/// A date in epoch seconds, without the time. Empty when the time can't be
/// read.
pub(crate) fn short_date(secs: i64) -> String {
    match (
        glib::DateTime::from_unix_local(secs),
        glib::DateTime::now_local(),
    ) {
        (Ok(at), Ok(now)) => short_date_to(&at, &now),
        _ => String::new(),
    }
}

/// [`short_date`] for a fixed `now`, so it can be tested.
pub(crate) fn short_date_to(at: &glib::DateTime, now: &glib::DateTime) -> String {
    let pattern = if at.year() == now.year() {
        // Translators: strftime format for a date in this year, such as "Sep 23".
        gettext("%b %-d")
    } else {
        // Translators: strftime format for a date in another year, such as "Sep 23, 2025".
        gettext("%b %-d, %Y")
    };
    format(at, &pattern)
}

/// The whole moment in epoch seconds, for a tooltip. Empty when the time
/// can't be read.
pub(crate) fn full(secs: i64) -> String {
    glib::DateTime::from_unix_local(secs)
        .map(|at| {
            // Translators: strftime format for a full date and time, such as
            // "Wednesday, September 23, 2026, 14:05".
            format(&at, &gettext("%A, %B %-d, %Y, %H:%M"))
        })
        .unwrap_or_default()
}

fn format(at: &glib::DateTime, pattern: &str) -> String {
    at.format(pattern)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: i32, d: i32, h: i32, min: i32) -> glib::DateTime {
        glib::DateTime::from_local(y, m, d, h, min, 0.0).unwrap()
    }

    #[test]
    fn relative_times_get_coarser_with_age() {
        let now = at(2026, 9, 23, 15, 0);
        assert_eq!(relative_to(&at(2026, 9, 23, 14, 59), &now), "1 min ago");
        assert_eq!(relative_to(&at(2026, 9, 23, 14, 55), &now), "5 min ago");
        assert_eq!(relative_to(&at(2026, 9, 23, 9, 5), &now), "09:05");
        assert_eq!(
            relative_to(&at(2026, 9, 22, 14, 5), &now),
            "Yesterday, 14:05"
        );
        assert_eq!(relative_to(&at(2026, 9, 20, 10, 0), &now), "Sunday");
        assert_eq!(relative_to(&at(2026, 9, 1, 10, 0), &now), "Sep 1");
        assert_eq!(relative_to(&at(2024, 3, 5, 10, 0), &now), "Mar 5, 2024");
    }

    #[test]
    fn a_moment_ago_or_slightly_ahead_is_just_now() {
        let now = at(2026, 9, 23, 15, 0);
        assert_eq!(relative_to(&now, &now), "Just now");
        assert_eq!(
            relative_to(&at(2026, 9, 23, 15, 0), &now.add_seconds(30.0).unwrap()),
            "Just now"
        );
    }

    #[test]
    fn short_dates_show_the_year_only_when_it_differs() {
        let now = at(2026, 9, 23, 15, 0);
        assert_eq!(short_date_to(&at(2026, 1, 2, 8, 0), &now), "Jan 2");
        assert_eq!(short_date_to(&at(2025, 12, 31, 8, 0), &now), "Dec 31, 2025");
    }
}
