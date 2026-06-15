//! Manual PDF import. `pdftotext` extracts the raw text, Claude restructures
//! it into clean Markdown, the pages that hold figures are rasterised with
//! `pdftoppm` and inlined as `[[FIG n]]` images, and it lands as a live
//! (written) catalog entry — readable in the TUI and, once Syncthing mirrors
//! it, on the phone.
//!
//! Entry points:
//!   * the TUI `a` key (pick a PDF on the laptop, tag a subject) — runs
//!     `build_book` on a background thread so the UI never blocks,
//!   * `library --import` (headless) — drains the inbox synchronously,
//!   * PDFs added on the phone arrive in `~/.library/inbox/` (a PDF plus a
//!     small `<stem>.json` sidecar) and are imported the same way.
//!
//! The phone never parses a PDF; it only copies the file into the synced
//! inbox. All conversion happens on the laptop, where poppler lives.

use std::path::{Path, PathBuf};

use crate::claude;
use crate::store::{self, Book, BookKind, Catalog};

/// Cap on rendered figure pages per book — keeps a diagram-dense book from
/// syncing dozens of MB of page PNGs to the phone. Excess markers are
/// dropped (and logged by the caller via the returned count, if wanted).
const MAX_FIGS: usize = 60;

pub fn inbox_dir() -> PathBuf { store::root().join("inbox") }

/// Expand a leading `~/` to `$HOME` for a user-typed path.
pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        store::home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// A filename stem turned into a plausible human title.
pub fn title_from_path(pdf: &Path) -> String {
    pdf.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run `pdftotext` and return the raw extracted text. Page breaks (`\f`)
/// are kept so `paginate` can tag page numbers for figure placement.
fn extract_text(pdf: &Path) -> Result<String, String> {
    let out = std::process::Command::new("pdftotext")
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

/// Insert `⟦PAGE n⟧` markers at each page boundary so the structuring model
/// can attribute figures to a page number.
fn paginate(raw: &str) -> String {
    let mut out = String::from("\n\n\u{27e6}PAGE 1\u{27e7}\n\n");
    let mut page = 1usize;
    for ch in raw.chars() {
        if ch == '\u{0c}' {
            page += 1;
            out.push_str(&format!("\n\n\u{27e6}PAGE {}\u{27e7}\n\n", page));
        } else {
            out.push(ch);
        }
    }
    out
}

/// First substantial paragraph, squeezed to one line and capped — a shelf hook.
fn hook_from(text: &str) -> String {
    let para = text.split("\n\n").map(|p| p.trim())
        .find(|p| p.len() > 30 && !p.starts_with('\u{27e6}'))
        .unwrap_or("");
    let one = para.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.is_empty() { return "Imported PDF.".into(); }
    let mut s: String = one.chars().take(200).collect();
    if one.chars().count() > 200 { s.push('\u{2026}'); }
    s
}

/// Render one PDF page to `img/fig{k}.png` (130 dpi). Returns true on success.
fn render_page(pdf: &Path, img_dir: &Path, page: usize, k: usize) -> bool {
    let prefix = img_dir.join(format!("fig{}", k)); // pdftoppm -singlefile → <prefix>.png
    std::process::Command::new("pdftoppm")
        .args(["-png", "-r", "130", "-singlefile",
               "-f", &page.to_string(), "-l", &page.to_string()])
        .arg(pdf)
        .arg(&prefix)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        && img_dir.join(format!("fig{}.png", k)).exists()
}

/// Turn `[[FIGPAGE n: caption]]` markers (emitted by the structuring model)
/// into rendered `[[FIG k: caption]]` figures. Each distinct page is
/// rasterised once into `img/figK.png`; repeats of a page reuse that image.
/// Returns the rewritten Markdown.
fn render_figures(pdf: &Path, id: &str, md: &str) -> String {
    use std::collections::HashMap;
    let img_dir = store::book_img_dir(id);
    let _ = std::fs::create_dir_all(&img_dir);
    let mut page_to_k: HashMap<usize, usize> = HashMap::new();
    let mut next_k = 1usize;
    let mut out = String::with_capacity(md.len());

    for line in md.lines() {
        let t = line.trim();
        let marker = t.strip_prefix("[[FIGPAGE")
            .and_then(|r| r.strip_suffix("]]"))
            .map(|inner| {
                let inner = inner.trim();
                match inner.split_once(':') {
                    Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
                    None => (inner.to_string(), String::new()),
                }
            });
        match marker {
            Some((n_str, caption)) => {
                let page = match n_str.parse::<usize>() {
                    Ok(p) if p >= 1 => p,
                    _ => continue, // unparseable page → drop the marker
                };
                let k = if let Some(&k) = page_to_k.get(&page) {
                    Some(k)
                } else if next_k > MAX_FIGS {
                    None // over the cap — drop quietly
                } else if render_page(pdf, &img_dir, page, next_k) {
                    let k = next_k;
                    page_to_k.insert(page, k);
                    next_k += 1;
                    Some(k)
                } else {
                    None // render failed — drop the marker rather than show a hole
                };
                if let Some(k) = k {
                    out.push_str(&format!("[[FIG {}: {}]]\n", k, caption));
                }
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// Build a live book from a PDF without touching the catalog: extract text,
/// restructure via Claude, render figure pages, write `books/<id>/book.md`
/// (+ `img/`), and return the `Book` (kind=real, written=true) for the
/// caller to add. `id` must already be unique on the shelf.
pub fn build_book(pdf: &Path, id: &str, title: &str, subject: &str, author: &str)
    -> Result<Book, String>
{
    let raw = extract_text(pdf)?;
    let hook = hook_from(&raw);
    let paginated = paginate(&raw);
    let md = claude::structure_pdf(title, author, &paginated)?;

    std::fs::create_dir_all(store::book_dir(id))
        .map_err(|e| format!("mkdir {}: {}", store::book_dir(id).display(), e))?;
    let md = render_figures(pdf, id, &md);
    std::fs::write(store::book_md(id), md.as_bytes())
        .map_err(|e| format!("write book.md: {}", e))?;

    let subject = subject.trim();
    Ok(Book {
        id: id.to_string(),
        title: title.to_string(),
        author: author.trim().to_string(),
        category: if subject.is_empty() { "Imported".into() } else { subject.to_string() },
        hook,
        kind: BookKind::Real,
        written: true,
        created_at: store::now_secs(),
        ..Default::default()
    })
}

/// Import one PDF into `cat` (headless path): resolve a unique id, build the
/// book, add the entry, save. Returns the resolved title.
pub fn import_pdf(cat: &mut Catalog, pdf: &Path, title: &str, subject: &str, author: &str)
    -> Result<String, String>
{
    let title = {
        let t = title.trim();
        if t.is_empty() { title_from_path(pdf) } else { t.to_string() }
    };
    if title.is_empty() { return Err("empty title".into()); }
    if cat.has_title(&title) { return Err(format!("'{}' is already on the shelf", title)); }
    let id = cat.unique_id(&title);
    let book = build_book(pdf, &id, &title, subject, author)?;
    cat.books.push(book);
    cat.save()?;
    Ok(title)
}

/// Parse a phone-dropped sidecar (`{ "title", "subject", "author" }`).
pub fn read_sidecar(json_path: &Path) -> (String, String, String) {
    let txt = std::fs::read_to_string(json_path).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&txt).unwrap_or(serde_json::Value::Null);
    let g = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    (g("title"), g("subject"), g("author"))
}

fn is_pdf(p: &Path) -> bool {
    p.extension().and_then(|x| x.to_str())
        .map(|x| x.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
}

/// Queued inbox PDFs (sorted), each optionally paired with a `<stem>.json`.
pub fn inbox_pdfs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(inbox_dir()) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| is_pdf(p)).collect(),
        Err(_) => return Vec::new(),
    };
    v.sort();
    v
}

/// Count queued inbox PDFs — a stat-only gate.
pub fn inbox_count() -> usize { inbox_pdfs().len() }

/// Import every queued inbox PDF synchronously (headless `--import`),
/// removing source files after each success. Returns (titles, errors).
pub fn drain_inbox(cat: &mut Catalog) -> (Vec<String>, Vec<String>) {
    let mut done = Vec::new();
    let mut errs = Vec::new();
    for pdf in inbox_pdfs() {
        let side = pdf.with_extension("json");
        let (title, subject, author) = if side.exists() {
            read_sidecar(&side)
        } else {
            (String::new(), String::new(), String::new())
        };
        match import_pdf(cat, &pdf, &title, &subject, &author) {
            Ok(t) => {
                let _ = std::fs::remove_file(&pdf);
                let _ = std::fs::remove_file(&side);
                done.push(t);
            }
            Err(e) => errs.push(format!("{}: {}",
                pdf.file_name().and_then(|s| s.to_str()).unwrap_or("?"), e)),
        }
    }
    (done, errs)
}
