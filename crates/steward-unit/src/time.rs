//! Time spans as systemd writes them (systemd.time(7)): `90`, `5s`, `500ms`,
//! `1min 30s`, `1h30min`, `infinity`. A bare number is seconds.

use std::time::Duration;

/// `infinity` parses to `Duration::MAX`.
pub fn parse_timespan(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("empty time span".into());
    }
    if text == "infinity" {
        return Ok(Duration::MAX);
    }
    if let Ok(seconds) = text.parse::<f64>() {
        return seconds_to_duration(seconds, text);
    }

    let mut total = 0f64;
    let mut rest = text;
    while !rest.is_empty() {
        let number_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if number_len == 0 {
            return Err(format!("{text:?} is not a time span"));
        }
        let number: f64 = rest[..number_len]
            .parse()
            .map_err(|_| format!("{:?} in {text:?} is not a number", &rest[..number_len]))?;
        rest = rest[number_len..].trim_start();

        let unit_len = rest
            .find(|c: char| !c.is_alphabetic())
            .unwrap_or(rest.len());
        let unit = &rest[..unit_len];
        let scale = match unit {
            "" => 1.0,
            "us" | "usec" | "µs" => 1e-6,
            "ms" | "msec" => 1e-3,
            "s" | "sec" | "second" | "seconds" => 1.0,
            "m" | "min" | "minute" | "minutes" => 60.0,
            "h" | "hr" | "hour" | "hours" => 3600.0,
            "d" | "day" | "days" => 86_400.0,
            "w" | "week" | "weeks" => 604_800.0,
            other => return Err(format!("unknown time unit {other:?} in {text:?}")),
        };
        total += number * scale;
        rest = rest[unit_len..].trim_start();
    }
    seconds_to_duration(total, text)
}

fn seconds_to_duration(seconds: f64, text: &str) -> Result<Duration, String> {
    Duration::try_from_secs_f64(seconds).map_err(|_| format!("{text:?} is out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans() {
        let ms = Duration::from_millis;
        assert_eq!(parse_timespan("90").unwrap(), ms(90_000));
        assert_eq!(parse_timespan("0.5").unwrap(), ms(500));
        assert_eq!(parse_timespan("5s").unwrap(), ms(5_000));
        assert_eq!(parse_timespan("500ms").unwrap(), ms(500));
        assert_eq!(parse_timespan("1min 30s").unwrap(), ms(90_000));
        assert_eq!(parse_timespan("1h30min").unwrap(), ms(5_400_000));
        assert_eq!(parse_timespan("2 min").unwrap(), ms(120_000));
        assert_eq!(parse_timespan("1.5s").unwrap(), ms(1_500));
        assert_eq!(parse_timespan("infinity").unwrap(), Duration::MAX);
    }

    #[test]
    fn nonsense() {
        assert!(parse_timespan("").is_err());
        assert!(parse_timespan("soon").is_err());
        assert!(parse_timespan("5 fortnights").is_err());
        assert!(parse_timespan("-5s").is_err());
    }
}
