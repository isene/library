//! Library storage model. Everything lives under `~/.library/`.
//!
//! `catalog.json` holds every book *idea* — cheap metadata only (title,
//! author persona, shelf, a one-line hook). Full content is generated
//! lazily into `books/<id>/book.md` (+ `img/`) the moment a book is
//! grabbed, so an enormous browsable library costs almost nothing until
//! you actually read something. The whole tree is plain JSON + Markdown
//! so Syncthing can mirror it to the phone for offline reading.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Whether a book is conjured by Claude on demand, or a real existing
/// book recommended into the library (fetched from a source on grab).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BookKind {
    #[default]
    Conjured,
    Real,
}

/// One book on the shelf. While `written` is false only the metadata
/// exists; grabbing it fills `books/<id>/book.md` (conjured → Claude
/// writes it; real → fetched from the configured source).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Book {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub author: String,
    pub category: String,
    #[serde(default)]
    pub subcategory: String,
    pub hook: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub kind: BookKind,
    /// Real books only: publication year + ISBN (model-provided, may
    /// want verifying at fetch time). Empty for conjured books.
    #[serde(default)]
    pub year: String,
    #[serde(default)]
    pub isbn: String,
    #[serde(default)]
    pub starred: bool,
    #[serde(default)]
    pub written: bool,
    /// True if the written content is the longer "deep dive" form.
    #[serde(default)]
    pub deep: bool,
    #[serde(default)]
    pub created_at: i64,
}

/// User-tunable 256-colour palette for the TUI. Struct-level `#[serde(default)]`
/// fills any missing field from `Default`, so old catalogs and partial edits
/// stay valid. Two groups: the shelf (library) view and the reader view.
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(default)]
pub struct Colors {
    pub header: u8,        // category heading
    pub body: u8,          // book title
    pub dim: u8,           // author / hints
    pub sel: u8,           // selection arrow / underline
    pub del: u8,           // marked for deletion
    pub real: u8,          // real (existing) book
    pub body_bright: u8,   // written conjured book
    pub real_bright: u8,   // written real book
    pub hook: u8,          // hook / detail body
    pub tag: u8,           // tags / figure captions
    pub border: u8,        // pane borders
    pub bar_bg: u8,        // top + status bar background
    pub reader_fg: u8,     // reader body text
    pub reader_h1: u8,     // reader title (#)
    pub reader_h2: u8,     // reader chapter (##)
    pub reader_h3: u8,     // reader subhead (###)
    pub reader_quote: u8,  // reader pull-quote
}

impl Default for Colors {
    fn default() -> Self {
        Colors {
            header: 73, body: 252, dim: 245, sel: 81, del: 88,
            real: 222, body_bright: 255, real_bright: 229, hook: 250,
            tag: 109, border: 238, bar_bg: 236,
            reader_fg: 255, reader_h1: 81, reader_h2: 73, reader_h3: 109, reader_quote: 245,
        }
    }
}

/// The full catalog: the interview/seed summary plus every book idea.
#[derive(Serialize, Deserialize, Default)]
pub struct Catalog {
    #[serde(default)]
    pub interests: String,
    #[serde(default)]
    pub books: Vec<Book>,
    /// Persisted shelf-list pane width (columns). 0 = use the default.
    #[serde(default)]
    pub list_w: u16,
    /// Persisted border mode: 0 none, 1 right, 2 both, 3 left.
    #[serde(default)]
    pub border: u8,
    /// Persisted reading-column width (columns) in the reader. 0 = default.
    #[serde(default)]
    pub read_w: u16,
    /// Optional custom real-book fetch command (shell template; placeholders
    /// @title @author @isbn). Empty = use the built-in legal sources
    /// (Project Gutenberg full text, else a Claude reader's companion).
    #[serde(default)]
    pub fetch_cmd: String,
    /// User-tuned colour overrides (the `P` config popup).
    #[serde(default)]
    pub colors: Colors,
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}
pub fn root() -> PathBuf { home().join(".library") }
pub fn catalog_path() -> PathBuf { root().join("catalog.json") }
pub fn books_dir() -> PathBuf { root().join("books") }
pub fn book_dir(id: &str) -> PathBuf { books_dir().join(id) }
pub fn book_md(id: &str) -> PathBuf { book_dir(id).join("book.md") }
pub fn book_img_dir(id: &str) -> PathBuf { book_dir(id).join("img") }

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Turn a title into a filesystem-safe slug for the book id.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') { out.pop(); }
    out.chars().take(48).collect()
}

impl Catalog {
    pub fn load() -> Catalog {
        std::fs::read_to_string(catalog_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        std::fs::create_dir_all(root()).map_err(|e| format!("mkdir: {}", e))?;
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("encode: {}", e))?;
        // Atomic write so a Syncthing mid-write read never sees half a file.
        let tmp = catalog_path().with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("write: {}", e))?;
        std::fs::rename(&tmp, catalog_path()).map_err(|e| format!("rename: {}", e))
    }

    /// True if a title (case/space-insensitive) is already on the shelf.
    pub fn has_title(&self, title: &str) -> bool {
        let key = slugify(title);
        self.books.iter().any(|b| slugify(&b.title) == key)
    }

    /// Add freshly-generated books, skipping titles already present and
    /// assigning unique ids + timestamps. Returns how many were added.
    pub fn add(&mut self, incoming: Vec<Book>) -> usize {
        let mut added = 0;
        for mut b in incoming {
            if b.title.trim().is_empty() || self.has_title(&b.title) { continue; }
            b.id = self.unique_id(&b.title);
            b.created_at = now_secs();
            b.written = false;
            self.books.push(b);
            added += 1;
        }
        added
    }

    fn unique_id(&self, title: &str) -> String {
        let base = {
            let s = slugify(title);
            if s.is_empty() { "book".to_string() } else { s }
        };
        if !self.books.iter().any(|b| b.id == base) {
            return base;
        }
        let mut n = 2;
        loop {
            let cand = format!("{}-{}", base, n);
            if !self.books.iter().any(|b| b.id == cand) { return cand; }
            n += 1;
        }
    }

    /// Distinct categories, in first-seen order (shelf headings).
    pub fn categories(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for b in &self.books {
            if !seen.iter().any(|c| c == &b.category) {
                seen.push(b.category.clone());
            }
        }
        seen
    }
}
