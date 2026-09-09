//! Spoken tracks for a book. Drop mp3 files in `books/<id>/audio/`: one
//! per chapter named after its heading (`dont-be-afraid.mp3`), numbered
//! in chapter order (`01.mp3`, `02.mp3`), or one file for the whole book.
//! mpv plays them over its socket; the reader asks where the voice is
//! once a second and scrolls the text to match.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use crate::store;

pub fn dir(id: &str) -> PathBuf { store::book_dir(id).join("audio") }

/// The book's tracks in name order.
pub fn tracks(id: &str) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir(id)) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |e| e.eq_ignore_ascii_case("mp3")))
        .collect();
    v.sort();
    v
}

/// Heading lines in the rendered text: (line index, level, text).
pub fn headings(md: &str, lines: &[String]) -> Vec<(usize, u8, String)> {
    let mut out = Vec::new();
    let mut cursor = 0;
    for raw in md.lines() {
        let t = raw.trim_end();
        let (level, text) = if let Some(x) = t.strip_prefix("### ") { (3, x) }
            else if let Some(x) = t.strip_prefix("## ") { (2, x) }
            else if let Some(x) = t.strip_prefix("# ") { (1, x) }
            else { continue };
        let text = text.trim();
        if let Some(pos) = lines[cursor..].iter().position(|l| crust::strip_ansi(l).trim() == text) {
            cursor += pos;
            out.push((cursor, level, text.to_string()));
            cursor += 1;
        }
    }
    out
}

/// Where each track starts and ends in the rendered lines. A track named
/// after a heading starts there. Tracks with no heading in their names
/// take the chapters in order. Anything else starts at the top. A span
/// ends where the nearest later track begins.
pub fn spans(tracks: &[PathBuf], headings: &[(usize, u8, String)], total: usize) -> Vec<(usize, usize)> {
    // Letters and digits only, so "Don't Be Afraid" meets dont-be-afraid.mp3.
    // A long name may have been cut short by whatever made the file, so a
    // heading that begins with a name of 24+ characters counts as well.
    let key = |s: &str| -> String { s.chars().flat_map(|c| c.to_lowercase()).filter(|c| c.is_alphanumeric()).collect() };
    let fits = |h: &str, s: &str| { let k = key(h); k == s || (s.len() >= 24 && k.starts_with(s)) };
    let stem = |p: &PathBuf| -> String {
        let s = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        key(s.trim_start_matches(|c: char| c.is_ascii_digit() || "-_ .".contains(c)))
    };
    let mut starts: Vec<Option<usize>> = tracks.iter().map(|t| {
        let s = stem(t);
        if s.is_empty() { return None; }
        headings.iter().find(|(_, _, h)| fits(h, &s)).map(|(l, _, _)| *l)
    }).collect();
    if tracks.len() > 1 && starts.iter().all(|s| s.is_none()) {
        let lvl = if headings.iter().filter(|h| h.1 == 2).count() >= 2 { 2 } else { 1 };
        let chapters: Vec<usize> = headings.iter().filter(|h| h.1 == lvl).map(|h| h.0).collect();
        for (s, line) in starts.iter_mut().zip(chapters) { *s = Some(line); }
    }
    (0..tracks.len()).map(|i| {
        let start = starts[i].unwrap_or(0);
        let end = starts.iter().flatten().filter(|&&s| s > start).min().copied().unwrap_or(total);
        (start, end)
    }).collect()
}

/// The track to play from reading position `pos`: the one whose span
/// starts nearest above it, the first of them on a tie, else the first.
pub fn track_at(spans: &[(usize, usize)], pos: usize) -> usize {
    let mut best: Option<(usize, usize)> = None;
    for (i, &(s, _)) in spans.iter().enumerate() {
        if s <= pos && best.map_or(true, |(_, bs)| s > bs) { best = Some((i, s)); }
    }
    best.map_or(0, |(i, _)| i)
}

/// The line to put at the top of the screen so the voice sits a third
/// of the way down, moved by the reader's own nudge.
pub fn voice_top(spans: &[(usize, usize)], track: usize, frac: f64, h: usize, nudge: i64, max_top: usize) -> usize {
    let (s, e) = spans.get(track).copied().unwrap_or((0, 0));
    let line = s as f64 + frac * (e.saturating_sub(s)) as f64;
    (line as i64 - h as i64 / 3 + nudge).clamp(0, max_top as i64) as usize
}

pub struct Player {
    child: Child,
    sock_path: PathBuf,
    sock: Option<BufReader<UnixStream>>,
}

impl Player {
    /// Start mpv on the whole track list, beginning at `index`.
    pub fn start(tracks: &[PathBuf], index: usize) -> Result<Player, String> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let sock_path = dir.join(format!("library-audio-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let child = Command::new("mpv")
            .arg("--no-video").arg("--really-quiet").arg("--no-terminal")
            .arg(format!("--input-ipc-server={}", sock_path.display()))
            .arg(format!("--playlist-start={}", index))
            .args(tracks)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .spawn().map_err(|e| format!("mpv: {}", e))?;
        Ok(Player { child, sock_path, sock: None })
    }

    fn connect(&mut self) -> Option<&mut BufReader<UnixStream>> {
        if self.sock.is_none() {
            let s = UnixStream::connect(&self.sock_path).ok()?;
            let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(500)));
            self.sock = Some(BufReader::new(s));
        }
        self.sock.as_mut()
    }

    /// One command, one reply. mpv's event lines are skipped.
    fn cmd(&mut self, args: &[&str]) -> Option<serde_json::Value> {
        let req = serde_json::json!({"command": args}).to_string() + "\n";
        let res = self.connect().and_then(|r| Self::exchange(r, &req));
        if res.is_none() { self.sock = None; }
        res
    }

    fn exchange(r: &mut BufReader<UnixStream>, req: &str) -> Option<serde_json::Value> {
        r.get_mut().write_all(req.as_bytes()).ok()?;
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).ok()? == 0 { return None; }
            let v: serde_json::Value = serde_json::from_str(&line).ok()?;
            if v.get("error").is_some() { return Some(v); }
        }
    }

    fn get_f64(&mut self, prop: &str) -> Option<f64> {
        self.cmd(&["get_property", prop])?.get("data")?.as_f64()
    }

    /// Current track index and how far into it the voice is (0..1).
    pub fn position(&mut self) -> Option<(usize, f64)> {
        let i = self.get_f64("playlist-pos")?;
        let pct = self.get_f64("percent-pos")?;
        Some((i.max(0.0) as usize, (pct / 100.0).clamp(0.0, 1.0)))
    }

    pub fn toggle_pause(&mut self) { self.cmd(&["cycle", "pause"]); }
    pub fn next(&mut self) { self.cmd(&["playlist-next"]); }
    pub fn prev(&mut self) { self.cmd(&["playlist-prev"]); }
    pub fn running(&mut self) -> bool { matches!(self.child.try_wait(), Ok(None)) }

    pub fn stop(mut self) {
        self.cmd(&["quit"]);
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(lines: &[(usize, u8, &str)]) -> Vec<(usize, u8, String)> {
        lines.iter().map(|&(l, v, t)| (l, v, t.to_string())).collect()
    }

    #[test]
    fn named_numbered_and_whole_book_tracks_find_their_lines() {
        let hs = h(&[(0, 1, "Title"), (10, 2, "Prologue"), (50, 2, "Don't Be Afraid"), (90, 2, "Part Two")]);
        let named = vec![PathBuf::from("02-dont-be-afraid.mp3"), PathBuf::from("prologue.mp3")];
        assert_eq!(spans(&named, &hs, 120), vec![(50, 120), (10, 50)]);
        let numbered = vec![PathBuf::from("01.mp3"), PathBuf::from("02.mp3"), PathBuf::from("03.mp3")];
        assert_eq!(spans(&numbered, &hs, 120), vec![(10, 50), (50, 90), (90, 120)]);
        let whole = vec![PathBuf::from("book.mp3")];
        assert_eq!(spans(&whole, &hs, 120), vec![(0, 120)]);
        // A name cut short by a 48-character slug still finds its heading.
        let hs = h(&[(0, 1, "Title"), (10, 2, "Why the regress does not terminate inside existence")]);
        let cut = vec![PathBuf::from("05-why-the-regress-does-not-terminate-inside-existe.mp3")];
        assert_eq!(spans(&cut, &hs, 120), vec![(10, 120)]);
    }

    #[test]
    fn the_track_under_the_cursor_is_the_nearest_start_above_it() {
        let sp = vec![(1, 40), (40, 90), (0, 1), (90, 120)];
        assert_eq!(track_at(&sp, 0), 2);
        assert_eq!(track_at(&sp, 5), 0);
        assert_eq!(track_at(&sp, 40), 1);
        assert_eq!(track_at(&sp, 200), 3);
    }

    #[test]
    fn the_voice_sits_a_third_down_and_takes_the_nudge() {
        let sp = vec![(100, 400)];
        assert_eq!(voice_top(&sp, 0, 0.5, 30, 0, 1000), 240);
        assert_eq!(voice_top(&sp, 0, 0.5, 30, -5, 1000), 235);
        assert_eq!(voice_top(&sp, 0, 0.0, 30, -500, 1000), 0);
    }
}
