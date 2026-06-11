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
use crate::store::{self, Book, BookKind, Catalog};

const C_HEADER: u8 = 73;   // category heading (teal)
const C_BODY:   u8 = 252;  // book title
const C_DIM:    u8 = 245;  // author / hints
const C_SEL:    u8 = 81;   // selection highlight
const C_DEL:    u8 = 88;   // marked-for-deletion (dark red)
const C_REAL:   u8 = 222;  // real (existing) books — warm gold
const C_BODY_BRIGHT: u8 = 255;  // written conjured book — bright + bold
const C_REAL_BRIGHT: u8 = 229;  // written real book — light gold + bold
const C_HOOK:   u8 = 250;  // hook body
const C_TAG:    u8 = 109;  // tags
const C_BORDER: u8 = 238;  // pane borders

const LIST_W: u16 = 46;
const GEN_N: usize = 10;   // books per `+`/`s` batch

const SPINNER: [&str; 10] = ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"];

enum Entry { Header(String), Book(usize) }

/// A finished generation batch (or its error) handed back from a worker
/// thread over the channel. Generation is fully async: the UI stays
/// interactive while batches brew, and results merge in when they land.
type GenResult = Result<Vec<Book>, String>;

/// (book id, write result) returned when a grabbed book finishes writing.
type WriteResult = (String, Result<(), String>);

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
    border: u8,
    search: String,
    write_tx: Sender<WriteResult>,
    write_rx: Receiver<WriteResult>,
    writing: HashSet<String>,
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
        // Body spans rows 3..rows-2. Row 2 + row rows-1 are gaps reserved for
        // the top/bottom borders; col 1 and col `cols` are the side-border
        // gaps. Content sits in the same place whether borders are on or off.
        let body_h = rows.saturating_sub(4);
        let list_w = if cat.list_w >= 24 && cat.list_w + 28 < cols { cat.list_w } else { LIST_W.min(cols.saturating_sub(28)) };
        let border = cat.border.min(3);
        let mut top = Pane::new(1, 1, cols, 1, C_SEL as u16, 236);
        top.scroll = false; top.wrap = false;
        let mut left = Pane::new(2, 3, list_w, body_h, C_BODY as u16, 0);
        left.scroll = false; left.wrap = false;
        let mut right = Pane::new(list_w + 4, 3, cols.saturating_sub(list_w + 4), body_h, C_BODY as u16, 0);
        right.scroll = false; right.wrap = false;
        let mut foot = Pane::new(1, rows, cols, 1, C_DIM as u16, 236);
        foot.scroll = false; foot.wrap = false;
        let (gen_tx, gen_rx) = mpsc::channel();
        let (write_tx, write_rx) = mpsc::channel();
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
            border,
            search: String::new(),
            write_tx, write_rx,
            writing: HashSet::new(),
        };
        app.apply_border_state();
        app.rebuild(None);
        app
    }

    /// Map the 0-3 border mode onto the two panes (0 none, 1 right, 2 both,
    /// 3 left — same as pointer/kastrup).
    fn apply_border_state(&mut self) {
        self.left.border = matches!(self.border, 2 | 3);
        self.left.border_fg = Some(C_BORDER as u16);
        self.right.border = matches!(self.border, 1 | 2);
        self.right.border_fg = Some(C_BORDER as u16);
    }

    fn refresh_borders(&mut self) {
        if self.left.border { self.left.border_refresh(); }
        if self.right.border { self.right.border_refresh(); }
    }

    /// `Ctrl-B` — cycle border mode (none → right → both → left), persisted.
    fn cycle_border(&mut self) {
        self.border = (self.border + 1) % 4;
        self.cat.border = self.border;
        let _ = self.cat.save();
        self.apply_border_state();
        self.relayout();
        let label = ["none", "right", "both", "left"][self.border as usize];
        self.render_foot(&format!(" Border: {}", label));
    }

    /// Recompute pane geometry after a width change and repaint clean.
    fn relayout(&mut self) {
        self.left.w = self.list_w;
        self.right.x = self.list_w + 4;
        self.right.w = self.cols.saturating_sub(self.list_w + 4);
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
        let max = self.cols.saturating_sub(28);
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
        let q = self.search.to_lowercase();
        let mut entries = Vec::new();
        for category in self.cat.categories() {
            // Books in this category that match the active search (if any).
            let hits: Vec<usize> = self.cat.books.iter().enumerate()
                .filter(|(_, b)| b.category == category && book_matches(b, &q))
                .map(|(i, _)| i)
                .collect();
            if hits.is_empty() { continue; } // skip empty categories under a search
            entries.push(Entry::Header(category.clone()));
            for i in hits { entries.push(Entry::Book(i)); }
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
        self.refresh_borders();
    }

    fn render_top(&mut self) {
        let n = self.cat.books.len();
        let written = self.cat.books.iter().filter(|b| b.written).count();
        let marked = self.delete_marked.len();
        let mark_s = if marked > 0 { format!("  \u{00b7}  {} marked", marked) } else { String::new() };
        let search_s = if self.search.is_empty() { String::new() } else { format!("  \u{00b7}  /{}", self.search) };
        let title = format!(" library   {} books \u{00b7} {} written{}{}", n, written, mark_s, search_s);
        // Right side: a live "brewing" indicator while batches generate in
        // the background. Keys live in the footer only (no duplicate map).
        let right = if self.gen_in_flight > 0 && !self.writing.is_empty() {
            format!("{} {} brewing \u{00b7} {} writing\u{2026} ", SPINNER[self.spin], self.gen_in_flight, self.writing.len())
        } else if self.gen_in_flight > 0 {
            format!("{} {} brewing\u{2026} ", SPINNER[self.spin], self.gen_in_flight)
        } else if !self.writing.is_empty() {
            format!("{} {} writing\u{2026} ", SPINNER[self.spin], self.writing.len())
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
                    let selected = idx == self.sel;
                    let star = if b.starred { '\u{2605}' } else { ' ' };
                    let flag = if marked { 'D' } else { ' ' };
                    let title = trunc(&b.title, (self.list_w as usize).saturating_sub(6));
                    let row = format!("{}{} {}", star, flag, title);
                    // Colour: marked = dark red; real = gold, conjured = grey;
                    // written books are brighter + bold (you can see what you own).
                    let color = if marked { C_DEL }
                        else if b.kind == BookKind::Real { if b.written { C_REAL_BRIGHT } else { C_REAL } }
                        else if b.written { C_BODY_BRIGHT } else { C_BODY };
                    let mut styled = style::fg(&row, color);
                    if b.written { styled = style::bold(&styled); }
                    // Pointer-style selector: a cyan arrow + underline, no reverse.
                    if selected { styled = style::underline(&styled); }
                    let arrow = if selected { style::fg("\u{2192}", C_SEL) } else { " ".to_string() };
                    lines.push_str(&format!("{} {}", arrow, styled));
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
            (" / find \u{00b7} d mark \u{00b7} < purge \u{00b7} * star \u{00b7} + more \u{00b7} s seed \u{00b7} i edit \u{00b7} w/W width \u{00b7} ^B border".to_string(), C_DIM)
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

    /// `/` — filter the shelf across title / hook / author / category / tags.
    /// Empty query clears the filter; ESC keeps the current one.
    fn do_search(&mut self) {
        let q = self.foot.ask("/", &self.search);
        if self.foot.last_escaped { self.render_foot(""); return; }
        self.search = q.trim().to_string();
        self.rebuild(None);
        self.render_all();
        if self.search.is_empty() {
            self.render_foot(" Search cleared.");
        } else {
            let n = self.entries.iter().filter(|e| matches!(e, Entry::Book(_))).count();
            self.render_foot(&format!(" {} match(es) for \u{201c}{}\u{201d} \u{00b7} / refine \u{00b7} /\u{21b5} (empty) clears", n, self.search));
        }
    }

    /// `Enter` — grab the selected book. Written → open the reader. Not yet
    /// written → kick off an async write (conjured: Claude writes it) and
    /// keep the UI live; the book opens when you press Enter again once ready.
    fn grab(&mut self) {
        let Some(bi) = self.selected_book_idx() else { return; };
        let id = self.cat.books[bi].id.clone();
        if self.cat.books[bi].written && store::book_md(&id).exists() {
            self.read_book(&id);
            return;
        }
        if self.writing.contains(&id) {
            self.render_foot(" Still writing this one\u{2026} keep browsing; \u{21b5} again when it's ready.");
            return;
        }
        if self.cat.books[bi].kind == BookKind::Real {
            self.render_foot(" Real-book fetching lands next \u{2014} grab a conjured book for now.");
            return;
        }
        let ans = self.foot.ask("Depth \u{2014} [q]uick read / [d]eep dive: ", "q");
        if self.foot.last_escaped { self.render_foot(""); return; }
        let deep = ans.trim().to_lowercase().starts_with('d');
        let b = &self.cat.books[bi];
        let (title, hook, category) = (b.title.clone(), b.hook.clone(), b.category.clone());
        let tx = self.write_tx.clone();
        let wid = id.clone();
        self.writing.insert(id);
        std::thread::spawn(move || {
            let res = claude::write_book(&title, &hook, &category, deep).and_then(|md| {
                std::fs::create_dir_all(store::book_dir(&wid)).map_err(|e| e.to_string())?;
                std::fs::write(store::book_md(&wid), md).map_err(|e| e.to_string())
            });
            let _ = tx.send((wid, res));
        });
        self.render_top();
        self.render_foot(" Writing your book in the background \u{2014} keep browsing; \u{21b5} when it's ready.");
    }

    /// Collect finished book writes (non-blocking): mark written + toast.
    fn drain_writes(&mut self) {
        while let Ok((id, res)) = self.write_rx.try_recv() {
            self.writing.remove(&id);
            match res {
                Ok(()) => {
                    let mut title = String::new();
                    if let Some(b) = self.cat.books.iter_mut().find(|b| b.id == id) {
                        b.written = true;
                        title = b.title.clone();
                    }
                    let _ = self.cat.save();
                    self.rebuild(None);
                    self.render_all();
                    self.render_foot(&format!(" \u{201c}{}\u{201d} is ready \u{2014} press \u{21b5} to read.", trunc(&title, 48)));
                }
                Err(e) => self.render_foot(&format!(" Book writing failed: {}", e)),
            }
        }
    }

    /// Full-screen reader: top bar = title + progress, body = the book,
    /// status bar = keys. Paginated scroll over the rendered Markdown.
    fn read_book(&mut self, id: &str) {
        let md = std::fs::read_to_string(store::book_md(id)).unwrap_or_default();
        if md.trim().is_empty() { self.render_foot(" (book is empty)"); return; }
        let title = self.cat.books.iter().find(|b| b.id == id)
            .map(|b| b.title.clone()).unwrap_or_default();

        let cols = self.cols as usize;
        let h = (self.rows as usize).saturating_sub(2); // content rows 2..rows-1
        let max_w = cols.saturating_sub(4).max(20);
        // Reading column width — w/W widen/narrow it (persisted). Default to a
        // comfortable ~86 cols even on a very wide screen.
        let mut wrap_w = if self.cat.read_w >= 40 { (self.cat.read_w as usize).min(max_w) } else { max_w.min(86) };
        let mut lines = render_markdown(&md, wrap_w);
        let mut top = 0usize;

        let mut body = Pane::new(2, 2, self.cols.saturating_sub(2), self.rows.saturating_sub(2), C_BODY as u16, 0);
        body.scroll = false; body.wrap = false;
        Crust::clear_screen();

        loop {
            let total = lines.len();
            let max_top = total.saturating_sub(h);
            if top > max_top { top = max_top; }
            let pct = if max_top == 0 { 100 } else { top * 100 / max_top };
            let tline = format!(" \u{1f4d6} {}", trunc(&title, cols.saturating_sub(18)));
            let prog = format!("{}% ", pct);
            let pad = cols.saturating_sub(crust::display_width(&tline) + crust::display_width(&prog));
            self.top.say(&format!("{}{}{}",
                style::bold(&style::fg(&tline, C_SEL)), " ".repeat(pad), style::fg(&prog, C_DIM)));

            let window = lines[top..(top + h).min(total)].join("\n");
            body.set_text(&window);
            body.full_refresh();

            self.foot.say(&style::fg(" j/k scroll \u{00b7} SPACE/b page \u{00b7} g/G start/end \u{00b7} w/W text width \u{00b7} q back", C_DIM));

            let Some(key) = Input::getchr(None) else { continue };
            match key.as_str() {
                "q" | "ESC" | "h" | "LEFT" => break,
                "j" | "DOWN" => if top < max_top { top += 1; },
                "k" | "UP" => top = top.saturating_sub(1),
                " " | "PgDOWN" | "f" => top = (top + h.saturating_sub(1)).min(max_top),
                "b" | "PgUP" => top = top.saturating_sub(h.saturating_sub(1)),
                "g" | "HOME" => top = 0,
                "G" | "END" => top = max_top,
                "w" | "W" => {
                    wrap_w = if key == "w" { (wrap_w + 6).min(max_w) } else { wrap_w.saturating_sub(6).max(40) };
                    self.cat.read_w = wrap_w as u16;
                    let _ = self.cat.save();
                    lines = render_markdown(&md, wrap_w);
                    Crust::clear_screen();
                }
                _ => {}
            }
        }

        Crust::clear_screen();
        self.top.invalidate();
        self.foot.invalidate();
        self.render_all();
    }

    fn run(&mut self) {
        loop {
            // Block forever when idle (zero idle wakeups); while batches are
            // brewing, wake every second to tick the spinner and collect
            // finished work — so the UI stays fully interactive throughout.
            let busy = self.gen_in_flight > 0 || !self.writing.is_empty();
            let timeout = if busy { Some(1) } else { None };
            let key = Input::getchr(timeout);
            self.drain_generations();
            self.drain_writes();
            let Some(key) = key else {
                if self.gen_in_flight > 0 || !self.writing.is_empty() { self.tick_spinner(); }
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
                "C-B" => self.cycle_border(),
                "/" => self.do_search(),
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

/// Render a book's Markdown into styled, wrapped display lines for the
/// reader. Headings are coloured/bold; `>` lines are dimmed pull-quotes;
/// body paragraphs wrap to `width` with inline **bold** / *italic*.
fn render_markdown(md: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in md.lines() {
        let line = raw.trim_end();
        if let Some(t) = line.strip_prefix("# ") {
            out.push(String::new());
            out.push(style::bold(&style::fg(t.trim(), C_SEL)));
            out.push(String::new());
        } else if let Some(t) = line.strip_prefix("## ") {
            out.push(String::new());
            out.push(style::bold(&style::fg(t.trim(), C_HEADER)));
            out.push(String::new());
        } else if let Some(t) = line.strip_prefix("### ") {
            out.push(style::bold(&style::fg(t.trim(), C_TAG)));
        } else if let Some(t) = line.strip_prefix("> ") {
            for wl in wrap(t.trim(), width.saturating_sub(2)).lines() {
                out.push(format!("  {}", style::italic(&style::fg(wl, C_DIM))));
            }
        } else if line.trim().is_empty() {
            out.push(String::new());
        } else {
            for wl in wrap(line, width).lines() {
                out.push(style::fg(&style_inline(wl), C_HOOK));
            }
        }
    }
    out
}

/// Apply inline **bold** then *italic*. Crust's bold/italic reset only
/// their own attribute, so this nests cleanly inside an outer fg().
fn style_inline(s: &str) -> String {
    let bolded = toggle_wrap(s, "**", style::bold);
    toggle_wrap(&bolded, "*", style::italic)
}

fn toggle_wrap(s: &str, marker: &str, f: fn(&str) -> String) -> String {
    if !s.contains(marker) { return s.to_string(); }
    let mut out = String::new();
    for (i, part) in s.split(marker).enumerate() {
        if i % 2 == 1 { out.push_str(&f(part)); } else { out.push_str(part); }
    }
    out
}

/// True if `b` matches the lowercased search `q` (empty = matches all).
fn book_matches(b: &Book, q: &str) -> bool {
    if q.is_empty() { return true; }
    let hay = format!("{} {} {} {} {}",
        b.title, b.hook, b.author, b.category, b.tags.join(" ")).to_lowercase();
    hay.contains(q)
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
