//! `10s`, `5m`, `2h`, `500ms`, or a bare number of seconds.

use std::time::Duration;

pub fn parse(text: &str) -> Option<Duration> {
    let text = text.trim();
    if let Ok(secs) = text.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let split = text.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    let millis = match unit.trim() {
        "ms" => number,
        "s" | "sec" | "secs" => number * 1_000.0,
        "m" | "min" | "mins" => number * 60_000.0,
        "h" | "hr" | "hrs" => number * 3_600_000.0,
        "d" => number * 86_400_000.0,
        _ => return None,
    };
    Some(Duration::from_millis(millis as u64))
}
