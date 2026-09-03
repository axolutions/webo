//! Turning values into text a model can act on. This is half the value of the
//! MCP server: the HTTP API answers in bytes and unix timestamps because a
//! browser renders them, but an agent reads the answer. 83361792 means nothing;
//! "79.5 MB" does. A 5760-point series is noise; "avg 0.1%, peak 0.4% 3h ago,
//! steady" is an answer.

/// Bytes with the unit a person would say out loud.
pub fn bytes(b: u64) -> String {
    const K: f64 = 1000.0;
    let v = b as f64;
    if v >= K * K * K {
        format!("{:.1} GB", v / (K * K * K))
    } else if v >= K * K {
        format!("{:.1} MB", v / (K * K))
    } else if v >= K {
        format!("{:.0} KB", v / K)
    } else {
        format!("{b} B")
    }
}

pub fn bytes_per_sec(b: u64) -> String {
    if b == 0 {
        return "idle".into();
    }
    format!("{}/s", bytes(b))
}

/// Percentages: enough precision to be useful, not enough to be noise.
pub fn pct(v: f32) -> String {
    if v >= 10.0 {
        format!("{v:.0}%")
    } else {
        format!("{v:.1}%")
    }
}

/// A span of seconds as the largest two units that matter.
pub fn duration(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}min", secs / 60);
    }
    if secs < 86400 {
        let (h, m) = (secs / 3600, (secs % 3600) / 60);
        return if m > 0 { format!("{h}h {m}min") } else { format!("{h}h") };
    }
    let (d, h) = (secs / 86400, (secs % 86400) / 3600);
    if h > 0 { format!("{d}d {h}h") } else { format!("{d}d") }
}

/// How long ago something happened, from a unix timestamp.
pub fn ago(ts: i64, now: i64) -> String {
    if ts <= 0 {
        return "never".into();
    }
    let delta = now.saturating_sub(ts);
    if delta < 0 {
        return "just now".into();
    }
    format!("{} ago", duration(delta as u64))
}

/// A wall-clock stamp, UTC, minute precision — for pinning an event in time.
pub fn clock(ts: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(ts) {
        Ok(t) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}Z",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute()
        ),
        Err(_) => "-".into(),
    }
}

/// What a time series is doing, without the series.
#[derive(Debug, PartialEq)]
pub struct Summary {
    pub points: usize,
    pub avg: f64,
    pub min: f64,
    pub max: f64,
    /// When the peak happened (unix seconds), so the agent can go look at the
    /// logs around it.
    pub peak_ts: i64,
    /// "steady" | "rising" | "falling" — second half against the first.
    pub trend: &'static str,
}

/// Reduces a series to what an agent needs to decide something.
pub fn summarize(series: &[(i64, f64)]) -> Option<Summary> {
    if series.is_empty() {
        return None;
    }
    let n = series.len();
    let sum: f64 = series.iter().map(|(_, v)| v).sum();
    let (mut min, mut max, mut peak_ts) = (f64::MAX, f64::MIN, series[0].0);
    for &(ts, v) in series {
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
            peak_ts = ts;
        }
    }
    let avg = sum / n as f64;
    // a single point has no trend; otherwise compare halves and call anything
    // under a tenth of the average "steady" so noise does not read as a trend
    let trend = if n < 4 {
        "steady"
    } else {
        let half = n / 2;
        let first: f64 = series[..half].iter().map(|(_, v)| v).sum::<f64>() / half as f64;
        let second: f64 = series[half..].iter().map(|(_, v)| v).sum::<f64>() / (n - half) as f64;
        let threshold = (avg.abs() * 0.1).max(f64::EPSILON);
        if second - first > threshold {
            "rising"
        } else if first - second > threshold {
            "falling"
        } else {
            "steady"
        }
    };
    Some(Summary { points: n, avg, min, max, peak_ts, trend })
}

/// Keeps at most `keep` evenly spread points — the shape without the volume.
pub fn decimate<T: Copy>(series: &[T], keep: usize) -> Vec<T> {
    if series.len() <= keep || keep == 0 {
        return series.to_vec();
    }
    let step = series.len() as f64 / keep as f64;
    (0..keep)
        .map(|i| series[((i as f64 * step) as usize).min(series.len() - 1)])
        .collect()
}

/// One line describing a series, ready to drop into a tool's answer.
/// `unit` formats a single value (bytes, pct, …).
pub fn series_line(label: &str, series: &[(i64, f64)], now: i64, unit: impl Fn(f64) -> String) -> String {
    match summarize(series) {
        None => format!("{label}: no samples in this window"),
        Some(s) => format!(
            "{label}: avg {} · peak {} ({}) · {} · {} samples",
            unit(s.avg),
            unit(s.max),
            ago(s.peak_ts, now),
            s.trend,
            s.points
        ),
    }
}

/// Indents a block so it reads as belonging to the line above it.
pub fn indent(text: &str, by: &str) -> String {
    text.lines().map(|l| format!("{by}{l}")).collect::<Vec<_>>().join("\n")
}

/// Cuts long text and says so — a stack trace must never eat the context.
pub fn cap(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}\n… {} more characters cut", count - max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_read_like_a_person_would_say_them() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(83_361_792), "83.4 MB");
        assert_eq!(bytes(1_800_000_000), "1.8 GB");
        assert_eq!(bytes(48_000), "48 KB");
        assert_eq!(bytes_per_sec(0), "idle", "zero is a state, not a number");
        assert_eq!(bytes_per_sec(1_500_000), "1.5 MB/s");
    }

    #[test]
    fn percentages_keep_precision_only_where_it_matters() {
        assert_eq!(pct(0.06), "0.1%");
        assert_eq!(pct(0.4), "0.4%");
        assert_eq!(pct(34.6), "35%", "nobody needs a decimal at 35% cpu");
    }

    #[test]
    fn durations_and_ago() {
        assert_eq!(duration(45), "45s");
        assert_eq!(duration(1560), "26min");
        assert_eq!(duration(3600), "1h");
        assert_eq!(duration(4980), "1h 23min");
        assert_eq!(duration(190_800), "2d 5h");
        assert_eq!(duration(172_800), "2d");
        assert_eq!(ago(1000, 2560), "26min ago");
        assert_eq!(ago(0, 1000), "never");
        assert_eq!(ago(2000, 1000), "just now", "clock skew is not a negative age");
    }

    #[test]
    fn clock_pins_an_event_in_time() {
        // 2025-09-02 00:49 UTC
        assert_eq!(clock(1_756_774_140), "2025-09-02 00:49Z");
        assert_eq!(clock(i64::MAX), "-");
    }

    #[test]
    fn a_series_becomes_an_answer() {
        // rising: the second half is clearly higher
        let rising: Vec<(i64, f64)> = (0..10).map(|i| (1000 + i * 60, i as f64)).collect();
        let s = summarize(&rising).unwrap();
        assert_eq!(s.points, 10);
        assert_eq!(s.max, 9.0);
        assert_eq!(s.peak_ts, 1000 + 9 * 60, "the peak carries its timestamp");
        assert_eq!(s.trend, "rising");

        let falling: Vec<(i64, f64)> = (0..10).map(|i| (1000 + i * 60, (9 - i) as f64)).collect();
        assert_eq!(summarize(&falling).unwrap().trend, "falling");

        // noise around a mean is steady, not a trend
        let noisy: Vec<(i64, f64)> = (0..12)
            .map(|i| (1000 + i * 60, 100.0 + if i % 2 == 0 { 1.0 } else { -1.0 }))
            .collect();
        assert_eq!(summarize(&noisy).unwrap().trend, "steady");

        assert!(summarize(&[]).is_none(), "an empty window says so, it does not lie");
        assert_eq!(summarize(&[(1, 5.0)]).unwrap().trend, "steady", "one point has no trend");
    }

    #[test]
    fn decimation_keeps_the_shape_and_the_ends() {
        let series: Vec<i64> = (0..1000).collect();
        let small = decimate(&series, 24);
        assert_eq!(small.len(), 24);
        assert_eq!(small[0], 0, "starts at the beginning");
        assert!(*small.last().unwrap() > 900, "reaches the end");
        // already small enough is untouched
        assert_eq!(decimate(&[1, 2, 3], 24), vec![1, 2, 3]);
        assert_eq!(decimate(&[1, 2, 3], 0), vec![1, 2, 3]);
    }

    #[test]
    fn a_series_line_is_readable_and_carries_the_peak() {
        let series: Vec<(i64, f64)> = (0..20).map(|i| (10_000 + i * 60, 80_000_000.0 + i as f64 * 1e6)).collect();
        let line = series_line("RAM", &series, 10_000 + 20 * 60, bytes_ish);
        assert!(line.starts_with("RAM: avg "), "{line}");
        assert!(line.contains("peak 99.0 MB"), "{line}");
        assert!(line.contains("ago"), "the peak says when: {line}");
        assert!(line.contains("rising"), "{line}");
        assert!(line.contains("20 samples"), "{line}");

        let empty = series_line("CPU", &[], 0, bytes_ish);
        assert_eq!(empty, "CPU: no samples in this window");
    }
    fn bytes_ish(v: f64) -> String {
        bytes(v as u64)
    }

    #[test]
    fn long_text_is_cut_and_says_so() {
        let short = cap("two lines\nhere", 100);
        assert_eq!(short, "two lines\nhere");
        let long = cap(&"x".repeat(500), 100);
        assert!(long.contains("400 more characters cut"), "{long}");
        assert_eq!(long.lines().next().unwrap().chars().count(), 100);
    }

    #[test]
    fn indent_marks_a_block_as_nested() {
        assert_eq!(indent("a\nb", "  "), "  a\n  b");
        assert_eq!(indent("", "  "), "");
    }
}
