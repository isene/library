//! Book → PDF export, for reading on a Remarkable (or anywhere). The
//! book's Markdown (with its inline `[[FIG n]]` figures) is rendered to a
//! clean, readable LaTeX document and compiled with `xelatex`. The PDF
//! lands next to the source at `~/.library/books/<id>/<title>.pdf`, so it
//! syncs to the phone alongside everything else.

use std::path::{Path, PathBuf};

use crate::store;

/// Unicode superscript/subscript digits and letters → raised/lowered LaTeX
/// on an empty base, so `c²`, `m₁`, `x⁰`, `Tμν`-style forms survive the engine.
/// Covers the full Latin-1, Superscripts-and-Subscripts, and Phonetic
/// super/subscript blocks. Returns None for anything else.
fn super_sub(c: char) -> Option<String> {
    let sup = |t: &str| { let mut s = String::from("\\ensuremath{{}^{"); s.push_str(t); s.push_str("}}"); Some(s) };
    let sub = |t: &str| { let mut s = String::from("\\ensuremath{{}_{"); s.push_str(t); s.push_str("}}"); Some(s) };
    match c {
        '\u{2070}' => sup("0"), '\u{00B9}' => sup("1"), '\u{00B2}' => sup("2"),
        '\u{00B3}' => sup("3"), '\u{2074}' => sup("4"), '\u{2075}' => sup("5"),
        '\u{2076}' => sup("6"), '\u{2077}' => sup("7"), '\u{2078}' => sup("8"),
        '\u{2079}' => sup("9"), '\u{207A}' => sup("+"), '\u{207B}' => sup("-"),
        '\u{207F}' => sup("n"), '\u{2071}' => sup("i"),
        '\u{2080}' => sub("0"), '\u{2081}' => sub("1"), '\u{2082}' => sub("2"),
        '\u{2083}' => sub("3"), '\u{2084}' => sub("4"), '\u{2085}' => sub("5"),
        '\u{2086}' => sub("6"), '\u{2087}' => sub("7"), '\u{2088}' => sub("8"),
        '\u{2089}' => sub("9"), '\u{208A}' => sub("+"), '\u{208B}' => sub("-"),
        '\u{2090}' => sub("a"), '\u{2091}' => sub("e"), '\u{2092}' => sub("o"),
        '\u{2093}' => sub("x"), '\u{2095}' => sub("h"), '\u{2096}' => sub("k"),
        '\u{2097}' => sub("l"), '\u{2098}' => sub("m"), '\u{2099}' => sub("n"),
        '\u{209A}' => sub("p"), '\u{209B}' => sub("s"), '\u{209C}' => sub("t"),
        '\u{1D62}' => sub("i"), '\u{1D63}' => sub("r"), '\u{1D64}' => sub("u"),
        '\u{1D65}' => sub("v"), '\u{2C7C}' => sub("j"),
        _ => None,
    }
}

/// Escape a run of body text for LaTeX, normalising the Unicode the book
/// writer tends to emit so the LaTeX engine never dies on it. Mirrors
/// scribe's exporter.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if let Some(t) = super_sub(c) { out.push_str(&t); continue; }
        match c {
            '\\' => out.push_str("\\textbackslash{}"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '$' => out.push_str("\\$"),
            '&' => out.push_str("\\&"),
            '%' => out.push_str("\\%"),
            '#' => out.push_str("\\#"),
            '_' => out.push_str("\\_"),
            '^' => out.push_str("\\^{}"),
            '~' => out.push_str("\\~{}"),
            '<' => out.push_str("\\textless{}"),
            '>' => out.push_str("\\textgreater{}"),
            '\u{2014}' => out.push_str("---"),
            '\u{2013}' => out.push_str("--"),
            '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{201C}' => out.push_str("``"),
            '\u{201D}' => out.push_str("''"),
            '\u{2026}' => out.push_str("\\ldots{}"),
            '\u{2022}' => out.push_str("\\textbullet{}"),
            '\u{2192}' => out.push_str("$\\rightarrow$"),
            '\u{2190}' => out.push_str("$\\leftarrow$"),
            '\u{2713}' | '\u{2714}' => out.push_str("\\checkmark{}"),
            '\u{2717}' | '\u{2718}' | '\u{2715}' => out.push_str("$\\times$"),
            '\u{00A0}' => out.push('~'),
            '\u{00D7}' => out.push_str("$\\times$"),
            '\u{00B0}' => out.push_str("$^{\\circ}$"),
            // Scientific Unicode the writer emits in prose/equations: map to
            // math-mode LaTeX so it renders crisply regardless of the text
            // font's coverage (Greek, operators, relations, brackets).
            '\u{03B1}' => out.push_str("\\ensuremath{\\alpha}"),
            '\u{03B2}' => out.push_str("\\ensuremath{\\beta}"),
            '\u{03B3}' => out.push_str("\\ensuremath{\\gamma}"),
            '\u{03B4}' => out.push_str("\\ensuremath{\\delta}"),
            '\u{03B5}' => out.push_str("\\ensuremath{\\varepsilon}"),
            '\u{03B6}' => out.push_str("\\ensuremath{\\zeta}"),
            '\u{03B7}' => out.push_str("\\ensuremath{\\eta}"),
            '\u{03B8}' => out.push_str("\\ensuremath{\\theta}"),
            '\u{03BA}' => out.push_str("\\ensuremath{\\kappa}"),
            '\u{03BB}' => out.push_str("\\ensuremath{\\lambda}"),
            '\u{03BC}' => out.push_str("\\ensuremath{\\mu}"),
            '\u{03BD}' => out.push_str("\\ensuremath{\\nu}"),
            '\u{03BE}' => out.push_str("\\ensuremath{\\xi}"),
            '\u{03C0}' => out.push_str("\\ensuremath{\\pi}"),
            '\u{03C1}' => out.push_str("\\ensuremath{\\rho}"),
            '\u{03C3}' => out.push_str("\\ensuremath{\\sigma}"),
            '\u{03C4}' => out.push_str("\\ensuremath{\\tau}"),
            '\u{03C6}' => out.push_str("\\ensuremath{\\varphi}"),
            '\u{03C7}' => out.push_str("\\ensuremath{\\chi}"),
            '\u{03C8}' => out.push_str("\\ensuremath{\\psi}"),
            '\u{03C9}' => out.push_str("\\ensuremath{\\omega}"),
            '\u{0393}' => out.push_str("\\ensuremath{\\Gamma}"),
            '\u{0394}' => out.push_str("\\ensuremath{\\Delta}"),
            '\u{0398}' => out.push_str("\\ensuremath{\\Theta}"),
            '\u{039B}' => out.push_str("\\ensuremath{\\Lambda}"),
            '\u{039E}' => out.push_str("\\ensuremath{\\Xi}"),
            '\u{03A0}' => out.push_str("\\ensuremath{\\Pi}"),
            '\u{03A3}' => out.push_str("\\ensuremath{\\Sigma}"),
            '\u{03A6}' => out.push_str("\\ensuremath{\\Phi}"),
            '\u{03A8}' => out.push_str("\\ensuremath{\\Psi}"),
            '\u{03A9}' => out.push_str("\\ensuremath{\\Omega}"),
            '\u{2207}' => out.push_str("\\ensuremath{\\nabla}"),
            '\u{2202}' => out.push_str("\\ensuremath{\\partial}"),
            '\u{210F}' => out.push_str("\\ensuremath{\\hbar}"),
            '\u{221A}' => out.push_str("\\ensuremath{\\surd}"),
            '\u{2211}' => out.push_str("\\ensuremath{\\sum}"),
            '\u{220F}' => out.push_str("\\ensuremath{\\prod}"),
            '\u{221E}' => out.push_str("\\ensuremath{\\infty}"),
            '\u{2248}' => out.push_str("\\ensuremath{\\approx}"),
            '\u{2260}' => out.push_str("\\ensuremath{\\neq}"),
            '\u{2264}' => out.push_str("\\ensuremath{\\leq}"),
            '\u{2265}' => out.push_str("\\ensuremath{\\geq}"),
            '\u{00B1}' => out.push_str("\\ensuremath{\\pm}"),
            '\u{2212}' => out.push_str("\\ensuremath{-}"),
            '\u{2261}' => out.push_str("\\ensuremath{\\equiv}"),
            '\u{221D}' => out.push_str("\\ensuremath{\\propto}"),
            '\u{2208}' => out.push_str("\\ensuremath{\\in}"),
            '\u{21D2}' => out.push_str("\\ensuremath{\\Rightarrow}"),
            '\u{21D0}' => out.push_str("\\ensuremath{\\Leftarrow}"),
            '\u{27E8}' => out.push_str("\\ensuremath{\\langle}"),
            '\u{27E9}' => out.push_str("\\ensuremath{\\rangle}"),
            '\u{00B7}' => out.push_str("\\ensuremath{\\cdot}"),
            '\u{2032}' => out.push_str("\\ensuremath{\\prime}"),
            '\u{0124}' => out.push_str("\\ensuremath{\\hat{H}}"),
            '\u{00BD}' => out.push_str("\\textonehalf{}"),
            '\u{00BC}' => out.push_str("\\textonequarter{}"),
            '\u{00BE}' => out.push_str("\\textthreequarters{}"),
            _ => out.push(c),
        }
    }
    out
}

fn find1(chars: &[char], from: usize, c: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == c)
}
fn find2(chars: &[char], from: usize, a: char, b: char) -> Option<usize> {
    let mut i = from;
    while i + 1 < chars.len() {
        if chars[i] == a && chars[i + 1] == b { return Some(i); }
        i += 1;
    }
    None
}

/// Inline Markdown (**bold**, *italic*, `code`) → LaTeX, escaping the
/// non-marker text as it goes.
fn inline(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find2(&chars, i + 2, '*', '*') {
                out.push_str(&esc(&plain)); plain.clear();
                let inner: String = chars[i + 2..end].iter().collect();
                out.push_str(&format!("\\textbf{{{}}}", esc(&inner)));
                i = end + 2; continue;
            }
        }
        if chars[i] == '*' {
            if let Some(end) = find1(&chars, i + 1, '*') {
                out.push_str(&esc(&plain)); plain.clear();
                let inner: String = chars[i + 1..end].iter().collect();
                out.push_str(&format!("\\textit{{{}}}", esc(&inner)));
                i = end + 1; continue;
            }
        }
        if chars[i] == '`' {
            if let Some(end) = find1(&chars, i + 1, '`') {
                out.push_str(&esc(&plain)); plain.clear();
                let inner: String = chars[i + 1..end].iter().collect();
                out.push_str(&format!("\\texttt{{{}}}", esc(&inner)));
                i = end + 1; continue;
            }
        }
        plain.push(chars[i]);
        i += 1;
    }
    out.push_str(&esc(&plain));
    out
}

/// Build the LaTeX source for a book. `img_dir` is the absolute path to
/// the book's `img/` folder (figures are referenced by absolute path so
/// xelatex finds them from its temp working dir).
fn to_latex(md: &str, title: &str, img_dir: &Path) -> String {
    let mut s = String::new();
    s.push_str("\\documentclass[11pt,a4paper]{article}\n");
    s.push_str("\\usepackage[margin=2.4cm]{geometry}\n");
    // XeLaTeX: native UTF-8. fontspec defaults to Latin Modern Roman, so the
    // look is unchanged, but any unmapped Unicode a real book carries becomes
    // a skipped glyph (a warning) instead of a fatal error.
    s.push_str("\\usepackage{fontspec}\n");
    s.push_str("\\usepackage{textcomp}\n");
    s.push_str("\\usepackage{amssymb}\n");
    s.push_str("\\usepackage{microtype}\n");
    s.push_str("\\usepackage{graphicx}\n");
    s.push_str("\\usepackage{parskip}\n");
    s.push_str("\\usepackage{ragged2e}\n");
    s.push_str("\\linespread{1.08}\n");
    s.push_str("\\setlength{\\emergencystretch}{2em}\n");
    s.push_str("\\pagestyle{plain}\n");
    s.push_str("\\begin{document}\n");
    // Title block.
    s.push_str(&format!(
        "\\begin{{center}}{{\\LARGE\\bfseries {}}}\\end{{center}}\n\\vspace{{1.2em}}\n",
        inline(title)));

    let lines: Vec<&str> = md.lines().collect();
    let mut i = 0;
    let first_h1_seen = &mut false;
    while i < lines.len() {
        let raw = lines[i];
        let line = raw.trim_end();
        let t = line.trim();
        // Figure marker.
        if let Some(inner) = t.strip_prefix("[[FIG").and_then(|r| r.strip_suffix("]]")) {
            let inner = inner.trim();
            let (n_str, caption) = match inner.split_once(':') {
                Some((n, c)) => (n.trim(), c.trim()),
                None => (inner, ""),
            };
            if let Ok(n) = n_str.parse::<usize>() {
                let png = img_dir.join(format!("fig{}.png", n));
                if png.exists() {
                    s.push_str("\\begin{center}\n");
                    s.push_str(&format!(
                        "\\includegraphics[width=0.8\\linewidth,height=0.32\\textheight,keepaspectratio]{{{}}}\\\\[4pt]\n",
                        png.display()));
                    if !caption.is_empty() {
                        s.push_str(&format!("{{\\small\\itshape {}}}\n", inline(caption)));
                    }
                    s.push_str("\\end{center}\n");
                }
                i += 1;
                continue;
            }
        }
        if let Some(h) = line.strip_prefix("# ") {
            // First # is the title (already shown); later ones become sections.
            if !*first_h1_seen {
                *first_h1_seen = true;
            } else {
                s.push_str(&format!("\\section*{{{}}}\n", inline(h.trim())));
            }
        } else if let Some(h) = line.strip_prefix("## ") {
            s.push_str(&format!("\\section*{{{}}}\n", inline(h.trim())));
        } else if let Some(h) = line.strip_prefix("### ") {
            s.push_str(&format!("\\subsection*{{{}}}\n", inline(h.trim())));
        } else if line.starts_with("> ") {
            // Gather consecutive quote lines into one block.
            let mut q = String::new();
            while i < lines.len() && lines[i].trim_start().starts_with("> ") {
                let qt = lines[i].trim_start().trim_start_matches("> ");
                if !q.is_empty() { q.push(' '); }
                q.push_str(qt.trim());
                i += 1;
            }
            s.push_str(&format!("\\begin{{quote}}\\itshape {}\\end{{quote}}\n", inline(&q)));
            continue;
        } else if t.is_empty() {
            s.push('\n');
        } else if let Some(b) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            s.push_str(&format!("\\noindent\\textbullet\\ {}\\par\n", inline(b.trim())));
        } else {
            s.push_str(&inline(line));
            s.push_str("\n\n");
        }
        i += 1;
    }
    s.push_str("\\end{document}\n");
    s
}

fn safe_filename(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('-'),
            _ => out.push(c),
        }
    }
    let trimmed = out.trim().trim_end_matches('.').to_string();
    if trimmed.is_empty() { "book".into() } else { trimmed.chars().take(120).collect() }
}

/// Export a book to a PDF beside its source. Returns the PDF path.
pub fn export_book_pdf(id: &str, title: &str, md: &str) -> Result<PathBuf, String> {
    let img_dir = store::book_img_dir(id);
    let latex = to_latex(md, title, &img_dir);
    let target = store::book_dir(id).join(format!("{}.pdf", safe_filename(title)));
    latex_to_pdf(&latex, &target)?;
    Ok(target)
}

/// Compile `latex` into a PDF at `target` using `xelatex`, in a private
/// temp dir. Two passes so headings/refs settle. Returns the first LaTeX
/// error line on failure.
fn latex_to_pdf(latex: &str, target: &Path) -> Result<(), String> {
    use std::process::Command;
    let tmp = std::env::temp_dir().join(format!("library-pdf-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("temp dir: {}", e))?;
    let tex = tmp.join("doc.tex");
    std::fs::write(&tex, latex).map_err(|e| format!("write .tex: {}", e))?;
    for _ in 0..2 {
        let run = Command::new("xelatex")
            .arg("-interaction=nonstopmode")
            .arg("-halt-on-error")
            .arg("-output-directory").arg(&tmp)
            .arg(&tex)
            .current_dir(&tmp)
            .output();
        if let Err(e) = run {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("xelatex not available ({})", e));
        }
    }
    let pdf = tmp.join("doc.pdf");
    let result = if pdf.exists() {
        std::fs::copy(&pdf, target).map(|_| ()).map_err(|e| format!("copy pdf: {}", e))
    } else {
        let log = std::fs::read_to_string(tmp.join("doc.log")).unwrap_or_default();
        let err = log.lines().find(|l| l.starts_with('!'))
            .map(|l| l.trim_start_matches("! ").to_string())
            .unwrap_or_else(|| "see LaTeX log".to_string());
        Err(err)
    };
    let _ = std::fs::remove_dir_all(&tmp);
    result
}
