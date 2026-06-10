//! docker json-file log format: one JSON object per line.
//! {"log":"hello\n","stream":"stdout","time":"2026-06-10T12:00:00.000000000Z"}
//!
//! Writer side is used by the per-container pump threads; reader side by the
//! /containers/{id}/logs endpoint (tail + follow).

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct LogLine {
    pub log: String,
    pub stream: String, // stdout | stderr
    pub time: String,
}

pub struct LogWriter {
    file: File,
}

impl LogWriter {
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        Ok(Self {
            file: OpenOptions::new().create(true).append(true).open(path)?,
        })
    }

    pub fn write(&mut self, stream: &str, chunk: &str) -> io::Result<()> {
        let line = LogLine {
            log: chunk.to_string(),
            stream: stream.to_string(),
            time: rfc3339_now(),
        };
        let mut buf = serde_json::to_vec(&line)?;
        buf.push(b'\n');
        self.file.write_all(&buf)
    }
}

/// RFC3339Nano UTC, no external time crate: computed from the unix epoch.
pub fn rfc3339_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    rfc3339(now.as_secs() as i64, now.subsec_nanos())
}

pub fn rfc3339(secs: i64, nanos: u32) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{nanos:09}Z")
}

/// days-from-civil inverse (Howard Hinnant's algorithm).
pub fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, mi, s) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, mi, s)
}

/// Parse the `time` field back to (secs, nanos) for --since/--until.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    // "YYYY-MM-DDTHH:MM:SS[.frac]Z"
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // days_from_civil
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + sec)
}

pub struct LogReadOpts {
    pub stdout: bool,
    pub stderr: bool,
    pub tail: Option<usize>, // None = all
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub timestamps: bool,
}

/// Read existing log content, returning (stream, payload-bytes) frames.
/// `pos` resumes from a byte offset (for follow); returns the new offset.
pub fn read_log(
    path: &Path,
    opts: &LogReadOpts,
    pos: u64,
    mut emit: impl FnMut(&str, &[u8]),
) -> io::Result<u64> {
    let Ok(file) = File::open(path) else {
        return Ok(pos); // no output yet
    };
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(pos))?;
    let mut lines: Vec<LogLine> = Vec::new();
    let mut line = String::new();
    let mut consumed = pos;
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        // Only consume complete lines so a partially-written line is retried
        // on the next follow poll.
        if !line.ends_with('\n') {
            break;
        }
        consumed += n as u64;
        if let Ok(l) = serde_json::from_str::<LogLine>(&line) {
            lines.push(l);
        }
    }
    let start = match opts.tail {
        Some(t) if pos == 0 && lines.len() > t => lines.len() - t,
        _ => 0,
    };
    for l in &lines[start..] {
        if (l.stream == "stdout" && !opts.stdout) || (l.stream == "stderr" && !opts.stderr) {
            continue;
        }
        if let Some(since) = opts.since {
            if parse_rfc3339(&l.time).map(|t| t < since).unwrap_or(false) {
                continue;
            }
        }
        if let Some(until) = opts.until {
            if parse_rfc3339(&l.time).map(|t| t > until).unwrap_or(false) {
                continue;
            }
        }
        if opts.timestamps {
            let mut payload = l.time.clone().into_bytes();
            payload.push(b' ');
            payload.extend_from_slice(l.log.as_bytes());
            emit(&l.stream, &payload);
        } else {
            emit(&l.stream, l.log.as_bytes());
        }
    }
    Ok(consumed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip() {
        // 2026-06-10 00:00:00 UTC = 1781049600
        let (y, m, d, h, _, _) = civil_from_unix(1_781_049_600);
        assert_eq!((y, m, d, h), (2026, 6, 10, 0));
        assert_eq!(parse_rfc3339("2026-06-10T00:00:00.000000000Z"), Some(1_781_049_600));
        assert_eq!(parse_rfc3339(&rfc3339(1_781_049_600, 0)), Some(1_781_049_600));
    }

    #[test]
    fn tail_and_streams() {
        let dir = std::env::temp_dir().join(format!("slimlog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.log");
        let mut w = LogWriter::open(&path).unwrap();
        for i in 0..5 {
            w.write("stdout", &format!("line{i}\n")).unwrap();
        }
        w.write("stderr", "err\n").unwrap();
        let mut got = Vec::new();
        let opts = LogReadOpts {
            stdout: true,
            stderr: false,
            tail: Some(2),
            since: None,
            until: None,
            timestamps: false,
        };
        read_log(&path, &opts, 0, |s, b| {
            got.push((s.to_string(), String::from_utf8_lossy(b).into_owned()));
        })
        .unwrap();
        // tail=2 applies pre-filter (docker semantics are close enough here):
        // last 2 lines are "line4" and "err"; stderr filtered out.
        assert_eq!(got, vec![("stdout".into(), "line4\n".into())]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
