//! Output formatting: docker-style tables, human sizes/durations, and
//! `--format` template rendering via slim-tmpl.

pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }
    let mut out = String::new();
    for (i, h) in headers.iter().enumerate() {
        out.push_str(&pad(h, widths[i], i + 1 == headers.len()));
    }
    out.push('\n');
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            out.push_str(&pad(cell, widths.get(i).copied().unwrap_or(0), i + 1 == row.len()));
        }
        out.push('\n');
    }
    out
}

fn pad(s: &str, w: usize, last: bool) -> String {
    if last {
        s.to_string()
    } else {
        format!("{s:<width$}   ", width = w)
    }
}

pub fn short_id(id: &str) -> String {
    id.trim_start_matches("sha256:").chars().take(12).collect()
}

pub fn human_size(bytes: i64) -> String {
    let b = bytes as f64;
    const U: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    let mut v = b;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{}B", bytes)
    } else {
        format!("{v:.3}{}", U[i]).replace(".000", "")
    }
}

/// "About a minute ago", "3 hours ago", "5 seconds ago" — relative to now.
pub fn relative_time(unix_secs: i64) -> String {
    if unix_secs == 0 {
        return "N/A".into();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(unix_secs);
    let d = (now - unix_secs).max(0);
    let s = match d {
        0..=4 => "Less than a second".to_string(),
        5..=59 => format!("{d} seconds"),
        60..=119 => "About a minute".to_string(),
        120..=3599 => format!("{} minutes", d / 60),
        3600..=7199 => "About an hour".to_string(),
        7200..=86399 => format!("{} hours", d / 3600),
        86400..=172799 => "About a day".to_string(),
        _ => format!("{} days", d / 86400),
    };
    format!("{s} ago")
}

/// Render a docker `--format` / inspect `-f` template.
pub fn render_template(tmpl: &str, value: &serde_json::Value) -> Result<String, String> {
    slim_tmpl::render(tmpl, value).map_err(|e| e.to_string())
}

/// `--format` may be a Go template or the literal `json`.
pub fn apply_format(fmt: &str, value: &serde_json::Value) -> Result<String, String> {
    if fmt == "json" {
        return Ok(serde_json::to_string(value).unwrap_or_default());
    }
    render_template(fmt, value)
}
