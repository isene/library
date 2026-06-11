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

mod claude;
mod store;

use store::{Book, Catalog};

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
            "--n" if i + 1 < args.len() => { n = args[i + 1].parse().unwrap_or(n); i += 2; }
            "-h" | "--help" => { print_help(); return; }
            _ => { i += 1; }
        }
    }

    match mode {
        Some("seed") => cmd_seed(&text, n),
        Some("more") => cmd_more(&text, n),
        Some("list") => print_shelf(&Catalog::load()),
        _ => {
            // TUI not built yet — show the shelf and point at the CLI.
            let cat = Catalog::load();
            if cat.books.is_empty() {
                println!("Your library is empty. Stock it:\n  library --seed \"the things you're curious about\"");
            } else {
                print_shelf(&cat);
                println!("\n(interactive browse/read TUI is the next build step)");
            }
        }
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
    let got = if b.written { " ✓read" } else { "" };
    let sub = if b.subcategory.is_empty() { String::new() } else { format!(" [{}]", b.subcategory) };
    println!("{star}{title}{sub}{got}", star = star, title = b.title, sub = sub, got = got);
    if !b.author.is_empty() {
        println!("    by {}", b.author);
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
         library                                 browse (TUI — coming next)\n\n\
         Data lives in ~/.library/ (catalog.json + books/<id>/)."
    );
}
