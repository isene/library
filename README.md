# library

<img src="img/library.svg" align="left" width="150" height="150">

**A personal library of the books that *should* exist — and a way to read them.**

![Version](https://img.shields.io/badge/version-0.1.1-blue) ![Rust](https://img.shields.io/badge/language-Rust-f74c00) ![License](https://img.shields.io/badge/license-Unlicense-green) ![Platform](https://img.shields.io/badge/platform-Linux-blue) ![Stay Amazing](https://img.shields.io/badge/Stay-Amazing-important)

Curate a shelf of books that ought to exist but mostly don't. Browse the
catalogue, star the ones you want, and when you grab a book Claude sits down
and *writes the whole thing* — chapters, prose, and clean inline figures — for
you to read right there in the terminal.

<br clear="left"/>

Part of the [Fe₂O₃](https://github.com/isene/fe2o3) Rust terminal suite. Built
on [crust](https://github.com/isene/crust) (TUI) and
[glow](https://github.com/isene/glow) (inline images). The phone reader is
[**books**](https://github.com/isene/nomad/tree/master/apps/books) in the
[nomad](https://github.com/isene/nomad) family — grab on the laptop, read
anywhere over Syncthing.

## The idea

A real library is full of books someone decided to write. This one is full of
books *you* decide should exist. You describe your interests once; an
acquisitions librarian (Claude, cheaply) stocks the shelves with enticing
titles — broad surveys, niche deep-dives, and odd illuminating corners — mixing
in real, actually-published books where they fit.

Nothing is written until you want it. Browsing an enormous catalogue costs
almost nothing: each book is just a title, a shelf, and a one-line hook. The
moment you **grab** one:

- a **conjured** book is written in full by Claude — 3–4 chapters for a focused
  read, or a longer deep dive — complete with simple SVG figures drawn to
  actually aid understanding;
- a **real** book is fetched from a legal source (Project Gutenberg full text
  where available), or given a rich reader's companion that points you to where
  to buy or borrow it.

Then you read it: a full-screen reader with chapter headings, pull-quotes,
inline figures, adjustable width, highlight-a-phrase-for-a-definition, and a
one-key "make this quick read into a proper book".

## Features

- **Generative catalogue.** Describe your interests; the shelves fill with
  books worth grabbing. Add more any time, focus a batch on a topic, search,
  star, and prune.
- **Books written on demand.** Grab a conjured title and Claude writes it in
  full, with figures, as Markdown — yours to keep under `~/.library`.
- **Real books too.** Marked in gold, fetched from legal sources, never pirated.
- **A proper reader.** Full-screen, paginated, inline figures via kitty
  graphics, adjustable reading width, reading-progress.
- **Highlight → definition.** Select a word or phrase and get the meaning that
  fits *this* passage.
- **Quick → deep.** Turn a short read into a full-length book with one key.
- **Syncs to your phone.** Plain JSON + Markdown under `~/.library`, so
  Syncthing mirrors it to the [books](https://github.com/isene/nomad) reader.
- **Cold when idle.** The event loop blocks on input and only wakes while a
  generation is actually in flight — no polling, no idle drain.

## Install

Download a binary from [Releases](https://github.com/isene/library/releases):

```bash
chmod +x library-*
mkdir -p ~/bin && mv library-linux-x86_64 ~/bin/library
```

Or build from source (clone alongside its Fe₂O₃ siblings):

```bash
cd ~/src
git clone https://github.com/isene/crust
git clone https://github.com/isene/glow
git clone https://github.com/isene/library
cd library && cargo build --release
```

### Requirements

- The [`claude`](https://docs.claude.com/en/docs/claude-code) CLI on your `PATH`
  (catalogue, book writing, definitions all run through `claude -p`).
- `rsvg-convert` (from librsvg) to rasterise figures.
- `curl` for fetching real books.
- A terminal with kitty graphics for inline figures (e.g.
  [glass](https://github.com/isene/CHasm), kitty, or wezterm); text still reads
  fine without it.

## Usage

```bash
library              # open the library TUI
library --seed       # first run: describe your interests, stock the shelves
library --more       # add more books in the spirit of the current shelves
library --list       # print the catalogue
```

### Keys

**Shelves**

| Key | Action |
|---|---|
| `↑`/`↓` `j`/`k` | move |
| `Enter` | grab / open the book |
| `s` | stock more (optionally focused on a topic; blank = more of the same) |
| `i` | edit your interests |
| `/` | search title, hook, author, shelf, tags |
| `*` | star |
| `d` / `<` | mark for removal / purge marked |
| `w` / `W` | narrow / widen the list pane |
| `^B` | cycle pane borders |
| `r` | reload from disk |
| `q` | quit |

**Reader**

| Key | Action |
|---|---|
| `↑`/`↓` `Space` | scroll |
| `w` / `W` | narrow / widen the text |
| `d` | define the highlighted word/phrase in context |
| `+` | extend a quick read into a full book |
| `q` / `Esc` | back to the shelves |

## Where it lives

```
~/.library/
├── catalog.json                every book idea (metadata only)
└── books/<id>/
    ├── book.md                 the written text, with [[FIG n: caption]] markers
    └── img/figN.{svg,png}       the figures
```

Point a Syncthing folder at `~/.library` to read everything on your phone with
[books](https://github.com/isene/nomad/tree/master/apps/books).

## License

[Unlicense](LICENSE) — public domain. Created by Geir Isene.
