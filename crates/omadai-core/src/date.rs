//! Calendar dates in UTC, enough for month and range arithmetic without a date crate.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    pub year: i32,
    pub month: u8,
    pub day: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Month {
    pub year: i32,
    pub month: u8,
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn parse_num(s: &str, len: usize) -> Option<u32> {
    if s.len() != len || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

impl Date {
    pub fn new(year: i32, month: u8, day: u8) -> Option<Self> {
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return None;
        }
        Some(Date { year, month, day })
    }

    /// Parses `YYYY-MM-DD`.
    pub fn parse(s: &str) -> Option<Self> {
        let mut it = s.split('-');
        let (y, m, d) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() {
            return None;
        }
        Date::new(
            parse_num(y, 4)? as i32,
            parse_num(m, 2)? as u8,
            parse_num(d, 2)? as u8,
        )
    }

    /// Days since 1970-01-01 (Howard Hinnant's days_from_civil).
    pub fn to_days(self) -> i64 {
        let y = i64::from(self.year) - i64::from(self.month <= 2);
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let m = i64::from(self.month);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// The inverse of [`Date::to_days`] (civil_from_days).
    pub fn from_days(z: i64) -> Self {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
        Date { year, month, day }
    }

    pub fn today_utc() -> Self {
        Date::from_days(unix_now() / 86_400)
    }

    pub fn month(self) -> Month {
        Month {
            year: self.year,
            month: self.month,
        }
    }

    pub fn succ(self) -> Self {
        Date::from_days(self.to_days() + 1)
    }
}

impl Month {
    pub fn new(year: i32, month: u8) -> Option<Self> {
        (1..=12).contains(&month).then_some(Month { year, month })
    }

    /// Parses `YYYY-MM`.
    pub fn parse(s: &str) -> Option<Self> {
        let (y, m) = s.split_once('-')?;
        Month::new(parse_num(y, 4)? as i32, parse_num(m, 2)? as u8)
    }

    pub fn current_utc() -> Self {
        Date::today_utc().month()
    }

    pub fn first_day(self) -> Date {
        Date {
            year: self.year,
            month: self.month,
            day: 1,
        }
    }

    pub fn last_day(self) -> Date {
        Date {
            year: self.year,
            month: self.month,
            day: days_in_month(self.year, self.month),
        }
    }

    pub fn succ(self) -> Self {
        if self.month == 12 {
            Month {
                year: self.year + 1,
                month: 1,
            }
        } else {
            Month {
                year: self.year,
                month: self.month + 1,
            }
        }
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl fmt::Display for Month {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}", self.year, self.month)
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The current UTC time as RFC 3339, second precision.
pub fn now_rfc3339() -> String {
    let secs = unix_now();
    let date = Date::from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{date}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

macro_rules! string_serde {
    ($t:ty, $what:literal) => {
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
                <$t>::parse(&s).ok_or_else(|| serde::de::Error::custom(concat!("invalid ", $what)))
            }
        }
    };
}
string_serde!(Date, "date, expected YYYY-MM-DD");
string_serde!(Month, "month, expected YYYY-MM");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_epochs() {
        assert_eq!(Date::new(1970, 1, 1).unwrap().to_days(), 0);
        assert_eq!(Date::new(2000, 3, 1).unwrap().to_days(), 11_017);
        assert_eq!(Date::new(2024, 2, 29).unwrap().to_days(), 19_782);
        assert_eq!(Date::from_days(19_782), Date::new(2024, 2, 29).unwrap());
        assert_eq!(Date::from_days(-1), Date::new(1969, 12, 31).unwrap());
        for z in -800_000..800_000 {
            if z % 997 == 0 {
                assert_eq!(Date::from_days(z).to_days(), z);
            }
        }
    }

    #[test]
    fn parsing() {
        assert_eq!(Date::parse("2026-11-05"), Date::new(2026, 11, 5));
        assert!(Date::parse("2025-02-29").is_none());
        assert!(Date::parse("2024-02-29").is_some());
        assert!(Date::parse("2026-1-05").is_none());
        assert!(Date::parse("2026-11-05-1").is_none());
        assert!(Date::parse("+026-11-05").is_none());
        assert_eq!(
            Month::parse("2026-02").unwrap().last_day(),
            Date::new(2026, 2, 28).unwrap()
        );
        assert!(Month::parse("2026-13").is_none());
        assert_eq!(
            Month::parse("2026-12").unwrap().succ(),
            Month::new(2027, 1).unwrap()
        );
    }

    #[test]
    fn rfc3339_shape() {
        let s = now_rfc3339();
        assert_eq!(s.len(), 20, "{s}");
        assert!(s.ends_with('Z'));
    }
}
