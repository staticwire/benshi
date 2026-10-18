//! The day a viewing was seen on, as a time zone names it.
//!
//! A list keeps the day an entry was begun and the day it was completed. The
//! store keeps the instant of a viewing in UTC, and the day is the one a zone
//! names for that instant. Read in the zone where it was seen, a viewing just
//! after midnight is on the day it was seen there. A past instant is read by
//! the rules of summer time the zone had then.

use std::time::SystemTime;

use benshi_sync::plan::Day;
use jiff::Timestamp;
use jiff::tz::TimeZone;

/// The day `zone` names for an instant, and none where that day is in a year
/// below 0 or the instant is past [`Timestamp::MAX`].
#[must_use]
pub fn day_in(at: SystemTime, zone: &TimeZone) -> Option<Day> {
    let instant = Timestamp::try_from(at).ok()?;
    let date = zone.to_datetime(instant).date();

    Some(Day {
        year: u16::try_from(date.year()).ok()?,
        month: u8::try_from(date.month()).ok()?,
        day: u8::try_from(date.day()).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use benshi_sync::plan::Day;
    use jiff::tz::{Offset, TimeZone};

    use super::day_in;

    /// The instant this RFC 3339 text names.
    fn at(text: &str) -> SystemTime {
        SystemTime::from(
            text.parse::<jiff::Timestamp>()
                .unwrap_or_else(|failed| panic!("{text}: {failed}")),
        )
    }

    fn day(year: u16, month: u8, day: u8) -> Day {
        Day { year, month, day }
    }

    /// A zone this many hours east of UTC, with no summer time.
    fn east(hours: i8) -> TimeZone {
        TimeZone::fixed(Offset::constant(hours))
    }

    #[test]
    fn an_evening_in_utc_is_the_next_day_three_hours_east() {
        let late = at("2026-10-13T22:30:00Z");

        assert_eq!(day_in(late, &TimeZone::UTC), Some(day(2026, 10, 13)));
        assert_eq!(day_in(late, &east(3)), Some(day(2026, 10, 14)));
    }

    #[test]
    fn a_morning_in_utc_is_the_day_before_far_enough_west() {
        assert_eq!(
            day_in(at("2026-10-14T02:00:00Z"), &east(-5)),
            Some(day(2026, 10, 13))
        );
    }

    #[test]
    fn an_instant_is_read_by_the_summer_time_of_its_own_date() {
        let central =
            TimeZone::posix("CET-1CEST,M3.5.0,M10.5.0/3").expect("a zone with summer time");

        // 22:30 in UTC is 00:30 the next day in summer, two hours east.
        assert_eq!(
            day_in(at("2026-07-01T22:30:00Z"), &central),
            Some(day(2026, 7, 2))
        );
        // The same hour in winter is 23:30 the same day, one hour east.
        assert_eq!(
            day_in(at("2026-01-01T22:30:00Z"), &central),
            Some(day(2026, 1, 1))
        );
        assert_eq!(
            day_in(at("2026-01-01T23:30:00Z"), &central),
            Some(day(2026, 1, 2))
        );
    }

    #[test]
    fn an_instant_before_the_first_year_has_no_day() {
        let long_ago = UNIX_EPOCH - Duration::from_hours(2_100 * 365 * 24);

        assert_eq!(day_in(long_ago, &TimeZone::UTC), None);
    }

    #[test]
    fn the_first_hour_of_year_zero_is_a_day_and_the_hour_before_is_none() {
        let first = at("0000-01-01T01:00:00Z");

        assert_eq!(day_in(first, &TimeZone::UTC), Some(day(0, 1, 1)));
        assert_eq!(day_in(first, &east(-3)), None);
    }

    #[test]
    fn the_last_instant_the_clock_reads_is_a_day_and_the_next_second_is_none() {
        let last = at("9999-12-30T22:00:00Z");

        assert_eq!(day_in(last, &TimeZone::UTC), Some(day(9999, 12, 30)));
        assert_eq!(day_in(last + Duration::from_secs(1), &TimeZone::UTC), None);
    }

    #[test]
    fn an_instant_before_1970_is_read_as_any_other() {
        assert_eq!(
            day_in(at("1969-12-31T22:30:00Z"), &east(3)),
            Some(day(1970, 1, 1))
        );
    }
}
