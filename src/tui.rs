//! The browse / curate TUI. Two panes like gazette: a shelf list on the
//! left (books grouped under category headings) and the selected book's
//! detail on the right. Curate with `*` (star), `d`/`<` (mark/purge), and
//! `+`/`s` (ask `claude -p` for more, generated on a background thread
//! with a spinner so the UI stays live). Grabbing a book to read is the
//! next build step.

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender};

use crust::{Crust, Input, Pane, style};

use crate::claude;
use crate::store::{Book, BookKind, Catalog};

const C_HEADER: u8 = 73;   // category heading (teal)
const C_BODY:   u8 = 252;  // book title
const C_DIM:    u8 = 245;  // author / hints
const C_SEL:    u8 = 81;   // selection highlight
const C_DEL:    u8 = 88;   // marked-for-deletion (dark red)
const C_REAL:   u8 = 222;  // real (existing) books — warm gold
const C_HOOK:   u8 = 250;  // hook body
const C_TAG:    u8 = 109;  // tags

const LIST_W: u16 = 46;
const GEN_N: usize = 10;   // books per `+`/`s` batch

const SPINNER: [&str; 10] = ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"];

enum Entry { Header(String), Book(usize) }

/// A finished generation batch (or its error) handed back from a worker
/// thread over the channel. Generation is fully async: the UI stays
/// interactive while batches brew, and results merge in when they land.
type GenResult = Result<Vec<Book>, String>;

pub struct App {
    cols: u16,
    rows: u16,
    top: Pane,
    left: Pane,
    right: Pane,
    foot: Pane,
    cat: Catalog,
    entries: Vec<Entry>,
    sel: usize,
    top_row: usize,
    delete_marked: HashSet<String>,
    gen_tx: Sender<GenResult>,
    gen_rx: Receiver<GenResult>,
    gen_in_flight: usize,
    spin: usize,
    list_w: u16,
}

pub fn run() {
    Crust::init();
    Crust::set_app_identity("Library");
    let mut app = App::new();
    app.render_all();
    app.run();
    let _ = app.cat.save();
    Crust::cleanup();
    Crust::clear_screen();
}

impl App {
    fn new() -> Self {
        let (cols, rows) = Crust::terminal_size();
        let cat = Catalog::load();
        let body_h = rows.saturating_sub(3);
        let list_w = if cat.list_w >= 24 && cat.list_w + 24 < cols { cat.list_w } else { LIST_W.min(cols.saturating_sub(24)) };
        let mut top = Pane::new(1, 1, cols, 1, C_SEL as u16, 236);
        top.scroll = false; top.wrap = false;
        let mut left = Pane::new(1, 3, list_w, body_h, C_BODY as u16, 0);
        left.scroll = false; left.wrap = false;
        let mut right = Pane::new(list_w + 2, 3, cols.saturating_sub(list_w + 1), body_h, C_BODY as u16, 0);
        right.scroll = false; right.wrap = false;
        let mut foot = Pane::new(1, rows, cols, 1, C_DIM as u16, 236);
        foot.scroll = false; foot.wrap = false;
        let (gen_tx, gen_rx) = mpsc::channel();
        let mut app = App {
            cols, rows, top, left, right, foot,
            cat,
            entries: Vec::new(),
            sel: 0,
            top_row: 0,
            delete_marked: HashSet::new(),
            gen_tx, gen_rx,
            gen_in_flight: 0,
            spin: 0,
            list_w,
        };
        app.rebuild(None);
        app
    }

    /// Recompute pane geometry after a width change and repaint clean.
    fn relayout(&mut self) {
        self.left.w = self.list_w;
        self.right.x = self.list_w + 2;
        self.right.w = self.cols.saturating_sub(self.list_w + 1);
        Crust::clear_screen();
        self.top.invalidate();
        self.foot.invalidate();
        self.render_all();
    }

    /// `w` / `W` — widen / narrow the shelf list (persisted), like
    /// pointer/kastrup. Clamped so both panes stay usable.
    fn cycle_width(&mut self, wider: bool) {
        let step = 4u16;
        let min = 24u16;
        let max = self.cols.saturating_sub(24);
        let new = if wider { self.list_w.saturating_add(step) } else { self.list_w.saturating_sub(step) };
        self.list_w = new.clamp(min, max.max(min));
        self.cat.list_w = self.list_w;
        let _ = self.cat.save();
        self.relayout();
    }

    /// `i` — edit the library's interests in $EDITOR (scribe), then save.
    /// New books (s / +) are generated to match. Never deletes books.
    fn edit_interests(&mut self) {
        let tmp = format!("/tmp/library_interests_{}.txt", std::process::id());
        let header = "# Your library interests — edit freely, then save & quit.\n\
                      # New books (s = seed, + = more) are generated to match this.\n\
                      # Lines starting with # are ignored.\n\n";
        let _ = std::fs::write(&tmp, format!("{}{}", header, self.cat.interests));
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "scribe".into());
        Crust::cleanup();
        let _ = std::process::Command::new("sh").arg("-c")
            .arg(format!("{} {}", editor, crust::shell_escape(&tmp)))
            .status();
        Crust::init();
        Crust::set_app_identity("Library");
        Crust::clear_screen();
        if let Ok(edited) = std::fs::read_to_string(&tmp) {
            let cleaned = edited.lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .collect::<Vec<_>>().join("\n")
                .trim().to_string();
            if cleaned != self.cat.interests {
                self.cat.interests = cleaned;
                let _ = self.cat.save();
            }
        }
        let _ = std::fs::remove_file(&tmp);
        self.render_all();
        self.render_foot(" Interests saved. Press s / + to grow the shelf to match.");
    }

    /// Rebuild the display list from the catalog (category heading then
    /// its books). `keep` is a book id to keep selected across the
    /// rebuild; otherwise the current selection is clamped.
    fn rebuild(&mut self, keep: Option<String>) {
        let want = keep.or_else(|| self.selected_book_id());
        let mut entries = Vec::new();
        for category in self.cat.categories() {
            entries.push(Entry::Header(category.clone()));
            for (i, b) in self.cat.books.iter().enumerate() {
                if b.category == category { entries.push(Entry::Book(i)); }
            }
        }
        self.entries = entries;
        // Restore selection by id, else land on the first book.
        self.sel = want
            .and_then(|id| self.entries.iter().position(|e| matches!(e, Entry::Book(i) if self.cat.books[*i].id == id)))
            .unwrap_or(0);
        if !matches!(self.entries.get(self.sel), Some(Entry::Book(_))) {
            self.sel = self.first_book().unwrap_or(0);
        }
    }

    fn first_book(&self) -> Option<usize> {
        self.entries.iter().position(|e| matches!(e, Entry::Book(_)))
    }

    fn selected_book_idx(&self) -> Option<usize> {
        match self.entries.get(self.sel) {
            Some(Entry::Book(i)) => Some(*i),
            _ => None,
        }
    }

    fn selected_book_id(&self) -> Option<String> {
        self.selected_book_idx().map(|i| self.cat.books[i].id.clone())
    }

    /// Move selection to the next/prev Book entry, skipping headings.
    fn move_sel(&mut self, down: bool) {
        if self.entries.is_empty() { return; }
        let mut i = self.sel;
        loop {
            if down {
                if i + 1 >= self.entries.len() { break; }
                i += 1;
            } else {
                if i == 0 { break; }
                i -= 1;
            }
            if matches!(self.entries[i], Entry::Book(_)) { self.sel = i; break; }
        }
        self.render_left();
        self.render_right();
    }

    fn go_edge(&mut self, last: bool) {
        if last {
            self.sel = self.entries.iter().rposition(|e| matches!(e, Entry::Book(_))).unwrap_or(self.sel);
        } else {
            self.sel = self.first_book().unwrap_or(0);
        }
        self.render_left();
        self.render_right();
    }

    fn render_all(&mut self) {
        self.render_top();
        self.render_left();
        self.render_right();
        self.render_foot("");
    }

    fn render_top(&mut self) {
        let n = self.cat.books.len();
        let written = self.cat.books.iter().filter(|b| b.written).count();
        let marked = self.delete_marked.len();
        let mark_s = if marked > 0 { format!("  \u{00b7}  {} marked", marked) } else { String::new() };
        let title = format!(" library   {} books \u{00b7} {} written{}", n, written, mark_s);
        // Right side: a live "brewing" indicator while batches generate in
        // the background. Keys live in the footer only (no duplicate map).
        let right = if self.gen_in_flight > 0 {
            format!("{} {} brewing\u{2026} ", SPINNER[self.spin], self.gen_in_flight)
        } else {
            String::new()
        };
        let pad = (self.cols as usize)
            .saturating_sub(crust::display_width(&title) + crust::display_width(&right));
        self.top.say(&format!("{}{}{}",
            style::bold(&style::fg(&title, C_SEL)),
            " ".repeat(pad),
            style::fg(&right, C_REAL)));
    }

    fn render_left(&mut self) {
        let h = self.left.h as usize;
        if h == 0 { return; }
        if self.sel < self.top_row { self.top_row = self.sel; }
        if self.sel >= self.top_row + h { self.top_row = self.sel + 1 - h; }
        let end = (self.top_row + h).min(self.entries.len());
        let mut lines = String::new();
        for idx in self.top_row..end {
            match &self.entries[idx] {
                Entry::Header(c) => {
                    lines.push_str(&style::bold(&style::fg(&format!("{}", c), C_HEADER)));
                }
                Entry::Book(bi) => {
                    let b = &self.cat.books[*bi];
                    let marked = self.delete_marked.contains(&b.id);
                    let star = if b.starred { '\u{2605}' } else { ' ' };
                    let flag = if marked { 'D' } else { ' ' };
                    // One plain string, one colour per line — no nested ANSI.
                    let title = trunc(&b.title, (self.list_w as usize).saturating_sub(6));
                    let plain = format!(" {}{} {}", star, flag, title);
                    let base = if b.kind == BookKind::Real { C_REAL } else { C_BODY };
                    let line = if idx == self.sel {
                        style::reverse(&style::fg(&pad_to(&plain, self.list_w as usize), C_SEL))
                    } else if marked {
                        style::fg(&plain, C_DEL)
                    } else {
                        style::fg(&plain, base)
                    };
                    lines.push_str(&line);
                }
            }
            lines.push('\n');
        }
        self.left.set_text(&lines);
        self.left.full_refresh();
    }

    fn render_right(&mut self) {
        let w = self.right.w as usize;
        let mut out = String::new();
        if let Some(bi) = self.selected_book_idx() {
            let b = &self.cat.books[bi];
            let real = b.kind == BookKind::Real;
            // Kind badge.
            let badge = if real { style::fg("\u{25c6} real book", C_REAL) } else { style::fg("\u{2726} conjured", C_DIM) };
            out.push_str(&badge);
            out.push('\n');
            out.push_str(&style::bold(&style::fg(&wrap(&b.title, w), if real { C_REAL } else { C_BODY })));
            out.push('\n');
            // Only real books get an author line — conjured books never show
            // a name, even if a legacy catalog row still carries one.
            if real && !b.author.is_empty() {
                let by = if !b.year.is_empty() {
                    format!("by {} ({})", b.author, b.year)
                } else {
                    format!("by {}", b.author)
                };
                out.push_str(&style::fg(&by, C_DIM));
                out.push('\n');
            }
            let shelf = if b.subcategory.is_empty() { b.category.clone() } else { format!("{} \u{203a} {}", b.category, b.subcategory) };
            out.push_str(&style::fg(&shelf, C_HEADER));
            out.push_str("\n\n");
            out.push_str(&style::fg(&wrap(&b.hook, w), C_HOOK));
            if !b.tags.is_empty() {
                out.push_str("\n\n");
                out.push_str(&style::fg(&wrap(&format!("#{}", b.tags.join("  #")), w), C_TAG));
            }
            out.push_str("\n\n");
            let status = match (real, b.written) {
                (false, false) => "press \u{21b5} to grab \u{2014} claude writes this book",
                (false, true)  => "\u{2713} written \u{2014} press \u{21b5} to read",
                (true, false)  => "press \u{21b5} to fetch this real book (configured source)",
                (true, true)   => "\u{2713} fetched \u{2014} press \u{21b5} to read",
            };
            out.push_str(&style::fg(status, C_DIM));
        } else if self.cat.books.is_empty() {
            out.push_str(&style::fg("Your shelves are empty.\n\nPress  s  to seed the library from your\ninterests, or  +  to add books on a topic.\nclaude will stock the shelves.", C_DIM));
        }
        self.right.set_text(&out);
        self.right.ix = 0;
        self.right.full_refresh();
    }

    fn render_foot(&mut self, msg: &str) {
        let (left, color) = if msg.is_empty() {
            (" d mark \u{00b7} < purge \u{00b7} * star \u{00b7} + more \u{00b7} s seed \u{00b7} i interests \u{00b7} w/W width \u{00b7} r reload".to_string(), C_DIM)
        } else {
            (msg.to_string(), C_HEADER)
        };
        let ver = format!("library v{} ", env!("CARGO_PKG_VERSION"));
        let pad = (self.cols as usize)
            .saturating_sub(crust::display_width(&left) + crust::display_width(&ver));
        self.foot.say(&format!("{}{}{}",
            style::fg(&left, color),
            " ".repeat(pad),
            style::fg(&ver, C_DIM)));
    }

    fn toggle_star(&mut self) {
        if let Some(i) = self.selected_book_idx() {
            self.cat.books[i].starred = !self.cat.books[i].starred;
            let _ = self.cat.save();
            self.render_left();
            self.render_right();
        }
    }

    fn toggle_delete(&mut self) {
        if let Some(id) = self.selected_book_id() {
            if !self.delete_marked.remove(&id) { self.delete_marked.insert(id); }
            self.render_top();
            self.render_left();
            // Convenience: stepping down after a mark mirrors d-d-d flow.
            self.move_sel(true);
        }
    }

    fn purge_marked(&mut self) {
        if self.delete_marked.is_empty() {
            self.render_foot(" Nothing marked. Press d to mark a book first.");
            return;
        }
        let n = self.delete_marked.len();
        self.cat.books.retain(|b| !self.delete_marked.contains(&b.id));
        self.delete_marked.clear();
        let _ = self.cat.save();
        self.rebuild(None);
        self.render_all();
        self.render_foot(&format!(" Discarded {} book(s).", n));
    }

    /// `+` / `s`: ask for a topic/interests and kick off a background
    /// generation. `seed` true → broaden the whole library (generate_catalog
    /// against the interests); false → focus on the given topic.
    fn request_more(&mut self, seed: bool) {
        let prompt = if seed {
            "Add interests (blank = more of the same): "
        } else {
            "More books on: "
        };
        let input = self.foot.ask(prompt, "");
        if self.foot.last_escaped { self.render_foot(""); return; } // ESC cancels
        let input = input.trim().to_string();

        // Resolve the interests to generate from + an optional topic focus.
        let (interests, topic): (String, Option<String>) = if seed {
            if !input.is_empty() {
                // Append to the saved interests so later batches share voice.
                if self.cat.interests.trim().is_empty() {
                    self.cat.interests = input.clone();
                } else if !self.cat.interests.contains(&input) {
                    self.cat.interests = format!("{}\n{}", self.cat.interests, input);
                }
                let _ = self.cat.save();
                (self.cat.interests.clone(), None)
            } else if !self.cat.interests.trim().is_empty() {
                // Blank seed with interests already set = "more like these".
                (self.cat.interests.clone(), None)
            } else {
                self.render_foot(" No interests yet \u{2014} type some, or press i to edit.");
                return;
            }
        } else {
            if input.is_empty() { self.render_foot(""); return; }
            let base = if self.cat.interests.trim().is_empty() { input.clone() } else { self.cat.interests.clone() };
            (base, Some(input))
        };

        let existing: Vec<String> = self.cat.books.iter().map(|b| b.title.clone()).collect();
        let tx = self.gen_tx.clone();
        std::thread::spawn(move || {
            let res = match topic {
                Some(t) => claude::more_like(&t, &interests, &existing, GEN_N),
                None => claude::generate_catalog(&interests, &existing, GEN_N),
            };
            let _ = tx.send(res);
        });
        self.gen_in_flight += 1;
        self.render_top();
        self.render_foot(" Brewing a batch in the background \u{2014} keep browsing.");
    }

    /// Collect any finished generation batches (non-blocking) and merge
    /// their books. Runs every loop iteration, so results land while the
    /// user keeps browsing/reading. Returns true if anything arrived.
    fn drain_generations(&mut self) -> bool {
        let mut landed = false;
        while let Ok(res) = self.gen_rx.try_recv() {
            self.gen_in_flight = self.gen_in_flight.saturating_sub(1);
            landed = true;
            match res {
                Ok(books) => {
                    let got = books.len();
                    let added = self.cat.add(books);
                    let _ = self.cat.save();
                    self.rebuild(None);
                    self.render_all();
                    self.render_foot(&format!(" Added {} new book(s) ({} duplicates skipped).", added, got - added));
                }
                Err(e) => self.render_foot(&format!(" Generation failed: {}", e)),
            }
        }
        if landed { self.render_top(); }
        landed
    }

    fn tick_spinner(&mut self) {
        self.spin = (self.spin + 1) % SPINNER.len();
        self.render_top();
    }

    fn grab(&mut self) {
        if self.selected_book_idx().is_some() {
            self.render_foot(" Reading (grab \u{2192} claude writes the book) lands in the next build.");
        }
    }

    fn run(&mut self) {
        loop {
            // Block forever when idle (zero idle wakeups); while batches are
            // brewing, wake every second to tick the spinner and collect
            // finished work — so the UI stays fully interactive throughout.
            let timeout = if self.gen_in_flight > 0 { Some(1) } else { None };
            let key = Input::getchr(timeout);
            self.drain_generations();
            let Some(key) = key else {
                if self.gen_in_flight > 0 { self.tick_spinner(); }
                continue;
            };
            match key.as_str() {
                "q" | "ESC" => break,
                "j" | "DOWN" => self.move_sel(true),
                "k" | "UP" => self.move_sel(false),
                "g" | "HOME" => self.go_edge(false),
                "G" | "END" => self.go_edge(true),
                "*" => self.toggle_star(),
                "d" => self.toggle_delete(),
                "<" => self.purge_marked(),
                "+" => self.request_more(false),
                "s" => self.request_more(true),
                "i" => self.edit_interests(),
                "w" => self.cycle_width(true),
                "W" => self.cycle_width(false),
                "ENTER" => self.grab(),
                "r" => {
                    self.cat = Catalog::load();
                    self.rebuild(None);
                    self.render_all();
                    self.render_foot(" Reloaded.");
                }
                _ => {}
            }
        }
    }
}

// ── small text helpers ────────────────────────────────────────────────

fn trunc(s: &str, max: usize) -> String {
    if crust::display_width(s) <= max { return s.to_string(); }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = crust::display_width(&c.to_string());
        if w + cw + 1 > max { out.push('\u{2026}'); break; }
        out.push(c);
        w += cw;
    }
    out
}

fn pad_to(s: &str, width: usize) -> String {
    let w = crust::display_width(s);
    if w >= width { s.to_string() } else { format!("{}{}", s, " ".repeat(width - w)) }
}

/// Greedy word-wrap to `width` columns.
fn wrap(text: &str, width: usize) -> String {
    if width == 0 { return text.to_string(); }
    let mut out = String::new();
    let mut line_w = 0;
    for word in text.split_whitespace() {
        let ww = crust::display_width(word);
        if line_w == 0 {
            out.push_str(word);
            line_w = ww;
        } else if line_w + 1 + ww <= width {
            out.push(' ');
            out.push_str(word);
            line_w += 1 + ww;
        } else {
            out.push('\n');
            out.push_str(word);
            line_w = ww;
        }
    }
    out
}
