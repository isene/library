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

/// Generate `n` more books focused on a topic/request, in the spirit of
/// the existing library, avoiding existing titles.
pub fn more_like(topic: &str, interests: &str, existing: &[String], n: usize) -> Result<Vec<Book>, String> {
    let seed = format!(
        "{}\n\nFocus this batch specifically on: {}",
        interests, topic
    );
    generate_catalog(&seed, existing, n)
}
