//! Synced reading positions. Stored OUTSIDE `~/.library` (which the phone
//! mirrors read-only) in `~/.library-state/` — a small sendreceive
//! Syncthing folder, so the bookmark follows you laptop <-> phone. One
//! file per book keeps same-time edits on different books from colliding.
//!
//! A bookmark is a fraction 0..1 of the way through the book, so it stays
//! roughly right even when the reading width (and thus line count) differs
//! between devices.

use std::path::PathBuf;

use crate::store::{home, now_secs};

pub fn state_dir() -> PathBuf { home().join(".library-state") }
fn path(id: &str) -> PathBuf { state_dir().join(format!("{}.json", id)) }

/// Saved reading position as a fraction 0..1, if any.
pub fn load(id: &str) -> Option<f32> {
    let s = std::fs::read_to_string(path(id)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("pos").and_then(|p| p.as_f64()).map(|p| p as f32)
}

/// Persist the reading position (atomic write so a mid-sync read never
/// sees half a file).
pub fn save(id: &str, pos: f32) {
    let _ = std::fs::create_dir_all(state_dir());
    let json = format!("{{\"pos\": {:.4}, \"updated\": {}}}\n",
        pos.clamp(0.0, 1.0), now_secs());
    let tmp = path(id).with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, path(id));
    }
}
