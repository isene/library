//! The browse / curate TUI. Two panes like gazette: a shelf list on the
//! left (books grouped under category headings) and the selected book's
//! detail on the right. Curate with `*` (star), `d`/`<` (mark/purge), and
//! `+`/`s` (ask `claude -p` for more, generated on a background thread
//! with a spinner so the UI stays live). Grabbing a book to read is the
//! next build step.

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{OnceLock, RwLock};

use crust::{Crust, Input, Pane, style};

use crate::bookmark;
use crate::claude;
use crate::import;
use crate::store::{self, Book, BookKind, Catalog, Colors};

// The palette is a process-global, seeded from the catalog at startup and
// updated live by the `P` config popup. `col()` returns a cheap Copy snapshot.
static COLORS: OnceLock<RwLock<Colors>> = OnceLock::new();
fn col() -> Colors { *COLORS.get_or_init(|| RwLock::new(Colors::default())).read().unwrap() }
fn set_colors(v: Colors) { *COLORS.get_or_init(|| RwLock::new(Colors::default())).write().unwrap() = v; }

/// Editable colour slots for the `P` popup: label + get/set accessors.
type ColorSlot = (&'static str, fn(&Colors) -> u8, fn(&mut Colors, u8));
const COLOR_SLOTS: &[ColorSlot] = &[
    ("Shelf: heading",     |c: &Colors| c.header,       |c: &mut Colors, v| c.header = v),
    ("Shelf: title",       |c: &Colors| c.body,         |c: &mut Colors, v| c.body = v),
    ("Shelf: dim / hint",  |c: &Colors| c.dim,          |c: &mut Colors, v| c.dim = v),
    ("Shelf: selection",   |c: &Colors| c.sel,          |c: &mut Colors, v| c.sel = v),
    ("Shelf: real book",   |c: &Colors| c.real,         |c: &mut Colors, v| c.real = v),
    ("Shelf: real (read)", |c: &Colors| c.real_bright,  |c: &mut Colors, v| c.real_bright = v),
    ("Shelf: written",     |c: &Colors| c.body_bright,  |c: &mut Colors, v| c.body_bright = v),
    ("Shelf: marked",      |c: &Colors| c.del,          |c: &mut Colors, v| c.del = v),
    ("Shelf: hook",        |c: &Colors| c.hook,         |c: &mut Colors, v| c.hook = v),
    ("Shelf: tags",        |c: &Colors| c.tag,          |c: &mut Colors, v| c.tag = v),
    ("Pane border",        |c: &Colors| c.border,       |c: &mut Colors, v| c.border = v),
    ("Bar background",     |c: &Colors| c.bar_bg,       |c: &mut Colors, v| c.bar_bg = v),
    ("Reader: body",       |c: &Colors| c.reader_fg,    |c: &mut Colors, v| c.reader_fg = v),
    ("Reader: title",      |c: &Colors| c.reader_h1,    |c: &mut Colors, v| c.reader_h1 = v),
    ("Reader: chapter",    |c: &Colors| c.reader_h2,    |c: &mut Colors, v| c.reader_h2 = v),
    ("Reader: subhead",    |c: &Colors| c.reader_h3,    |c: &mut Colors, v| c.reader_h3 = v),
    ("Reader: quote",      |c: &Colors| c.reader_quote, |c: &mut Colors, v| c.reader_quote = v),
];

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

/// (reserved id, title, built book) returned when a background PDF import
/// finishes. The book is added to the catalog on the main thread.
type ImportResult = (String, String, Result<Book, String>);

/// Shelf filter, cycled with `f`. Orthogonal to the `/` text search.
#[derive(Clone, Copy, PartialEq)]
enum Filter { All, Rendered, Starred, Unread }

impl Filter {
    fn accepts(&self, b: &Book) -> bool {
        match self {
            Filter::All => true,
            Filter::Rendered => b.written,
            Filter::Starred => b.starred,
            Filter::Unread => !b.read,
        }
    }
    fn next(self) -> Self {
        match self {
            Filter::All => Filter::Rendered,
            Filter::Rendered => Filter::Starred,
            Filter::Starred => Filter::Unread,
            Filter::Unread => Filter::All,
        }
    }
    fn label(&self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Rendered => "rendered",
            Filter::Starred => "starred",
            Filter::Unread => "unread",
        }
    }
}

/// Key help shown by `?` in the shelf view.
const SHELF_HELP: &[(&str, &str)] = &[
    ("j / k  \u{2191}\u{2193}", "move (books and shelf headers)"),
    ("PgDn / PgUp", "page through the shelf"),
    ("g / G", "first / last"),
    ("\u{21b5} / \u{2192}", "open book (on a header: enter shelf)"),
    ("/", "search"),
    ("f", "filter: all / rendered / starred / unread"),
    ("* / R", "star / mark read"),
    ("Ctrl+\u{2191}/\u{2193}", "reorder book (on a header: move shelf)"),
    ("M", "move book to a shelf (new name = new shelf)"),
    ("a / y", "add a PDF/EPUB / copy book path"),
    ("d / <", "mark / purge deletion"),
    ("+ / s", "generate more / seed books"),
    ("i", "edit interests"),
    ("w / W", "shelf-pane width"),
    ("Ctrl+B / P", "border / colours"),
    ("r / ?", "reload / this help"),
    ("q", "quit"),
];

/// Key help shown by `?` in the reader.
const READER_HELP: &[(&str, &str)] = &[
    ("j / k", "scroll a line"),
    ("Space / b", "page down / up"),
    ("g / G", "top / bottom"),
    ("w / W", "reading width"),
    ("m", "set bookmark"),
    ("e", "export PDF"),
    ("c", "discuss with Claude"),
    ("d", "define highlighted term"),
    ("+", "deepen (longer rewrite)"),
    ("P", "colours"),
    ("? / q", "this help / back to shelf"),
];

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
    import_tx: Sender<ImportResult>,
    import_rx: Receiver<ImportResult>,
    /// In-flight PDF imports: (reserved id, title, inbox pdf to delete on
    /// success). Drives the spinner and reserves ids so concurrent imports
    /// don't collide. Inbox cleanup happens on the main thread after the
    /// catalog entry is added, so a quit mid-import never loses the queue.
    importing: Vec<(String, String, Option<std::path::PathBuf>)>,
    /// Active shelf filter (cycled with `f`).
    filter: Filter,
}

pub fn run() {
    Crust::init();
    Crust::set_app_identity("Library");
    let mut app = App::new();
    app.render_all();
    app.start_inbox_imports();
    app.run();
    let _ = app.cat.save();
    Crust::cleanup();
    Crust::clear_screen();
}

impl App {
    fn new() -> Self {
        let (cols, rows) = Crust::terminal_size();
        let cat = Catalog::load();
        set_colors(cat.colors); // seed the palette before any pane is built
        // Body spans rows 3..rows-2. Row 2 + row rows-1 are gaps reserved for
        // the top/bottom borders; col 1 and col `cols` are the side-border
        // gaps. Content sits in the same place whether borders are on or off.
        let body_h = rows.saturating_sub(4);
        let list_w = if cat.list_w >= 24 && cat.list_w + 28 < cols { cat.list_w } else { LIST_W.min(cols.saturating_sub(28)) };
        let border = cat.border.min(3);
        let mut top = Pane::new(1, 1, cols, 1, col().sel as u16, col().bar_bg as u16);
        top.scroll = false; top.wrap = false;
        let mut left = Pane::new(2, 3, list_w, body_h, col().body as u16, 0);
        left.scroll = false; left.wrap = false;
        let mut right = Pane::new(list_w + 4, 3, cols.saturating_sub(list_w + 4), body_h, col().body as u16, 0);
        right.scroll = false; right.wrap = false;
        let mut foot = Pane::new(1, rows, cols, 1, col().dim as u16, col().bar_bg as u16);
        foot.scroll = false; foot.wrap = false;
        let (gen_tx, gen_rx) = mpsc::channel();
        let (write_tx, write_rx) = mpsc::channel();
        let (import_tx, import_rx) = mpsc::channel();
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
            import_tx, import_rx,
            importing: Vec::new(),
            filter: Filter::All,
        };
        app.apply_border_state();
        app.rebuild(None);
        app
    }

    /// Map the 0-3 border mode onto the two panes (0 none, 1 right, 2 both,
    /// 3 left — same as pointer/kastrup).
    fn apply_border_state(&mut self) {
        self.left.border = matches!(self.border, 2 | 3);
        self.left.border_fg = Some(col().border as u16);
        self.right.border = matches!(self.border, 1 | 2);
        self.right.border_fg = Some(col().border as u16);
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

    /// `a` — import a PDF from the laptop. Prompts for the file, a title,
    /// a subject (shelf), and an optional author, then runs the import on a
    /// background thread (pdftotext → Claude-structure → figure pages →
    /// live book). The UI stays interactive; the book lands via
    /// `drain_imports` when ready, exactly like a grabbed conjured book.
    fn import_book(&mut self) {
        let raw_path = self.foot.ask("Import PDF/EPUB \u{2014} path: ", "");
        if self.foot.last_escaped || raw_path.trim().is_empty() { self.render_foot(""); return; }
        let pdf = import::expand_tilde(raw_path.trim());
        if !pdf.exists() {
            self.render_foot(&format!(" No such file: {}", pdf.display()));
            return;
        }
        let default_title = pdf.file_stem().and_then(|s| s.to_str())
            .unwrap_or("").replace(['_', '-'], " ");
        let title = self.foot.ask("Title: ", default_title.trim());
        if self.foot.last_escaped { self.render_foot(""); return; }
        // Offer the existing shelves as the subject default so imports land
        // alongside related books rather than scattering new headings.
        let cats = self.cat.categories();
        let default_subject = cats.first().map(|s| s.as_str()).unwrap_or("Imported").to_string();
        if !cats.is_empty() {
            self.render_foot(&format!(" Shelves: {}", cats.join(" \u{00b7} ")));
        }
        let subject = self.foot.ask("Subject: ", &default_subject);
        if self.foot.last_escaped { self.render_foot(""); return; }
        let author = self.foot.ask("Author (optional): ", "");
        if self.foot.last_escaped { self.render_foot(""); return; }

        let disp = if title.trim().is_empty() { default_title } else { title.trim().to_string() };
        self.start_import(pdf, title, subject, author, false);
        self.render_top();
        self.render_foot(&format!(
            " Importing \u{201c}{}\u{201d} in the background \u{2014} keep browsing; it'll appear when ready.",
            trunc(&disp, 36)));
    }

    /// On launch, kick off background imports for any PDFs the phone queued
    /// in `~/.library/inbox/`. Stat-gated: zero work when the inbox is empty
    /// (the normal case). Non-blocking — the shelf stays usable while big
    /// books structure in the background.
    fn start_inbox_imports(&mut self) {
        let pdfs = import::inbox_pdfs();
        if pdfs.is_empty() { return; }
        let n = pdfs.len();
        for pdf in pdfs {
            let side = pdf.with_extension("json");
            let (title, subject, author) = if side.exists() {
                import::read_sidecar(&side)
            } else {
                (String::new(), String::new(), String::new())
            };
            self.start_import(pdf, title, subject, author, true);
        }
        self.render_top();
        self.render_foot(&format!(
            " Importing {} PDF(s) added on your phone \u{2014} each appears as it finishes.", n));
    }

    /// Spawn one background import. `cleanup_inbox` marks an inbox-sourced
    /// PDF whose source + sidecar are removed (on the main thread) once the
    /// catalog entry is added — so a quit mid-import never drops the queue.
    fn start_import(&mut self, pdf: std::path::PathBuf, title: String, subject: String,
                    author: String, cleanup_inbox: bool) {
        let title = {
            let t = title.trim();
            if t.is_empty() { import::title_from_path(&pdf) } else { t.to_string() }
        };
        if self.cat.has_title(&title) {
            self.render_foot(&format!(" \u{201c}{}\u{201d} is already on the shelf.", trunc(&title, 40)));
            return;
        }
        let id = self.unique_import_id(&title);
        let cleanup = if cleanup_inbox { Some(pdf.clone()) } else { None };
        self.importing.push((id.clone(), title.clone(), cleanup));
        let tx = self.import_tx.clone();
        std::thread::spawn(move || {
            let res = import::build_book(&pdf, &id, &title, &subject, &author);
            let _ = tx.send((id, title, res));
        });
    }

    /// Collect finished background imports (non-blocking): add the built
    /// book to the catalog, save, clean up any inbox source, and toast.
    fn drain_imports(&mut self) {
        while let Ok((id, title, res)) = self.import_rx.try_recv() {
            let cleanup = self.importing.iter()
                .find(|(i, _, _)| i == &id)
                .and_then(|(_, _, c)| c.clone());
            self.importing.retain(|(i, _, _)| i != &id);
            match res {
                Ok(book) => {
                    self.cat.books.push(book);
                    let _ = self.cat.save();
                    if let Some(pdf) = cleanup {
                        let _ = std::fs::remove_file(&pdf);
                        let _ = std::fs::remove_file(pdf.with_extension("json"));
                    }
                    self.rebuild(Some(id));
                    self.render_all();
                    self.render_foot(&format!(
                        " \u{201c}{}\u{201d} imported \u{2014} press \u{21b5} to read.", trunc(&title, 44)));
                }
                Err(e) => {
                    // Leave any inbox source in place so the next launch retries.
                    self.render_all();
                    self.render_foot(&format!(" Import failed ({}): {}", trunc(&title, 28), e));
                }
            }
        }
    }

    /// `y` — copy the selected book's `book.md` path to the clipboard
    /// (OSC 52 + xclip fallback, via crust). Lets you open it elsewhere.
    fn copy_book_path(&mut self) {
        let Some(id) = self.selected_book_id() else {
            self.render_foot(" No book selected.");
            return;
        };
        let path = store::book_md(&id);
        if !path.exists() {
            self.render_foot(" Not written yet \u{2014} grab it first, then y copies its path.");
            return;
        }
        let p = path.to_string_lossy().to_string();
        crust::clipboard_copy(&p, "clipboard");
        self.render_foot(&format!(" Copied path: {}", p));
    }

    /// A unique book id that avoids both the catalog and in-flight imports.
    fn unique_import_id(&self, title: &str) -> String {
        let base = {
            let s = store::slugify(title);
            if s.is_empty() { "import".to_string() } else { s }
        };
        let taken = |id: &str| self.cat.books.iter().any(|b| b.id == id)
            || self.importing.iter().any(|(i, _, _)| i == id);
        if !taken(&base) { return base; }
        let mut n = 2;
        loop {
            let cand = format!("{}-{}", base, n);
            if !taken(&cand) { return cand; }
            n += 1;
        }
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
                .filter(|(_, b)| b.category == category && book_matches(b, &q) && self.filter.accepts(b))
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

    /// Move the selection one entry — a book OR a shelf header (headers are
    /// landable so Ctrl+Up/Down can move a whole shelf).
    fn move_sel(&mut self, down: bool) {
        if self.entries.is_empty() { return; }
        if down {
            if self.sel + 1 < self.entries.len() { self.sel += 1; }
        } else {
            self.sel = self.sel.saturating_sub(1);
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

    /// PgDn / PgUp — jump one pane-height of rows, snapping to the nearest
    /// Book in the direction of travel (skips category headings).
    fn page_sel(&mut self, down: bool) {
        if self.entries.is_empty() { return; }
        let page = (self.left.h as usize).max(1);
        let n = self.entries.len();
        let target = if down { (self.sel + page).min(n - 1) } else { self.sel.saturating_sub(page) };
        let is_book = |i: &usize| matches!(self.entries[*i], Entry::Book(_));
        let pick = if down {
            (target..n).find(is_book).or_else(|| (0..=target).rev().find(is_book))
        } else {
            (0..=target).rev().find(is_book).or_else(|| (target..n).find(is_book))
        };
        if let Some(i) = pick { self.sel = i; }
        self.render_left();
        self.render_right();
    }

    /// `f` — cycle the shelf filter: all → rendered → starred → unread.
    fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
        self.rebuild(None);
        self.render_all();
        let n = self.entries.iter().filter(|e| matches!(e, Entry::Book(_))).count();
        self.render_foot(&format!(" Filter: {} \u{2014} {} book(s) \u{00b7} f cycles", self.filter.label(), n));
    }

    /// `R` — toggle the "read" mark on the selected book (persisted).
    fn toggle_read(&mut self) {
        if let Some(i) = self.selected_book_idx() {
            self.cat.books[i].read = !self.cat.books[i].read;
            let now_read = self.cat.books[i].read;
            let _ = self.cat.save();
            // Under the Unread filter a freshly-read book leaves the list, so
            // rebuild to drop it; otherwise just repaint the marker.
            if self.filter == Filter::Unread && now_read {
                self.rebuild(None);
                self.render_all();
            } else {
                self.render_left();
                self.render_right();
            }
            self.render_foot(if now_read { " Marked as read." } else { " Marked as unread." });
        }
    }

    /// Ctrl+Up / Ctrl+Down — reorder the selected book within its shelf by
    /// swapping it with the adjacent same-shelf book in the displayed list.
    /// Bounded to the shelf (won't cross into another category). Persisted.
    fn move_book(&mut self, down: bool) {
        let Some(cur) = self.selected_book_idx() else { return; };
        let cat = self.cat.books[cur].category.clone();
        // Nearest Book entry in the travel direction (skip headings).
        let mut i = self.sel;
        let adj = loop {
            if down {
                if i + 1 >= self.entries.len() { break None; }
                i += 1;
            } else {
                if i == 0 { break None; }
                i -= 1;
            }
            if let Entry::Book(bi) = self.entries[i] { break Some(bi); }
        };
        match adj {
            Some(bi) if self.cat.books[bi].category == cat => {
                let id = self.cat.books[cur].id.clone();
                self.cat.books.swap(cur, bi);
                let _ = self.cat.save();
                self.rebuild(Some(id));
                self.render_left();
                self.render_right();
            }
            _ => self.render_foot(if down {
                " Already at the bottom of this shelf."
            } else {
                " Already at the top of this shelf."
            }),
        }
    }

    /// `M` — move the selected book to another shelf (category). The existing
    /// shelves are listed in the right pane while you type the target (a new
    /// name creates a shelf). Persisted.
    fn move_to_shelf(&mut self) {
        let Some(cur) = self.selected_book_idx() else { return; };
        let cur_cat = self.cat.books[cur].category.clone();
        // List the shelves in the right pane so the target is easy to pick.
        let mut panel = style::bold(&style::fg(" Move to which shelf?", col().header));
        panel.push_str("\n\n");
        for c in self.cat.categories() {
            let line = if c == cur_cat { format!(" \u{2192} {}  (current)", c) } else { format!("   {}", c) };
            panel.push_str(&style::fg(&line, if c == cur_cat { col().sel } else { col().body }));
            panel.push('\n');
        }
        panel.push_str(&style::fg("\n Type a name (a new one creates a shelf), or ESC.", col().dim));
        self.right.set_text(&panel);
        self.right.ix = 0;
        self.right.full_refresh();

        let target = self.foot.ask("Move to shelf: ", "");
        let target = target.trim().to_string();
        if self.foot.last_escaped || target.is_empty() || target == cur_cat {
            self.render_right();
            self.render_foot(" Shelf unchanged.");
            return;
        }
        let id = self.cat.books[cur].id.clone();
        self.cat.books[cur].category = target.clone();
        let _ = self.cat.save();
        self.rebuild(Some(id));
        self.render_all();
        self.render_foot(&format!(" Moved to \u{201c}{}\u{201d}.", target));
    }

    /// The category whose header is selected, or None when a book is selected.
    fn selected_category(&self) -> Option<String> {
        match self.entries.get(self.sel) {
            Some(Entry::Header(c)) => Some(c.clone()),
            _ => None,
        }
    }

    /// Ctrl+Up / Ctrl+Down on a shelf header — move that whole shelf up or
    /// down in the shelf order. Regroups the catalog by the new order,
    /// preserving each shelf's internal book order. Persisted.
    fn move_shelf(&mut self, down: bool) {
        let (cat, book_id) = match self.entries.get(self.sel) {
            Some(Entry::Header(c)) => (c.clone(), None),
            Some(Entry::Book(i)) => (self.cat.books[*i].category.clone(), Some(self.cat.books[*i].id.clone())),
            None => return,
        };
        let cats = self.cat.categories();
        let Some(pos) = cats.iter().position(|c| *c == cat) else { return; };
        if down && pos + 1 >= cats.len() { self.render_foot(" Shelf already at the bottom."); return; }
        if !down && pos == 0 { self.render_foot(" Shelf already at the top."); return; }
        let target = if down { pos + 1 } else { pos - 1 };
        let mut order = cats;
        order.swap(pos, target);
        let rank: std::collections::HashMap<String, usize> =
            order.iter().enumerate().map(|(i, c)| (c.clone(), i)).collect();
        // Stable sort by shelf rank keeps each shelf's books in their order.
        let mut books = std::mem::take(&mut self.cat.books);
        books.sort_by_key(|b| *rank.get(&b.category).unwrap_or(&usize::MAX));
        self.cat.books = books;
        let _ = self.cat.save();
        self.rebuild(book_id);
        // If we moved from the header, follow it to its new position.
        if self.selected_category().as_deref() != Some(&cat) {
            if let Some(p) = self.entries.iter().position(|e| matches!(e, Entry::Header(c) if *c == cat)) {
                self.sel = p;
            }
        }
        self.render_all();
        self.render_foot(&format!(" Moved shelf \u{201c}{}\u{201d} {}.",
            cat, if down { "down" } else { "up" }));
    }

    /// Render a centred key-help popup (any key closes it). Shared by the
    /// shelf `?` and the reader `?`.
    fn show_help(&self, title: &str, rows: &[(&str, &str)]) {
        let kw = rows.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(6);
        let cw = rows.iter()
            .map(|(k, d)| kw + 2 + d.chars().count())
            .max().unwrap_or(24)
            .max(title.chars().count());
        let pw = ((cw as u16) + 4).min(self.cols.saturating_sub(2)).max(24);
        let ph = ((rows.len() as u16) + 5).min(self.rows.saturating_sub(2)).max(6);
        let px = self.cols.saturating_sub(pw) / 2 + 1;
        let py = self.rows.saturating_sub(ph) / 2 + 1;
        let mut pane = Pane::new(px, py, pw, ph, col().body as u16, col().bar_bg as u16);
        pane.scroll = false;
        pane.wrap = false;
        pane.border = true;
        pane.border_fg = Some(col().border as u16);
        let mut s = style::bold(&style::fg(&format!(" {}", title), col().sel));
        s.push_str("\n\n");
        for (k, d) in rows {
            s.push_str(&format!(" {}  {}\n",
                style::bold(&style::fg(&format!("{:>kw$}", k, kw = kw), col().header)),
                style::fg(d, col().body)));
        }
        s.push_str(&style::fg("\n Press any key to close.", col().dim));
        pane.say(&s);
        pane.border_refresh();
        let _ = Input::getchr(None);
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
        let filter_s = if self.filter == Filter::All { String::new() } else { format!("  \u{00b7}  [{}]", self.filter.label()) };
        let title = format!(" library   {} books \u{00b7} {} written{}{}{}", n, written, mark_s, filter_s, search_s);
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
            style::bold(&style::fg(&title, col().sel)),
            " ".repeat(pad),
            style::fg(&right, col().real)));
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
                    let selected = idx == self.sel;
                    let mut name = style::bold(&style::fg(c, col().header));
                    if selected { name = style::underline(&name); }
                    // Shared arrow column with the book rows so selection lines up.
                    let arrow = if selected { style::fg("\u{2192} ", col().sel) } else { "  ".to_string() };
                    lines.push_str(&format!("{}{}", arrow, name));
                }
                Entry::Book(bi) => {
                    let b = &self.cat.books[*bi];
                    let marked = self.delete_marked.contains(&b.id);
                    let selected = idx == self.sel;
                    let title = trunc(&b.title, (self.list_w as usize).saturating_sub(6));
                    // Markers sit tight against the title — no padding columns to
                    // get swept under the selection underline. ✓ = read, ★ = star.
                    let mut content = String::new();
                    if b.read { content.push_str("\u{2713} "); }
                    if b.starred { content.push_str("\u{2605} "); }
                    content.push_str(&title);
                    // Colour: marked = dark red; read = dimmed (recedes); real =
                    // gold, conjured = grey; written books are brighter + bold.
                    let color = if marked { col().del }
                        else if b.read { col().dim }
                        else if b.kind == BookKind::Real { if b.written { col().real_bright } else { col().real } }
                        else if b.written { col().body_bright } else { col().body };
                    let mut styled = style::fg(&content, color);
                    if b.written && !b.read { styled = style::bold(&styled); }
                    if selected { styled = style::underline(&styled); }
                    // Pointer-style: a cyan arrow, one plain space, the title.
                    let arrow = if selected { style::fg("\u{2192}", col().sel) } else { " ".to_string() };
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
        if let Some(cat) = self.selected_category() {
            // Shelf header selected: show a summary of the shelf.
            let books: Vec<&Book> = self.cat.books.iter().filter(|b| b.category == cat).collect();
            let written = books.iter().filter(|b| b.written).count();
            let read = books.iter().filter(|b| b.read).count();
            out.push_str(&style::bold(&style::fg(&wrap(&cat, w), col().header)));
            out.push_str("\n\n");
            out.push_str(&style::fg(&format!("{} book(s) \u{00b7} {} rendered \u{00b7} {} read",
                books.len(), written, read), col().dim));
            out.push_str("\n\n");
            for b in &books {
                let mark = if b.read { "\u{2713} " } else if b.starred { "\u{2605} " } else { "  " };
                out.push_str(&style::fg(&format!("{}{}", mark, trunc(&b.title, w.saturating_sub(3))), col().body));
                out.push('\n');
            }
            out.push_str(&style::fg("\n Ctrl+\u{2191}/\u{2193} moves this shelf \u{00b7} \u{21b5} enters it", col().dim));
            self.right.set_text(&out);
            self.right.ix = 0;
            self.right.full_refresh();
            return;
        }
        if let Some(bi) = self.selected_book_idx() {
            let b = &self.cat.books[bi];
            let real = b.kind == BookKind::Real;
            // Kind badge.
            let badge = if real { style::fg("\u{25c6} real book", col().real) } else { style::fg("\u{2726} conjured", col().dim) };
            out.push_str(&badge);
            out.push('\n');
            out.push_str(&style::bold(&style::fg(&wrap(&b.title, w), if real { col().real } else { col().body })));
            out.push('\n');
            // Only real books get an author line — conjured books never show
            // a name, even if a legacy catalog row still carries one.
            if real && !b.author.is_empty() {
                let by = if !b.year.is_empty() {
                    format!("by {} ({})", b.author, b.year)
                } else {
                    format!("by {}", b.author)
                };
                out.push_str(&style::fg(&by, col().dim));
                out.push('\n');
            }
            let shelf = if b.subcategory.is_empty() { b.category.clone() } else { format!("{} \u{203a} {}", b.category, b.subcategory) };
            out.push_str(&style::fg(&shelf, col().header));
            out.push_str("\n\n");
            out.push_str(&style::fg(&wrap(&b.hook, w), col().hook));
            if !b.tags.is_empty() {
                out.push_str("\n\n");
                out.push_str(&style::fg(&wrap(&format!("#{}", b.tags.join("  #")), w), col().tag));
            }
            out.push_str("\n\n");
            let status = match (real, b.written) {
                (false, false) => "press \u{21b5} to grab \u{2014} claude writes this book",
                (false, true)  => "\u{2713} written \u{2014} press \u{21b5} to read",
                (true, false)  => "press \u{21b5} to fetch this real book (configured source)",
                (true, true)   => "\u{2713} fetched \u{2014} press \u{21b5} to read",
            };
            out.push_str(&style::fg(status, col().dim));
        } else if self.cat.books.is_empty() {
            out.push_str(&style::fg("Your shelves are empty.\n\nPress  s  to seed the library from your\ninterests, or  +  to add books on a topic.\nclaude will stock the shelves.", col().dim));
        }
        self.right.set_text(&out);
        self.right.ix = 0;
        self.right.full_refresh();
    }

    fn render_foot(&mut self, msg: &str) {
        let (left, color) = if msg.is_empty() {
            (" / find \u{00b7} * star \u{00b7} R read \u{00b7} f filter \u{00b7} ^\u{2191}\u{2193} reorder \u{00b7} M move \u{00b7} d mark \u{00b7} < purge \u{00b7} + more \u{00b7} s seed \u{00b7} i edit \u{00b7} w/W width \u{00b7} ^B border \u{00b7} P colours \u{00b7} ? help".to_string(), col().dim)
        } else {
            (msg.to_string(), col().header)
        };
        let ver = format!("library v{} ", env!("CARGO_PKG_VERSION"));
        let pad = (self.cols as usize)
            .saturating_sub(crust::display_width(&left) + crust::display_width(&ver));
        self.foot.say(&format!("{}{}{}",
            style::fg(&left, color),
            " ".repeat(pad),
            style::fg(&ver, col().dim)));
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
        // Enter on a shelf header descends to the shelf's first book.
        if self.selected_category().is_some() {
            if self.sel + 1 < self.entries.len()
                && matches!(self.entries[self.sel + 1], Entry::Book(_)) {
                self.sel += 1;
                self.render_left();
                self.render_right();
            }
            return;
        }
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
            self.fetch_book_async(bi);
            self.render_top();
            self.render_foot(" Fetching this book in the background \u{2014} keep browsing; \u{21b5} when it's ready.");
            return;
        }
        let ans = self.foot.ask("Depth \u{2014} [q]uick read / [d]eep dive: ", "q");
        if self.foot.last_escaped { self.render_foot(""); return; }
        let deep = ans.trim().to_lowercase().starts_with('d');
        self.cat.books[bi].deep = deep;
        let _ = self.cat.save();
        self.write_book_async(bi, deep);
        self.render_top();
        self.render_foot(" Writing your book in the background \u{2014} keep browsing; \u{21b5} when it's ready.");
    }

    /// Spawn the async book write for `bi` (conjured book). Parses the
    /// response into Markdown + SVG figures, renders the figures to PNG.
    fn write_book_async(&mut self, bi: usize, deep: bool) {
        let b = &self.cat.books[bi];
        let (id, title, hook, category) = (b.id.clone(), b.title.clone(), b.hook.clone(), b.category.clone());
        let tx = self.write_tx.clone();
        self.writing.insert(id.clone());
        std::thread::spawn(move || {
            let res = claude::write_book(&title, &hook, &category, deep).and_then(|raw| {
                let (md, figs) = claude::parse_book(&raw);
                std::fs::create_dir_all(store::book_dir(&id)).map_err(|e| e.to_string())?;
                let img = store::book_img_dir(&id);
                let _ = std::fs::create_dir_all(&img);
                for (n, svg) in figs {
                    let svg_path = img.join(format!("fig{}.svg", n));
                    if std::fs::write(&svg_path, &svg).is_ok() {
                        let png_path = img.join(format!("fig{}.png", n));
                        let _ = std::process::Command::new("rsvg-convert")
                            .args(["-w", "900"]).arg(&svg_path)
                            .arg("-o").arg(&png_path)
                            .status();
                    }
                }
                // Render LaTeX equations (eq{n}.png) + inline Unicode, then write.
                let md = crate::mathrender::render_math(&id, &md);
                std::fs::write(store::book_md(&id), md).map_err(|e| e.to_string())?;
                Ok(())
            });
            let _ = tx.send((id, res));
        });
    }

    /// Fetch a REAL book in the background: a custom source command if
    /// configured, else Project Gutenberg full text, else a Claude reader's
    /// companion. Reuses the write channel/`writing` set.
    fn fetch_book_async(&mut self, bi: usize) {
        let b = &self.cat.books[bi];
        let (id, title, author, year, isbn) =
            (b.id.clone(), b.title.clone(), b.author.clone(), b.year.clone(), b.isbn.clone());
        let fetch_cmd = self.cat.fetch_cmd.clone();
        let tx = self.write_tx.clone();
        self.writing.insert(id.clone());
        std::thread::spawn(move || {
            let res = fetch_real_book(&id, &title, &author, &year, &isbn, &fetch_cmd);
            let _ = tx.send((id, res));
        });
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

        let is_deep = self.cat.books.iter().find(|b| b.id == id).map(|b| b.deep).unwrap_or(false);
        let img_dir = store::book_img_dir(id);
        let mut display = glow::Display::new();
        let images_ok = display.supported();
        let term_w = self.cols;
        let term_h = self.rows;
        let cols = self.cols as usize;
        let h = (self.rows as usize).saturating_sub(2); // content rows 2..rows-1
        let max_w = cols.saturating_sub(4).max(20);
        let mut wrap_w = if self.cat.read_w >= 40 { (self.cat.read_w as usize).min(max_w) } else { max_w.min(86) };
        let (mut lines, mut figs) = render_markdown(&md, wrap_w, &img_dir, images_ok);
        // Resume at the synced bookmark (a fraction of the way through).
        let mut top = {
            let mt = lines.len().saturating_sub(h);
            (bookmark::load(id).unwrap_or(0.0) * mt as f32).round() as usize
        };
        let mut note: Option<String> = None;
        let mut shown: Vec<(u16, u16, u16, u16)> = Vec::new();

        let mut body = Pane::new(2, 2, self.cols.saturating_sub(2), self.rows.saturating_sub(2), col().body as u16, 0);
        body.scroll = false; body.wrap = false;
        Crust::clear_screen();

        let mut extend = false;
        loop {
            let total = lines.len();
            let max_top = total.saturating_sub(h);
            if top > max_top { top = max_top; }
            let pct = if max_top == 0 { 100 } else { top * 100 / max_top };
            let depth_tag = if is_deep { " [deep]" } else { "" };
            let tline = format!(" \u{1f4d6} {}{}", trunc(&title, cols.saturating_sub(24)), depth_tag);
            let prog = format!("{}% ", pct);
            let pad = cols.saturating_sub(crust::display_width(&tline) + crust::display_width(&prog));
            self.top.say(&format!("{}{}{}",
                style::bold(&style::fg(&tline, col().sel)), " ".repeat(pad), style::fg(&prog, col().dim)));

            // Clear images from the previous frame before repainting text.
            for (x, y, w, hh) in shown.drain(..) { display.clear(x, y, w, hh, term_w, term_h); }

            let window = lines[top..(top + h).min(total)].join("\n");
            body.set_text(&window);
            body.full_refresh();

            let ext_hint = if is_deep { "" } else { " \u{00b7} + deepen" };
            match &note {
                Some(m) => self.foot.say(&style::fg(&format!(" {}", m), col().header)),
                None => self.foot.say(&style::fg(&format!(" j/k \u{00b7} SPACE/b \u{00b7} g/G \u{00b7} w/W width \u{00b7} m mark \u{00b7} e pdf \u{00b7} c discuss \u{00b7} d define{} \u{00b7} q back", ext_hint), col().dim)),
            }

            // Show figures fully inside the current view.
            if images_ok {
                for fig in &figs {
                    if fig.line >= top && fig.line + fig.rows <= top + h {
                        let x = 4u16;
                        let y = 2 + (fig.line - top) as u16;
                        let w = wrap_w.min(max_w) as u16;
                        let hh = fig.rows as u16;
                        if display.show(&fig.png.to_string_lossy(), x, y, w, hh) {
                            shown.push((x, y, w, hh));
                        }
                    }
                }
            }

            let Some(key) = Input::getchr(None) else { continue };
            note = None; // a status note shows until the next key
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
                    let (l, f) = render_markdown(&md, wrap_w, &img_dir, images_ok);
                    lines = l; figs = f;
                    for (x, y, w, hh) in shown.drain(..) { display.clear(x, y, w, hh, term_w, term_h); }
                    Crust::clear_screen();
                }
                "d" => {
                    let phrase = self.foot.ask("Define: ", "");
                    let phrase = phrase.trim().to_string();
                    if !self.foot.last_escaped && !phrase.is_empty() {
                        let ctx: String = lines[top..(top + h).min(total)].iter()
                            .map(|l| crust::strip_ansi(l)).collect::<Vec<_>>().join("\n");
                        for (x, y, w, hh) in shown.drain(..) { display.clear(x, y, w, hh, term_w, term_h); }
                        self.foot.say(&style::fg(&format!(" defining \u{201c}{}\u{201d}\u{2026}", phrase), col().dim));
                        match claude::define(&phrase, &ctx) {
                            Ok(def) => {
                                Crust::clear_screen();
                                let mut dl = vec![String::new(),
                                    format!("  {}", style::bold(&style::fg(&phrase, col().sel))), String::new()];
                                for wl in wrap(def.trim(), wrap_w).lines() {
                                    dl.push(format!("  {}", style::fg(wl, col().hook)));
                                }
                                dl.push(String::new());
                                dl.push(style::fg("  (any key returns to the book)", col().dim));
                                body.set_text(&dl.join("\n"));
                                body.full_refresh();
                                self.foot.say(&style::fg(" definition \u{00b7} any key returns", col().dim));
                                let _ = Input::getchr(None);
                                Crust::clear_screen();
                            }
                            Err(e) => self.render_foot(&format!(" define failed: {}", e)),
                        }
                    }
                }
                "m" => {
                    let frac = if max_top == 0 { 0.0 } else { top as f32 / max_top as f32 };
                    bookmark::save(id, frac);
                    note = Some(format!("\u{1f516} Bookmark set at {}%", (frac * 100.0).round() as i32));
                }
                "e" => {
                    for (x, y, w, hh) in shown.drain(..) { display.clear(x, y, w, hh, term_w, term_h); }
                    self.foot.say(&style::fg(" exporting PDF\u{2026}", col().dim));
                    match crate::export::export_book_pdf(id, &title, &md) {
                        Ok(p) => note = Some(format!("Saved PDF \u{2192} {}", p.display())),
                        Err(e) => note = Some(format!("PDF failed: {}", e)),
                    }
                }
                "c" => {
                    let frac = if max_top == 0 { 0.0 } else { top as f32 / max_top as f32 };
                    let context = if is_deep { current_chapter(&md, frac) } else { md.clone() };
                    for (x, y, w, hh) in shown.drain(..) { display.clear(x, y, w, hh, term_w, term_h); }
                    self.discuss(&title, &context, is_deep);
                    Crust::clear_screen();
                    self.top.invalidate();
                    self.foot.invalidate();
                    note = Some("back from discussion".into());
                }
                "P" => {
                    for (x, y, w, hh) in shown.drain(..) { display.clear(x, y, w, hh, term_w, term_h); }
                    self.color_config();
                    self.apply_bar_bg();
                    let (l, f) = render_markdown(&md, wrap_w, &img_dir, images_ok);
                    lines = l; figs = f;
                    Crust::clear_screen();
                    self.top.invalidate();
                    self.foot.invalidate();
                    note = Some("colours updated".into());
                }
                "+" if !is_deep => { extend = true; break; }
                "?" => {
                    for (x, y, w, hh) in shown.drain(..) { display.clear(x, y, w, hh, term_w, term_h); }
                    self.show_help("Reader \u{2014} keys", READER_HELP);
                    Crust::clear_screen();
                }
                _ => {}
            }
        }

        for (x, y, w, hh) in shown.drain(..) { display.clear(x, y, w, hh, term_w, term_h); }
        Crust::clear_screen();
        self.top.invalidate();
        self.foot.invalidate();

        if extend {
            if let Some(bi) = self.cat.books.iter().position(|b| b.id == id) {
                self.cat.books[bi].deep = true;
                let _ = self.cat.save();
                self.write_book_async(bi, true);
                self.render_all();
                self.render_foot(" Extending into a full deep-dive in the background \u{2014} \u{21b5} when ready.");
                return;
            }
        }
        self.render_all();
    }

    /// Re-point the bar panes at the (possibly changed) `bar_bg` colour.
    fn apply_bar_bg(&mut self) {
        self.top.bg = col().bar_bg as u16;
        self.foot.bg = col().bar_bg as u16;
    }

    /// `P` — live colour configuration popup for the shelf + reader palette.
    /// h/l adjust by 1, H/L by 10; Enter saves to the catalog, ESC reverts.
    fn color_config(&mut self) {
        let orig = col();
        let mut cols = orig;
        let mut sel = 0usize;
        let pw = 46u16.min(self.cols.saturating_sub(4));
        let ph = (COLOR_SLOTS.len() as u16 + 4).min(self.rows.saturating_sub(2)).max(8);
        let px = self.cols.saturating_sub(pw) / 2 + 1;
        let py = self.rows.saturating_sub(ph) / 2 + 1;
        let mut pane = Pane::new(px, py, pw, ph, cols.body as u16, cols.bar_bg as u16);
        pane.scroll = false; pane.wrap = false;
        pane.border = true;
        pane.border_fg = Some(cols.border as u16);
        loop {
            set_colors(cols); // live swatches
            let mut s = String::new();
            s.push_str(&style::bold(&style::fg(" Colours", col().sel)));
            s.push_str(&style::fg("   h/l \u{00b1}1 \u{00b7} H/L \u{00b1}10 \u{00b7} \u{21b5} save \u{00b7} ESC cancel\n\n", col().dim));
            for (i, slot) in COLOR_SLOTS.iter().enumerate() {
                let v = (slot.1)(&cols);
                let arrow = if i == sel { style::fg("\u{2192} ", col().sel) } else { "  ".to_string() };
                let sw = style::fg("\u{2588}\u{2588}\u{2588}\u{2588}", v);
                let name = if i == sel { style::bold(slot.0) } else { slot.0.to_string() };
                s.push_str(&format!("{}{} {:>3}  {}\n", arrow, sw, v, name));
            }
            pane.say(&s);
            pane.border_refresh();
            let Some(k) = Input::getchr(None) else { continue };
            let (_, get, setf) = COLOR_SLOTS[sel];
            match k.as_str() {
                "j" | "DOWN" => if sel + 1 < COLOR_SLOTS.len() { sel += 1; },
                "k" | "UP" => sel = sel.saturating_sub(1),
                "g" | "HOME" => sel = 0,
                "G" | "END" => sel = COLOR_SLOTS.len() - 1,
                "l" | "RIGHT" | "+" | "=" => { let nv = get(&cols).wrapping_add(1); setf(&mut cols, nv); }
                "h" | "LEFT" | "-" | "_" => { let nv = get(&cols).wrapping_sub(1); setf(&mut cols, nv); }
                "L" => { let nv = get(&cols).wrapping_add(10); setf(&mut cols, nv); }
                "H" => { let nv = get(&cols).wrapping_sub(10); setf(&mut cols, nv); }
                "ENTER" => { set_colors(cols); self.cat.colors = cols; let _ = self.cat.save(); break; }
                "q" | "ESC" => { set_colors(orig); break; }
                _ => {}
            }
        }
    }

    /// Discuss the text in a break-out Claude session (mirrors scribe's
    /// `:chat`). The book (quick read) or current chapter (deep dive) is
    /// dropped to a tempfile and referenced in the opening prompt; `/exit`
    /// in claude returns to the reader.
    fn discuss(&mut self, title: &str, context: &str, is_deep: bool) {
        use std::io::Write as _;
        let tmpfile = format!("/tmp/library-discuss-{}.md", std::process::id());
        let _ = std::fs::write(&tmpfile, context);
        let scope = if is_deep { "the current chapter" } else { "the full book" };
        let initial = format!(
            "I'm reading a book titled \"{}\" in my library app and want to discuss it. \
             The text of {} is in {} \u{2014} read it, then let's talk: ideas, questions, \
             pushback, connections to other things. When I'm done, /exit returns me to \
             the reader.",
            title, scope, tmpfile);
        print!("\x1b[?2004l");
        let _ = std::io::stdout().flush();
        Crust::cleanup();
        Crust::clear_screen();
        let _ = std::process::Command::new("claude").arg(&initial).status();
        Crust::init();
        Crust::set_app_identity("Library");
        print!("\x1b[?2004h");
        let _ = std::io::stdout().flush();
        let _ = std::fs::remove_file(&tmpfile);
    }

    fn run(&mut self) {
        loop {
            // Block forever when idle (zero idle wakeups); while batches are
            // brewing, wake every second to tick the spinner and collect
            // finished work — so the UI stays fully interactive throughout.
            let busy = self.gen_in_flight > 0 || !self.writing.is_empty() || !self.importing.is_empty();
            let timeout = if busy { Some(1) } else { None };
            let key = Input::getchr(timeout);
            self.drain_generations();
            self.drain_writes();
            self.drain_imports();
            let Some(key) = key else {
                if self.gen_in_flight > 0 || !self.writing.is_empty() || !self.importing.is_empty() {
                    self.tick_spinner();
                }
                continue;
            };
            match key.as_str() {
                "q" | "ESC" => break,
                "j" | "DOWN" => self.move_sel(true),
                "k" | "UP" => self.move_sel(false),
                "PgDOWN" => self.page_sel(true),
                "PgUP" => self.page_sel(false),
                "g" | "HOME" => self.go_edge(false),
                "G" | "END" => self.go_edge(true),
                "*" => self.toggle_star(),
                "f" => self.cycle_filter(),
                "R" => self.toggle_read(),
                "C-UP" => if self.selected_category().is_some() { self.move_shelf(false) } else { self.move_book(false) },
                "C-DOWN" => if self.selected_category().is_some() { self.move_shelf(true) } else { self.move_book(true) },
                "M" => self.move_to_shelf(),
                "?" => {
                    self.show_help("Library \u{2014} keys", SHELF_HELP);
                    Crust::clear_screen();
                    self.top.invalidate();
                    self.foot.invalidate();
                    self.render_all();
                }
                "d" => self.toggle_delete(),
                "<" => self.purge_marked(),
                "+" => self.request_more(false),
                "s" => self.request_more(true),
                "i" => self.edit_interests(),
                "a" => self.import_book(),
                "y" => self.copy_book_path(),
                "w" => self.cycle_width(true),
                "W" => self.cycle_width(false),
                "C-B" => self.cycle_border(),
                "P" => {
                    self.color_config();
                    self.apply_bar_bg();
                    Crust::clear_screen();
                    self.top.invalidate();
                    self.foot.invalidate();
                    self.render_all();
                    self.render_foot(" Colours updated.");
                }
                "/" => self.do_search(),
                "ENTER" | "RIGHT" => self.grab(),
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

/// A figure to render inline in the reader: its top line in the rendered
/// text, the PNG to show, and how many rows it reserves.
struct FigPos { line: usize, png: std::path::PathBuf, rows: usize }

const FIG_ROWS: usize = 18;
/// Rows reserved for a rendered equation image — shorter than a figure;
/// glow fits the image to its aspect within the box.
const EQ_ROWS: usize = 6;

/// Render a book's Markdown into styled, wrapped display lines plus the
/// inline figures. Headings are coloured/bold; `>` lines are dimmed
/// pull-quotes; `[[FIG n: caption]]` reserves space + records the figure;
/// body paragraphs wrap to `width` with inline **bold** / *italic*.
/// The chapter (a `## ` section) the reader is currently in, picked by
/// reading fraction across the source. Used to scope a deep-dive discussion.
fn current_chapter(md: &str, frac: f32) -> String {
    let mut chapters: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in md.lines() {
        if line.starts_with("## ") && !cur.trim().is_empty() {
            chapters.push(std::mem::take(&mut cur));
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.trim().is_empty() { chapters.push(cur); }
    if chapters.is_empty() { return md.to_string(); }
    let total: usize = chapters.iter().map(|c| c.len()).sum();
    let target = (frac.clamp(0.0, 1.0) as f64 * total as f64) as usize;
    let mut acc = 0;
    for c in &chapters {
        acc += c.len();
        if target <= acc { return c.clone(); }
    }
    chapters.last().cloned().unwrap_or_else(|| md.to_string())
}

fn render_markdown(md: &str, width: usize, img_dir: &std::path::Path, images_ok: bool)
    -> (Vec<String>, Vec<FigPos>)
{
    let mut out: Vec<String> = Vec::new();
    let mut figs: Vec<FigPos> = Vec::new();
    for raw in md.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim();
        if let Some(inner) = trimmed.strip_prefix("[[EQ").and_then(|r| r.strip_suffix("]]")) {
            if let Ok(n) = inner.trim().parse::<usize>() {
                let png = img_dir.join(format!("eq{}.png", n));
                out.push(String::new());
                if images_ok && png.exists() {
                    let start = out.len();
                    for _ in 0..EQ_ROWS { out.push(String::new()); }
                    figs.push(FigPos { line: start, png, rows: EQ_ROWS });
                } else if !png.exists() {
                    out.push(style::fg("   (equation unavailable)", col().dim));
                }
                out.push(String::new());
                continue;
            }
        }
        if let Some(inner) = trimmed.strip_prefix("[[FIG").and_then(|r| r.strip_suffix("]]")) {
            let inner = inner.trim();
            let (n_str, caption) = match inner.split_once(':') {
                Some((n, c)) => (n.trim(), c.trim()),
                None => (inner, ""),
            };
            if let Ok(n) = n_str.parse::<usize>() {
                let png = img_dir.join(format!("fig{}.png", n));
                let cap = if caption.is_empty() { format!("Figure {}", n) }
                          else { format!("Figure {}: {}", n, caption) };
                out.push(String::new());
                out.push(style::fg(&format!("   \u{2014} {} \u{2014}", cap), col().tag));
                if images_ok && png.exists() {
                    let start = out.len();
                    for _ in 0..FIG_ROWS { out.push(String::new()); }
                    figs.push(FigPos { line: start, png, rows: FIG_ROWS });
                } else if !png.exists() {
                    out.push(style::fg("   (figure unavailable)", col().dim));
                }
                out.push(String::new());
                continue;
            }
        }
        if let Some(t) = line.strip_prefix("# ") {
            out.push(String::new());
            out.push(style::bold(&style::fg(t.trim(), col().reader_h1)));
            out.push(String::new());
        } else if let Some(t) = line.strip_prefix("## ") {
            out.push(String::new());
            out.push(style::bold(&style::fg(t.trim(), col().reader_h2)));
            out.push(String::new());
        } else if let Some(t) = line.strip_prefix("### ") {
            out.push(style::bold(&style::fg(t.trim(), col().reader_h3)));
        } else if let Some(t) = line.strip_prefix("> ") {
            for wl in wrap(t.trim(), width.saturating_sub(2)).lines() {
                out.push(format!("  {}", style::italic(&style::fg(wl, col().reader_quote))));
            }
        } else if trimmed.is_empty() {
            out.push(String::new());
        } else {
            for wl in wrap(line, width).lines() {
                out.push(style::fg(&style_inline(wl), col().reader_fg));
            }
        }
    }
    (out, figs)
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

/// Fetch a real book to `books/<id>/book.md`. Custom source command if
/// configured, else Project Gutenberg full text, else a Claude reader's
/// companion. Legal-first — never pirated full text.
fn fetch_real_book(id: &str, title: &str, author: &str, year: &str, isbn: &str, fetch_cmd: &str)
    -> Result<(), String>
{
    std::fs::create_dir_all(store::book_dir(id)).map_err(|e| e.to_string())?;
    if !fetch_cmd.trim().is_empty() {
        let cmd = fetch_cmd.replace("@title", title).replace("@author", author).replace("@isbn", isbn);
        if let Ok(out) = std::process::Command::new("sh").arg("-c").arg(&cmd).output() {
            if out.status.success() && !out.stdout.is_empty() {
                std::fs::write(store::book_md(id), &out.stdout).map_err(|e| e.to_string())?;
                return Ok(());
            }
        }
    }
    if let Some(text) = gutenberg_text(title, author) {
        let md = format!("# {}\n\n*Full text \u{2014} Project Gutenberg (public domain).*\n\n{}", title, text);
        std::fs::write(store::book_md(id), md).map_err(|e| e.to_string())?;
        return Ok(());
    }
    let md = crate::claude::reader_companion(title, author, year)?;
    std::fs::write(store::book_md(id), md).map_err(|e| e.to_string())?;
    Ok(())
}

/// Search Project Gutenberg (gutendex) and return the plain-text body of a
/// matching public-domain edition, if any.
fn gutenberg_text(title: &str, author: &str) -> Option<String> {
    let out = std::process::Command::new("curl")
        .args(["-s", "--max-time", "20", "-G", "--data-urlencode"])
        .arg(format!("search={} {}", title, author))
        .arg("https://gutendex.com/books")
        .output().ok()?;
    if !out.status.success() { return None; }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    for r in json.get("results")?.as_array()? {
        let rtitle = r.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if !title_matches(title, rtitle) { continue; }
        let Some(formats) = r.get("formats").and_then(|f| f.as_object()) else { continue };
        let Some(url) = formats.iter()
            .find(|(k, _)| k.starts_with("text/plain"))
            .and_then(|(_, v)| v.as_str()) else { continue };
        if let Ok(t) = std::process::Command::new("curl")
            .args(["-sL", "--max-time", "60", url]).output()
        {
            if t.status.success() && !t.stdout.is_empty() {
                return Some(strip_gutenberg_boilerplate(&String::from_utf8_lossy(&t.stdout)));
            }
        }
    }
    None
}

/// Loose title match: the part of `want` before any ':' appears in `got`.
fn title_matches(want: &str, got: &str) -> bool {
    let main = want.split(':').next().unwrap_or(want).trim().to_lowercase();
    !main.is_empty() && got.to_lowercase().contains(&main)
}

/// Strip Project Gutenberg's header/footer boilerplate.
fn strip_gutenberg_boilerplate(s: &str) -> String {
    let mut body = s;
    if let Some(p) = s.find("*** START OF") {
        if let Some(nl) = s[p..].find('\n') { body = &s[p + nl + 1..]; }
    }
    if let Some(p) = body.find("*** END OF") { body = &body[..p]; }
    body.trim().to_string()
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
