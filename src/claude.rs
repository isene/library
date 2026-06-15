//! `claude -p` integration. The prompt goes in on stdin, the response
//! comes back on stdout (same pattern drain + kastrup-triage use). Three
//! jobs: stock the shelves (catalog), write a grabbed book, and define a
//! highlighted phrase in context.

use std::io::Write;
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::store::{Book, BookKind};

/// Run `claude -p` with `prompt` on stdin, return its stdout text.
/// `model` is optional (`""` = the CLI default).
pub fn run_claude(prompt: &str, model: &str) -> Result<String, String> {
    let mut cmd = Command::new("claude");
    cmd.arg("-p");
    if !model.is_empty() {
        cmd.arg("--model").arg(model);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn claude: {} — is the `claude` CLI on PATH?", e))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes())
            .map_err(|e| format!("write prompt: {}", e))?;
    }
    let out = child.wait_with_output().map_err(|e| format!("wait: {}", e))?;
    if !out.status.success() {
        return Err(format!("claude failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[derive(Deserialize)]
struct GenBook {
    title: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    subcategory: String,
    #[serde(default)]
    hook: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    year: String,
    #[serde(default)]
    isbn: String,
}

/// Pull the first JSON array out of a model response, tolerating
/// ```json fences or stray prose around it.
fn extract_json_array(raw: &str) -> Option<&str> {
    let start = raw.find('[')?;
    let end = raw.rfind(']')?;
    if end > start { Some(&raw[start..=end]) } else { None }
}

fn parse_books(raw: &str) -> Result<Vec<Book>, String> {
    let json = extract_json_array(raw)
        .ok_or_else(|| format!("no JSON array in response: {}", raw.chars().take(120).collect::<String>()))?;
    let gen: Vec<GenBook> = serde_json::from_str(json)
        .map_err(|e| format!("parse books: {}", e))?;
    Ok(gen.into_iter().map(|g| {
        let kind = if g.kind.trim().eq_ignore_ascii_case("real") { BookKind::Real } else { BookKind::Conjured };
        // Conjured books carry no author — never present a made-up name.
        let author = if kind == BookKind::Real { g.author.trim().to_string() } else { String::new() };
        Book {
            title: g.title.trim().to_string(),
            author,
            category: if g.category.trim().is_empty() { "Miscellany".into() } else { g.category.trim().to_string() },
            subcategory: g.subcategory.trim().to_string(),
            hook: g.hook.trim().to_string(),
            tags: g.tags,
            kind,
            year: g.year.trim().to_string(),
            isbn: g.isbn.trim().to_string(),
            ..Default::default()
        }
    }).collect())
}

const CATALOG_MODEL: &str = "claude-sonnet-4-6";

/// Generate `n` fresh book ideas for the given interests, avoiding any
/// titles already on the shelf.
pub fn generate_catalog(interests: &str, existing: &[String], n: usize) -> Result<Vec<Book>, String> {
    let avoid = if existing.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nDo NOT repeat any of these already on the shelves:\n- {}",
            existing.join("\n- ")
        )
    };
    let prompt = format!(
        "You are the acquisitions librarian for a personal library of books \
that *should* exist but mostly don't. The owner's interests:\n\n{interests}\n\n\
Propose {n} NEW books to add to the shelves. Deliberately vary the scope:\n\
- some broad surveys of an entire field\n\
- some niche deep-dives into one specialised topic\n\
- some that illuminate just one small, surprising corner of a subject\n\
Favour the genuinely interesting and non-obvious over generic textbooks. \
Make each enticing — a book the owner would grab with both hands and start \
reading.\n\n\
MIX IN REAL BOOKS: make roughly one in four a REAL, actually-published book \
that genuinely fits the interests (kind \"real\") — use its true title, real \
author, and publication year. The rest are invented books that should exist \
(kind \"conjured\"); these have NO author — do not invent one. Both kinds get \
a hook.{avoid}\n\n\
Respond with ONLY a JSON array (no markdown fences, no prose before or \
after). Each element:\n\
{{\"title\": \"...\", \"author\": \"the real author for real books; empty \
string for conjured books (never invent an author)\", \"kind\": \"real\" or \
\"conjured\", \"year\": \
\"publication year for real books, else empty\", \"isbn\": \"ISBN if known \
for real books, else empty\", \"category\": \"top-level shelf, e.g. Physics, \
Philosophy, History, Mathematics, Technology\", \"subcategory\": \"finer \
section (may be empty)\", \"hook\": \"1-2 sentences on what makes it worth \
reading\", \"tags\": [\"a\", \"few\", \"keywords\"]}}",
        interests = interests, n = n, avoid = avoid
    );
    parse_books(&run_claude(&prompt, CATALOG_MODEL)?)
}

const BOOK_MODEL: &str = "claude-opus-4-8";

/// Write the full text of a grabbed book as Markdown. `deep` picks a
/// short-book length over a one-sitting read.
pub fn write_book(title: &str, hook: &str, category: &str, deep: bool) -> Result<String, String> {
    let spec = if deep {
        "a substantial short book of roughly 8000-12000 words across 6-9 chapters"
    } else {
        "a focused, satisfying read of roughly 2500-3500 words across 3-4 short chapters"
    };
    let prompt = format!(
        "Write the full text of a book titled \"{title}\".\n\
         What it is about: {hook}\n\
         Shelf: {category}\n\n\
         Write {spec}, for a curious, intelligent generalist reader. Make it \
         genuinely illuminating and a pleasure to read \u{2014} vivid and \
         concrete, well-structured, honest, never padded. Open with a hook \
         that earns the reader's attention and close with a resonant ending.\n\n\
         Use Markdown: a single top-level '# {title}' heading, '## ' chapter \
         headings, flowing prose paragraphs (*italic*, **bold**, and '>' \
         pull-quotes are fine).\n\n\
         ILLUSTRATIONS: include 2-4 simple, genuinely useful figures \u{2014} \
         clean diagrams/schematics that aid understanding, never decoration. \
         In the prose, put a marker line `[[FIG n: short caption]]` on its own \
         line exactly where each figure belongs (n = 1,2,3...). Then, AFTER the \
         entire book, output a line `===FIGURES===`, and for each figure a line \
         `---FIG n---` followed by a complete standalone `<svg ...>...</svg>` \
         (set a viewBox; no external refs/images/fonts; design for a DARK \
         background \u{2014} use light strokes/text, e.g. stroke=\"#ddd\" \
         fill=\"none\" or light fills, transparent background; keep it legible \
         and not too wide, roughly 640x400).\n\n\
         Output ONLY the book Markdown, then the figures block \u{2014} no \
         preamble, no commentary, no code fences.",
        title = title, hook = hook, category = category, spec = spec
    );
    Ok(strip_code_fences(&run_claude(&prompt, BOOK_MODEL)?))
}

/// Split a book-writer response into (markdown, [(n, svg)]). The figures
/// follow a `===FIGURES===` divider, each introduced by `---FIG n---`.
pub fn parse_book(raw: &str) -> (String, Vec<(usize, String)>) {
    let (md, figs_part) = match raw.split_once("===FIGURES===") {
        Some((a, b)) => (a.trim_end().to_string(), b),
        None => return (raw.trim().to_string(), Vec::new()),
    };
    let mut figs = Vec::new();
    // Each figure: a "---FIG n---" header then its SVG up to the next header.
    let mut current_n: Option<usize> = None;
    let mut buf = String::new();
    let flush = |n: Option<usize>, buf: &str, figs: &mut Vec<(usize, String)>| {
        if let Some(n) = n {
            if let Some(start) = buf.find("<svg") {
                if let Some(end) = buf.rfind("</svg>") {
                    figs.push((n, buf[start..end + 6].to_string()));
                }
            }
        }
    };
    for line in figs_part.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("---FIG").and_then(|r| r.strip_suffix("---")) {
            flush(current_n, &buf, &mut figs);
            buf.clear();
            current_n = rest.trim().parse::<usize>().ok();
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    flush(current_n, &buf, &mut figs);
    (md, figs)
}

/// Defensively unwrap a whole-document ```…``` fence the model may add
/// despite being told not to.
fn strip_code_fences(s: &str) -> String {
    let t = s.trim();
    if t.starts_with("```") {
        let mut lines: Vec<&str> = t.lines().collect();
        lines.remove(0); // drop ``` or ```markdown
        if lines.last().map(|l| l.trim() == "```").unwrap_or(false) { lines.pop(); }
        return lines.join("\n");
    }
    s.to_string()
}

const IMPORT_MODEL: &str = "claude-sonnet-4-6";

/// Re-render raw `pdftotext` output into clean, readable Markdown for an
/// imported book. Long inputs are processed in paragraph-aligned chunks:
/// a model response is output-capped, so a whole long book can't come back
/// in one shot. The first chunk carries the `# {title}` heading; later
/// chunks continue without re-adding it. Content is preserved verbatim in
/// meaning — never summarised, never invented.
pub fn structure_pdf(title: &str, author: &str, raw: &str) -> Result<String, String> {
    let chunks = chunk_text(raw, 12000);
    let total = chunks.len();
    let mut out = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let md = structure_chunk(title, author, chunk, i == 0, i + 1, total)?;
        let md = md.trim();
        if md.is_empty() { continue; }
        if !out.is_empty() { out.push_str("\n\n"); }
        out.push_str(md);
    }
    if out.trim().is_empty() {
        return Err("structuring produced no text".into());
    }
    Ok(out)
}

fn structure_chunk(title: &str, author: &str, chunk: &str, first: bool, n: usize, total: usize)
    -> Result<String, String>
{
    let by = if author.trim().is_empty() { String::new() } else { format!(" by {}", author.trim()) };
    let head = if first {
        format!("Start with a single top-level '# {}' heading.", title)
    } else {
        "Do NOT add a top-level '# ' heading — this is a continuation; pick up where the text left off.".to_string()
    };
    let part = if total > 1 { format!(" (part {} of {})", n, total) } else { String::new() };
    let prompt = format!(
        "The text below is raw `pdftotext` output for the book \"{title}\"{by}{part}. \
         It has hard-wrapped lines, words split by end-of-line hyphens, page numbers, \
         and running headers/footers. It also contains \u{27e6}PAGE n\u{27e7} markers \
         showing where each PDF page begins. Re-render it as clean, readable Markdown:\n\
         - Rejoin hard-wrapped lines into flowing paragraphs; repair hyphen-split words.\n\
         - Drop page numbers, running headers/footers, and other layout cruft.\n\
         - Add '## ' headings only where a real chapter or section heading actually \
           occurs in the text.\n\
         - FIGURES: wherever the book has a figure, diagram, illustration, chart, or \
           table (a captioned float, or a clearly-drawn diagram the prose discusses), \
           emit a line `[[FIGPAGE n: short caption]]` on its own line at that spot, \
           where n is the number from the nearest preceding \u{27e6}PAGE n\u{27e7} \
           marker. Be conservative: only mark figures clearly present, not every \
           passing mention. Reuse the figure's own caption text where it has one.\n\
         - Do NOT include the \u{27e6}PAGE n\u{27e7} markers themselves in your output.\n\
         - {head}\n\
         Preserve ALL the prose and its meaning. Do NOT summarise, omit, paraphrase, \
         or invent text — keep the original wording. Output ONLY the Markdown: no \
         preamble, no commentary, no code fences.\n\n\
         RAW TEXT:\n{chunk}",
        title = title, by = by, part = part, head = head, chunk = chunk
    );
    Ok(strip_code_fences(&run_claude(&prompt, IMPORT_MODEL)?))
}

/// Split text into ~`target`-char chunks, breaking only at blank lines
/// (paragraph boundaries) so a paragraph is never cut mid-sentence.
fn chunk_text(raw: &str, target: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for para in raw.split("\n\n") {
        if !cur.is_empty() && cur.len() + para.len() > target {
            chunks.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() { cur.push_str("\n\n"); }
        cur.push_str(para);
    }
    if !cur.trim().is_empty() { chunks.push(cur); }
    if chunks.is_empty() { chunks.push(raw.to_string()); }
    chunks
}

const DEFINE_MODEL: &str = "claude-haiku-4-5-20251001";

/// Context-aware definition of a highlighted word/phrase (scribe pattern).
pub fn define(phrase: &str, context: &str) -> Result<String, String> {
    let ctx: String = context.chars().take(2000).collect();
    let prompt = format!(
        "Define \"{phrase}\" as it is used in the passage below \u{2014} the \
         meaning that fits THIS context. 2-4 plain sentences, no preamble. If \
         it is a term of art, name the field and what it means specifically here.\
         \n\nPassage:\n{ctx}",
        phrase = phrase, ctx = ctx
    );
    run_claude(&prompt, DEFINE_MODEL)
}

/// A reader's companion for an in-copyright real book we won't reproduce:
/// a substantial synopsis + key ideas + where to read it legally.
pub fn reader_companion(title: &str, author: &str, year: &str) -> Result<String, String> {
    let by = if author.is_empty() { String::new() } else { format!(" by {}", author) };
    let yr = if year.is_empty() { String::new() } else { format!(" ({})", year) };
    let prompt = format!(
        "Write a rich reader's companion (Markdown) for the real, in-copyright \
         book \"{title}\"{by}{yr}. Do NOT reproduce the book's text. Include: a \
         '# {title}' heading, a vivid synopsis, its central ideas and arguments \
         chapter by chapter where you can, why it matters, and a final '## Where \
         to read it' section pointing to buying or borrowing it (bookshop, \
         publisher, or a library). ~1500-2500 words. Markdown only, no fences.",
        title = title, by = by, yr = yr
    );
    Ok(strip_code_fences(&run_claude(&prompt, BOOK_MODEL)?))
}

/// Generate `n` more books focused on a topic/request, in the spirit of
/// the existing library, avoiding existing titles.
pub fn more_like(topic: &str, interests: &str, existing: &[String], n: usize) -> Result<Vec<Book>, String> {
    let seed = format!(
        "{}\n\nFocus this batch specifically on: {}",
        interests, topic
    );
    generate_catalog(&seed, existing, n)
}
