//! Render LaTeX math in a book's Markdown so equations display properly in
//! both readers (TUI via glow, phone via the figure-image path).
//!
//! * Display math — `$$ … $$` or `\[ … \]` (may span lines) — is rendered to
//!   `img/eq{n}.png` with LaTeX (standalone class → pdflatex → pdftoppm) and
//!   replaced by a `[[EQ n]]` marker on its own line. `[[EQ n]]` is a distinct
//!   namespace from `[[FIG n]]`, so equation and figure numbering never clash.
//! * Inline math — `$ … $` / `\( … \)` — is converted to a best-effort Unicode
//!   rendering and kept inline as text (no image; cheap and reflowable).
//!
//! Used by both the PDF import (`build_book`) and the conjured-book write path.

use std::path::Path;

use crate::store;

/// Per-book cap on rendered display equations — bounds pdflatex runs and sync
/// size. Over the cap, equations fall back to inline Unicode.
const MAX_EQS: usize = 120;

/// Render display equations to images and inline math to Unicode. Returns the
/// rewritten Markdown. `img/eq{n}.png` files are written under the book dir.
pub fn render_math(id: &str, md: &str) -> String {
    let img_dir = store::book_img_dir(id);
    let _ = std::fs::create_dir_all(&img_dir);
    let mut next = 1usize;
    let with_display = replace_display(md, &img_dir, &mut next);
    replace_inline(&with_display)
}

/// Replace `$$…$$` / `\[…\]` display blocks with rendered `[[EQ n]]` markers.
fn replace_display(md: &str, img_dir: &Path, next: &mut usize) -> String {
    let mut out = String::with_capacity(md.len());
    let mut rest = md;
    loop {
        // Earliest display-math opener.
        let dd = rest.find("$$");
        let br = rest.find("\\[");
        let (open_at, close_pat, open_len) = match (dd, br) {
            (None, None) => { out.push_str(rest); break; }
            (Some(a), None) => (a, "$$", 2),
            (None, Some(b)) => (b, "\\]", 2),
            (Some(a), Some(b)) => if a <= b { (a, "$$", 2) } else { (b, "\\]", 2) },
        };
        let after_open = &rest[open_at + open_len..];
        let Some(close_rel) = after_open.find(close_pat) else {
            // No closing delimiter — leave the rest untouched.
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open_at]);
        let eq = after_open[..close_rel].trim();
        emit_display(eq, img_dir, next, &mut out);
        rest = &after_open[close_rel + close_pat.len()..];
    }
    out
}

/// Render one display equation to `eq{n}.png` and append `[[EQ n]]`; on any
/// failure (or over the cap) fall back to inline Unicode so nothing is lost.
fn emit_display(eq: &str, img_dir: &Path, next: &mut usize, out: &mut String) {
    if eq.is_empty() { return; }
    if *next <= MAX_EQS {
        let png = img_dir.join(format!("eq{}.png", *next));
        if latex_display_to_png(eq, &png) {
            // Own line so the reader treats it as a block.
            if !out.ends_with('\n') { out.push('\n'); }
            out.push_str(&format!("\n[[EQ {}]]\n\n", *next));
            *next += 1;
            return;
        }
    }
    // Fallback: readable inline Unicode on its own line.
    if !out.ends_with('\n') { out.push('\n'); }
    out.push_str(&format!("\n{}\n\n", tex_to_unicode(eq)));
}

/// LaTeX (standalone, content-cropped) → pdflatex → pdftoppm PNG.
fn latex_display_to_png(latex: &str, out_png: &Path) -> bool {
    let dir = std::env::temp_dir().join(format!("lib-eq-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err() { return false; }
    let tex = format!(
        "\\documentclass[border=3pt,varwidth=18cm]{{standalone}}\n\
         \\usepackage{{amsmath,amssymb,amsfonts}}\n\
         \\begin{{document}}\n\\[{}\\]\n\\end{{document}}\n",
        latex
    );
    if std::fs::write(dir.join("eq.tex"), tex).is_err() { return false; }
    let ok = std::process::Command::new("pdflatex")
        .args(["-interaction=nonstopmode", "-halt-on-error", "eq.tex"])
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok || !dir.join("eq.pdf").exists() { return false; }
    // 150 dpi: crisp without being huge.
    let prefix = dir.join("eqimg");
    let rendered = std::process::Command::new("pdftoppm")
        .args(["-png", "-r", "150", "-singlefile"])
        .arg(dir.join("eq.pdf"))
        .arg(&prefix)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let src = dir.join("eqimg.png");
    if rendered && src.exists() {
        std::fs::copy(&src, out_png).is_ok()
    } else {
        false
    }
}

/// Convert inline `$ … $` / `\( … \)` spans to Unicode, leaving prose intact.
fn replace_inline(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut rest = md;
    loop {
        // `\( … \)` first (unambiguous), then single `$ … $`.
        let paren = rest.find("\\(");
        let dollar = find_inline_dollar(rest);
        let (open_at, close_pat, open_len) = match (paren, dollar) {
            (None, None) => { out.push_str(rest); break; }
            (Some(a), None) => (a, "\\)", 2),
            (None, Some(b)) => (b, "$", 1),
            (Some(a), Some(b)) => if a <= b { (a, "\\)", 2) } else { (b, "$", 1) },
        };
        let after = &rest[open_at + open_len..];
        let Some(close_rel) = after.find(close_pat) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open_at]);
        out.push_str(&tex_to_unicode(after[..close_rel].trim()));
        rest = &after[close_rel + close_pat.len()..];
    }
    out
}

/// Find a single `$` that opens inline math (not part of `$$`).
fn find_inline_dollar(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            if i + 1 < b.len() && b[i + 1] == b'$' { i += 2; continue; }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Best-effort LaTeX → Unicode for inline math. Handles common commands,
/// `^`/`_` for single chars or `{…}` groups, and strips braces/spacing.
fn tex_to_unicode(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                // Read command name (letters), else a single escaped char.
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_ascii_alphabetic() { j += 1; }
                if j == i + 1 {
                    // \,  \;  \!  \(  \)  \{  \}  etc.
                    if j < chars.len() {
                        match chars[j] {
                            '{' | '}' => out.push(chars[j]),
                            ',' | ';' | '!' | ' ' => {}
                            other => out.push(other),
                        }
                        i = j + 1;
                    } else { i = j; }
                } else {
                    let name: String = chars[i + 1..j].iter().collect();
                    if let Some(sym) = greek_or_symbol(&name) {
                        out.push_str(sym);
                    } else {
                        // unknown command: keep its name as text (e.g. \log → log)
                        out.push_str(&name);
                    }
                    i = j;
                }
            }
            '^' | '_' => {
                let sup = c == '^';
                i += 1;
                let group = read_group(&chars, &mut i);
                for g in group.chars() {
                    out.push(map_script(g, sup).unwrap_or(g));
                }
            }
            '{' | '}' | '$' => { i += 1; }
            _ => { out.push(c); i += 1; }
        }
    }
    out
}

/// Read the argument after `^`/`_`: a `{…}` group or one char/command.
fn read_group(chars: &[char], i: &mut usize) -> String {
    if *i >= chars.len() { return String::new(); }
    if chars[*i] == '{' {
        *i += 1;
        let mut g = String::new();
        let mut depth = 1;
        while *i < chars.len() && depth > 0 {
            match chars[*i] {
                '{' => { depth += 1; g.push('{'); }
                '}' => { depth -= 1; if depth > 0 { g.push('}'); } }
                ch => g.push(ch),
            }
            *i += 1;
        }
        // Recursively normalise commands inside the group.
        tex_to_unicode(&g)
    } else if chars[*i] == '\\' {
        // a command like ^\circ
        let mut j = *i + 1;
        while j < chars.len() && chars[j].is_ascii_alphabetic() { j += 1; }
        let name: String = chars[*i + 1..j].iter().collect();
        *i = j;
        greek_or_symbol(&name).unwrap_or("").to_string()
    } else {
        let ch = chars[*i];
        *i += 1;
        ch.to_string()
    }
}

/// Superscript / subscript Unicode for a single char, if one exists.
fn map_script(c: char, sup: bool) -> Option<char> {
    let pair = if sup {
        "0⁰1¹2²3³4⁴5⁵6⁶7⁷8⁸9⁹+⁺-⁻=⁼(⁽)⁾n\u{207F}i\u{2071}aᵃbᵇ"
    } else {
        "0₀1₁2₂3₃4₄5₅6₆7₇8₈9₉+₊-₋=₌(₍)₎nₙiᵢaₐeₑoₒxₓ"
    };
    let v: Vec<char> = pair.chars().collect();
    let mut k = 0;
    while k + 1 < v.len() {
        if v[k] == c { return Some(v[k + 1]); }
        k += 2;
    }
    None
}

/// Map a LaTeX command name to a Unicode symbol.
fn greek_or_symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        // lower greek
        "alpha" => "α", "beta" => "β", "gamma" => "γ", "delta" => "δ",
        "epsilon" | "varepsilon" => "ε", "zeta" => "ζ", "eta" => "η",
        "theta" | "vartheta" => "θ", "iota" => "ι", "kappa" => "κ",
        "lambda" => "λ", "mu" => "μ", "nu" => "ν", "xi" => "ξ", "pi" => "π",
        "rho" => "ρ", "sigma" => "σ", "tau" => "τ", "upsilon" => "υ",
        "phi" | "varphi" => "φ", "chi" => "χ", "psi" => "ψ", "omega" => "ω",
        // upper greek
        "Gamma" => "Γ", "Delta" => "Δ", "Theta" => "Θ", "Lambda" => "Λ",
        "Xi" => "Ξ", "Pi" => "Π", "Sigma" => "Σ", "Phi" => "Φ", "Psi" => "Ψ",
        "Omega" => "Ω",
        // relations / arrows
        "to" | "rightarrow" => "→", "leftarrow" => "←", "leftrightarrow" => "↔",
        "Rightarrow" => "⇒", "Leftarrow" => "⇐", "mapsto" => "↦",
        "leq" | "le" => "≤", "geq" | "ge" => "≥", "neq" | "ne" => "≠",
        "approx" => "≈", "equiv" => "≡", "cong" => "≅", "sim" => "∼",
        "simeq" => "≃", "propto" => "∝", "ll" => "≪", "gg" => "≫",
        // operators
        "times" => "×", "cdot" => "⋅", "div" => "÷", "pm" => "±", "mp" => "∓",
        "ast" => "∗", "star" => "⋆", "circ" => "∘", "bullet" => "•",
        "oplus" => "⊕", "otimes" => "⊗", "odot" => "⊙",
        // big ops
        "sum" => "∑", "prod" => "∏", "int" => "∫", "oint" => "∮",
        "sqrt" => "√", "partial" => "∂", "nabla" => "∇", "infty" => "∞",
        // sets / logic
        "in" => "∈", "notin" => "∉", "ni" => "∋", "subset" => "⊂",
        "subseteq" => "⊆", "supset" => "⊃", "supseteq" => "⊇",
        "cup" => "∪", "cap" => "∩", "emptyset" | "varnothing" => "∅",
        "forall" => "∀", "exists" => "∃", "nexists" => "∄", "neg" | "lnot" => "¬",
        "wedge" | "land" => "∧", "vee" | "lor" => "∨",
        "setminus" => "∖", "mid" => "∣",
        // misc
        "angle" => "∠", "perp" => "⊥", "parallel" => "∥", "prime" => "′",
        "ldots" | "dots" => "…", "cdots" => "⋯", "vdots" => "⋮",
        "langle" => "⟨", "rangle" => "⟩", "lceil" => "⌈", "rceil" => "⌉",
        "lfloor" => "⌊", "rfloor" => "⌋", "hbar" => "ℏ", "ell" => "ℓ",
        "Re" => "ℜ", "Im" => "ℑ", "aleph" => "ℵ", "deg" => "°",
        "mathbb" | "mathcal" | "mathbf" | "mathrm" | "text" | "left" | "right"
            | "displaystyle" | "quad" | "qquad" => "",
        _ => return None,
    })
}
