//! Manual PDF import. `pdftotext` extracts the raw text, Claude restructures
//! it into clean Markdown, and it lands as a live (written) catalog entry —
//! readable in the TUI and, once Syncthing mirrors it, on the phone.
//!
//! Three entry points share `import_pdf`:
//!   * the TUI `a` key (pick a PDF on the laptop, tag a subject),
//!   * `library --import` (headless),
//!   * `drain_inbox` — PDFs added on the phone arrive in `~/.library/inbox/`
//!     (a PDF plus a small `<stem>.json` sidecar) and are imported here.
//!
//! The phone never parses a PDF; it only copies the file into the synced
//! inbox. All conversion happens on the laptop, where `pdftotext` lives.

use std::path::{Path, PathBuf};

use crate::claude;
use crate::store::{self, Book, BookKind, Catalog};

pub fn inbox_dir() -> PathBuf { store::root().join("inbox") }

/// Expand a leading `~/` to `$HOME` for a user-typed path.
pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        store::home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// A filename stem turned into a plausible human title (underscores and
/// hyphens become spaces).
fn title_from_path(pdf: &Path) -> String {
    pdf.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run `pdftotext` and return the raw extracted UTF-8 text.
fn extract_text(pdf: &Path) -> Result<String, String> {
    let out = std::process::Command::new("pdftotext")
        .arg("-nopgbrk") // strip form-feed page breaks
        .arg("-q")
        .arg(pdf)
        .arg("-") // write to stdout
        .output()
        .map_err(|e| format!("pdftotext: {} — install poppler-utils", e))?;
    if !out.status.success() {
        return Err(format!("pdftotext failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if text.chars().filter(|c| !c.is_whitespace()).count() < 40 {
        return Err("no extractable text — looks like a scanned/image PDF".into());
    }
    Ok(text)
}

/// First substantial paragraph, squeezed to one line and capped — a shelf hook.
fn hook_from(text: &str) -> String {
    let para = text.split("\n\n").map(|p| p.trim()).find(|p| p.len() > 30).unwrap_or("");
    let one = para.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.is_empty() { return "Imported PDF.".into(); }
    let mut s: String = one.chars().take(200).collect();
    if one.chars().count() > 200 { s.push('\u{2026}'); }
    s
}

/// Import one PDF into `cat` as a live book: extract text, restructure via
/// Claude, write `books/<id>/book.md`, add the catalog entry (kind=real,
/// written=true, category = the tagged subject) and save the catalog.
/// Returns the resolved title. Source-file cleanup is the caller's job.
pub fn import_pdf(cat: &mut Catalog, pdf: &Path, title: &str, subject: &str, author: &str)
    -> Result<String, String>
{
    let title = {
        let t = title.trim();
        if t.is_empty() { title_from_path(pdf) } else { t.to_string() }
    };
    if title.is_empty() {
        return Err("empty title".into());
    }
    if cat.has_title(&title) {
        return Err(format!("'{}' is already on the shelf", title));
    }
    let raw = extract_text(pdf)?;
    let hook = hook_from(&raw);
    let md = claude::structure_pdf(&title, author, &raw)?;

    let id = cat.unique_id(&title);
    let dir = store::book_dir(&id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {}", dir.display(), e))?;
    std::fs::write(store::book_md(&id), md.as_bytes()).map_err(|e| format!("write book.md: {}", e))?;

    let subject = subject.trim();
    cat.books.push(Book {
        id,
        title: title.clone(),
        author: author.trim().to_string(),
        category: if subject.is_empty() { "Imported".into() } else { subject.to_string() },
        subcategory: String::new(),
        hook,
        tags: Vec::new(),
        kind: BookKind::Real,
        year: String::new(),
        isbn: String::new(),
        starred: false,
        written: true,
        deep: false,
        created_at: store::now_secs(),
    });
    cat.save()?;
    Ok(title)
}

/// Parse a phone-dropped sidecar (`{ "title", "subject", "author" }`).
fn read_sidecar(json_path: &Path) -> (String, String, String) {
    let txt = std::fs::read_to_string(json_path).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&txt).unwrap_or(serde_json::Value::Null);
    let g = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    (g("title"), g("subject"), g("author"))
}

/// Count queued inbox PDFs without importing — a stat-only gate so the TUI
/// does zero work at startup when nothing is queued.
pub fn inbox_count() -> usize {
    match std::fs::read_dir(inbox_dir()) {
        Ok(rd) => rd.flatten().filter(|e| is_pdf(&e.path())).count(),
        Err(_) => 0,
    }
}

fn is_pdf(p: &Path) -> bool {
    p.extension().and_then(|x| x.to_str())
        .map(|x| x.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
}

/// Import every `~/.library/inbox/*.pdf` (each optionally paired with a
/// `<stem>.json` sidecar from the phone), removing the source files after a
/// successful import. Returns (imported titles, errors). A failed PDF is left
/// in place so the next drain retries it.
pub fn drain_inbox(cat: &mut Catalog) -> (Vec<String>, Vec<String>) {
    let mut done = Vec::new();
    let mut errs = Vec::new();
    let mut pdfs: Vec<PathBuf> = match std::fs::read_dir(inbox_dir()) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| is_pdf(p)).collect(),
        Err(_) => return (done, errs),
    };
    pdfs.sort();
    for pdf in &pdfs {
        let side = pdf.with_extension("json");
        let (title, subject, author) = if side.exists() {
            read_sidecar(&side)
        } else {
            (String::new(), String::new(), String::new())
        };
        match import_pdf(cat, pdf, &title, &subject, &author) {
            Ok(t) => {
                let _ = std::fs::remove_file(pdf);
                let _ = std::fs::remove_file(&side);
                done.push(t);
            }
            Err(e) => errs.push(format!("{}: {}",
                pdf.file_name().and_then(|s| s.to_str()).unwrap_or("?"), e)),
        }
    }
    (done, errs)
}
