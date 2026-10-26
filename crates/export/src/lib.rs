//! pdfcraft-export — Export a PDF ▸ Word, HTML, RTF (L4).
//!
//! The engine reduces each page to [`Page`]: paragraphs (text, box, size, bold/italic) and
//! images (encoded bytes and box), in reading order. The writers turn that into a flowing
//! document: headings are the paragraphs set larger than the body text, and a block that
//! continues another on its line (an italic word, a link) is a run of that block's paragraph.
//! Word keeps pages with
//! rotated text, or text overlapped by artwork or watermark-sized text, as positioned, editable
//! shapes to avoid reflowing overlays; other pages flow with images inline.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::collections::HashMap;

mod pptx;
mod zip;

pub use pptx::pptx;
pub use zip::Zip;

/// A paragraph of a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub text: String,
    /// [x0, y0, x1, y1] on the displayed page in points (y up).
    pub rect: [f64; 4],
    /// Font size in points.
    pub size: f64,
    pub bold: bool,
    pub italic: bool,
    /// Text fill colour as sRGB components in 0..=1 (black when unknown).
    pub color: [f64; 3],
    /// Counterclockwise baseline angle in user space (degrees).
    pub rotation: f64,
    /// Whether the source had a word space just before (`[0]`) and just after (`[1]`) `text`,
    /// which `text` leaves out. A block that continues this one on its line is a run of the same
    /// paragraph, and these say whether a space separates them (#4158).
    pub space_around: [bool; 2],
}

/// An image of a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    /// "png" or "jpg".
    pub ext: &'static str,
    /// Shared: a picture drawn in many places is held once.
    pub bytes: std::sync::Arc<[u8]>,
    pub rect: [f64; 4],
    /// Width and height before rotation, in points.
    pub size: [f64; 2],
    /// Counterclockwise rotation in user space (degrees).
    pub rotation: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Page {
    pub width: f64,
    pub height: f64,
    pub blocks: Vec<Block>,
    pub images: Vec<Image>,
    /// The colour the whole page is painted before its content (a page background), as sRGB
    /// components in 0..=1; `None` for a page with no background of its own.
    pub background: Option<[f64; 3]>,
}

/// One cell of a detected table. `span` is how many grid columns the cell covers (a header
/// row that stretches over sub-columns).
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub text: String,
    pub size: f64,
    pub bold: bool,
    pub italic: bool,
    /// Text fill colour as sRGB components in 0..=1.
    pub color: [f64; 3],
    pub span: usize,
}

/// A table detected from the page's text layout: rows of cells, and the left edge of each
/// grid column (user space, y up) so the writers can size the columns.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub rect: [f64; 4],
    pub cols: Vec<f64>,
    pub rows: Vec<Vec<Cell>>,
}

/// What goes into the output, in order.
enum Item<'a> {
    /// A paragraph's runs (at least one) and its heading level.
    Para(Vec<Run<'a>>, u8),
    /// Owned: tables are detected per page inside [`items`], so they cannot borrow.
    Table(Table),
    Img(&'a Image),
    PageBreak,
}

#[cfg(test)]
thread_local! {
    /// Entries read to find a font size's or a colour's slot, for the tests that keep it constant.
    static LOOKUP_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Counts `n` entries read by a lookup (tests only).
#[inline]
fn count_lookup_steps(_n: usize) {
    #[cfg(test)]
    LOOKUP_STEPS.with(|c| c.set(c.get().saturating_add(_n)));
}

/// The body text size: the size most of the document's characters are set in.
fn body_size(pages: &[Page]) -> f64 {
    // Sizes in first-seen order (which breaks ties), and where each one is in it.
    let mut by: Vec<(i64, usize)> = Vec::new();
    let mut slot: HashMap<i64, usize> = HashMap::new();
    for b in pages.iter().flat_map(|p| &p.blocks) {
        let k = (b.size * 2.0).round() as i64;
        count_lookup_steps(1);
        let i = *slot.entry(k).or_insert_with(|| {
            by.push((k, 0));
            by.len() - 1
        });
        if let Some(e) = by.get_mut(i) {
            e.1 = e.1.saturating_add(b.text.len());
        }
    }
    by.into_iter().max_by_key(|e| e.1).map_or(11.0, |e| e.0 as f64 / 2.0)
}

/// Heading level for a block: 1 for ≥ 1.6× the body size, 2 for ≥ 1.25×, 0 for body text.
fn level(b: &Block, body: f64) -> u8 {
    let short = b.text.len() < 200;
    if short && b.size >= body * 1.6 {
        1
    } else if short && (b.size >= body * 1.25 || (b.bold && b.size >= body && b.text.len() < 80 && !b.text.ends_with('.'))) {
        2
    } else {
        0
    }
}

/// How far two cell left edges may drift (points) and still be the same grid column.
const COL_TOL: f64 = 4.0;
/// More grid columns than this isn't a table. It is Word's limit: a .docx whose table has 64 or
/// more columns doesn't open. It also bounds a table's size: every row is materialized to all
/// its columns, so a hostile page laid out as a staircase of blocks would otherwise make
/// (blocks / 2)² cells.
const MAX_COLS: usize = 63;
/// Two blocks in a row this close (as a fraction of their font size) are one line of running
/// text that changed font or colour, such as a link inside a sentence, not two table cells.
const INLINE_GAP: f64 = 0.5;
/// How many blocks ending near a block's start are looked at to find the one it continues; a
/// page laid out so that many end at the same place stays quick.
const CONTINUATION_PROBE: usize = 64;
/// Per block, whether it continues another on the same line: it starts where that one ends (up to
/// [`INLINE_GAP`] later, or a point earlier for rounding), so it is the same line of text running
/// on (a link inside a sentence, a superscript), not a cell of its own.
fn continuations(blocks: &[Block]) -> Vec<bool> {
    continued_from(blocks).into_iter().map(|from| from.is_some()).collect()
}

/// Per block, the block it continues on the same line (see [`continuations`]): of those it could
/// continue, the one that ends nearest to where it starts.
fn continued_from(blocks: &[Block]) -> Vec<Option<usize>> {
    let mut by_end: Vec<(f64, usize)> = blocks.iter().enumerate().map(|(i, b)| (b.rect[2], i)).collect();
    by_end.sort_by(|a, b| a.0.total_cmp(&b.0));
    blocks
        .iter()
        .enumerate()
        .map(|(bi, b)| {
            let k = by_end.partition_point(|(end, _)| *end < b.rect[0] - INLINE_GAP * b.size);
            by_end
                .get(k..)
                .unwrap_or_default()
                .iter()
                .take_while(|(end, _)| *end <= b.rect[0] + 1.0)
                .take(CONTINUATION_PROBE)
                .filter_map(|(_, ai)| blocks.get(*ai).filter(|_| *ai != bi).map(|a| (*ai, a)))
                .filter(|(_, a)| a.rect[3].min(b.rect[3]) - a.rect[1].max(b.rect[1]) > 0.5 * (a.rect[3] - a.rect[1]).min(b.rect[3] - b.rect[1]))
                .min_by(|(_, a), (_, c)| (a.rect[2] - b.rect[0]).abs().total_cmp(&(c.rect[2] - b.rect[0]).abs()))
                .map(|(ai, _)| ai)
        })
        .collect()
}

/// A gap between two runs wider than this (as a fraction of the font size) is a word space the
/// text left out. It is the gap the reading-block grouping uses for the same purpose.
const WORD_GAP: f64 = 0.12;

/// One run of a paragraph: a block, and the text it contributes (its own, plus the word space
/// before the next run when the source had one there or the page shows a gap, but the text has
/// none).
pub(crate) struct Run<'a> {
    pub(crate) block: &'a Block,
    pub(crate) text: String,
}

/// A page's paragraphs, each a list of runs from left to right along its line. A block that
/// continues another on the same line ([`continued_from`]: an italic case name, a link, a
/// superscript) is a run in its own style inside that block's paragraph, not a paragraph of its
/// own (#4158). Blocks marked in `skip` (table cells, a slide title) are left out and join
/// nothing. Paragraphs come in the order of their first runs in `blocks`.
pub(crate) fn paragraphs<'a>(blocks: &'a [Block], skip: &[bool]) -> Vec<Vec<Run<'a>>> {
    let skipped = |i: usize| skip.get(i).copied().unwrap_or(false);
    let from = continued_from(blocks);
    // Left to right. A block joins only one that starts strictly further left (in `total_cmp`'s
    // order, NaN included), so that one is visited first and has its paragraph already, and the
    // links cannot form a cycle.
    let mut order: Vec<(f64, usize)> = blocks.iter().enumerate().filter(|(i, _)| !skipped(*i)).map(|(i, b)| (b.rect[0], i)).collect();
    order.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    // Each block's paragraph, named by its first block, and each paragraph's blocks in order.
    let mut head: Vec<usize> = (0..blocks.len()).collect();
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); blocks.len()];
    for &(x0, bi) in &order {
        let Some(b) = blocks.get(bi) else { continue };
        let joined = from
            .get(bi)
            .copied()
            .flatten()
            .filter(|ai| !skipped(*ai))
            .filter(|ai| blocks.get(*ai).is_some_and(|a| a.rect[0].total_cmp(&x0).is_lt() && (a.rotation - b.rotation).abs() <= 0.1))
            .and_then(|ai| head.get(ai).copied());
        let h = joined.unwrap_or(bi);
        if let Some(slot) = head.get_mut(bi) {
            *slot = h;
        }
        if let Some(m) = members.get_mut(h) {
            m.push(bi);
        }
    }
    members
        .into_iter()
        .filter(|m| !m.is_empty())
        .map(|m| {
            let mut runs: Vec<Run<'a>> = Vec::with_capacity(m.len());
            for b in m.into_iter().filter_map(|bi| blocks.get(bi)) {
                if let Some(prev) = runs.last_mut()
                    && !prev.text.ends_with(char::is_whitespace)
                    && !b.text.starts_with(char::is_whitespace)
                    && (prev.block.space_around[1] || b.space_around[0] || b.rect[0] - prev.block.rect[2] > b.size * WORD_GAP)
                {
                    prev.text.push(' ');
                }
                runs.push(Run { block: b, text: b.text.clone() });
            }
            runs
        })
        .collect()
}

/// A paragraph's heading level: the lowest of its runs', so body text that runs on after a bold
/// lead-in on the same line keeps the paragraph body text. A marker set smaller than the first run
/// (a footnote number, "st" in "1st") does not count, so a heading keeps its footnote marker.
fn para_level(runs: &[Run], body: f64) -> u8 {
    let Some(first) = runs.first() else { return 0 };
    let marker = |r: &Run| r.block.size < first.block.size * 0.9 && r.block.text.trim().chars().count() <= 3;
    runs.iter().filter(|r| !marker(r)).map(|r| level(r.block, body)).min().unwrap_or(0)
}

/// A cell is short: taller blocks are body text (or a multi-column layout), not table cells.
fn is_cell_like(b: &Block) -> bool {
    (b.rect[3] - b.rect[1]) <= b.size * 5.0 && !b.text.trim().is_empty()
}

/// Detect tables in a page's blocks from their text layout: columns are left edges shared by
/// several short blocks, rows are blocks whose vertical spans overlap. Returns the tables and,
/// per block, whether a table consumed it (so `items` can drop it from the paragraph flow).
pub(crate) fn tables(blocks: &[Block]) -> (Vec<Table>, Vec<bool>) {
    let mut consumed = vec![false; blocks.len()];
    // Candidate columns: cluster the left edges of cell-like blocks.
    let mut xs: Vec<(f64, usize)> = blocks.iter().enumerate().filter(|(_, b)| is_cell_like(b)).map(|(i, b)| (b.rect[0], i)).collect();
    xs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut cols: Vec<(f64, Vec<usize>)> = Vec::new(); // (left edge, member block indices)
    for (x, i) in xs {
        match cols.last_mut() {
            Some((e, m)) if (x - *e).abs() <= COL_TOL => {
                *e = (*e * m.len() as f64 + x) / (m.len() + 1) as f64;
                m.push(i);
            }
            _ => cols.push((x, vec![i])),
        }
    }
    let cols: Vec<(f64, Vec<usize>)> = cols.into_iter().filter(|(_, m)| m.len() >= 2).collect();
    if cols.len() < 2 {
        return (Vec::new(), consumed);
    }
    // Each block's column (an index into `cols`), so lookups stay linear on pages with many blocks.
    let mut col_of: Vec<Option<usize>> = vec![None; blocks.len()];
    for (ci, (_, members)) in cols.iter().enumerate() {
        for &i in members {
            if let Some(c) = col_of.get_mut(i) {
                *c = Some(ci);
            }
        }
    }
    // The column whose edge is within tolerance of `x` (edges are in ascending order).
    let column_at = |x: f64| -> Option<usize> {
        let k = cols.partition_point(|(e, _)| *e < x - COL_TOL);
        cols.get(k).filter(|(e, _)| (*e - x).abs() <= COL_TOL).map(|_| k)
    };
    // Assign every cell-like block to its nearest column (within tolerance).
    let mut rows: Vec<Vec<(usize, usize)>> = Vec::new(); // (column index, block index), top to bottom
    let mut order: Vec<usize> = (0..blocks.len()).collect();
    order.sort_by(|a, b| blocks[*b].rect[3].total_cmp(&blocks[*a].rect[3]).then(blocks[*a].rect[0].total_cmp(&blocks[*b].rect[0])));
    for &bi in &order {
        let b = &blocks[bi];
        if !is_cell_like(b) {
            continue;
        }
        let Some(ci) = column_at(b.rect[0]) else { continue };
        let joins = rows
            .last()
            .and_then(|row| row.first())
            .map(|(_, fi)| {
                let rb = &blocks[*fi];
                let overlap = rb.rect[3].min(b.rect[3]) - rb.rect[1].max(b.rect[1]);
                overlap > 0.5 * (rb.rect[3] - rb.rect[1]).min(b.rect[3] - b.rect[1])
            })
            .unwrap_or(false);
        if joins {
            if let Some(row) = rows.last_mut() {
                row.push((ci, bi));
            }
        } else {
            rows.push(vec![(ci, bi)]);
        }
    }
    // Runs of consecutive rows that have two or more distinct columns are tables.
    let mut out = Vec::new();
    let flush = |run: &mut Vec<Vec<(usize, usize)>>, out: &mut Vec<Table>, consumed: &mut Vec<bool>| {
        if run.len() < 2 {
            run.clear();
            return;
        }
        let mut used: Vec<usize> = run.iter().flatten().map(|(_, i)| *i).collect();
        used.sort_unstable();
        used.dedup();
        let mut edges: Vec<f64> = used.iter().filter_map(|&i| col_of.get(i).copied().flatten()).filter_map(|ci| cols.get(ci)).map(|c| c.0).collect();
        edges.sort_by(f64::total_cmp);
        edges.dedup_by(|a, b| (*a - *b).abs() <= COL_TOL);
        let ncols = edges.len();
        let ok = (ncols >= 3 || (ncols == 2 && run.len() >= 3)) && ncols <= MAX_COLS;
        if !ok {
            run.clear();
            return;
        }
        let right = used.iter().map(|&i| blocks[i].rect[2]).fold(f64::MIN, f64::max) + COL_TOL;
        let mut rows_out = Vec::new();
        let mut slots: Vec<Option<(Cell, Vec<usize>)>> = vec![None; ncols];
        for row in run.drain(..) {
            for slot in slots.iter_mut() {
                *slot = None;
            }
            for (_ci, bi) in row {
                let b = &blocks[bi];
                let Some(slot) = edges.iter().position(|e| (*e - b.rect[0]).abs() <= COL_TOL) else { continue };
                let span = edges[slot + 1..].iter().filter(|e| **e > b.rect[0] + COL_TOL && **e < b.rect[2] - COL_TOL).count() + 1;
                let cell = Cell { text: b.text.trim().to_string(), size: b.size, bold: b.bold, italic: b.italic, color: b.color, span };
                match slots.get_mut(slot) {
                    Some(Some((c, sources))) => {
                        c.text.push(' ');
                        c.text.push_str(&cell.text);
                        sources.push(bi);
                    }
                    Some(empty @ None) => *empty = Some((cell, vec![bi])),
                    None => continue,
                }
            }
            let mut materialized = Vec::with_capacity(ncols);
            let mut column = 0;
            while column < ncols {
                match slots.get_mut(column).and_then(Option::take) {
                    Some((mut cell, sources)) => {
                        // A spanning cell can skip later slots. Only consume source blocks
                        // when their text is emitted, leaving covered text in paragraph flow.
                        for bi in sources {
                            if let Some(taken) = consumed.get_mut(bi) {
                                *taken = true;
                            }
                        }
                        cell.span = cell.span.min(ncols - column).max(1);
                        column += cell.span;
                        materialized.push(cell);
                    }
                    None => {
                        column += 1;
                        materialized.push(Cell { text: String::new(), size: 11.0, bold: false, italic: false, color: [0.0; 3], span: 1 });
                    }
                }
            }
            while materialized.last().is_some_and(|cell| cell.text.is_empty()) && materialized.iter().any(|cell| cell.span > 1) {
                materialized.pop();
            }
            rows_out.push(materialized);
        }
        let rect = [
            edges[0],
            used.iter().map(|&i| blocks[i].rect[1]).fold(f64::MAX, f64::min),
            right,
            used.iter().map(|&i| blocks[i].rect[3]).fold(f64::MIN, f64::max),
        ];
        out.push(Table { rect, cols: edges, rows: rows_out });
    };
    // Runs of consecutive multi-column rows are tables. Rows on either side of a run that
    // hold a single cell wide enough to span two columns (a header band, a totals row) join
    // the table; other single-column rows (captions, following text) stay paragraphs.
    let spans_run = |edges: &[f64], bi: usize| -> bool {
        let b = &blocks[bi];
        edges.iter().any(|e| (*e - b.rect[0]).abs() <= COL_TOL) && edges.iter().any(|e| *e > b.rect[0] + COL_TOL && *e < b.rect[2] - COL_TOL)
    };
    let run_edges = |run: &[Vec<(usize, usize)>]| -> Vec<f64> {
        let mut es: Vec<f64> =
            run.iter().flatten().filter_map(|(_, i)| col_of.get(*i).copied().flatten()).filter_map(|ci| cols.get(ci)).map(|c| c.0).collect();
        es.sort_by(f64::total_cmp);
        es.dedup_by(|a, b| (*a - *b).abs() <= COL_TOL);
        es
    };
    let cont = continuations(blocks);
    let qual: Vec<bool> = rows
        .iter()
        .map(|row| {
            // A block that continues another adds no column of its own, and the cells of a table row
            // sit side by side: blocks that overlap are a paragraph's lines, not cells (#2806).
            let cells: Vec<(usize, usize)> = row.iter().copied().filter(|(_, bi)| !cont.get(*bi).copied().unwrap_or(false)).collect();
            let mut spans: Vec<[f64; 2]> = cells.iter().filter_map(|(_, bi)| blocks.get(*bi)).map(|b| [b.rect[0], b.rect[2]]).collect();
            spans.sort_by(|a, b| a[0].total_cmp(&b[0]));
            let apart = spans.windows(2).all(|w| w[1][0] >= w[0][1] - COL_TOL);
            let mut cs: Vec<usize> = cells.iter().map(|(c, _)| *c).collect();
            cs.sort_unstable();
            cs.dedup();
            apart && cs.len() >= 2
        })
        .collect();
    let mut i = 0;
    while i < rows.len() {
        if !qual[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && qual[i] {
            i += 1;
        }
        let end = i; // run = rows[start..end]
        let mut pieces: Vec<Vec<(usize, usize)>> = Vec::new();
        if start > 0 && rows[start - 1].len() == 1 && spans_run(&run_edges(&rows[start..end]), rows[start - 1][0].1) {
            pieces.push(rows[start - 1].clone());
        }
        pieces.extend(rows[start..end].iter().cloned());
        if end < rows.len() && rows[end].len() == 1 && spans_run(&run_edges(&rows[start..end]), rows[end][0].1) {
            pieces.push(rows[end].clone());
        }
        flush(&mut pieces, &mut out, &mut consumed);
    }
    (out, consumed)
}

fn items(pages: &[Page]) -> Vec<Item<'_>> {
    let body = body_size(pages);
    let mut out = Vec::new();
    for (i, p) in pages.iter().enumerate() {
        if i > 0 {
            out.push(Item::PageBreak);
        }
        let (tables, consumed) = tables(&p.blocks);
        // Blocks and images by their top edge, top to bottom.
        // A paragraph sits where its first run does: its other runs follow along the same line.
        let mut parts: Vec<(f64, f64, Item)> = paragraphs(&p.blocks, &consumed)
            .into_iter()
            .filter_map(|runs| {
                let first = runs.first()?.block;
                let lvl = para_level(&runs, body);
                Some((first.rect[3], first.rect[0], Item::Para(runs, lvl)))
            })
            .collect();
        parts.extend(tables.into_iter().map(|t| (t.rect[3], t.rect[0], Item::Table(t))));
        parts.extend(p.images.iter().map(|im| (im.rect[3], im.rect[0], Item::Img(im))));
        parts.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.total_cmp(&b.1)));
        out.extend(parts.into_iter().map(|x| x.2));
    }
    out
}

/// Whether a character may appear in the output. XML 1.0 forbids the C0 controls other than tab,
/// line feed and carriage return, and U+FFFE/U+FFFF. Text extracted from PDFs often holds them
/// (fonts without a usable `/ToUnicode`), and Word refuses a whole document over one (#72).
fn allowed(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || !(c.is_ascii_control() || c == '\u{FFFE}' || c == '\u{FFFF}')
}

pub(crate) fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars().filter(|c| allowed(*c)) {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            c => o.push(c),
        }
    }
    o
}

fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(A[(n >> 18) as usize & 63] as char);
        s.push(A[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    s
}

/// One HTML file: headings and paragraphs, tables as real `<table>` elements, images inline, a
/// rule between pages.
pub fn html(pages: &[Page], title: &str) -> String {
    let mut s = format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>{}</title>\n<style>body{{max-width:46em;margin:2em auto;font-family:sans-serif;line-height:1.45}}\
img{{max-width:100%}}hr{{border:0;border-top:1px solid #ccc;margin:2em 0}}\
table{{border-collapse:collapse;margin:1em 0}}td,th{{border:1px solid #999;padding:2px 6px;vertical-align:top}}</style>\n</head>\n<body>\n",
        esc(title)
    );
    for it in items(pages) {
        match it {
            Item::Para(runs, lvl) => {
                let Some(first) = runs.first().map(|r| r.block) else { continue };
                let mut t = String::new();
                for r in &runs {
                    let mut run = esc(&r.text);
                    if r.block.italic {
                        run = format!("<em>{run}</em>");
                    }
                    if r.block.bold && lvl == 0 {
                        run = format!("<strong>{run}</strong>");
                    }
                    // The paragraph has its first run's colour; a run in another colour says so.
                    let colour = rgb8(r.block.color);
                    if colour != rgb8(first.color) {
                        let [red, green, blue] = colour;
                        run = format!("<span style=\"color:#{red:02X}{green:02X}{blue:02X}\">{run}</span>");
                    }
                    t.push_str(&run);
                }
                let style = hex(first.color).map_or(String::new(), |h| format!(" style=\"color:#{h}\""));
                match lvl {
                    0 => s.push_str(&format!("<p{style}>{t}</p>\n")),
                    l => s.push_str(&format!("<h{l}{style}>{t}</h{l}>\n")),
                }
            }
            Item::Table(t) => {
                s.push_str("<table>\n");
                for row in &t.rows {
                    s.push_str("<tr>");
                    for c in row {
                        let mut body = esc(&c.text);
                        if c.italic {
                            body = format!("<em>{body}</em>");
                        }
                        if c.bold {
                            body = format!("<strong>{body}</strong>");
                        }
                        let span = if c.span > 1 { format!(" colspan=\"{}\"", c.span) } else { String::new() };
                        let style = hex(c.color).map_or(String::new(), |h| format!(" style=\"color:#{h}\""));
                        s.push_str(&format!("<td{span}{style}>{body}</td>"));
                    }
                    s.push_str("</tr>\n");
                }
                s.push_str("</table>\n");
            }
            Item::Img(im) => {
                let mime = if im.ext == "jpg" { "image/jpeg" } else { "image/png" };
                s.push_str(&format!("<p><img alt=\"\" src=\"data:{mime};base64,{}\"></p>\n", base64(&im.bytes)));
            }
            Item::PageBreak => s.push_str("<hr>\n"),
        }
    }
    s.push_str("</body>\n</html>\n");
    s
}

/// A colour's 8-bit sRGB components (NaN and out-of-range values clamp; NaN reads as 0).
pub(crate) fn rgb8(c: [f64; 3]) -> [u8; 3] {
    // The value is clamped to 0..=255 before the cast, so it cannot truncate.
    c.map(|v| if v.is_finite() { (v.clamp(0.0, 1.0) * 255.0).round() as u8 } else { 0 })
}

/// The colour as `RRGGBB`, or `None` for black (the default every format already uses).
pub(crate) fn hex(c: [f64; 3]) -> Option<String> {
    let [r, g, b] = rgb8(c);
    (r | g | b != 0).then(|| format!("{r:02X}{g:02X}{b:02X}"))
}

/// A formatted text run (shared by paragraphs and table cells).
fn run_xml(text: &str, size: f64, bold: bool, italic: bool, color: [f64; 3]) -> String {
    let mut rpr = String::new();
    if bold {
        rpr.push_str("<w:b/>");
    }
    if italic {
        rpr.push_str("<w:i/>");
    }
    if let Some(h) = hex(color) {
        rpr.push_str(&format!("<w:color w:val=\"{h}\"/>"));
    }
    let half_points = if size.is_finite() { (size * 2.0).round().clamp(2.0, 3276.0) as i64 } else { 24 };
    rpr.push_str(&format!("<w:sz w:val=\"{half_points}\"/>"));
    format!("<w:r><w:rPr>{rpr}</w:rPr><w:t xml:space=\"preserve\">{}</w:t></w:r>", esc(text))
}

/// One Word table: explicit absent borders (alignment alone cannot prove PDF rules), a grid sized
/// from the detected column edges, `gridSpan` for spanning cells, empty cells for holes.
fn docx_table(t: &Table) -> String {
    let mut edges = t.cols.clone();
    edges.push(t.rect[2]);
    let widths: Vec<i64> = edges.windows(2).map(|w| ((w[1] - w[0]).max(20.0) * 20.0).round().clamp(60.0, 31680.0) as i64).collect();
    let mut s = String::from("<w:tbl><w:tblPr><w:tblW w:w=\"0\" w:type=\"auto\"/><w:tblBorders>");
    for edge in ["top", "left", "bottom", "right", "insideH", "insideV"] {
        s.push_str(&format!("<w:{edge} w:val=\"nil\"/>"));
    }
    s.push_str("</w:tblBorders></w:tblPr><w:tblGrid>");
    for w in &widths {
        s.push_str(&format!("<w:gridCol w:w=\"{w}\"/>"));
    }
    s.push_str("</w:tblGrid>");
    for row in &t.rows {
        s.push_str("<w:tr>");
        let mut column = 0usize;
        for c in row {
            let span = if c.span > 1 { format!("<w:gridSpan w:val=\"{}\"/>", c.span) } else { String::new() };
            let w: i64 = widths.iter().skip(column).take(c.span).sum();
            s.push_str(&format!(
                "<w:tc><w:tcPr><w:tcW w:w=\"{w}\" w:type=\"dxa\"/>{span}</w:tcPr><w:p>{}</w:p></w:tc>",
                run_xml(&c.text, c.size, c.bold, c.italic, c.color)
            ));
            column = column.saturating_add(c.span);
        }
        // Pad the grid so every row covers all columns (Word rejects short rows).
        for _ in column..t.cols.len() {
            s.push_str("<w:tc><w:tcPr><w:tcW w:w=\"0\" w:type=\"auto\"/></w:tcPr><w:p/></w:tc>");
        }
        s.push_str("</w:tr>");
    }
    s.push_str("</w:tbl>");
    s
}

/// Pages with rotated text, or text that actually overlaps an image or an oversized
/// (watermark-like) text block, cannot be put in paragraph flow without changing their layout.
/// Keep those pages as editable, absolutely positioned text boxes. Every other page, including
/// one with a logo or a big title that sits clear of the text, flows as paragraphs and tables.
fn positioned(page: &Page) -> bool {
    if page.blocks.iter().any(|b| b.rotation.abs() > 0.1) {
        return true;
    }
    let body = body_size(std::slice::from_ref(page)).max(1.0);
    let overlays = page
        .images
        .iter()
        .map(|im| (im.rect, None))
        .chain(page.blocks.iter().enumerate().filter(|(_, b)| b.size >= 2.0 * body).map(|(i, b)| (b.rect, Some(i))));
    // Bound the pairwise test on hostile pages (many overlays times many blocks): past the
    // budget, keep the default flowing layout.
    let mut budget: usize = 1_000_000;
    for (r, own) in overlays {
        for (i, b) in page.blocks.iter().enumerate() {
            if budget == 0 {
                return false;
            }
            budget -= 1;
            if Some(i) != own && overlaps(r, b.rect) {
                return true;
            }
        }
    }
    false
}

/// Whether two [x0, y0, x1, y1] boxes share more than a hairline (0.5 pt each way); boxes that
/// only touch, or hostile non-finite coordinates, do not count.
fn overlaps(a: [f64; 4], b: [f64; 4]) -> bool {
    let w = a[2].min(b[2]) - a[0].max(b[0]);
    let h = a[3].min(b[3]) - a[1].max(b[1]);
    w.is_finite() && h.is_finite() && w > 0.5 && h > 0.5
}

/// Bound every value written into a shape's style, including hostile PDF coordinates.
fn layout_number(n: f64) -> f64 {
    if n.is_finite() { n.clamp(-1584.0, 1584.0) } else { 0.0 }
}

/// Most images a Word export embeds, and most image bytes (the zip writer has 32-bit sizes and
/// a 16-bit entry count; a hostile PDF must not be able to overflow them).
const MAX_MEDIA: usize = 1000;
const MAX_MEDIA_BYTES: usize = 512 << 20;

/// Whether `im` can still be embedded: it has data, and the package stays within its limits.
fn room_for(media: &[(String, &Image)], im: &Image) -> bool {
    !im.bytes.is_empty()
        && media.len() < MAX_MEDIA
        && media.iter().map(|(_, m)| m.bytes.len()).fold(im.bytes.len(), usize::saturating_add) <= MAX_MEDIA_BYTES
}

/// Points as EMU (12700 per point); non-finite values are 0.
fn emu(pt: f64) -> i64 {
    if pt.is_finite() { (pt * 12700.0).round() as i64 } else { 0 }
}

/// The `a:graphic` of one picture (DrawingML), rotated by `rotation` degrees when given.
fn picture_xml(n: usize, name: &str, (cx, cy): (i64, i64), rotation: Option<f64>) -> String {
    let rot = rotation.map_or(String::new(), |r| format!(" rot=\"{}\"", (r * 60000.0).round() as i64));
    format!(
        "<wp:docPr id=\"{n}\" name=\"Picture {n}\"/><wp:cNvGraphicFramePr/>\
<a:graphic xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:pic xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><pic:nvPicPr><pic:cNvPr id=\"{n}\" name=\"{name}\"/><pic:cNvPicPr/></pic:nvPicPr>\
<pic:blipFill><a:blip r:embed=\"rIdImg{n}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
<pic:spPr><a:xfrm{rot}><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic>\
</a:graphicData></a:graphic>"
    )
}

fn positioned_page<'a>(page: &'a Page, media: &mut Vec<(String, &'a Image)>, shape_id: &mut usize) -> String {
    // One 1 pt paragraph anchors the whole page. Shapes never reserve body space, so even a
    // page-sized watermark cannot push the invoice footer onto another page.
    let mut s = String::from("<w:p><w:pPr><w:spacing w:before=\"0\" w:after=\"0\" w:line=\"20\" w:lineRule=\"exact\"/></w:pPr>");
    let body = body_size(std::slice::from_ref(page)).max(1.0);
    for b in &page.blocks {
        *shape_id = shape_id.saturating_add(1);
        let rotation = layout_number(-b.rotation).rem_euclid(360.0);
        let (sin, cos) = rotation.to_radians().sin_cos();
        let (bw, bh) = (layout_number(b.rect[2] - b.rect[0]).max(1.0), layout_number(b.rect[3] - b.rect[1]).max(1.0));
        let h = if b.rotation.abs() > 0.1 { layout_number(b.size).max(1.0) } else { bh };
        // Recover the unrotated width from the axis-aligned box. Use the larger axis so
        // quarter turns and 45-degree watermarks do not divide by (nearly) zero.
        let w = if b.rotation.abs() <= 0.1 {
            bw
        } else if cos.abs() >= sin.abs() {
            (bw - h * sin.abs()) / cos.abs()
        } else {
            (bh - h * cos.abs()) / sin.abs()
        };
        let w = layout_number(w).max(1.0);
        let (x, y) = (layout_number(b.rect[0] + (bw - w) / 2.0), layout_number(page.height - b.rect[3] + (bh - h) / 2.0));
        let z = if b.size >= 2.0 * body { -2 } else { 1 };
        let style = format!(
            "position:absolute;margin-left:{x:.3}pt;margin-top:{y:.3}pt;width:{w:.3}pt;height:{h:.3}pt;rotation:{rotation:.3};z-index:{z};mso-position-horizontal-relative:page;mso-position-vertical-relative:page"
        );
        s.push_str(&format!(
            "<w:r><w:pict><v:rect id=\"Text{}\" style=\"{style}\" filled=\"f\" stroked=\"f\"><v:textbox inset=\"0,0,0,0\" style=\"mso-fit-shape-to-text:t\"><w:txbxContent><w:p><w:pPr><w:spacing w:before=\"0\" w:after=\"0\"/></w:pPr>{}</w:p></w:txbxContent></v:textbox><w10:wrap type=\"none\"/></v:rect></w:pict></w:r>",
            shape_id, run_xml(&b.text, b.size, b.bold, b.italic, b.color)
        ));
    }
    for im in &page.images {
        if !room_for(media, im) {
            continue;
        }
        *shape_id = shape_id.saturating_add(1);
        let n = media.len().saturating_add(1);
        let (w, h) = (layout_number(im.size[0]).max(1.0), layout_number(im.size[1]).max(1.0));
        let (x, y) = (layout_number((im.rect[0] + im.rect[2] - w) / 2.0), layout_number(page.height - (im.rect[1] + im.rect[3] + h) / 2.0));
        let rotation = layout_number(-im.rotation).rem_euclid(360.0);
        let name = format!("image{n}.{}", im.ext);
        let pic = picture_xml(n, &name, (emu(w), emu(h)), Some(rotation));
        // A DrawingML anchor, the way Word itself stores a picture placed behind text: a VML
        // rectangle with an image does not show the picture.
        s.push_str(&format!(
            "<w:r><w:drawing><wp:anchor distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\" simplePos=\"0\" relativeHeight=\"{}\" behindDoc=\"1\" locked=\"0\" layoutInCell=\"1\" allowOverlap=\"1\">\
<wp:simplePos x=\"0\" y=\"0\"/><wp:positionH relativeFrom=\"page\"><wp:posOffset>{}</wp:posOffset></wp:positionH><wp:positionV relativeFrom=\"page\"><wp:posOffset>{}</wp:posOffset></wp:positionV>\
<wp:extent cx=\"{}\" cy=\"{}\"/><wp:effectExtent l=\"0\" t=\"0\" r=\"0\" b=\"0\"/><wp:wrapNone/>",
            n.saturating_add(1),
            emu(x),
            emu(y),
            emu(w),
            emu(h),
        ));
        s.push_str(&pic);
        s.push_str("</wp:anchor></w:drawing></w:r>");
        media.push((name, im));
    }
    s.push_str("</w:p>");
    s
}

fn word_page_size(page: &Page) -> (i64, i64) {
    let twips = |pt: f64| if pt.is_finite() { (pt * 20.0).round().clamp(144.0, 31680.0) as i64 } else { 12240 };
    (twips(page.width), twips(page.height))
}

fn section(page: &Page) -> String {
    let (pw, ph) = word_page_size(page);
    let (mx, my) = ((pw / 8).min(1440), (ph / 8).min(1440));
    format!(
        "<w:sectPr><w:type w:val=\"nextPage\"/><w:pgSz w:w=\"{pw}\" w:h=\"{ph}\"/><w:pgMar w:top=\"{my}\" w:right=\"{mx}\" w:bottom=\"{my}\" w:left=\"{mx}\" w:header=\"{}\" w:footer=\"{}\" w:gutter=\"0\"/></w:sectPr>",
        my / 2,
        my / 2
    )
}

/// The page colour of a Word document: Word has one background for the whole document, so it
/// is written only when every page has the same, non-white background (#2992).
fn word_background(pages: &[Page]) -> Option<String> {
    let first = rgb8(pages.first()?.background?);
    let same = pages.iter().all(|p| p.background.map(rgb8) == Some(first));
    (same && first != [255, 255, 255]).then(|| format!("{:02X}{:02X}{:02X}", first[0], first[1], first[2]))
}

/// A Word document (.docx, Office Open XML). Ordinary pages flow as paragraphs and tables;
/// pages with rotated text, or text that overlaps artwork, use floating shapes to retain their layout.
pub fn docx(pages: &[Page], title: &str) -> Vec<u8> {
    let mut body = String::new();
    let mut media: Vec<(String, &Image)> = Vec::new();
    // Word needs a paragraph between adjacent tables and after the last one in the body.
    let mut after_table = false;
    let mut shape_id = 0usize;
    for (page_index, page) in pages.iter().enumerate() {
        let (pw, _) = word_page_size(page);
        let text_w = (pw - 2 * (pw / 8).min(1440)) as f64 / 20.0;
        if positioned(page) {
            body.push_str(&positioned_page(page, &mut media, &mut shape_id));
            after_table = false;
        } else {
            for it in items(std::slice::from_ref(page)) {
                match &it {
                    Item::Para(runs, lvl) => {
                        let style = match lvl {
                            1 => "<w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr>",
                            2 => "<w:pPr><w:pStyle w:val=\"Heading2\"/></w:pPr>",
                            _ => "",
                        };
                        body.push_str("<w:p>");
                        body.push_str(style);
                        for r in runs {
                            body.push_str(&run_xml(&r.text, r.block.size, r.block.bold, r.block.italic, r.block.color));
                        }
                        body.push_str("</w:p>");
                    }
                    Item::Table(t) => {
                        if after_table {
                            body.push_str("<w:p/>");
                        }
                        body.push_str(&docx_table(t));
                    }
                    Item::Img(im) => {
                        if room_for(&media, im) {
                            let n = media.len() + 1;
                            let name = format!("image{n}.{}", im.ext);
                            // Size on the page, at most the text width.
                            let (w, h) = ((im.rect[2] - im.rect[0]).max(1.0), (im.rect[3] - im.rect[1]).max(1.0));
                            let k = (text_w / w).min(1.0);
                            let (cx, cy) = (emu(w * k), emu(h * k));
                            body.push_str(&format!("<w:p><w:r><w:drawing><wp:inline><wp:extent cx=\"{cx}\" cy=\"{cy}\"/>"));
                            body.push_str(&picture_xml(n, &name, (cx, cy), None));
                            body.push_str("</wp:inline></w:drawing></w:r></w:p>");
                            media.push((name, im));
                        }
                    }
                    Item::PageBreak => body.push_str("<w:p><w:r><w:br w:type=\"page\"/></w:r></w:p>"),
                }
                after_table = matches!(it, Item::Table(_));
            }
        }
        // Section breaks carry this source page's size and create exactly one next page.
        // The last section must be a direct child of the body (OOXML 17.6.17).
        if page_index + 1 < pages.len() {
            body.push_str(&format!(
                "<w:p><w:pPr><w:spacing w:before=\"0\" w:after=\"0\" w:line=\"20\" w:lineRule=\"exact\"/>{}</w:pPr></w:p>",
                section(page)
            ));
        } else {
            if after_table || page.blocks.is_empty() && page.images.is_empty() {
                body.push_str("<w:p><w:pPr><w:spacing w:after=\"0\" w:line=\"20\" w:lineRule=\"exact\"/></w:pPr></w:p>");
            }
            body.push_str(&section(page));
        }
        after_table = false;
    }
    // Word shows a document background only when its settings say so (`settings.xml` below).
    let page_colour = word_background(pages);
    let background = page_colour.as_deref().map(|c| format!("<w:background w:color=\"{c}\"/>")).unwrap_or_default();
    let doc = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" xmlns:v=\"urn:schemas-microsoft-com:vml\" xmlns:w10=\"urn:schemas-microsoft-com:office:word\">{background}<w:body>{body}\
</w:body></w:document>"
    );
    let mut types = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Default Extension=\"png\" ContentType=\"image/png\"/><Default Extension=\"jpg\" ContentType=\"image/jpeg\"/>\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
<Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
<Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>",
    );
    if page_colour.is_some() {
        types.push_str(
            "<Override PartName=\"/word/settings.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml\"/>",
        );
    }
    types.push_str("</Types>");
    let rels = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/></Relationships>";
    let mut doc_rels = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rIdStyles\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>",
    );
    for (i, (name, _)) in media.iter().enumerate() {
        doc_rels.push_str(&format!(
            "<Relationship Id=\"rIdImg{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"media/{name}\"/>",
            i + 1
        ));
    }
    if page_colour.is_some() {
        doc_rels.push_str("<Relationship Id=\"rIdSettings\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings\" Target=\"settings.xml\"/>");
    }
    doc_rels.push_str("</Relationships>");
    let styles = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/><w:pPr><w:spacing w:after=\"120\"/></w:pPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Heading1\"><w:name w:val=\"heading 1\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:pPr><w:keepNext/><w:spacing w:before=\"240\"/><w:outlineLvl w:val=\"0\"/></w:pPr><w:rPr><w:b/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Heading2\"><w:name w:val=\"heading 2\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:pPr><w:keepNext/><w:spacing w:before=\"200\"/><w:outlineLvl w:val=\"1\"/></w:pPr><w:rPr><w:b/></w:rPr></w:style>\
</w:styles>";
    let core = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>{}</dc:title></cp:coreProperties>",
        esc(title)
    );
    let mut z = Zip::default();
    z.add("[Content_Types].xml", types.as_bytes(), true);
    z.add("_rels/.rels", rels.as_bytes(), true);
    z.add("docProps/core.xml", core.as_bytes(), true);
    z.add("word/document.xml", doc.as_bytes(), true);
    z.add("word/styles.xml", styles.as_bytes(), true);
    z.add("word/_rels/document.xml.rels", doc_rels.as_bytes(), true);
    if page_colour.is_some() {
        z.add("word/settings.xml", b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:settings xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:displayBackgroundShape/></w:settings>", true);
    }
    for (name, im) in &media {
        z.add(&format!("word/media/{name}"), &im.bytes, false);
    }
    z.finish()
}

fn rtf_text(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars().filter(|c| allowed(*c)) {
        match c {
            '\\' | '{' | '}' => {
                o.push('\\');
                o.push(c);
            }
            c if (c as u32) < 128 => o.push(c),
            c => {
                // \uN with a ? fallback; UTF-16 units as signed 16-bit numbers.
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    o.push_str(&format!("\\u{}?", *u as i16));
                }
            }
        }
    }
    o
}

/// Rich Text Format: paragraphs with their sizes and bold/italic, real table rows, page breaks
/// between pages (images are left out).
pub fn rtf(pages: &[Page]) -> String {
    let items = items(pages);
    // The colour table: entry 0 is the default (auto) colour, then each non-black colour once.
    let mut colors: Vec<[u8; 3]> = Vec::new();
    let mut slot: HashMap<[u8; 3], usize> = HashMap::new();
    let mut note = |c: [f64; 3]| {
        let c = rgb8(c);
        count_lookup_steps(1);
        if c != [0, 0, 0] && !slot.contains_key(&c) {
            slot.insert(c, colors.len());
            colors.push(c);
        }
    };
    for it in &items {
        match it {
            Item::Para(runs, _) => runs.iter().for_each(|r| note(r.block.color)),
            Item::Table(t) => t.rows.iter().flatten().for_each(|c| note(c.color)),
            Item::Img(_) | Item::PageBreak => {}
        }
    }
    // `\cfN` for a colour (nothing for black, which keeps the default).
    let cf = |c: [f64; 3]| {
        let c = rgb8(c);
        count_lookup_steps(1);
        slot.get(&c).map_or(String::new(), |i| format!("\\cf{}", i + 1))
    };
    let mut s = String::from("{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Helvetica;}}\n");
    if !colors.is_empty() {
        s.push_str("{\\colortbl;");
        for [r, g, b] in &colors {
            s.push_str(&format!("\\red{r}\\green{g}\\blue{b};"));
        }
        s.push_str("}\n");
    }
    for it in items {
        match it {
            Item::Para(runs, _) => {
                // The first run's formatting is the paragraph's; each later run is a group that
                // turns on or off what differs from it.
                let fmt = |b: &Block, later: bool| {
                    let mut fmt = format!("\\fs{}", (b.size * 2.0).round() as i64);
                    if b.bold {
                        fmt.push_str("\\b");
                    } else if later {
                        fmt.push_str("\\b0");
                    }
                    if b.italic {
                        fmt.push_str("\\i");
                    } else if later {
                        fmt.push_str("\\i0");
                    }
                    let colour = cf(b.color);
                    fmt.push_str(if colour.is_empty() && later { "\\cf0" } else { &colour });
                    fmt
                };
                let mut runs = runs.iter();
                let Some(first) = runs.next() else { continue };
                s.push_str(&format!("{{\\pard{} {}", fmt(first.block, false), rtf_text(&first.text)));
                for r in runs {
                    s.push_str(&format!("{{{} {}}}", fmt(r.block, true), rtf_text(&r.text)));
                }
                s.push_str("\\par}\n");
            }
            Item::Table(t) => {
                let mut edges = t.cols.clone();
                edges.push(t.rect[2]);
                let cellx: Vec<i64> = edges.iter().skip(1).map(|e| (e * 20.0).round() as i64).collect();
                for row in &t.rows {
                    s.push_str("\\trowd\\trgaph108");
                    // The row's cell boundaries come first: each cell ends at the right edge of
                    // the last grid column it spans.
                    let mut column = 0;
                    for c in row {
                        column += c.span.max(1);
                        if let Some(x) = cellx.get(column.min(cellx.len()).saturating_sub(1)) {
                            s.push_str(&format!("\\cellx{x}"));
                        }
                    }
                    for c in row {
                        let mut fmt = format!("\\intbl\\fs{}", (c.size * 2.0).round() as i64);
                        if c.bold {
                            fmt.push_str("\\b");
                        }
                        if c.italic {
                            fmt.push_str("\\i");
                        }
                        fmt.push_str(&cf(c.color));
                        s.push_str(&format!("{{{fmt} {}}}\\cell", rtf_text(&c.text)));
                    }
                    s.push_str("\\row\n");
                }
            }
            Item::Img(_) => {}
            Item::PageBreak => s.push_str("\\page\n"),
        }
    }
    s.push('}');
    s
}

#[cfg(test)]
mod tests;
