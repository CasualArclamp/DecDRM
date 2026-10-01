//! Broadcast seasons (HFCC): **A** from the last Sunday of March, **B** from the last
//! Sunday of October to the last Sunday of March of the next year — the days the summer
//! time changes. A season is named after the year it starts in, so the B season over
//! New Year 2026/27 is `b26`. EiBi names its files after them (`sked-a26.csv`); Dream
//! computes the same in `CSchedule::SetAnalogUrl` (`src/GUI-QT/Schedule.cpp`).

use crate::time::Date;
use std::fmt;

/// A broadcast season. Ordered chronologically (A before B of the same year).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Season {
    year: i32,
    b: bool,
}

impl Season {
    /// The A (`b = false`) or B season starting in `year`.
    pub fn new(year: i32, b: bool) -> Season {
        Season { year, b }
    }

    /// The season `d` falls in. The change happens on the Sunday itself (at 01:00 UTC;
    /// days are resolution enough here).
    pub fn at(d: Date) -> Season {
        let y = d.year();
        let march = Date::last_sunday(y, 3).expect("March exists");
        let october = Date::last_sunday(y, 10).expect("October exists");
        if d < march {
            Season::new(y - 1, true)
        } else if d < october {
            Season::new(y, false)
        } else {
            Season::new(y, true)
        }
    }

    /// The year the season starts in.
    pub fn year(self) -> i32 {
        self.year
    }

    pub fn is_b(self) -> bool {
        self.b
    }

    /// `a26`, `b26`: the letter and the start year's last two digits.
    pub fn code(self) -> String {
        format!(
            "{}{:02}",
            if self.b { 'b' } else { 'a' },
            self.year.rem_euclid(100)
        )
    }

    /// Parse a code like `a26` or `B07` (years 2000–2099).
    pub fn parse(code: &str) -> Option<Season> {
        let code = code.trim();
        let (letter, yy) = code.split_at_checked(1)?;
        let b = match letter {
            "a" | "A" => false,
            "b" | "B" => true,
            _ => return None,
        };
        (yy.len() == 2 && yy.bytes().all(|c| c.is_ascii_digit()))
            .then(|| Season::new(2000 + yy.parse::<i32>().unwrap_or(0), b))
    }

    /// First day.
    pub fn start(self) -> Date {
        Date::last_sunday(self.year, if self.b { 10 } else { 3 }).expect("valid month")
    }

    /// The following season.
    pub fn next(self) -> Season {
        if self.b {
            Season::new(self.year + 1, false)
        } else {
            Season::new(self.year, true)
        }
    }

    /// Last day (the day before the next season starts).
    pub fn end(self) -> Date {
        self.next().start().add_days(-1)
    }

    /// The date of `month`/`day` within this season: for a B season, July–December fall
    /// in the start year and January–June in the next. (A day just outside the season
    /// still lands next to it, which keeps validity bounds that EiBi writes as `mmdd`
    /// right even when they lie a little before or after the season.)
    pub fn date_of(self, month: u32, day: u32) -> Option<Date> {
        let year = if self.b && month < 7 {
            self.year + 1
        } else {
            self.year
        };
        Date::new_clamped(year, month, day)
    }
}

impl fmt::Display for Season {
    /// `A26`, `B26`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.code().to_uppercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> Date {
        Date::new(y, m, day).unwrap()
    }

    #[test]
    fn seasons_change_on_the_last_sundays() {
        assert_eq!(Season::at(d(2026, 10, 1)).code(), "a26");
        assert_eq!(Season::at(d(2026, 10, 24)).code(), "a26");
        assert_eq!(Season::at(d(2026, 10, 25)).code(), "b26"); // last Sunday of October
        assert_eq!(Season::at(d(2026, 12, 31)).code(), "b26");
        assert_eq!(
            Season::at(d(2027, 1, 1)).code(),
            "b26",
            "B spans New Year: start year"
        );
        assert_eq!(Season::at(d(2027, 3, 27)).code(), "b26");
        assert_eq!(Season::at(d(2027, 3, 28)).code(), "a27"); // last Sunday of March
        assert_eq!(Season::at(d(2026, 3, 28)).code(), "b25");
        assert_eq!(Season::at(d(2026, 3, 29)).code(), "a26");
    }

    #[test]
    fn bounds_codes_and_order() {
        let a26 = Season::new(2026, false);
        assert_eq!(a26.start(), d(2026, 3, 29));
        assert_eq!(a26.end(), d(2026, 10, 24));
        assert_eq!(a26.next().start(), d(2026, 10, 25));
        assert_eq!(a26.next().end(), d(2027, 3, 27));
        assert!(a26 < a26.next() && a26.next() < Season::new(2027, false));
        assert_eq!(Season::parse("a26"), Some(a26));
        assert_eq!(Season::parse("B07"), Some(Season::new(2007, true)));
        for bad in ["", "a", "c26", "a2", "a266", "a2x", "ü26"] {
            assert_eq!(Season::parse(bad), None, "{bad}");
        }
        assert_eq!(Season::new(2009, true).code(), "b09");
        assert_eq!(a26.to_string(), "A26");
        assert_eq!(a26.next().date_of(1, 15), Some(d(2027, 1, 15)));
        assert_eq!(a26.next().date_of(12, 1), Some(d(2026, 12, 1)));
        assert_eq!(a26.date_of(10, 1), Some(d(2026, 10, 1)));
    }
}
