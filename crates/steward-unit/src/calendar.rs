//! Calendar events as systemd writes them (systemd.time(7)), for
//! `OnCalendar=`: `daily`, `Mon..Fri 09:00`, `*-*-01 03:30:00`,
//! `Sat,Sun 10:00`, `*:0/15`, `*-02~01` (the last day of February).
//!
//! An event is `[weekdays] [year-month-day] [hour:minute[:second]] [UTC]`.
//! A date left out is every day; a time left out is midnight; seconds left out
//! are `:00`. Each field is `*`, a number, a range `a..b`, any of those with a
//! repetition `/n`, or a list of them with `,`. `~` in place of the last `-`
//! counts the day back from the end of the month: `~01` is the last day,
//! `~07/1` the last seven. The shorthands `minutely`, `hourly`, `daily`,
//! `weekly`, `monthly`, `quarterly`, `semiannually` and `yearly` (or
//! `annually`) are what they say, at the start of each.
//!
//! Times are local, unless the event ends in `UTC`. Other time zones, which
//! systemd names from the IANA database, are not supported.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MIN_YEAR: u32 = 1970;
/// systemd's limit too.
const MAX_YEAR: u32 = 2199;

/// A date and a time of day, to the second, in no zone in particular.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Civil {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Civil {
    /// Monday is 0, Sunday 6.
    pub fn weekday(&self) -> u32 {
        (days_from_civil(self.year, self.month, self.day) + 3).rem_euclid(7) as u32
    }

    /// `Mon` .. `Sun`.
    pub fn weekday_name(&self) -> &'static str {
        WEEKDAYS[self.weekday() as usize].0
    }

    fn next_second(self) -> Civil {
        let mut c = self;
        c.second += 1;
        if c.second == 60 {
            c.second = 0;
            c.minute += 1;
        }
        if c.minute == 60 {
            c.minute = 0;
            c.hour += 1;
        }
        if c.hour == 24 {
            c.hour = 0;
            c.day += 1;
        }
        if c.day > days_in_month(c.year, c.month) {
            c.day = 1;
            c.month += 1;
        }
        if c.month == 13 {
            c.month = 1;
            c.year += 1;
        }
        c
    }
}

impl fmt::Display for Civil {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// A time zone: what the clock on the wall says at a moment, and back.
pub trait Zone {
    fn civil(&self, t: SystemTime) -> Civil;
    /// The moment `c` names. Where a zone's clock skips or repeats an hour,
    /// whichever moment the zone picks; `None` if it names none at all.
    fn moment(&self, c: Civil) -> Option<SystemTime>;
}

/// Coordinated Universal Time.
#[derive(Debug, Clone, Copy)]
pub struct Utc;

impl Zone for Utc {
    fn civil(&self, t: SystemTime) -> Civil {
        let seconds = match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs_f64().ceil() as i64),
        };
        let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
        let of_day = seconds.rem_euclid(86_400) as u32;
        Civil {
            year,
            month,
            day,
            hour: of_day / 3600,
            minute: of_day / 60 % 60,
            second: of_day % 60,
        }
    }

    fn moment(&self, c: Civil) -> Option<SystemTime> {
        let seconds = days_from_civil(c.year, c.month, c.day) * 86_400
            + i64::from(c.hour * 3600 + c.minute * 60 + c.second);
        if seconds >= 0 {
            UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
        }
    }
}

fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    (year, month, day)
}

const WEEKDAYS: [(&str, &str); 7] = [
    ("Mon", "Monday"),
    ("Tue", "Tuesday"),
    ("Wed", "Wednesday"),
    ("Thu", "Thursday"),
    ("Fri", "Friday"),
    ("Sat", "Saturday"),
    ("Sun", "Sunday"),
];

/// `start`, or `start..end`, every `step`th value; no `end` is as far as the
/// field goes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Component {
    start: u32,
    end: Option<u32>,
    step: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    components: Vec<Component>,
    max: u32,
}

impl Field {
    fn matches(&self, value: u32) -> bool {
        self.components.iter().any(|c| {
            let end = c.end.unwrap_or(self.max);
            (c.start..=end).contains(&value) && (value - c.start).is_multiple_of(c.step)
        })
    }
}

/// An `OnCalendar=` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calendar {
    text: String,
    /// Bit n is weekday n, Monday 0.
    weekdays: u8,
    year: Field,
    month: Field,
    day: Field,
    /// The day counts back from the end of the month (`~`).
    from_month_end: bool,
    hour: Field,
    minute: Field,
    second: Field,
    utc: bool,
}

impl fmt::Display for Calendar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl Calendar {
    /// The first moment after `after` that the event names. `local` is the
    /// zone its times are in, unless it says UTC.
    pub fn next_after(&self, after: SystemTime, local: &dyn Zone) -> Option<SystemTime> {
        let zone: &dyn Zone = if self.utc { &Utc } else { local };
        let mut from = zone.civil(after);
        // A time the zone skips, or one that is not after `after` (the second
        // time round an hour the clocks go back over): try the next.
        for _ in 0..1000 {
            let civil = self.next_civil(from)?;
            match zone.moment(civil) {
                Some(t) if t > after => return Some(t),
                _ => from = civil,
            }
        }
        None
    }

    /// The first civil time after `after`, to the second, that the event
    /// names.
    fn next_civil(&self, after: Civil) -> Option<Civil> {
        let from = after.next_second();
        let first_year = u32::try_from(from.year).ok()?.max(MIN_YEAR);
        for year in first_year..=MAX_YEAR {
            if !self.year.matches(year) {
                continue;
            }
            let year = year as i32;
            let this_year = year == from.year;
            for month in (if this_year { from.month } else { 1 })..=12 {
                if !self.month.matches(month) {
                    continue;
                }
                let this_month = this_year && month == from.month;
                let length = days_in_month(year, month);
                for day in (if this_month { from.day } else { 1 })..=length {
                    let date = Civil {
                        year,
                        month,
                        day,
                        hour: 0,
                        minute: 0,
                        second: 0,
                    };
                    if !self.day_matches(day, length) || self.weekdays & (1 << date.weekday()) == 0
                    {
                        continue;
                    }
                    let earliest = if this_month && day == from.day {
                        (from.hour, from.minute, from.second)
                    } else {
                        (0, 0, 0)
                    };
                    if let Some((hour, minute, second)) = self.time_from(earliest) {
                        return Some(Civil {
                            hour,
                            minute,
                            second,
                            ..date
                        });
                    }
                }
            }
        }
        None
    }

    fn day_matches(&self, day: u32, length: u32) -> bool {
        if !self.from_month_end {
            return self.day.matches(day);
        }
        // Counted back: 1 is the last day. A repetition with no end runs on
        // to the end of the month, as systemd's does.
        let back = |n: u32| (length + 1).checked_sub(n).filter(|&d| d >= 1);
        self.day.components.iter().any(|c| {
            let Some(start) = back(c.start) else {
                return false;
            };
            let end = match c.end {
                None => length,
                Some(e) => back(e).unwrap_or(1),
            };
            let (low, high) = (start.min(end), start.max(end));
            (low..=high).contains(&day) && (day - low).is_multiple_of(c.step)
        })
    }

    /// The first time of day at or after `(hour, minute, second)` the event
    /// names.
    fn time_from(&self, (h0, m0, s0): (u32, u32, u32)) -> Option<(u32, u32, u32)> {
        for hour in h0..24 {
            if !self.hour.matches(hour) {
                continue;
            }
            for minute in (if hour == h0 { m0 } else { 0 })..60 {
                if !self.minute.matches(minute) {
                    continue;
                }
                let first = if hour == h0 && minute == m0 { s0 } else { 0 };
                if let Some(second) = (first..60).find(|&s| self.second.matches(s)) {
                    return Some((hour, minute, second));
                }
            }
        }
        None
    }
}

/// Read an `OnCalendar=` value.
pub fn parse_calendar(text: &str) -> Result<Calendar, String> {
    let original = text.trim();
    let mut tokens: Vec<&str> = original.split_whitespace().collect();
    let utc = tokens.last().is_some_and(|t| t.eq_ignore_ascii_case("UTC"));
    if utc {
        tokens.pop();
    }
    if let Some(zone) = tokens
        .last()
        .filter(|t| t.contains('/') && t.starts_with(|c: char| c.is_ascii_alphabetic()))
    {
        return Err(format!(
            "time zone {zone} is not supported; only local time and UTC are"
        ));
    }
    let shorthand = match tokens.as_slice() {
        [one] => match *one {
            "minutely" => Some("*-*-* *:*:00"),
            "hourly" => Some("*-*-* *:00:00"),
            "daily" => Some("*-*-* 00:00:00"),
            "weekly" => Some("Mon *-*-* 00:00:00"),
            "monthly" => Some("*-*-01 00:00:00"),
            "quarterly" => Some("*-01,04,07,10-01 00:00:00"),
            "semiannually" | "semi-annually" => Some("*-01,07-01 00:00:00"),
            "yearly" | "annually" => Some("*-01-01 00:00:00"),
            _ => None,
        },
        _ => None,
    };
    if let Some(expanded) = shorthand {
        tokens = expanded.split_whitespace().collect();
    }
    if tokens.is_empty() {
        return Err("empty calendar event".into());
    }

    let bad = || format!("{original:?} is not a calendar event");
    let mut rest = tokens.as_slice();
    let mut weekdays = 0x7f;
    if let [first, others @ ..] = rest {
        if first.starts_with(|c: char| c.is_ascii_alphabetic()) {
            weekdays = parse_weekdays(first)?;
            rest = others;
        }
    }
    let (mut date, mut time) = (None, None);
    for &token in rest {
        if token.contains(':') && time.is_none() {
            time = Some(token);
        } else if (token.contains('-') || token.contains('~')) && date.is_none() && time.is_none() {
            date = Some(token);
        } else {
            return Err(bad());
        }
    }

    let (year, month, day, from_month_end) = match date {
        None => (any(MIN_YEAR, MAX_YEAR), any(1, 12), any(1, 31), false),
        Some(date) => {
            let (head, day, from_month_end) = match date.split_once('~') {
                Some((head, day)) => (head, day, true),
                None => match date.rsplit_once('-') {
                    Some((head, day)) => (head, day, false),
                    None => return Err(bad()),
                },
            };
            let (year, month) = match head.split_once('-') {
                Some((year, month)) => (year_field(year)?, field(month, 1, 12, "month")?),
                None => (any(MIN_YEAR, MAX_YEAR), field(head, 1, 12, "month")?),
            };
            (year, month, field(day, 1, 31, "day")?, from_month_end)
        }
    };
    let (hour, minute, second) = match time {
        None => (exactly(0, 23), exactly(0, 59), exactly(0, 59)),
        Some(time) => match time.split(':').collect::<Vec<_>>().as_slice() {
            [h, m] => (
                field(h, 0, 23, "hour")?,
                field(m, 0, 59, "minute")?,
                exactly(0, 59),
            ),
            [h, m, s] => (
                field(h, 0, 23, "hour")?,
                field(m, 0, 59, "minute")?,
                field(s, 0, 59, "second")?,
            ),
            _ => return Err(bad()),
        },
    };
    Ok(Calendar {
        text: original.to_owned(),
        weekdays,
        year,
        month,
        day,
        from_month_end,
        hour,
        minute,
        second,
        utc,
    })
}

fn any(min: u32, max: u32) -> Field {
    Field {
        components: vec![Component {
            start: min,
            end: None,
            step: 1,
        }],
        max,
    }
}

fn exactly(value: u32, max: u32) -> Field {
    Field {
        components: vec![Component {
            start: value,
            end: Some(value),
            step: 1,
        }],
        max,
    }
}

fn parse_weekdays(text: &str) -> Result<u8, String> {
    let day = |name: &str| {
        WEEKDAYS
            .iter()
            .position(|(short, long)| {
                name.eq_ignore_ascii_case(short) || name.eq_ignore_ascii_case(long)
            })
            .ok_or_else(|| format!("{name:?} is not a day of the week"))
    };
    let mut days = 0u8;
    for item in text.split(',') {
        match item.split_once("..") {
            Some((from, to)) => {
                let (from, to) = (day(from)?, day(to)?);
                // Sat..Mon wraps round the weekend.
                let mut d = from;
                loop {
                    days |= 1 << d;
                    if d == to {
                        break;
                    }
                    d = (d + 1) % 7;
                }
            }
            None => days |= 1 << day(item)?,
        }
    }
    Ok(days)
}

/// A year, where two digits mean 1970 to 2069, as in systemd.
fn year_field(text: &str) -> Result<Field, String> {
    let widen = |digits: &str, n: u32| match n {
        _ if digits.len() > 2 => n,
        0..=69 => n + 2000,
        _ => n + 1900,
    };
    field_of(text, MIN_YEAR, MAX_YEAR, "year", widen)
}

fn field(text: &str, min: u32, max: u32, what: &str) -> Result<Field, String> {
    field_of(text, min, max, what, |_, n| n)
}

/// A field whose numbers are `value(digits, number)`.
fn field_of(
    text: &str,
    min: u32,
    max: u32,
    what: &str,
    value: impl Fn(&str, u32) -> u32,
) -> Result<Field, String> {
    let number = |s: &str| -> Result<u32, String> {
        let n = s
            .parse()
            .map(|n| value(s, n))
            .map_err(|_| format!("{s:?} is not a valid {what}"))?;
        if (min..=max).contains(&n) {
            Ok(n)
        } else {
            Err(format!("{what} {n} is not between {min} and {max}"))
        }
    };
    let mut components = Vec::new();
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .ok()
                    .filter(|&s| s > 0)
                    .ok_or_else(|| format!("{step:?} in {item:?} is not a repetition"))?;
                (range, Some(step))
            }
            None => (item, None),
        };
        let component = match (range, range.split_once("..")) {
            ("*", _) => Component {
                start: min,
                end: None,
                step: step.unwrap_or(1),
            },
            (_, Some((from, to))) => {
                let (start, end) = (number(from)?, number(to)?);
                if start > end {
                    return Err(format!("{item:?} is a range backwards"));
                }
                Component {
                    start,
                    end: Some(end),
                    step: step.unwrap_or(1),
                }
            }
            // `5` is 5 alone; `5/10` repeats from 5 to the end.
            (_, None) => {
                let start = number(range)?;
                Component {
                    start,
                    end: step.is_none().then_some(start),
                    step: step.unwrap_or(1),
                }
            }
        };
        components.push(component);
    }
    Ok(Field { components, max })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Civil {
        let (date, time) = text.split_once(' ').unwrap();
        let d: Vec<u32> = date.split('-').map(|n| n.parse().unwrap()).collect();
        let t: Vec<u32> = time.split(':').map(|n| n.parse().unwrap()).collect();
        Civil {
            year: d[0] as i32,
            month: d[1],
            day: d[2],
            hour: t[0],
            minute: t[1],
            second: t[2],
        }
    }

    /// The next few times `event` names after `from`, in UTC.
    fn next(event: &str, from: &str, count: usize) -> Vec<String> {
        let calendar = parse_calendar(event).unwrap();
        let mut t = Utc.moment(at(from)).unwrap();
        (0..count)
            .map_while(|_| {
                t = calendar.next_after(t, &Utc)?;
                Some(Utc.civil(t).to_string())
            })
            .collect()
    }

    #[test]
    fn civil_arithmetic() {
        for text in [
            "1970-01-01 00:00:00",
            "2000-02-29 12:34:56",
            "2026-09-12 23:59:59",
            "2199-12-31 00:00:00",
        ] {
            assert_eq!(Utc.civil(Utc.moment(at(text)).unwrap()), at(text));
        }
        // 2026-09-12 is a Saturday.
        assert_eq!(at("2026-09-12 00:00:00").weekday_name(), "Sat");
        assert_eq!(at("1970-01-01 00:00:00").weekday_name(), "Thu");
        assert_eq!(days_in_month(2100, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
    }

    #[test]
    fn shorthands() {
        let from = "2026-09-12 10:30:15";
        assert_eq!(next("minutely", from, 1), ["2026-09-12 10:31:00"]);
        assert_eq!(next("hourly", from, 1), ["2026-09-12 11:00:00"]);
        assert_eq!(next("daily", from, 1), ["2026-09-13 00:00:00"]);
        assert_eq!(next("weekly", from, 1), ["2026-09-14 00:00:00"]);
        assert_eq!(next("monthly", from, 1), ["2026-10-01 00:00:00"]);
        assert_eq!(next("quarterly", from, 1), ["2026-10-01 00:00:00"]);
        assert_eq!(next("semiannually", from, 1), ["2027-01-01 00:00:00"]);
        assert_eq!(next("yearly", from, 1), ["2027-01-01 00:00:00"]);
        assert_eq!(next("annually", from, 1), ["2027-01-01 00:00:00"]);
    }

    #[test]
    fn strictly_after() {
        // Exactly at an event: the next one.
        assert_eq!(
            next("daily", "2026-09-12 00:00:00", 2),
            ["2026-09-13 00:00:00", "2026-09-14 00:00:00"]
        );
    }

    #[test]
    fn weekdays_and_times() {
        // Friday evening: the working week starts again on Monday.
        assert_eq!(
            next("Mon..Fri 09:00", "2026-09-11 17:00:00", 2),
            ["2026-09-14 09:00:00", "2026-09-15 09:00:00"]
        );
        assert_eq!(
            next("Sat,Sunday 10:00", "2026-09-12 11:00:00", 2),
            ["2026-09-13 10:00:00", "2026-09-19 10:00:00"]
        );
        // A range round the weekend.
        assert_eq!(
            next("Sat..Mon", "2026-09-12 00:00:00", 3),
            [
                "2026-09-13 00:00:00",
                "2026-09-14 00:00:00",
                "2026-09-19 00:00:00"
            ]
        );
        assert_eq!(
            next("wed", "2026-09-12 00:00:00", 1),
            ["2026-09-16 00:00:00"]
        );
    }

    #[test]
    fn repetitions_lists_and_ranges() {
        assert_eq!(
            next("*:0/15", "2026-09-12 10:31:00", 3),
            [
                "2026-09-12 10:45:00",
                "2026-09-12 11:00:00",
                "2026-09-12 11:15:00"
            ]
        );
        assert_eq!(
            next("*-*-* 8..17:00", "2026-09-12 17:30:00", 2),
            ["2026-09-13 08:00:00", "2026-09-13 09:00:00"]
        );
        assert_eq!(
            next("*-*-* 08,20:30", "2026-09-12 09:00:00", 2),
            ["2026-09-12 20:30:00", "2026-09-13 08:30:00"]
        );
        assert_eq!(
            next("*:*:0/20", "2026-09-12 10:00:05", 3),
            [
                "2026-09-12 10:00:20",
                "2026-09-12 10:00:40",
                "2026-09-12 10:01:00"
            ]
        );
        // Every other day of the month, from the 1st.
        assert_eq!(
            next("*-*-1/2", "2026-09-29 12:00:00", 2),
            ["2026-10-01 00:00:00", "2026-10-03 00:00:00"]
        );
    }

    #[test]
    fn dates() {
        assert_eq!(
            next("*-*-01 03:30", "2026-09-12 00:00:00", 2),
            ["2026-10-01 03:30:00", "2026-11-01 03:30:00"]
        );
        assert_eq!(
            next("2027-03-15 12:00:00", "2026-09-12 00:00:00", 2),
            ["2027-03-15 12:00:00"]
        );
        // Month and day, every year.
        assert_eq!(
            next("12-25", "2026-09-12 00:00:00", 1),
            ["2026-12-25 00:00:00"]
        );
        // Two digits are this century's.
        assert_eq!(
            next("27-01-01", "2026-09-12 00:00:00", 1),
            ["2027-01-01 00:00:00"]
        );
        // Every other year counts from 1970.
        assert_eq!(
            next("*/2-01-01", "2026-09-12 00:00:00", 1),
            ["2028-01-01 00:00:00"]
        );
        // A leap day, from a year without one.
        assert_eq!(
            next("*-02-29", "2026-09-12 00:00:00", 1),
            ["2028-02-29 00:00:00"]
        );
        // A day that never comes.
        assert!(next("*-02-30", "2026-09-12 00:00:00", 1).is_empty());
        // A year that has been.
        assert!(next("2020-01-01", "2026-09-12 00:00:00", 1).is_empty());
    }

    #[test]
    fn counting_back_from_the_end_of_the_month() {
        assert_eq!(
            next("*-02~01", "2027-01-01 00:00:00", 2),
            ["2027-02-28 00:00:00", "2028-02-29 00:00:00"]
        );
        assert_eq!(
            next("*-*~03", "2026-09-01 00:00:00", 1),
            ["2026-09-28 00:00:00"]
        );
        assert_eq!(
            next("*-*~01..03", "2026-09-01 00:00:00", 3),
            [
                "2026-09-28 00:00:00",
                "2026-09-29 00:00:00",
                "2026-09-30 00:00:00"
            ]
        );
        // The last Monday in May.
        assert_eq!(
            next("Mon *-05~07/1", "2026-01-01 00:00:00", 2),
            ["2026-05-25 00:00:00", "2027-05-31 00:00:00"]
        );
    }

    /// Two hours ahead of UTC, all year.
    struct Plus2;

    impl Zone for Plus2 {
        fn civil(&self, t: SystemTime) -> Civil {
            Utc.civil(t + Duration::from_secs(7200))
        }
        fn moment(&self, c: Civil) -> Option<SystemTime> {
            Some(Utc.moment(c)? - Duration::from_secs(7200))
        }
    }

    #[test]
    fn local_time_and_utc() {
        let from = Utc.moment(at("2026-09-12 21:00:00")).unwrap();
        // Local midnight is 22:00 UTC.
        let local = parse_calendar("daily").unwrap();
        let t = local.next_after(from, &Plus2).unwrap();
        assert_eq!(Utc.civil(t), at("2026-09-12 22:00:00"));
        // UTC's midnight is UTC's, whatever the local zone.
        let utc = parse_calendar("daily UTC").unwrap();
        assert_eq!(utc.to_string(), "daily UTC");
        let t = utc.next_after(from, &Plus2).unwrap();
        assert_eq!(Utc.civil(t), at("2026-09-13 00:00:00"));
    }

    #[test]
    fn a_skipped_hour_is_skipped() {
        /// A zone whose clocks skip from 02:00 to 03:00 every night.
        struct Gap;
        impl Zone for Gap {
            fn civil(&self, t: SystemTime) -> Civil {
                Utc.civil(t)
            }
            fn moment(&self, c: Civil) -> Option<SystemTime> {
                (c.hour != 2).then(|| Utc.moment(c)).flatten()
            }
        }
        let calendar = parse_calendar("*-*-* 02,04:30").unwrap();
        let from = Utc.moment(at("2026-09-12 00:00:00")).unwrap();
        let t = calendar.next_after(from, &Gap).unwrap();
        assert_eq!(Utc.civil(t), at("2026-09-12 04:30:00"));
    }

    #[test]
    fn nonsense() {
        for (text, message) in [
            ("", "empty calendar event"),
            ("sometimes", "\"sometimes\" is not a day of the week"),
            ("Mon..Funday", "\"Funday\" is not a day of the week"),
            ("*-13-01", "month 13 is not between 1 and 12"),
            ("*-*-* 24:00", "hour 24 is not between 0 and 23"),
            ("*:0/0", "\"0\" in \"0/0\" is not a repetition"),
            (
                "*-*-* 10:00 Europe/Berlin",
                "time zone Europe/Berlin is not supported; only local time and UTC are",
            ),
            ("10:00 10:00", "\"10:00 10:00\" is not a calendar event"),
            ("*-*-5..3", "\"5..3\" is a range backwards"),
            ("*-*-* 1:2:3:4", "\"*-*-* 1:2:3:4\" is not a calendar event"),
            ("*-*-* 10:00:00.5", "\"00.5\" is not a valid second"),
            ("1969-01-01", "year 1969 is not between 1970 and 2199"),
        ] {
            assert_eq!(parse_calendar(text).unwrap_err(), message, "{text}");
        }
    }
}
