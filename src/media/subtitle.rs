//! Minimal SRT subtitle parser used for the on-screen subtitle overlay.
//!
//! Only external `.srt` files are supported (same file stem as the video).
//! Embedded subtitle streams and styled ASS are future work.

#[derive(Debug, Clone, Default)]
pub struct SubtitleFile {
    cues: Vec<Cue>,
}

#[derive(Debug, Clone)]
struct Cue {
    start: f64,
    end: f64,
    text: String,
}

impl SubtitleFile {
    pub fn parse_ass(ass: &str) -> Self {
        let mut cues = Vec::new();
        for line in ass.lines() {
            let line = line.trim();
            if !line.starts_with("Dialogue:") {
                continue;
            }
            // Dialogue: layer,start,end,style,name,marginL,marginR,marginV,effect,text
            let fields = line.splitn(10, ',').collect::<Vec<_>>();
            if fields.len() < 10 {
                continue;
            }
            let Some(start) = parse_time_ass(fields[1].trim()) else {
                continue;
            };
            let Some(end) = parse_time_ass(fields[2].trim()) else {
                continue;
            };
            let text = strip_ass_tags(fields[9].trim());
            if !text.is_empty() {
                cues.push(Cue { start, end, text });
            }
        }
        cues.sort_by(|a, b| {
            a.start
                .partial_cmp(&b.start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self { cues }
    }

    pub fn parse(srt: &str) -> Self {
        let mut cues = Vec::new();
        for block in srt.split("\n\n") {
            let mut lines = block.lines();
            let _index = lines.next().unwrap_or("").trim().parse::<u64>().ok();
            let Some(time_line) = lines.next() else {
                continue;
            };
            let Some((start, end)) = parse_time_range(time_line) else {
                continue;
            };
            let text = lines.collect::<Vec<_>>().join("\n").trim().to_string();
            if !text.is_empty() {
                cues.push(Cue { start, end, text });
            }
        }
        cues.sort_by(|a, b| {
            a.start
                .partial_cmp(&b.start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self { cues }
    }

    /// Return the subtitle text active at `pos_secs`.
    pub fn for_time(&self, pos_secs: f64) -> Option<&str> {
        self.cues
            .iter()
            .find(|c| pos_secs >= c.start && pos_secs <= c.end)
            .map(|c| c.text.as_str())
    }
}

/// Parse an ASS timestamp `H:MM:SS.cc`.
fn parse_time_ass(s: &str) -> Option<f64> {
    let mut parts = s.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let secs: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + secs)
}

/// Remove `{\...}` override blocks and convert `\N` to a newline.
fn strip_ass_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            while let Some(&n) = chars.peek() {
                chars.next();
                if n == '}' {
                    break;
                }
            }
        } else if c == '\\' && chars.peek() == Some(&'N') {
            chars.next();
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_time_range(s: &str) -> Option<(f64, f64)> {
    let mut parts = s.split("-->");
    let a = parse_time(parts.next()?.trim())?;
    let b = parse_time(parts.next()?.trim())?;
    Some((a, b))
}

fn parse_time(s: &str) -> Option<f64> {
    // HH:MM:SS,mmm or HH:MM:SS.mmm
    let (rest, ms) = if let Some(comma) = s.find(',') {
        (&s[..comma], &s[comma + 1..])
    } else if let Some(dot) = s.rfind('.') {
        (&s[..dot], &s[dot + 1..])
    } else {
        (s, "0")
    };
    let mut parts = rest.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let secs: f64 = parts.next()?.parse().ok()?;
    let ms: f64 = ms.parse().ok().unwrap_or(0.0);
    Some(h * 3600.0 + m * 60.0 + secs + ms / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_srt() {
        let srt = "1\n00:00:01,000 --> 00:00:04,000\nHello\nWorld\n\n2\n00:00:05,000 --> 00:00:06,500\nSecond\n";
        let subs = SubtitleFile::parse(srt);
        assert_eq!(subs.for_time(0.0), None);
        assert_eq!(subs.for_time(1.5), Some("Hello\nWorld"));
        assert_eq!(subs.for_time(5.2), Some("Second"));
    }
}
