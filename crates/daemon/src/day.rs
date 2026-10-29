//! The day a viewing was seen on, as a time zone names it.
//!
//! A list keeps the day an entry was begun and the day it was completed. The
//! store keeps the instant of a viewing in UTC, and the day is the one a zone
//! names for that instant. Read in the zone where it was seen, a viewing just
//! after midnight is on the day it was seen there. A past instant is read by
//! the offset the zone had then, summer time included.

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

    /// A zone two hours east of UTC up to the start of 2016 and three hours
    /// east from then on, built as `TZif` data of version 1, RFC 8536: the
    /// counts, one transition, two local time types and their designations.
    /// RFC 8536 leaves the time from the last transition on to the footer,
    /// and version 1 has none. `jiff` reads that time by the type of the last
    /// transition.
    fn moved_east_in_2016() -> TimeZone {
        // The magic, NUL for version 1, and 15 reserved bytes.
        let mut data = b"TZif\0".to_vec();
        data.extend([0; 15]);
        // UT indicators, standard indicators, leap seconds, transitions,
        // local time types, and bytes of designations.
        for count in [0_u32, 0, 0, 1, 2, 8] {
            data.extend(count.to_be_bytes());
        }
        // 2016-01-01T00:00:00Z, from when the second type is in force.
        data.extend(1_451_606_400_i32.to_be_bytes());
        data.push(1);
        for (offset, designation) in [(7_200_i32, 0_u8), (10_800, 4)] {
            data.extend(offset.to_be_bytes());
            data.extend([0, designation]);
        }
        data.extend(b"TWO\0THR\0");

        TimeZone::tzif("Test/Moved", &data).expect("the data is TZif")
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
    fn a_past_instant_is_read_by_the_offset_its_zone_had_then() {
        let zone = moved_east_in_2016();

        // 21:30 in UTC was 23:30 the same day, two hours east, before the
        // zone moved, and is 00:30 the next day, three hours east, after.
        assert_eq!(
            day_in(at("2015-07-01T21:30:00Z"), &zone),
            Some(day(2015, 7, 1))
        );
        assert_eq!(
            day_in(at("2017-07-01T21:30:00Z"), &zone),
            Some(day(2017, 7, 2))
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
