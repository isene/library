//! `claude -p` integration. The prompt goes in on stdin, the response
//! comes back on stdout (same pattern drain + kastrup-triage use). Three
//! jobs: stock the shelves (catalog), write a grabbed book, and define a
//! highlighted phrase in context.

use std::io::Write;
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::store::Book;

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
    Ok(gen.into_iter().map(|g| Book {
        title: g.title.trim().to_string(),
        author: g.author.trim().to_string(),
        category: if g.category.trim().is_empty() { "Miscellany".into() } else { g.category.trim().to_string() },
        subcategory: g.subcategory.trim().to_string(),
        hook: g.hook.trim().to_string(),
        tags: g.tags,
        ..Default::default()
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
reading.{avoid}\n\n\
Respond with ONLY a JSON array (no markdown fences, no prose before or \
after). Each element:\n\
{{\"title\": \"...\", \"author\": \"a fitting author persona (a real-sounding \
expert name)\", \"category\": \"top-level shelf, e.g. Physics, Philosophy, \
History, Mathematics, Technology\", \"subcategory\": \"finer section (may be \
empty)\", \"hook\": \"1-2 sentences on what makes it worth reading\", \
\"tags\": [\"a\", \"few\", \"keywords\"]}}",
        interests = interests, n = n, avoid = avoid
    );
    parse_books(&run_claude(&prompt, CATALOG_MODEL)?)
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
