//! library — a generative personal library.
//!
//! Browse shelves of books that *should* exist (cheap catalog ideas),
//! curate them, then grab one and have it written on demand. Phase 1
//! here is the generation spine + headless CLI; the crust browse/read
//! TUI lands next.
//!
//! CLI:
//!   library --seed "<interests>" [--n N]   stock the shelves from an interest blurb
//!   library --more "<topic>"     [--n N]   add more books on a topic
//!   library --list                          print the current shelf
//!   library                                 (TUI — coming next)

mod bookmark;
mod claude;
mod export;
mod import;
mod mathrender;
mod store;
mod tui;

use store::{Book, BookKind, Catalog};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut mode: Option<&str> = None;
    let mut text = String::new();
    let mut n: usize = 12;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" if i + 1 < args.len() => { mode = Some("seed"); text = args[i + 1].clone(); i += 2; }
            "--more" if i + 1 < args.len() => { mode = Some("more"); text = args[i + 1].clone(); i += 2; }
            "--list" => { mode = Some("list"); i += 1; }
            "--pdf" if i + 1 < args.len() => { mode = Some("pdf"); text = args[i + 1].clone(); i += 2; }
            "--import" => { mode = Some("import"); i += 1; }
            "--remath" if i + 1 < args.len() => { mode = Some("remath"); text = args[i + 1].clone(); i += 2; }
            "--n" if i + 1 < args.len() => { n = args[i + 1].parse().unwrap_or(n); i += 2; }
            "-h" | "--help" => { print_help(); return; }
            _ => { i += 1; }
        }
    }

    match mode {
        Some("seed") => cmd_seed(&text, n),
        Some("more") => cmd_more(&text, n),
        Some("list") => print_shelf(&Catalog::load()),
        Some("pdf") => cmd_pdf(&text),
        Some("import") => cmd_import(),
        Some("remath") => cmd_remath(&text),
        _ => tui::run(),
    }
}

fn cmd_seed(interests: &str, n: usize) {
    let mut cat = Catalog::load();
    // Remember/extend the interview seed so later --more batches share voice.
    if cat.interests.trim().is_empty() {
        cat.interests = interests.to_string();
    } else if !cat.interests.contains(interests) {
        cat.interests = format!("{}\n{}", cat.interests, interests);
    }
    eprintln!("stocking the shelves… (claude is choosing {} books)", n);
    generate_into(&mut cat, None, interests, n);
}

fn cmd_more(topic: &str, n: usize) {
    let mut cat = Catalog::load();
    if cat.books.is_empty() && cat.interests.trim().is_empty() {
        eprintln!("library is empty — run `library --seed \"…\"` first");
        return;
    }
    let interests = cat.interests.clone();
    eprintln!("finding {} more on “{}”…", n, topic);
    generate_into(&mut cat, Some(topic), &interests, n);
}

/// Shared generate → dedup-add → save → print path.
fn generate_into(cat: &mut Catalog, topic: Option<&str>, interests: &str, n: usize) {
    let existing: Vec<String> = cat.books.iter().map(|b| b.title.clone()).collect();
    let result = match topic {
        Some(t) => claude::more_like(t, interests, &existing, n),
        None => claude::generate_catalog(interests, &existing, n),
    };
    match result {
        Ok(books) => {
            let got = books.len();
            let added = cat.add(books);
            if let Err(e) = cat.save() {
                eprintln!("save failed: {}", e);
                return;
            }
            eprintln!("added {} new book(s) ({} skipped as duplicates). shelf now holds {}.",
                added, got - added, cat.books.len());
            print_shelf(cat);
        }
        Err(e) => eprintln!("generation failed: {}", e),
    }
}

/// Headless PDF export: `library --pdf <book-id>` (also the reader's `e`).
fn cmd_pdf(id: &str) {
    let cat = Catalog::load();
    let Some(book) = cat.books.iter().find(|b| b.id == id || store::slugify(&b.title) == store::slugify(id)) else {
        eprintln!("no book with id/title '{}'", id);
        std::process::exit(1);
    };
    let md = std::fs::read_to_string(store::book_md(&book.id)).unwrap_or_default();
    if md.trim().is_empty() {
        eprintln!("'{}' has not been written yet", book.title);
        std::process::exit(1);
    }
    match export::export_book_pdf(&book.id, &book.title, &md) {
        Ok(p) => println!("exported → {}", p.display()),
        Err(e) => { eprintln!("pdf export failed: {}", e); std::process::exit(1); }
    }
}

/// Headless inbox drain: import every PDF added on the phone (or dropped
/// into `~/.library/inbox/`). Run from cron or by hand; the TUI also drains
/// the inbox on launch.
fn cmd_import() {
    let mut cat = Catalog::load();
    let n = import::inbox_count();
    if n == 0 {
        eprintln!("inbox empty — drop a PDF in {} (with an optional <name>.json sidecar)",
            import::inbox_dir().display());
        return;
    }
    eprintln!("importing {} PDF(s) — structuring with Claude…", n);
    let (done, errs) = import::drain_inbox(&mut cat);
    for t in &done { eprintln!("  ✓ {}", t); }
    for e in &errs { eprintln!("  ✗ {}", e); }
    eprintln!("done: {} imported, {} failed. shelf now holds {}.",
        done.len(), errs.len(), cat.books.len());
}

/// Re-render the LaTeX math already present in a written book's `book.md`
/// into `[[EQ n]]` images, in place — no Claude, just LaTeX → PNG. Useful
/// after the importer gains math support, or to refresh equations.
fn cmd_remath(id_or_title: &str) {
    let cat = Catalog::load();
    let Some(book) = cat.books.iter()
        .find(|b| b.id == id_or_title || store::slugify(&b.title) == store::slugify(id_or_title))
    else {
        eprintln!("no book with id/title '{}'", id_or_title);
        std::process::exit(1);
    };
    let path = store::book_md(&book.id);
    let md = std::fs::read_to_string(&path).unwrap_or_default();
    if md.trim().is_empty() {
        eprintln!("'{}' has no written content", book.title);
        std::process::exit(1);
    }
    let before = md.matches("$$").count() / 2;
    eprintln!("rendering {} display equation(s) in '{}'…", before, book.title);
    let rendered = mathrender::render_math(&book.id, &md);
    if let Err(e) = std::fs::write(&path, rendered.as_bytes()) {
        eprintln!("write failed: {}", e);
        std::process::exit(1);
    }
    let eqs = std::fs::read_to_string(&path).unwrap_or_default().matches("[[EQ ").count();
    eprintln!("done: {} equation image(s) now in {}", eqs, store::book_img_dir(&book.id).display());
}

/// Print the shelf grouped by category — the read-only stand-in until
/// the TUI exists.
fn print_shelf(cat: &Catalog) {
    if cat.books.is_empty() {
        println!("(empty shelf)");
        return;
    }
    for category in cat.categories() {
        println!("\n=== {} ===", category);
        for b in cat.books.iter().filter(|b| b.category == category) {
            print_book(b);
        }
    }
}

fn print_book(b: &Book) {
    let star = if b.starred { "★ " } else { "  " };
    let kind = if b.kind == BookKind::Real { "◆ " } else { "" };
    let got = if b.written { " ✓read" } else { "" };
    let sub = if b.subcategory.is_empty() { String::new() } else { format!(" [{}]", b.subcategory) };
    println!("{star}{kind}{title}{sub}{got}", star = star, kind = kind, title = b.title, sub = sub, got = got);
    if b.kind == BookKind::Real && !b.author.is_empty() {
        let yr = if !b.year.is_empty() { format!(" ({})", b.year) } else { String::new() };
        println!("    by {}{}", b.author, yr);
    }
    if !b.hook.is_empty() {
        println!("    {}", b.hook);
    }
}

fn print_help() {
    println!(
        "library — a generative personal library\n\n\
         library --seed \"<interests>\" [--n N]   stock the shelves from an interest blurb\n\
         library --more \"<topic>\"     [--n N]   add more books on a topic\n\
         library --list                          print the current shelf\n\
         library --import                        import PDFs queued in ~/.library/inbox/\n\
         library                                 browse (TUI; press 'a' to add a PDF)\n\n\
         Data lives in ~/.library/ (catalog.json + books/<id>/)."
    );
}
