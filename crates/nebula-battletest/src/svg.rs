//! Dependency-free SVG line charts for the report. Deliberately simple:
//! linear scales, 5 ticks, fixed palette, legend. Good enough for README
//! benchmarks; not a plotting library.

const W: f64 = 800.0;
const H: f64 = 480.0;
const ML: f64 = 70.0; // left margin (y labels)
const MR: f64 = 20.0;
const MT: f64 = 44.0; // title
const MB: f64 = 56.0; // x labels

const PALETTE: &[&str] = &[
    "#4e79a7", "#f28e2b", "#e15759", "#76b7b2", "#59a14f", "#edc948", "#b07aa1", "#9c755f",
];

pub struct Series {
    pub name: String,
    pub points: Vec<(f64, f64)>,
}

pub fn line_chart(title: &str, x_label: &str, y_label: &str, series: &[Series]) -> String {
    let all: Vec<(f64, f64)> = series
        .iter()
        .flat_map(|s| s.points.iter().copied())
        .collect();
    if all.is_empty() {
        return format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}"><text x="20" y="40">{}: no data</text></svg>"##,
            esc(title)
        );
    }
    let (xmin, xmax) = pad_range(min_max(all.iter().map(|p| p.0)));
    let (ymin, ymax) = pad_range_zero(min_max(all.iter().map(|p| p.1)));
    let sx = |x: f64| ML + (x - xmin) / (xmax - xmin).max(1e-9) * (W - ML - MR);
    let sy = |y: f64| H - MB - (y - ymin) / (ymax - ymin).max(1e-9) * (H - MT - MB);

    let mut s = String::new();
    s.push_str(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" font-family="ui-monospace,Menlo,monospace" font-size="12">"##
    ));
    s.push_str(r##"<rect width="100%" height="100%" fill="#0d1117"/>"##);
    s.push_str(&format!(
        r##"<text x="{}" y="26" fill="#e6edf3" font-size="16" text-anchor="middle">{}</text>"##,
        W / 2.0,
        esc(title)
    ));

    // Gridlines + ticks.
    for i in 0..=5 {
        let yv = ymin + (ymax - ymin) * i as f64 / 5.0;
        let y = sy(yv);
        s.push_str(&format!(
            r##"<line x1="{ML}" y1="{y:.1}" x2="{:.1}" y2="{y:.1}" stroke="#21262d"/>"##,
            W - MR
        ));
        s.push_str(&format!(
            r##"<text x="{:.1}" y="{:.1}" fill="#8b949e" text-anchor="end">{}</text>"##,
            ML - 8.0,
            y + 4.0,
            fmt_num(yv)
        ));
        let xv = xmin + (xmax - xmin) * i as f64 / 5.0;
        let x = sx(xv);
        s.push_str(&format!(
            r##"<text x="{x:.1}" y="{:.1}" fill="#8b949e" text-anchor="middle">{}</text>"##,
            H - MB + 18.0,
            fmt_num(xv)
        ));
    }
    s.push_str(&format!(
        r##"<text x="{}" y="{}" fill="#8b949e" text-anchor="middle">{}</text>"##,
        ML + (W - ML - MR) / 2.0,
        H - 14.0,
        esc(x_label)
    ));
    s.push_str(&format!(
        r##"<text x="16" y="{}" fill="#8b949e" text-anchor="middle" transform="rotate(-90 16 {})">{}</text>"##,
        MT + (H - MT - MB) / 2.0,
        MT + (H - MT - MB) / 2.0,
        esc(y_label)
    ));

    for (i, ser) in series.iter().enumerate() {
        let color = PALETTE[i % PALETTE.len()];
        let mut pts = ser.points.clone();
        pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let path: Vec<String> = pts
            .iter()
            .map(|p| format!("{:.1},{:.1}", sx(p.0), sy(p.1)))
            .collect();
        s.push_str(&format!(
            r##"<polyline points="{}" fill="none" stroke="{color}" stroke-width="2"/>"##,
            path.join(" ")
        ));
        for p in &pts {
            s.push_str(&format!(
                r##"<circle cx="{:.1}" cy="{:.1}" r="3.5" fill="{color}"/>"##,
                sx(p.0),
                sy(p.1)
            ));
        }
        // Legend.
        let ly = MT + 6.0 + i as f64 * 18.0;
        s.push_str(&format!(
            r##"<rect x="{:.1}" y="{ly:.1}" width="12" height="12" fill="{color}"/>"##,
            ML + 12.0
        ));
        s.push_str(&format!(
            r##"<text x="{:.1}" y="{:.1}" fill="#e6edf3">{}</text>"##,
            ML + 30.0,
            ly + 10.0,
            esc(&ser.name)
        ));
    }
    s.push_str("</svg>");
    s
}

fn min_max(it: impl Iterator<Item = f64>) -> (f64, f64) {
    it.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
        (lo.min(v), hi.max(v))
    })
}

fn pad_range((lo, hi): (f64, f64)) -> (f64, f64) {
    if lo == hi {
        return (lo - 1.0, hi + 1.0);
    }
    let pad = (hi - lo) * 0.05;
    (lo - pad, hi + pad)
}

/// Y axes start at zero (counts/MiB — a truncated axis lies).
fn pad_range_zero((_, hi): (f64, f64)) -> (f64, f64) {
    if hi <= 0.0 {
        return (0.0, 1.0);
    }
    (0.0, hi * 1.08)
}

fn fmt_num(v: f64) -> String {
    if v.abs() >= 10_000.0 {
        format!("{:.0}k", v / 1000.0)
    } else if v.abs() >= 100.0 || v.fract().abs() < 1e-6 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_valid_svg() {
        let svg = line_chart(
            "t",
            "x",
            "y",
            &[Series {
                name: "a".into(),
                points: vec![(1.0, 2.0), (2.0, 4.0)],
            }],
        );
        assert!(svg.starts_with("<svg"));
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains("polyline"));
    }
}
