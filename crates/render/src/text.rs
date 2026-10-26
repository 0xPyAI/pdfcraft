//! Text extraction (bootstrap for `pdfcraft-text`, architecture §8).
//!
//! A hayro `Device` that ignores paint and records every glyph: its Unicode value (ToUnicode →
//! encoding → glyph name fallbacks, handled by hayro) and its box in *page view space*, i.e.
//! points with the origin at the top-left of the displayed page, rotation applied, y down. The UI
//! maps view space to the screen with a single scale.
//!
//! On top of the glyph list: line grouping (baseline clustering), word gaps, plain-text output,
//! case-insensitive search and reading-order selection.

use hayro::hayro_interpret::font::Glyph;
use hayro::hayro_interpret::hayro_syntax::Pdf;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache, InterpreterSettings, Paint, PathDrawMode, SoftMask, interpret_page,
};
use kurbo::{Affine, BezPath, Rect, Shape};
use std::collections::{HashMap, HashSet};

/// Semantic exclusions for statistics. Untagged text is retained; no layout guess is made.
#[derive(Clone, Debug, Default)]
pub struct TextFilter {
    pub tables: bool,
    pub code: bool,
    /// Exclude Figure-marked text and invisible text layers, including OCR.
    pub images: bool,
    /// Page-local MCIDs whose structure ancestors match the selected exclusions.
    pub mcids: HashSet<i32>,
}

impl TextFilter {
    fn excludes(&self, tag: &[u8], mcid: Option<i32>) -> bool {
        (self.tables && matches!(tag, b"Table" | b"TR" | b"TH" | b"TD" | b"THead" | b"TBody" | b"TFoot"))
            || (self.code && tag == b"Code")
            || (self.images && tag == b"Figure")
            || mcid.is_some_and(|id| self.mcids.contains(&id))
    }
}

mod search;

/// Whether `(x, y)` lies in or on the convex quadrilateral `quad`, whichever way it winds. A
/// quad with a NaN corner, or no area, holds nothing.
fn quad_contains(quad: &[[f32; 2]; 4], x: f32, y: f32) -> bool {
    let (mut positive, mut negative) = (false, false);
    for (k, a) in quad.iter().enumerate() {
        let b = quad[(k + 1) % 4];
        let cross = (b[0] - a[0]) * (y - a[1]) - (b[1] - a[1]) * (x - a[0]);
        positive |= cross > 0.0;
        negative |= cross < 0.0;
        if cross.is_nan() {
            return false;
        }
    }
    positive != negative
}

/// One glyph on the page.
#[derive(Clone, Debug, PartialEq)]
pub struct TextGlyph {
    /// The glyph's Unicode text: one character, or several for a ligature.
    pub text: String,
    /// Box in page view space (points, y down): [x0, y0, x1, y1]. The bounding box of `quad`.
    pub rect: [f32; 4],
    /// The glyph's em box in page view space, as it is drawn (rotated, skewed or mirrored): the
    /// start and end of its edge towards the next line, then the end and start of the opposite
    /// edge. `quad[0]` → `quad[1]` runs in the writing direction.
    pub quad: [[f32; 2]; 4],
    /// Unit vector of the writing direction in page view space (`[1, 0]`: left to right).
    pub direction: [f32; 2],
}

/// `s` folded for case-insensitive search, one character for one character.
///
/// The dotted and dotless i of Turkish and Azerbaijani (`İ` U+0130, `ı` U+0131) and the Latin
/// `I`/`i` all fold to `i`. `str::to_lowercase` follows no locale: it turns `İ` into `i` plus a
/// combining dot (two characters) and leaves `ı` alone, so neither `İSTANBUL` nor `IRMAK` could
/// be found by typing `istanbul` or `ırmak`. Search can't know the document's language, so the
/// four are treated as one letter; this only adds matches where an `ı` or `İ` is involved.
/// Greek sigma's normal and word-final forms (`σ`/`ς`) both fold to `σ`, as does `Σ`.
pub fn fold_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'I' | 'i' | '\u{130}' | '\u{131}' => out.push('i'),
            'Σ' | 'σ' | 'ς' => out.push('σ'),
            _ => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// The text of one page, in content order, with line structure.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageText {
    pub glyphs: Vec<TextGlyph>,
    /// For each glyph: its line index (lines are in reading order, top to bottom).
    pub line_of: Vec<u32>,
    /// `true` when a word break precedes the glyph on the same line.
    pub space_before: Vec<bool>,
}

impl PageText {
    pub fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }

    /// Text of glyphs `range` with spaces and line breaks reconstructed.
    pub fn text_of(&self, range: std::ops::Range<usize>) -> String {
        let mut s = String::new();
        let mut prev_line = None;
        for i in range.clone() {
            let Some(g) = self.glyphs.get(i) else { break };
            if i != range.start {
                if prev_line != Some(self.line_of[i]) {
                    s.push('\n');
                } else if self.space_before[i] {
                    s.push(' ');
                }
            }
            s.push_str(&g.text);
            prev_line = Some(self.line_of[i]);
        }
        s
    }

    pub fn plain_text(&self) -> String {
        self.text_of(0..self.glyphs.len())
    }

    /// Case-insensitive search. Returns glyph ranges of every match (words may span line breaks,
    /// which count as a single space).
    pub fn find(&self, needle: &str) -> Vec<std::ops::Range<usize>> {
        self.find_opts(needle, false, false)
    }

    /// Search with Acrobat's find options: case-sensitive, whole words only.
    pub fn find_opts(&self, needle: &str, case_sensitive: bool, whole_words: bool) -> Vec<std::ops::Range<usize>> {
        self.find_opts_limited(needle, case_sensitive, whole_words, usize::MAX)
    }

    /// Search with Acrobat's find options, returning at most `limit` glyph ranges.
    pub fn find_opts_limited(&self, needle: &str, case_sensitive: bool, whole_words: bool, limit: usize) -> Vec<std::ops::Range<usize>> {
        search::find(self, needle, case_sensitive, whole_words, limit)
    }

    /// Glyph ranges of the matches a character matcher finds in the page text (words and lines
    /// separated by one space, original case). Used for pattern search (Search & Redact).
    pub fn find_with(&self, matcher: impl Fn(&[char]) -> Vec<std::ops::Range<usize>>) -> Vec<std::ops::Range<usize>> {
        let mut chars: Vec<char> = Vec::new();
        // `None` is a word or line separator inserted between glyphs. It is not part of either
        // glyph, so a match that starts or ends on it must not take the neighbouring glyph.
        let mut owner: Vec<Option<usize>> = Vec::new();
        for (i, g) in self.glyphs.iter().enumerate() {
            let gap = self.space_before.get(i).copied().unwrap_or(false) || self.line_of.get(i) != self.line_of.get(i.saturating_sub(1));
            if i > 0 && gap && chars.last().is_some_and(|c| *c != ' ') {
                chars.push(' ');
                owner.push(None);
            }
            for c in g.text.chars() {
                chars.push(if c.is_whitespace() { ' ' } else { c });
                owner.push(Some(i));
            }
        }
        matcher(&chars)
            .into_iter()
            .filter_map(|r| {
                if r.start >= r.end || r.end > chars.len() {
                    return None;
                }
                let glyphs = owner.get(r.start..r.end)?;
                let start = glyphs.iter().find_map(|g| *g)?;
                let end = glyphs.iter().rev().find_map(|g| *g)?.checked_add(1)?;
                (start < end).then_some(start..end)
            })
            .collect()
    }

    /// Index of the glyph nearest to a view-space point (for selection anchors).
    ///
    /// A glyph whose drawn em box (`quad`) holds the point wins. Skewed or turned text has bounding
    /// boxes wider than the glyphs, so the box of the letter before overlaps this one and would
    /// otherwise be found first (#3693). Failing that, the nearest bounding box wins, and a tie
    /// goes to the glyph whose centre is closer.
    pub fn nearest(&self, x: f32, y: f32) -> Option<usize> {
        let centre_distance = |g: &TextGlyph| {
            let (cx, cy) = (g.quad.iter().map(|q| q[0]).sum::<f32>() / 4.0, g.quad.iter().map(|q| q[1]).sum::<f32>() / 4.0);
            (cx - x).powi(2) + (cy - y).powi(2)
        };
        let inside = self
            .glyphs
            .iter()
            .enumerate()
            .filter(|(_, g)| quad_contains(&g.quad, x, y))
            .min_by(|a, b| centre_distance(a.1).total_cmp(&centre_distance(b.1)))
            .map(|(i, _)| i);
        if inside.is_some() {
            return inside;
        }
        self.glyphs
            .iter()
            .enumerate()
            .map(|(i, g)| {
                let dx = if x < g.rect[0] {
                    g.rect[0] - x
                } else if x > g.rect[2] {
                    x - g.rect[2]
                } else {
                    0.0
                };
                let dy = if y < g.rect[1] {
                    g.rect[1] - y
                } else if y > g.rect[3] {
                    y - g.rect[3]
                } else {
                    0.0
                };
                // Prefer the same line: vertical distance weighs more.
                (i, dx + dy * 3.0, centre_distance(g))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1).then(a.2.total_cmp(&b.2)))
            .map(|(i, _, _)| i)
    }

    /// The word around glyph `i` as its first and last glyph (double-click): it stops at word
    /// gaps, blank glyphs and line ends.
    pub fn word_at(&self, i: usize) -> Option<(usize, usize)> {
        self.glyphs.get(i)?;
        let blank = |k: usize| self.glyphs.get(k).is_none_or(|g| g.text.trim().is_empty());
        // Glyph `k` (≥ 1) follows glyph `k - 1` with no word gap or line break between them.
        let joined = |k: usize| !self.space_before.get(k).copied().unwrap_or(true) && self.line_of.get(k - 1) == self.line_of.get(k);
        let mut first = i;
        while first > 0 && joined(first) && !blank(first - 1) {
            first -= 1;
        }
        let mut last = i;
        while joined(last + 1) && !blank(last + 1) {
            last += 1;
        }
        Some((first, last))
    }

    /// The line around glyph `i` as its first and last glyph (triple-click). A line's glyphs are
    /// consecutive.
    pub fn line_at(&self, i: usize) -> Option<(usize, usize)> {
        self.glyphs.get(i)?;
        let line = self.line_of.get(i)?;
        let same = |k: usize| self.line_of.get(k) == Some(line);
        let mut first = i;
        while first > 0 && same(first - 1) {
            first -= 1;
        }
        let mut last = i;
        while same(last + 1) {
            last += 1;
        }
        Some((first, last))
    }

    /// Merge the boxes of `range` into one rectangle per line (for highlighting).
    pub fn line_rects(&self, range: std::ops::Range<usize>) -> Vec<[f32; 4]> {
        self.merged_rects(range)
    }

    /// One rectangle per run of consecutive glyphs on one line, for glyph indices in ascending
    /// order (a reading-order range, or [`Self::glyphs_in`]). A run stops at a skipped glyph, so
    /// the rectangle never covers text that is not selected.
    pub fn glyph_rects(&self, glyphs: &[usize]) -> Vec<[f32; 4]> {
        self.merged_rects(glyphs.iter().copied())
    }

    fn merged_rects(&self, glyphs: impl IntoIterator<Item = usize>) -> Vec<[f32; 4]> {
        let mut out: Vec<(u32, usize, [f32; 4])> = Vec::new();
        for i in glyphs {
            let (Some(g), Some(&line)) = (self.glyphs.get(i), self.line_of.get(i)) else { break };
            match out.last_mut() {
                Some((l, last, r)) if *l == line && last.checked_add(1) == Some(i) => {
                    *last = i;
                    union(r, &g.rect);
                }
                _ => out.push((line, i, g.rect)),
            }
        }
        out.into_iter().map(|(_, _, r)| r).collect()
    }

    /// The glyphs whose centre lies inside `rect` (view space, corners in either order), in
    /// reading order: what a column selection covers (Alt/Option-drag, Acrobat's column select).
    /// A glyph only partly inside counts when its centre is, so a rectangle drawn a little into
    /// the next column does not pick up the edge of its text.
    pub fn glyphs_in(&self, rect: [f32; 4]) -> Vec<usize> {
        let (x0, x1) = (rect[0].min(rect[2]), rect[0].max(rect[2]));
        let (y0, y1) = (rect[1].min(rect[3]), rect[1].max(rect[3]));
        let inside = |g: &TextGlyph| {
            let (cx, cy) = ((g.rect[0] + g.rect[2]) / 2.0, (g.rect[1] + g.rect[3]) / 2.0);
            (x0..=x1).contains(&cx) && (y0..=y1).contains(&cy)
        };
        self.glyphs.iter().enumerate().filter(|(_, g)| inside(g)).map(|(i, _)| i).collect()
    }

    /// The text of a column selection ([`Self::glyphs_in`]) as it looks on the page: one row per
    /// visual line, top to bottom. Pieces of different lines that sit side by side (table cells,
    /// newspaper columns) share a row and are separated by a tab, so a pasted table lands in
    /// spreadsheet cells. Rows of right-to-left text put their pieces right to left. When any
    /// selected glyph does not run horizontally (CJK vertical writing, turned text, a page shown
    /// turned by `/Rotate`) rows mean nothing, and the lines come out one per row in reading order
    /// instead.
    pub fn column_text(&self, glyphs: &[usize]) -> String {
        // The selected glyphs of each line, with their box: (glyphs, bbox).
        let mut lines: Vec<(Vec<usize>, [f32; 4])> = Vec::new();
        let mut line_ids: Vec<u32> = Vec::new();
        for &i in glyphs {
            let (Some(g), Some(&line)) = (self.glyphs.get(i), self.line_of.get(i)) else { continue };
            match (lines.last_mut(), line_ids.last()) {
                (Some((members, bb)), Some(&l)) if l == line => {
                    members.push(i);
                    union(bb, &g.rect);
                }
                _ => {
                    lines.push((vec![i], g.rect));
                    line_ids.push(line);
                }
            }
        }
        // Within the layout's own tolerance for reading a glyph as unturned.
        let vertical = glyphs.iter().any(|&i| self.glyphs.get(i).is_some_and(|g| g.direction[1].abs() > WORD_GAP));
        let piece = |members: &[usize]| {
            let mut s = String::new();
            let mut prev: Option<usize> = None;
            for &i in members {
                let Some(g) = self.glyphs.get(i) else { continue };
                let gap = self.space_before.get(i).copied().unwrap_or(false) || prev.and_then(|p| p.checked_add(1)) != Some(i);
                if prev.is_some() && gap {
                    s.push(' ');
                }
                s.push_str(&g.text);
                prev = Some(i);
            }
            s
        };
        if vertical {
            return lines.iter().map(|(members, _)| piece(members)).collect::<Vec<_>>().join("\n");
        }
        // Rows: a line joins the row whose first line its vertical centre falls within.
        let mut order: Vec<usize> = (0..lines.len()).collect();
        order.sort_by(|a, b| {
            let cy = |k: usize| lines.get(k).map_or(0.0, |(_, bb)| (bb[1] + bb[3]) / 2.0);
            cy(*a).total_cmp(&cy(*b))
        });
        let mut rows: Vec<(f32, f32, Vec<usize>)> = Vec::new();
        for k in order {
            let Some((_, bb)) = lines.get(k) else { continue };
            let cy = (bb[1] + bb[3]) / 2.0;
            match rows.last_mut() {
                Some((top, bottom, members)) if cy >= *top && cy <= *bottom => members.push(k),
                _ => rows.push((bb[1], bb[3], vec![k])),
            }
        }
        let rtl = |members: &[usize]| members.iter().any(|&i| self.glyphs.get(i).is_some_and(|g| g.text.chars().any(is_rtl)));
        let mut out = Vec::with_capacity(rows.len());
        for (_, _, mut members) in rows {
            let right_to_left = members.iter().all(|&k| lines.get(k).is_some_and(|(m, _)| rtl(m)));
            members.sort_by(|a, b| {
                let x = |k: usize| lines.get(k).map_or(0.0, |(_, bb)| bb[0]);
                if right_to_left { x(*b).total_cmp(&x(*a)) } else { x(*a).total_cmp(&x(*b)) }
            });
            let cells: Vec<String> = members.iter().filter_map(|&k| lines.get(k)).map(|(m, _)| piece(m)).collect();
            out.push(cells.join("\t"));
        }
        out.join("\n")
    }
}

/// Largest sine of the angle between a glyph's writing direction and a flow's for the glyph to be
/// laid out in that flow's frame. Turning an em box by θ moves its edges along the line by up to
/// h·sin θ, so below the layout's word-gap tolerance (`WORD_GAP` · h) the boxes still read as they
/// would unturned.
const DIRECTION_TOLERANCE: f64 = WORD_GAP as f64;

/// A word gap is wider than the line's typical letter gap by this fraction of the line height.
const WORD_GAP: f32 = 0.15;

/// A glyph with its writing direction to the nearest quarter turn (right, down, left, up) and as
/// a unit vector.
type Drawn = (usize, [f64; 2], TextGlyph);

struct TextDevice {
    /// Every glyph in content order.
    glyphs: Vec<Drawn>,
    /// How many marked content sequences are open.
    depth: usize,
    /// The outermost open `/ActualText` replacement: its depth, text and the glyphs drawn inside
    /// it (with no text of their own yet).
    actual: Option<(usize, String, Vec<Drawn>)>,
    /// Every `/ActualText` replacement made on the page.
    replaced: Vec<Replaced>,
    filter: TextFilter,
    excluded_depth: Option<usize>,
    actual_filtered: bool,
    partial_replacement: bool,
}

/// An `/ActualText` replacement: the glyph standing for it in `TextDevice::glyphs` (none for an
/// empty replacement), its text, and the text and box of each replaced glyph that has Unicode of
/// its own.
struct Replaced {
    at: Option<usize>,
    text: String,
    originals: Vec<(String, [f32; 4])>,
}

/// Grid cell size, in points, for finding the glyphs drawn at a place.
const CELL: f32 = 8.0;

/// Most grid cells searched around one place: a glyph so tall its tolerance spans more is skipped.
const MAX_CELLS: i64 = 4096;

/// Most work (grid cells visited plus glyphs compared) the redraw dedupe in `TextDevice::finish`
/// may do on one page. A hostile page stacking ~10^5 glyphs and ~10^5 replacements at one spot
/// would otherwise make ~10^10 comparisons; past the budget the remaining replacements keep
/// their unmarked redraws (the text reads twice, as it would without `/ActualText`).
const MAX_REDRAW_WORK: usize = 1 << 22;

/// Most replacement text bytes the redraw dedupe in `TextDevice::finish` copies on one page.
const MAX_REDRAW_TEXT: usize = 1 << 22;

#[cfg(test)]
thread_local! {
    /// Replacement text bytes given to redraws on this thread.
    static REDRAW_TEXT_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn cell(v: f32) -> i64 {
    // Glyph boxes are finite (`draw_glyph` and `merge` drop the rest); `as` saturates.
    (v / CELL).floor() as i64
}

/// How far a redrawn glyph may sit from the one it redraws: a fifth of its height, as in `redraws`.
fn redraw_tolerance(rect: &[f32; 4]) -> f32 {
    0.2 * (rect[3] - rect[1]).max(0.1)
}

/// One glyph carrying `text`, boxed around `drawn` in their writing direction; `None` when they
/// span several lines or writing directions (one box would break selection and line grouping).
fn merge(drawn: &[Drawn], text: String) -> Option<Drawn> {
    let first = drawn.first()?;
    let (quarter, u) = (first.0, first.1);
    // Across the line: from the writing direction a quarter turn towards the line's bottom.
    let n = [-u[1], u[0]];
    let along = |p: &[f32; 2]| f64::from(p[0]) * u[0] + f64::from(p[1]) * u[1];
    let across = |p: &[f32; 2]| f64::from(p[0]) * n[0] + f64::from(p[1]) * n[1];
    let height = across(&first.2.quad[0]) - across(&first.2.quad[3]);
    let (mut s0, mut s1, mut t0, mut t1) = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
    for p in drawn.iter().flat_map(|d| d.2.quad.iter()) {
        (s0, s1) = (s0.min(along(p)), s1.max(along(p)));
        (t0, t1) = (t0.min(across(p)), t1.max(across(p)));
    }
    let one_line = drawn.iter().all(|d| compatible(u, d.1)) && t1 - t0 <= 2.0 * height.abs();
    if !(one_line && s0.is_finite() && s1.is_finite() && t0.is_finite() && t1.is_finite()) {
        return None;
    }
    // The edge towards the next line is the one the first glyph's quad starts on.
    let (near, far) = if height >= 0.0 { (t1, t0) } else { (t0, t1) };
    let point = |s: f64, t: f64| [(s * u[0] + t * n[0]) as f32, (s * u[1] + t * n[1]) as f32];
    let quad = [point(s0, near), point(s1, near), point(s1, far), point(s0, far)];
    let rect = quad.iter().fold([f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY], |r, p| {
        [r[0].min(p[0]), r[1].min(p[1]), r[2].max(p[0]), r[3].max(p[1])]
    });
    Some((quarter, u, TextGlyph { text, rect, quad, direction: first.2.direction }))
}

impl TextDevice {
    fn new() -> Self {
        TextDevice {
            glyphs: Vec::new(),
            depth: 0,
            actual: None,
            replaced: Vec::new(),
            filter: TextFilter::default(),
            excluded_depth: None,
            actual_filtered: false,
            partial_replacement: false,
        }
    }

    /// Ends the open `/ActualText` replacement (ISO 32000-2 §14.9.4): its glyphs become one glyph
    /// carrying the replacement text, boxed around them all. Chrome (Skia) writes one around
    /// each Thai, Indic or Arabic cluster whose shaped glyphs (a tone mark moved clear of a vowel,
    /// a ligature) have no Unicode of their own, so without it those marks are lost and the
    /// words can't be found.
    ///
    /// A replacement spanning several lines or writing directions stays per glyph (each glyph's own
    /// Unicode). An empty one (a soft hyphen at a line break, say) stands for no text at all.
    fn end_actual_text(&mut self) {
        let Some((_, text, drawn)) = self.actual.take() else { return };
        if std::mem::take(&mut self.actual_filtered) {
            // ActualText has no mapping from replacement words to individual glyphs. A mixed
            // span cannot be filtered honestly; a wholly excluded span can simply be omitted.
            self.partial_replacement |= !drawn.is_empty();
            return;
        }
        let Some(glyph) = merge(&drawn, text.clone()) else {
            self.glyphs.extend(drawn.into_iter().filter(|d| !d.2.text.chars().all(char::is_control)));
            return;
        };
        let originals = drawn.iter().filter(|d| !d.2.text.chars().all(char::is_control)).map(|d| (d.2.text.clone(), d.2.rect)).collect();
        let at = if text.chars().all(char::is_control) {
            None
        } else {
            self.glyphs.push(glyph);
            Some(self.glyphs.len() - 1)
        };
        self.replaced.push(Replaced { at, text, originals });
    }

    /// Ends the page. Some producers draw text twice, the copy for an outline, shadow or
    /// glow, and mark only one drawing with `/ActualText` (Canva). The unmarked redraw of a
    /// replacement's glyphs (the same text at the same places, in content order) takes the same
    /// replacement, so the two read alike and line layout drops the copy as a redraw.
    fn finish(&mut self) {
        self.end_actual_text();
        if self.replaced.is_empty() {
            return;
        }
        let mut taken = vec![false; self.glyphs.len()];
        for at in self.replaced.iter().filter_map(|r| r.at) {
            if let Some(t) = taken.get_mut(at) {
                *t = true;
            }
        }
        let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for (i, g) in self.glyphs.iter().enumerate() {
            if !taken.get(i).copied().unwrap_or(true) {
                grid.entry((cell(g.2.rect[0]), cell(g.2.rect[1]))).or_default().push(i);
            }
        }
        // Redraws found: the glyph range and the replacement standing for it.
        let mut redraws: Vec<(std::ops::Range<usize>, usize)> = Vec::new();
        let mut budget = MAX_REDRAW_WORK;
        'replacements: for (k, r) in self.replaced.iter().enumerate() {
            let Some((_, rect)) = r.originals.first() else { continue };
            let tol = redraw_tolerance(rect);
            let (x0, x1, y0, y1) = (cell(rect[0] - tol), cell(rect[0] + tol), cell(rect[1] - tol), cell(rect[1] + tol));
            if x1.saturating_sub(x0).saturating_add(1).saturating_mul(y1.saturating_sub(y0).saturating_add(1)) > MAX_CELLS {
                continue;
            }
            // Checking one candidate walks its run twice (free, then same).
            let candidate_cost = r.originals.len().saturating_mul(2).saturating_add(1);
            for key in (x0..=x1).flat_map(|x| (y0..=y1).map(move |y| (x, y))) {
                let Some(rest) = budget.checked_sub(1) else { break 'replacements };
                budget = rest;
                for &i in grid.get(&key).map_or(&[][..], Vec::as_slice) {
                    let Some(rest) = budget.checked_sub(candidate_cost) else { break 'replacements };
                    budget = rest;
                    let Some(range) = i.checked_add(r.originals.len()).map(|end| i..end) else { continue };
                    let Some(run) = self.glyphs.get(range.clone()) else { continue };
                    let free = taken.get(range.clone()).is_some_and(|t| t.iter().all(|t| !t));
                    let same = run.iter().zip(&r.originals).all(|(g, (text, o))| {
                        let tol = redraw_tolerance(o);
                        g.2.text == *text && (g.2.rect[0] - o[0]).abs() < tol && (g.2.rect[1] - o[1]).abs() < tol
                    });
                    if free && same {
                        if let Some(t) = taken.get_mut(range.clone()) {
                            t.fill(true);
                        }
                        redraws.push((range, k));
                    }
                }
            }
        }
        if redraws.is_empty() {
            return;
        }
        redraws.sort_by_key(|(range, _)| range.start);
        let glyphs = std::mem::take(&mut self.glyphs);
        let mut redraws = redraws.into_iter().peekable();
        let mut text_left = MAX_REDRAW_TEXT;
        let mut i = 0;
        while let Some(g) = glyphs.get(i) {
            let Some((range, k)) = redraws.next_if(|(range, _)| range.start == i) else {
                self.glyphs.push(g.clone());
                i += 1;
                continue;
            };
            let run = glyphs.get(range.clone()).unwrap_or_default();
            let replacement = self.replaced.get(k).map_or("", |r| r.text.as_str());
            // Each redraw takes its own copy of the replacement, and one long replacement can
            // have many redraws: past the budget a redraw keeps its glyphs, as past the work budget.
            let Some(left) = text_left.checked_sub(replacement.len()) else {
                self.glyphs.extend_from_slice(run);
                i = range.end.max(i + 1);
                continue;
            };
            text_left = left;
            let text = replacement.to_owned();
            #[cfg(test)]
            REDRAW_TEXT_BYTES.with(|c| c.set(c.get() + text.len()));
            if !text.chars().all(char::is_control) {
                match merge(run, text) {
                    Some(glyph) => self.glyphs.push(glyph),
                    None => self.glyphs.extend_from_slice(run),
                }
            }
            i = range.end.max(i + 1);
        }
    }
}

/// Whether `PDFCRAFT_TEXT_DEBUG` is set. `draw_glyph` runs once per glyph and `getenv` scans the
/// whole environment under a lock, so read it once.
fn text_debug() -> bool {
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEBUG.get_or_init(|| std::env::var_os("PDFCRAFT_TEXT_DEBUG").is_some())
}

impl<'a> Device<'a> for TextDevice {
    fn set_soft_mask(&mut self, _: Option<SoftMask<'a>>) {}
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn draw_path(&mut self, _: &BezPath, _: Affine, _: &Paint<'a>, _: &PathDrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph(&mut self, glyph: &Glyph<'a>, transform: Affine, glyph_transform: Affine, _: &Paint<'a>, mode: &GlyphDrawMode) {
        if self.excluded_depth.is_some() || (self.filter.images && matches!(mode, GlyphDrawMode::Invisible)) {
            self.actual_filtered |= self.actual.is_some();
            return;
        }
        // Inside an `/ActualText` replacement only the glyph's box matters.
        let replaced = self.actual.is_some();
        let text = match glyph.as_unicode() {
            Some(hayro::hayro_interpret::hayro_cmap::BfString::Char(c)) => c.to_string(),
            Some(hayro::hayro_interpret::hayro_cmap::BfString::String(s)) => s,
            None if replaced => String::new(),
            None => {
                if text_debug() {
                    eprintln!(
                        "NOUNICODE {} {:?}",
                        if matches!(glyph, Glyph::Type3(_)) { "t3" } else { "ol" },
                        (transform * glyph_transform).as_coeffs()
                    );
                }
                return;
            }
        };
        if !replaced && text.chars().all(char::is_control) {
            return;
        }
        // Glyph space → view space. Glyph space uses 1000 units per em for outline glyphs; the em
        // box spans descender (−200) to ascender (800), the advance gives the width.
        let t = transform * glyph_transform;
        if text_debug() {
            let kind = if matches!(glyph, Glyph::Type3(_)) { "t3" } else { "ol" };
            eprintln!("{kind} {text:?} transform={:?} glyph_transform={:?}", transform.as_coeffs(), glyph_transform.as_coeffs());
        }
        let advance = match glyph {
            Glyph::Outline(g) => {
                g.advance_width().filter(|a| *a > 0.0).map(f64::from).unwrap_or_else(|| g.outline().bounding_box().width().max(500.0))
            }
            Glyph::Type3(g) => g.advance_width().filter(|a| a.is_finite() && *a > 0.0).map(f64::from).unwrap_or(600.0),
        };
        // The baseline direction in view space (y down).
        let coefficients = t.as_coeffs();
        let (dx, dy) = if matches!(glyph, Glyph::Outline(g) if g.is_vertical()) {
            // PDF vertical fonts advance along negative glyph-space y (WMode 1), while
            // keeping the glyph outline upright. Its x axis is not the writing direction.
            (-coefficients[2], -coefficients[3])
        } else {
            (coefficients[0], coefficients[1])
        };
        let dir = if dx.abs() >= dy.abs() {
            if dx >= 0.0 { 0 } else { 2 }
        } else if dy > 0.0 {
            1
        } else {
            3
        };
        let em = Rect::new(0.0, -200.0, advance, 800.0);
        let corners = if matches!(glyph, Glyph::Outline(g) if g.is_vertical()) {
            [(em.x0, em.y1), (em.x0, em.y0), (em.x1, em.y0), (em.x1, em.y1)]
        } else {
            [(em.x0, em.y0), (em.x1, em.y0), (em.x1, em.y1), (em.x0, em.y1)]
        }
        .map(|(x, y)| t * kurbo::Point::new(x, y));
        let b = corners.iter().fold(Rect::new(f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY), |b, p| {
            Rect::new(b.x0.min(p.x), b.y0.min(p.y), b.x1.max(p.x), b.y1.max(p.y))
        });
        if !(b.x0.is_finite() && b.y0.is_finite() && b.x1.is_finite() && b.y1.is_finite()) || b.width() > 10_000.0 || b.height() > 10_000.0 {
            return;
        }
        let length = dx.hypot(dy);
        let direction = if length.is_finite() && length > 0.0 { [dx / length, dy / length] } else { [1.0, 0.0] };
        // A glyph whose Unicode is several characters (a ligature such as "ffi") stays one glyph
        // with its own box: the PDF gives no position for the characters inside it.
        let glyph = TextGlyph {
            text,
            rect: [b.x0 as f32, b.y0 as f32, b.x1 as f32, b.y1 as f32],
            quad: corners.map(|p| [p.x as f32, p.y as f32]),
            direction: direction.map(|d| d as f32),
        };
        match &mut self.actual {
            Some((_, _, drawn)) => drawn.push((dir, direction, glyph)),
            None => self.glyphs.push((dir, direction, glyph)),
        }
    }
    fn draw_image(&mut self, _: Image<'a, '_>, _: Affine) {}
    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}
    fn begin_marked_content(&mut self, tag: &[u8], mcid: Option<i32>) {
        self.depth = self.depth.saturating_add(1);
        if self.excluded_depth.is_none() && self.filter.excludes(tag, mcid) {
            self.excluded_depth = Some(self.depth);
        }
    }
    fn actual_text(&mut self, text: &[u8]) {
        // A replacement nested in another is already covered by the outer one.
        if self.actual.is_none() && self.excluded_depth.is_none() {
            self.actual = Some((self.depth, pdfcraft_cos::PdfString::literal(text).to_text(), Vec::new()));
        }
    }
    fn end_marked_content(&mut self) {
        if self.actual.as_ref().is_some_and(|(depth, _, _)| *depth == self.depth) {
            self.end_actual_text();
        }
        if self.excluded_depth == Some(self.depth) {
            self.excluded_depth = None;
        }
        self.depth = self.depth.saturating_sub(1);
    }
}

/// Extract the text of one page. Panics inside the interpreter are the caller's to catch.
pub(crate) fn extract_page(pdf: &Pdf, page: usize, settings: &InterpreterSettings) -> Option<PageText> {
    extract_page_filtered(pdf, page, settings, TextFilter::default()).ok()
}

pub(crate) fn extract_page_filtered(pdf: &Pdf, page: usize, settings: &InterpreterSettings, filter: TextFilter) -> Result<PageText, String> {
    let pages = pdf.pages();
    let p = pages.get(page).ok_or("the page could not be read")?;
    let (w, h) = p.render_dimensions();
    let cache = InterpreterCache::new();
    // Annotation appearances are included: form-field values and comment text are searchable,
    // as in Acrobat, and match what other extractors report.
    let settings = settings.clone();
    let initial = p.initial_transform(true).to_kurbo();
    let mut ctx = Context::new(initial, Rect::new(0.0, 0.0, w as f64, h as f64), &cache, p.xref(), settings);
    let mut dev = TextDevice::new();
    dev.filter = filter;
    interpret_page(p, &mut ctx, &mut dev);
    // A replacement the content never closed ends with the page.
    dev.finish();
    if dev.partial_replacement {
        return Err("a replacement text span mixes included and excluded content; disable the exclusions to count this page".into());
    }
    let glyphs = dev.glyphs;
    // Text on a curved path: glyphs that each continue the previous one's line while the
    // direction turns. Each such run is one line.
    let turned = |u: [f64; 2]| u[0].abs().min(u[1].abs()) > DIRECTION_TOLERANCE;
    let mut in_curve = vec![false; glyphs.len()];
    // Turned text: curved runs, and straight flows along one writing direction. The two are kept
    // apart so finding a glyph's direction scans only the directions (a few dozen at most: each
    // differs from the others by more than the tolerance), not every curved run.
    let mut curves: Vec<Vec<usize>> = Vec::new();
    let mut straight: Vec<([f64; 2], Vec<usize>)> = Vec::new();
    let mut start = 0;
    for end in 1..=glyphs.len() {
        if end < glyphs.len() && continues(&glyphs[end - 1], &glyphs[end]) {
            continue;
        }
        let run = &glyphs[start..end];
        if run.iter().any(|g| turned(g.1)) && run.iter().any(|g| !compatible(run[0].1, g.1)) {
            in_curve[start..end].fill(true);
            curves.push((start..end).collect());
        }
        start = end;
    }
    let mut quarters: [Vec<usize>; 4] = Default::default();
    for (i, (quarter, u, _)) in glyphs.iter().enumerate() {
        if in_curve[i] {
            continue;
        }
        if !turned(*u) {
            if let Some(q) = quarters.get_mut(*quarter) {
                q.push(i);
            }
        } else if let Some((_, ids)) = straight.iter_mut().find(|(d, _)| compatible(*d, *u)) {
            ids.push(i);
        } else {
            straight.push((*u, vec![i]));
        }
    }
    // A horizontal heading must not force a vertical body back into horizontal line grouping.
    // Group each writing direction independently, then join flows from their topmost position.
    let upright = |quarters: [Vec<usize>; 4]| {
        let mut flows: Vec<Flow> = quarters
            .into_iter()
            .enumerate()
            .filter(|(_, ids)| !ids.is_empty())
            .map(|(quarter, ids)| {
                let rects = ids.iter().map(|&i| to_upright(glyphs[i].2.rect, quarter as u8, w, h)).collect();
                arrange_flow(&glyphs, ids, rects, Some(quarter as u8))
            })
            .collect();
        flows.sort_by(|a, b| a.top.total_cmp(&b.top));
        flows
    };
    let flows = upright(quarters);
    let mut extra: Vec<Flow> = curves
        .into_iter()
        .map(|ids| (None, ids))
        .chain(straight.into_iter().map(|(u, ids)| (Some(u), ids)))
        .map(|(u, ids)| {
            let rects = match u {
                // Turned text is laid out along its own baseline.
                Some(u) => ids.iter().map(|&i| along_baseline(&glyphs[i].2.quad, u).map(|c| c as f32)).collect(),
                None => unrolled(ids.iter().map(|&i| &glyphs[i])),
            };
            arrange_flow(&glyphs, ids, rects, None)
        })
        .collect();
    // Turned text takes the place in the reading order where laying it out among the nearest
    // quarter turn's text puts its first glyph.
    let mut rank = Vec::new();
    if !extra.is_empty() {
        rank = vec![usize::MAX; glyphs.len()];
        let mut nearest: [Vec<usize>; 4] = Default::default();
        for (i, g) in glyphs.iter().enumerate() {
            if let Some(q) = nearest.get_mut(g.0) {
                q.push(i);
            }
        }
        for (r, i) in upright(nearest).into_iter().flat_map(|f| f.ids).enumerate() {
            rank[i] = r;
        }
    }
    let first = |ids: &[usize]| ids.iter().filter_map(|&i| rank.get(i).copied()).min().unwrap_or(usize::MAX);
    extra.sort_by_key(|f| first(&f.ids));
    let mut extra = extra.into_iter().peekable();
    let mut slots: Vec<Option<TextGlyph>> = glyphs.into_iter().map(|g| Some(g.2)).collect();
    let mut text = PageText::default();
    let mut emit = |text: &mut PageText, ids: &[usize], line_of: &[u32], space_before: &[bool], quarter: Option<u8>| {
        let offset = text.line_of.last().map_or(0, |line| line.saturating_add(1));
        for ((&i, &line), &space) in ids.iter().zip(line_of).zip(space_before) {
            let Some(mut g) = slots.get_mut(i).and_then(Option::take) else { continue };
            if let Some(quarter) = quarter {
                g.rect = from_upright(to_upright(g.rect, quarter, w, h), quarter, w, h);
            }
            text.glyphs.push(g);
            text.line_of.push(line.saturating_add(offset));
            text.space_before.push(space);
        }
    };
    for flow in flows {
        let mut k = 0;
        while k < flow.ids.len() {
            let line = flow.line_of[k];
            let end = flow.line_of[k..].iter().position(|l| *l != line).map_or(flow.ids.len(), |n| k + n);
            let before = first(&flow.ids[k..end]);
            while let Some(f) = extra.next_if(|f| first(&f.ids) < before) {
                emit(&mut text, &f.ids, &f.line_of, &f.space_before, None);
            }
            emit(&mut text, &flow.ids[k..end], &vec![0; end - k], &flow.space_before[k..end], flow.quarter);
            k = end;
        }
    }
    for f in extra {
        emit(&mut text, &f.ids, &f.line_of, &f.space_before, None);
    }
    Ok(text)
}

/// Glyphs in reading order, as indices into the page's glyphs, with their lines and word gaps.
struct Flow {
    /// The top of the flow's topmost glyph in view space.
    top: f32,
    /// The quarter turn whose frame the flow was laid out in.
    quarter: Option<u8>,
    ids: Vec<usize>,
    line_of: Vec<u32>,
    space_before: Vec<bool>,
}

/// Lay out the page's glyphs `ids`, giving each the box at the same position in `rects`.
fn arrange_flow(glyphs: &[Drawn], ids: Vec<usize>, rects: Vec<[f32; 4]>, quarter: Option<u8>) -> Flow {
    let top = ids.iter().filter_map(|&i| glyphs.get(i)).map(|g| g.2.rect[1]).fold(f32::INFINITY, f32::min);
    let framed: Vec<TextGlyph> = ids.iter().zip(rects).filter_map(|(&i, rect)| glyphs.get(i).map(|g| TextGlyph { rect, ..g.2.clone() })).collect();
    let (order, line_of, space_before) = arrange(&framed);
    let ids = order.into_iter().filter_map(|k| ids.get(k).copied()).collect();
    Flow { top, quarter, ids, line_of, space_before }
}

/// Boxes for a run of glyphs set along a curve: each glyph's box along its own direction, placed
/// where the pen moved in the previous glyph's direction. The path straightened out.
fn unrolled<'g>(run: impl Iterator<Item = &'g Drawn>) -> Vec<[f32; 4]> {
    let mut pen = [0.0, 0.0];
    let mut previous: Option<([f64; 2], [f64; 4])> = None;
    run.map(|(_, u, g)| {
        let b = along_baseline(&g.quad, *u);
        if let Some((pu, pb)) = previous {
            let at = along_baseline(&g.quad, pu);
            pen = [pen[0] + at[0] - pb[0], pen[1] + (at[1] + at[3] - pb[1] - pb[3]) / 2.0];
        }
        previous = Some((*u, b));
        let half = (b[3] - b[1]) / 2.0;
        [pen[0], pen[1] - half, pen[0] + b[2] - b[0], pen[1] + half].map(|c| c as f32)
    })
    .collect()
}

/// View-space box → the frame where text runs left to right, for text running `quarter` × 90°
/// clockwise from that in a `w` × `h` view (1: text runs downwards).
fn to_upright(r: [f32; 4], quarter: u8, w: f32, h: f32) -> [f32; 4] {
    let [x0, y0, x1, y1] = r;
    match quarter {
        1 => [y0, w - x1, y1, w - x0],
        2 => [w - x1, h - y1, w - x0, h - y0],
        3 => [h - y1, x0, h - y0, x1],
        _ => r,
    }
}

/// Whether two writing directions are close enough to share a frame ([`DIRECTION_TOLERANCE`]).
fn compatible(a: [f64; 2], b: [f64; 2]) -> bool {
    a[0] * b[0] + a[1] * b[1] > 0.0 && (a[0] * b[1] - a[1] * b[0]).abs() <= DIRECTION_TOLERANCE
}

/// Whether glyph `g` continues the line of glyph `p` (content order), by the test `layout` uses
/// for consecutive glyphs, taken in `p`'s own frame. The direction may turn by less than half a
/// quarter turn, as text set along a curve does.
fn continues(p: &Drawn, g: &Drawn) -> bool {
    let (pu, gu) = (p.1, g.1);
    if (pu[0] * gu[1] - pu[1] * gu[0]).abs() >= pu[0] * gu[0] + pu[1] * gu[1] {
        return false;
    }
    let (pb, gb, own) = (along_baseline(&p.2.quad, pu), along_baseline(&g.2.quad, pu), along_baseline(&g.2.quad, gu));
    let h = (pb[3] - pb[1]).min(own[3] - own[1]).max(0.1);
    let cy = |b: [f64; 4]| (b[1] + b[3]) / 2.0;
    (cy(gb) - cy(pb)).abs() < h * 0.5 && gb[0] - pb[2] < h * 3.0 && gb[0] > pb[0] - h * 2.0
}

/// A view-space quad → its box in the frame where `u` runs left to right and `u` turned a
/// quarter clockwise runs downwards. Along `u` it spans the middles of the quad's start and end
/// sides, so a skewed glyph is no wider than its advance; across, it spans every corner.
fn along_baseline(quad: &[[f32; 2]; 4], u: [f64; 2]) -> [f64; 4] {
    let p = quad.map(|[x, y]| [f64::from(x) * u[0] + f64::from(y) * u[1], f64::from(y) * u[0] - f64::from(x) * u[1]]);
    let (a0, a1) = ((p[0][0] + p[3][0]) / 2.0, (p[1][0] + p[2][0]) / 2.0);
    let (c0, c1) = p.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), q| (lo.min(q[1]), hi.max(q[1])));
    [a0.min(a1), c0, a0.max(a1), c1]
}

/// The inverse of [`to_upright`].
fn from_upright(r: [f32; 4], quarter: u8, w: f32, h: f32) -> [f32; 4] {
    let [u0, v0, u1, v1] = r;
    match quarter {
        1 => [w - v1, u0, w - v0, u1],
        2 => [w - u1, h - v1, w - u0, h - v0],
        3 => [v0, h - u1, v1, h - u0],
        _ => r,
    }
}

use hayro::hayro_interpret::TransformExt;

fn is_rtl(c: char) -> bool {
    matches!(c as u32, 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF)
}

/// Scripts written with explicit spaces whose combining signs can sit apart from their base
/// (Indic, Thai, Lao, Myanmar, Khmer): never infer a space inside a tight cluster.
fn is_complex(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x0900..=0x0DFF | 0x0E00..=0x0EFF | 0x1000..=0x109F | 0x1780..=0x17FF)
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x3134F | 0x1100..=0x11FF)
}

/// Glyphs `start..end` of the glyph list [`arrange`] lays out, drawn one after another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    start: usize,
    end: usize,
}

impl Run {
    fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    fn range(self) -> std::ops::Range<usize> {
        self.start..self.end
    }
}

/// Marks an absent link in a union-find or run list.
const NONE: usize = usize::MAX;

/// A segment: runs drawn on one baseline, placed together. Its glyph order is worked out when it is emitted, so a
/// segment holds run ranges and geometry only.
struct Seg {
    /// The first run. Merged segments keep their other runs in `extra`.
    first: Run,
    /// The other runs of a merged segment. Boxed, and absent for most segments.
    extra: Option<Box<SegExtra>>,
    bbox: [f32; 4],
    h: f32,
}

#[derive(Default)]
struct SegExtra {
    more: Vec<Run>,
}

impl Seg {
    fn new(first: Run, bbox: [f32; 4], h: f32) -> Self {
        Seg { first, extra: None, bbox, h }
    }

    fn cy(&self) -> f32 {
        (self.bbox[1] + self.bbox[3]) / 2.0
    }

    fn runs(&self) -> impl Iterator<Item = Run> + '_ {
        std::iter::once(self.first).chain(self.extra.iter().flat_map(|e| e.more.iter().copied()))
    }

    fn merge(&mut self, other: Seg) {
        union(&mut self.bbox, &other.bbox);
        self.h = self.h.max(other.h);
        let extra = self.extra.get_or_insert_with(Default::default);
        extra.more.push(other.first);
        if let Some(o) = other.extra {
            extra.more.extend(o.more);
        }
    }
}

/// Two glyphs of different runs that overlap by more than this fraction of the line height collide.
const RUN_COLLISION: f32 = 0.25;
/// A run that starts inside the runs before it by more than this fraction of the line height starts a new word.
const RUN_OVERLAP_SPACE: f32 = 0.5;
/// Earlier runs with the same key that a run is checked against for a redraw, newest first. A run past this limit is kept,
/// so a page costs at most this many checks per run, however many runs share a key.
const REDRAW_CANDIDATES: usize = 32;
/// Pair tests that the collision sweep may make per glyph of a segment. Ordinary text overlaps each glyph with about one
/// neighbour, so it stays far below this. A segment that needs more is dense, and [`collide`] joins its runs instead.
const COLLIDE_PAIRS_PER_GLYPH: usize = 16;
/// Glyph positions within a fifth of their height are considered the same for redraws.
const REDRAW_POSITION_TOLERANCE: f32 = 0.2;
/// A reset needs three matching glyphs at the start of the run, keeping local kerning from splitting it.
const REDRAW_PREFIX_GLYPHS: usize = 3;

/// Reusable buffers, so a page allocates them once rather than per segment. Glyph placement works in them, one
/// segment at a time, when the segment is emitted.
#[derive(Default)]
struct Scratch {
    /// The glyphs of the text runs, each with its run, in x order.
    owner: Vec<(usize, usize)>,
    /// The text runs of the segment (runs that are not all zero-width).
    text: Vec<Run>,
    /// The glyphs of the zero-width runs, in run order.
    marks: Vec<usize>,
    /// Union-find parents over `text`, and for each root the list of its runs through `next`.
    parent: Vec<usize>,
    head: Vec<usize>,
    next: Vec<usize>,
    /// The runs of one colliding component, by left edge.
    rows: Vec<Row>,
    /// The glyphs of one colliding component: each run's glyphs in x order, in run order.
    group: Vec<usize>,
    /// The units to place, and their glyphs in `flat`.
    units: Vec<Unit>,
    flat: Vec<usize>,
    /// The current segment's glyphs in visual reading order, and its forced word breaks.
    placed: Vec<usize>,
    breaks: Vec<usize>,
    /// The gaps between the current segment's glyphs, and its glyphs in logical order when it has right-to-left text.
    gaps: Vec<f32>,
    logical: Vec<usize>,
    /// `placed` with the marks merged in, built before it is swapped in.
    merged: Vec<usize>,
}

/// One run of a colliding component.
struct Row {
    left: f32,
    right: f32,
    /// The run's first glyph in content order, which is its smallest index.
    first: usize,
    /// The run's glyph with the smallest x, where a word break is recorded.
    lead: usize,
    /// The run's glyphs in `Scratch::group`.
    glyphs: std::ops::Range<usize>,
}

/// A part of a segment placed as one: a free glyph, or a colliding component.
struct Unit {
    left: f32,
    first: usize,
    /// The unit's glyphs in `Scratch::flat`.
    glyphs: std::ops::Range<usize>,
}

fn union_find(parent: &mut [usize], mut r: usize) -> usize {
    while parent[r] != r {
        parent[r] = parent[parent[r]];
        r = parent[r];
    }
    r
}

/// A 64-bit fingerprint of a run's glyph texts. Each text is hashed with its length, so ["ab", "c"] and
/// ["a", "bc"] differ. Equal fingerprints are only candidates: `redraws` decides.
fn fingerprint(glyphs: &[&TextGlyph], run: Run) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for g in run.range().filter_map(|i| glyphs.get(i)) {
        for b in (g.text.len() as u64).to_le_bytes().into_iter().chain(g.text.bytes()) {
            h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// Whether run `b` redraws run `a` glyph for glyph: the same text, each glyph within a fifth of its
/// height of the same glyph in `a`.
fn redraws(glyphs: &[&TextGlyph], a: Run, b: Run) -> bool {
    a.len() == b.len()
        && a.range().zip(b.range()).all(|(i, j)| match (glyphs.get(i), glyphs.get(j)) {
            (Some(p), Some(g)) => {
                let tol = REDRAW_POSITION_TOLERANCE * (g.rect[3] - g.rect[1]).max(0.1);
                p.text == g.text && (p.rect[0] - g.rect[0]).abs() < tol && (p.rect[1] - g.rect[1]).abs() < tol
            }
            _ => false,
        })
}

fn within_position_tolerance(a: f32, b: f32, tolerance: f32) -> bool {
    (a - b).abs().partial_cmp(&tolerance) == Some(std::cmp::Ordering::Less)
}

/// Whether `next` restarts `start` after the run has advanced by a glyph height. The fixed prefix
/// bounds this check and requires more than a single backward adjustment to split a run.
fn restarts_run(glyphs: &[&TextGlyph], start: usize, next: usize, progress: f32) -> bool {
    let (Some(first), Some(candidate)) = (glyphs.get(start), glyphs.get(next)) else { return false };
    let tol = REDRAW_POSITION_TOLERANCE * (candidate.rect[3] - candidate.rect[1]).max(0.1);
    if first.text != candidate.text
        || !within_position_tolerance(first.rect[0], candidate.rect[0], tol)
        || !within_position_tolerance(first.rect[1], candidate.rect[1], tol)
    {
        return false;
    }
    let h = (first.rect[3] - first.rect[1]).max(0.1);
    if !progress.is_finite() || progress <= h {
        return false;
    }
    for offset in 1..REDRAW_PREFIX_GLYPHS {
        let (Some(a), Some(b)) = (start.checked_add(offset).and_then(|i| glyphs.get(i)), next.checked_add(offset).and_then(|i| glyphs.get(i))) else {
            return false;
        };
        let tol = REDRAW_POSITION_TOLERANCE * (b.rect[3] - b.rect[1]).max(0.1);
        if a.text != b.text || !within_position_tolerance(a.rect[0], b.rect[0], tol) || !within_position_tolerance(a.rect[1], b.rect[1], tol) {
            return false;
        }
    }
    true
}

/// Puts a segment's glyphs in visual reading order into `sc.placed`, and its forced word breaks into `sc.breaks`.
///
/// Glyphs are placed by x. Runs that collide, meaning that glyphs of different runs overprint each other, stay whole
/// and are placed by their left edge. Runs that only interleave along the line, such as symbols between letters, keep
/// their glyph-by-glyph order, so the common case is one sort. A run of zero-width glyphs (a combining mark) has no
/// extent of its own, so it goes next to the glyph it starts at.
fn place_glyphs(glyphs: &[&TextGlyph], height: &dyn Fn(usize) -> f32, s: &Seg, sc: &mut Scratch) {
    let x0 = |i: usize| glyphs[i].rect[0];
    let zero_width = |i: usize| glyphs[i].rect[2] - glyphs[i].rect[0] <= 0.05 * height(i);
    sc.text.clear();
    sc.marks.clear();
    sc.placed.clear();
    sc.breaks.clear();
    for run in s.runs() {
        if run.range().all(&zero_width) {
            sc.marks.extend(run.range());
        } else {
            sc.text.push(run);
        }
    }
    if sc.text.len() <= 1 {
        // A single run cannot collide with itself: its glyphs only need sorting by x.
        sc.placed.extend(sc.text.iter().flat_map(|run| run.range()));
        sc.placed.sort_unstable_by(|a, b| x0(*a).total_cmp(&x0(*b)).then(a.cmp(b)));
    } else {
        collide(glyphs, height, s.h, sc);
    }
    // Each mark goes before the first placed glyph whose x is greater than its own. Marks are visited in x order, and
    // that position never moves back, so one pass over `placed` puts all of them in.
    sc.marks.sort_by(|a, b| x0(*a).total_cmp(&x0(*b)));
    if !sc.marks.is_empty() {
        sc.merged.clear();
        let mut next = 0;
        for &m in &sc.marks {
            while next < sc.placed.len() && x0(sc.placed[next]) <= x0(m) {
                sc.merged.push(sc.placed[next]);
                next += 1;
            }
            sc.merged.push(m);
        }
        sc.merged.extend_from_slice(&sc.placed[next..]);
        std::mem::swap(&mut sc.placed, &mut sc.merged);
    }
}

/// `place_glyphs` for a segment of two or more text runs. `h` is the segment's line height. Returns whether the segment
/// was too dense to sweep ([`COLLIDE_PAIRS_PER_GLYPH`]), in which case all its text runs form one component.
///
/// The sweep tests each glyph against the glyphs after it that start before its right edge. Its work is bounded by the
/// budget, so a page costs O(glyphs) pair tests plus the sorts. A dense segment is joined with one pass over its runs,
/// which keeps each run whole. It may keep runs whole that do not overprint, but never interleaves overprinted glyphs.
fn collide(glyphs: &[&TextGlyph], height: &dyn Fn(usize) -> f32, h: f32, sc: &mut Scratch) -> bool {
    let x0 = |i: usize| glyphs[i].rect[0];
    let runs = sc.text.len();
    sc.owner.clear();
    for (r, run) in sc.text.iter().enumerate() {
        sc.owner.extend(run.range().map(|i| (i, r)));
    }
    sc.owner.sort_unstable_by(|a, b| x0(a.0).total_cmp(&x0(b.0)).then(a.0.cmp(&b.0)));
    sc.parent.clear();
    sc.parent.extend(0..runs);
    let mut budget = COLLIDE_PAIRS_PER_GLYPH.saturating_mul(sc.owner.len());
    let mut collided = false;
    let mut dense = false;
    'sweep: for a in 0..sc.owner.len() {
        let (i, ri) = sc.owner[a];
        let (x1, hi) = (glyphs[i].rect[2], height(i));
        for b in a + 1..sc.owner.len() {
            let (j, rj) = sc.owner[b];
            if x0(j) >= x1 {
                break;
            }
            if budget == 0 {
                dense = true;
                break 'sweep;
            }
            budget -= 1;
            let (ra, rb) = (union_find(&mut sc.parent, ri), union_find(&mut sc.parent, rj));
            let overlap = x1.min(glyphs[j].rect[2]) - x0(j);
            if ra != rb && overlap > RUN_COLLISION * hi.min(height(j)) {
                sc.parent[ra] = rb;
                collided = true;
            }
        }
    }
    if dense {
        // Run 0 is its own root, so every run now has it as its root.
        sc.parent.iter_mut().for_each(|p| *p = 0);
        collided = true;
    }
    if !collided {
        sc.placed.extend(sc.owner.iter().map(|&(i, _)| i));
        return dense;
    }

    // Link each component's runs, so they can be walked without a map.
    sc.head.clear();
    sc.head.resize(runs, NONE);
    sc.next.clear();
    sc.next.resize(runs, NONE);
    for r in 0..runs {
        let root = union_find(&mut sc.parent, r);
        sc.next[r] = sc.head[root];
        sc.head[root] = r;
    }
    sc.units.clear();
    sc.flat.clear();
    for root in 0..runs {
        if union_find(&mut sc.parent, root) != root {
            continue;
        }
        let head = sc.head[root];
        if sc.next[head] == NONE {
            // A run that collides with nothing is placed glyph by glyph.
            for i in sc.text[head].range() {
                let at = sc.flat.len();
                sc.flat.push(i);
                sc.units.push(Unit { left: x0(i), first: i, glyphs: at..at + 1 });
            }
            continue;
        }
        sc.rows.clear();
        sc.group.clear();
        let mut m = head;
        while m != NONE {
            let run = sc.text[m];
            let start = sc.group.len();
            sc.group.extend(run.range());
            sc.group[start..].sort_unstable_by(|a, b| x0(*a).total_cmp(&x0(*b)).then(a.cmp(b)));
            let left = sc.group[start..].iter().map(|&i| x0(i)).fold(f32::INFINITY, f32::min);
            let right = sc.group[start..].iter().map(|&i| glyphs[i].rect[2]).fold(f32::NEG_INFINITY, f32::max);
            let lead = sc.group[start];
            sc.rows.push(Row { left, right, first: run.start, lead, glyphs: start..sc.group.len() });
            m = sc.next[m];
        }
        sc.rows.sort_unstable_by(|a, b| a.left.total_cmp(&b.left).then(a.first.cmp(&b.first)));
        let start = sc.flat.len();
        let mut reach = f32::NEG_INFINITY;
        for row in &sc.rows {
            if row.left < reach - h * RUN_OVERLAP_SPACE {
                sc.breaks.push(row.lead);
            }
            reach = reach.max(row.right);
            sc.flat.extend_from_slice(&sc.group[row.glyphs.clone()]);
        }
        let left = sc.rows.iter().map(|r| r.left).fold(f32::INFINITY, f32::min);
        let first = sc.rows.iter().map(|r| r.first).min().unwrap_or(0);
        sc.units.push(Unit { left, first, glyphs: start..sc.flat.len() });
    }
    sc.units.sort_unstable_by(|a, b| a.left.total_cmp(&b.left).then(a.first.cmp(&b.first)));
    for unit in &sc.units {
        sc.placed.extend_from_slice(&sc.flat[unit.glyphs.clone()]);
    }
    // Sorted, so that each glyph's membership is a binary search rather than a scan of the breaks.
    sc.breaks.sort_unstable();
    dense
}

/// Simplified bidi class of a glyph or word gap (UAX #9: strong L/R, numbers, neutrals).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bidi {
    L,
    R,
    Num,
    N,
}

fn bidi_class(text: &str) -> Bidi {
    if text.chars().any(|c| is_rtl(c) && !c.is_numeric()) {
        Bidi::R
    } else if text.chars().any(char::is_numeric) {
        Bidi::Num
    } else if text.chars().any(char::is_alphabetic) {
        Bidi::L
    } else {
        Bidi::N
    }
}

/// Reorders a line of `idx` from visual to logical order with a simplified bidi level pass (#2667), carrying the
/// word gaps along: `spaces[w]` is a gap left of `idx[w]` (#1062). The paragraph is right-to-left when it has
/// more right-to-left than left-to-right glyphs; numbers keep their left-to-right order, and neutrals
/// (punctuation, gaps) take the direction of their neighbours, or the paragraph's between opposite ones.
fn reverse_rtl(glyphs: &[&TextGlyph], idx: &mut [usize], spaces: &mut [bool]) {
    // Glyphs and word gaps (None) in visual order.
    let mut items: Vec<(Option<usize>, Bidi)> = Vec::with_capacity(idx.len() * 2);
    for (w, &i) in idx.iter().enumerate() {
        if w > 0 && spaces.get(w).copied().unwrap_or(false) {
            items.push((None, Bidi::N));
        }
        items.push((Some(i), bidi_class(&glyphs[i].text)));
    }
    let texts: Vec<&str> = items.iter().map(|it| it.0.map_or("", |i| glyphs[i].text.as_str())).collect();
    let text = |k: usize| texts.get(k).copied().unwrap_or("");
    let count = |b: Bidi| items.iter().filter(|it| it.1 == b).count();
    let para = if count(Bidi::R) > count(Bidi::L) { Bidi::R } else { Bidi::L };
    let n = items.len();
    // W4: a single separator between two digits belongs to the number.
    for k in 1..n.saturating_sub(1) {
        let sep = matches!(text(k), "." | "," | ":" | "/" | "+" | "-" | "\u{066B}" | "\u{066C}");
        if sep && items[k - 1].1 == Bidi::Num && items[k + 1].1 == Bidi::Num {
            items[k].1 = Bidi::Num;
        }
    }
    // W5: terminators (percent, currency) next to a number belong to it.
    let term =
        |s: &str| !s.is_empty() && s.chars().all(|c| matches!(c, '%' | '‰' | '#' | '$' | '¢'..='¥' | '°' | '\u{066A}' | '\u{20A0}'..='\u{20CF}'));
    for k in 1..n {
        if items[k].1 == Bidi::N && items[k - 1].1 == Bidi::Num && term(text(k)) {
            items[k].1 = Bidi::Num;
        }
    }
    for k in (0..n.saturating_sub(1)).rev() {
        if items[k].1 == Bidi::N && items[k + 1].1 == Bidi::Num && term(text(k)) {
            items[k].1 = Bidi::Num;
        }
    }
    // W7: a number after left-to-right text (or at the start of a left-to-right paragraph) is left-to-right.
    let mut strong = para;
    for it in &mut items {
        match it.1 {
            Bidi::L | Bidi::R => strong = it.1,
            Bidi::Num if strong == Bidi::L => it.1 = Bidi::L,
            _ => {}
        }
    }
    // N1/N2: neutrals between same-direction text take it (numbers count as right-to-left), else the paragraph's.
    let dir = |b: Bidi| if b == Bidi::L { Bidi::L } else { Bidi::R };
    let mut k = 0;
    while k < n {
        if items[k].1 != Bidi::N {
            k += 1;
            continue;
        }
        let mut e = k;
        while e < n && items[e].1 == Bidi::N {
            e += 1;
        }
        let before = if k == 0 { para } else { dir(items[k - 1].1) };
        let after = if e == n { para } else { dir(items[e].1) };
        let resolved = if before == after { before } else { para };
        for it in &mut items[k..e] {
            it.1 = resolved;
        }
        k = e;
    }
    // Levels (I1/I2), then L2: reverse every run at or above each level, highest first.
    let rtl_para = para == Bidi::R;
    let levels: Vec<u8> = items
        .iter()
        .map(|it| match it.1 {
            Bidi::R => 1,
            Bidi::Num => 2,
            _ if rtl_para => 2,
            _ => 0,
        })
        .collect();
    let mut order: Vec<usize> = (0..n).collect();
    for level in (1..=levels.iter().copied().max().unwrap_or(0)).rev() {
        let mut k = 0;
        while k < n {
            if levels[order[k]] < level {
                k += 1;
                continue;
            }
            let mut e = k;
            while e < n && levels[order[e]] >= level {
                e += 1;
            }
            order[k..e].reverse();
            k = e;
        }
    }
    let mut j = 0;
    let mut gap = false;
    for &o in &order {
        match items[o].0 {
            None => gap = true,
            Some(i) => {
                if let (Some(slot), Some(space)) = (idx.get_mut(j), spaces.get_mut(j)) {
                    *slot = i;
                    *space = gap && j > 0;
                }
                gap = false;
                j += 1;
            }
        }
    }
}

fn union(a: &mut [f32; 4], b: &[f32; 4]) {
    a[0] = a[0].min(b[0]);
    a[1] = a[1].min(b[1]);
    a[2] = a[2].max(b[2]);
    a[3] = a[3].max(b[3]);
}

/// Reading-order layout. Glyphs are reordered so that `glyphs` is in reading order, lines are
/// numbered in that order, and word gaps are inferred per line.
///
/// Heuristics (documented so they can be tuned and tested; plan/architecture.md §8):
/// 1. *Segments:* consecutive glyphs (content order) on one baseline with no column-sized gap.
///    A smaller glyph right next to the previous one (a sub- or superscript) stays in the segment.
///    Segments on the same baseline that nearly touch are merged (text drawn out of order).
/// 2. *Blocks:* segments stacked with line-sized gaps and overlapping horizontally.
/// 3. *Block order:* repeatedly take the top-most remaining block, preferring a block to its left
///    that overlaps it vertically (so columns read left, then right). A column goes on first (#3658):
///    the nearest block below the last one, overlapping it horizontally and at most 2.5 heights of
///    its last line below it, comes next, unless an unread block sits above it in its own span, sits
///    beside it on its left, or shares its first line (the first lines overlap by at least 30% of the
///    taller one: a table or form row, which reads across, even with labels and values in different
///    sizes or on shifted baselines).
///    Known limit: when a paragraph in the next column starts on the same line as the continuation
///    (text on a baseline grid), it counts as a row and the top-most rule applies, so the column
///    order of #3658 is lost there. Telling prose from a row in that case needs more evidence.
/// 4. Within a segment glyphs run left to right; runs of right-to-left script are reversed so the
///    text comes out in logical order.
pub fn layout(glyphs: Vec<TextGlyph>) -> PageText {
    let (order, line_of, space_before) = arrange(&glyphs);
    let mut slots: Vec<Option<TextGlyph>> = glyphs.into_iter().map(Some).collect();
    // Each glyph is emitted exactly once.
    let glyphs = order.iter().filter_map(|i| slots.get_mut(*i).and_then(Option::take)).collect();
    PageText { glyphs, line_of, space_before }
}

/// [`layout`] as the reading order of indices into `glyphs`, with each one's line and word gap.
fn arrange(glyphs: &[TextGlyph]) -> (Vec<usize>, Vec<u32>, Vec<bool>) {
    // Drop "fake bold" duplicates: the same character redrawn at (almost) the same place. A redraw
    // also overlaps most of its own width: a narrow letter in condensed text (80 Tz and below) sits
    // closer than a fifth of its height to its double, as the second "l" of "Alle" (#1467), and
    // OCR layers squeeze words to 20 Tz and below (#2457). Only a zero-width mark, which has no
    // width to overlap, is matched by its height alone.
    let mut keep = vec![true; glyphs.len()];
    for i in 1..glyphs.len() {
        let g = &glyphs[i];
        let h = (g.rect[3] - g.rect[1]).max(0.1);
        let w = g.rect[2] - g.rect[0];
        let dx = if w > 0.0 { (h * 0.2).min(w * 0.5) } else { h * 0.2 };
        for j in (i.saturating_sub(4)..i).rev() {
            let p = &glyphs[j];
            if keep[j] && p.text == g.text && (p.rect[0] - g.rect[0]).abs() < dx && (p.rect[1] - g.rect[1]).abs() < h * 0.2 {
                keep[i] = false;
                break;
            }
        }
    }
    let kept: Vec<usize> = (0..glyphs.len()).filter(|i| keep[*i]).collect();
    let glyphs: Vec<&TextGlyph> = kept.iter().map(|i| &glyphs[*i]).collect();
    let n = glyphs.len();
    if n == 0 {
        return Default::default();
    }
    let height = |i: usize| (glyphs[i].rect[3] - glyphs[i].rect[1]).max(0.1);
    let cy = |i: usize| (glyphs[i].rect[1] + glyphs[i].rect[3]) / 2.0;

    // 1. Segments in content order. Every run is a range of consecutive glyphs.
    let mut segs: Vec<Seg> = Vec::new();
    let mut run_start_x = 0.0;
    let mut furthest_progress = 0.0;
    for i in 0..n {
        let g = glyphs[i].rect;
        let cont = segs.last().is_some_and(|s| {
            let Some(p) = s.first.end.checked_sub(1) else { return false };
            let ph = height(p).min(height(i));
            let gap = g[0] - glyphs[p].rect[2];
            // A sub- or superscript is a smaller glyph on a shifted baseline: its em box centre sits off the line by
            // more than half its own height, but by less than half the height of the full-size glyph beside it.
            let centre_limit = if gap.abs() < ph { height(p).max(height(i)) * 0.5 } else { ph * 0.5 };
            (cy(i) - cy(p)).abs() < centre_limit && gap < ph * 3.0 && g[0] > glyphs[p].rect[0] - ph * 2.0
        });
        let reset = cont && segs.last().is_some_and(|s| restarts_run(&glyphs, s.first.start, i, furthest_progress));
        if cont
            && !reset
            && let Some(s) = segs.last_mut()
        {
            s.first.end = i + 1;
            union(&mut s.bbox, &g);
            s.h = s.h.max(height(i));
            furthest_progress = furthest_progress.max((g[0] - run_start_x).abs()).max((g[2] - run_start_x).abs());
        } else {
            segs.push(Seg::new(Run { start: i, end: i + 1 }, g, height(i)));
            run_start_x = g[0];
            furthest_progress = 0.0;
        }
    }
    // A run that redraws an earlier run at the same place (doubled or shadowed text) is dropped, as a doubled glyph is
    // above. Runs are keyed by a fingerprint of their glyph texts and a grid cell a fifth of a line high. A key leads to
    // the latest retained run with it, and `older[k]` links each retained run to the previous one with the same key, so
    // the earlier candidates are checked newest first, up to `REDRAW_CANDIDATES`. Both exist only for this pass.
    let retained = {
        let candidates = segs.iter().filter(|s| s.first.len() >= 2).count();
        let mut latest: HashMap<(u64, i32, i32), usize> = HashMap::with_capacity(candidates);
        let mut older = vec![NONE; segs.len()];
        let mut kept_runs = 0;
        for i in 0..segs.len() {
            let run = segs[i].first;
            if run.len() >= 2 {
                let tol = 0.2 * segs[i].h;
                let (cx, cy_) = ((glyphs[run.start].rect[0] / tol).floor() as i32, (glyphs[run.start].rect[1] / tol).floor() as i32);
                let fp = fingerprint(&glyphs, run);
                let mut checks = REDRAW_CANDIDATES;
                let twin = (-1..=1i32).any(|dx| {
                    (-1..=1i32).any(|dy| {
                        let mut link = latest.get(&(fp, cx.saturating_add(dx), cy_.saturating_add(dy))).copied();
                        while let Some(k) = link {
                            let Some(prev) = segs.get(k) else { break };
                            if checks == 0 {
                                return false;
                            }
                            checks -= 1;
                            if redraws(&glyphs, prev.first, run) {
                                return true;
                            }
                            link = older.get(k).copied().filter(|&o| o != NONE);
                        }
                        false
                    })
                });
                if twin {
                    continue;
                }
                older[kept_runs] = latest.insert((fp, cx, cy_), kept_runs).unwrap_or(NONE);
            }
            segs.swap(kept_runs, i);
            kept_runs += 1;
        }
        kept_runs
    };
    segs.truncate(retained);
    // Merge same-baseline segments that nearly touch.
    let mut merged = true;
    while merged {
        merged = false;
        'outer: for a in 0..segs.len() {
            for b in 0..segs.len() {
                if a == b {
                    continue;
                }
                let (sa, sb) = (&segs[a], &segs[b]);
                let h = sa.h.min(sb.h);
                let gap = (sb.bbox[0] - sa.bbox[2]).max(sa.bbox[0] - sb.bbox[2]);
                if (sa.cy() - sb.cy()).abs() < h * 0.3 && (sa.h / sb.h - 1.0).abs() < 0.35 && gap < h * 1.2 {
                    let sb = segs.remove(b);
                    let a = if b < a { a - 1 } else { a };
                    segs[a].merge(sb);
                    merged = true;
                    break 'outer;
                }
            }
        }
    }

    // 2. Blocks.
    let mut by_top: Vec<usize> = (0..segs.len()).collect();
    by_top.sort_by(|a, b| segs[*a].bbox[1].total_cmp(&segs[*b].bbox[1]));
    let mut blocks: Vec<(Vec<usize>, [f32; 4])> = Vec::new();
    for si in by_top {
        let s = &segs[si];
        let target = blocks.iter().position(|(members, bb)| {
            let Some(&m) = members.last() else { return false };
            let last = &segs[m];
            let vgap = s.bbox[1] - last.bbox[3];
            let overlap = s.bbox[2].min(bb[2]) - s.bbox[0].max(bb[0]);
            let minw = (s.bbox[2] - s.bbox[0]).min(bb[2] - bb[0]).max(1.0);
            vgap > -last.h * 0.5 && vgap < last.h.max(s.h) * 1.1 && overlap > minw * 0.3 && (last.h / s.h - 1.0).abs() < 0.6
        });
        match target {
            Some(b) => {
                blocks[b].0.push(si);
                let bb = s.bbox;
                union(&mut blocks[b].1, &bb);
            }
            None => blocks.push((vec![si], s.bbox)),
        }
    }

    // 3. Block order.
    let mut remaining: Vec<usize> = (0..blocks.len()).collect();
    let mut block_order: Vec<usize> = Vec::with_capacity(blocks.len());
    // A block to the left of `cb` that shares at least half the height of the shorter one: it reads first.
    let beside_left = |cb: &[f32; 4], lb: &[f32; 4]| {
        let v = lb[3].min(cb[3]) - lb[1].max(cb[1]);
        lb[2] <= cb[0] + 1.0 && v > 0.5 * (lb[3] - lb[1]).min(cb[3] - cb[1])
    };
    // The box of a block's first line. Every block is created with one segment, so the block's own box is a fallback
    // that is never used.
    let first_line = |b: usize| blocks[b].0.first().and_then(|&s| segs.get(s)).map_or(blocks[b].1, |s| s.bbox);
    while let Some(&top) = remaining.iter().min_by(|a, b| blocks[**a].1[1].total_cmp(&blocks[**b].1[1])) {
        // A column goes on past a paragraph gap (#3658): the nearest block below the one just read that overlaps it
        // horizontally comes next, if the gap above it is at most two and a half heights of the last line read (a
        // footer, a footnote or a caption under a figure is further) and no other unread block
        // - sits above it in its own horizontal span (the rest of a column above a full-width heading or footnote),
        // - is beside it on its left, or
        // - shares its first line: their first lines overlap by at least 30% of the taller one (a table or form row,
        //   which reads across, even with labels and values in different sizes or on shifted baselines; a line that
        //   only touches it, or a small superscript or footnote marker, is not a row).
        let next_in_column = block_order.last().and_then(|&prev| {
            let pb = blocks[prev].1;
            // The gap is measured in the height of the block's last line, which borders it.
            let pl = blocks[prev].0.last().and_then(|&s| segs.get(s)).map_or(pb, |s| s.bbox);
            let tol = (pl[3] - pl[1]).max(0.1) * 0.5;
            let c = remaining
                .iter()
                .copied()
                .filter(|&c| {
                    let cb = blocks[c].1;
                    let overlap = cb[2].min(pb[2]) - cb[0].max(pb[0]);
                    let minw = (cb[2] - cb[0]).min(pb[2] - pb[0]).max(1.0);
                    cb[1] >= pb[3] - tol && overlap > minw * 0.3
                })
                .min_by(|a, b| blocks[*a].1[1].total_cmp(&blocks[*b].1[1]))?;
            let cb = blocks[c].1;
            if cb[1] - pb[3] >= tol * 5.0 {
                return None;
            }
            let cl = first_line(c);
            let blocked = remaining.iter().any(|&r| {
                if r == c {
                    return false;
                }
                let rb = blocks[r].1;
                let rl = first_line(r);
                let shared = rl[3].min(cl[3]) - rl[1].max(cl[1]);
                let row = shared > 0.3 * (rl[3] - rl[1]).max(cl[3] - cl[1]);
                (rb[1] < cb[1] && rb[2].min(cb[2]) > rb[0].max(cb[0])) || beside_left(&cb, &rb) || row
            });
            (!blocked).then_some(c)
        });
        let pick = next_in_column.unwrap_or_else(|| {
            let tb = blocks[top].1;
            remaining
                .iter()
                .copied()
                .filter(|c| beside_left(&tb, &blocks[*c].1))
                .min_by(|a, b| blocks[*a].1[0].total_cmp(&blocks[*b].1[0]))
                .unwrap_or(top)
        });
        remaining.retain(|r| *r != pick);
        block_order.push(pick);
    }

    // 4. Emit each segment in block order. Its glyphs are placed here, into scratch that every segment reuses, and
    // appended straight to the output. Blocks and their order use only segment boxes, so placement can wait until now.
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut line_of: Vec<u32> = Vec::with_capacity(n);
    let mut space_before: Vec<bool> = Vec::with_capacity(n);
    let mut line = 0u32;
    let mut sc = Scratch::default();
    // `spaces[w]`: a word gap left of the w-th glyph of the segment, by position on the line, not by drawing order
    // (#1062). Reused by every segment.
    let mut spaces: Vec<bool> = Vec::new();
    for b in block_order {
        for &si in &blocks[b].0 {
            place_glyphs(&glyphs, &height, &segs[si], &mut sc);
            let h = segs[si].h;
            // Word gaps relative to this line's typical letter gap (handles tracking).
            // Long runs of repeated punctuation belong to leaders, not letter tracking: a TOC leader
            // must not raise the threshold above the word gaps in its title (#3086).
            sc.gaps.clear();
            sc.gaps.extend(sc.placed.windows(2).enumerate().filter_map(|(w, pair)| {
                let &[p, c] = pair else { return None };
                let (p, c) = (glyphs.get(p)?, glyphs.get(c)?);
                if p.text == c.text && p.text.chars().all(|ch| !ch.is_alphanumeric() && !ch.is_whitespace()) {
                    // Keep short tracked punctuation, including an ellipsis with a final period.
                    // Treat five or more matching glyphs as a leader. Three neighbours on each
                    // side suffice to classify this pair, bounding work even on long runs.
                    let same = |&&i: &&usize| glyphs.get(i).is_some_and(|g| g.text == p.text);
                    let before = sc.placed.get(..w).unwrap_or_default().iter().rev().take(3).take_while(same).count();
                    let after = sc.placed.get(w.saturating_add(2)..).unwrap_or_default().iter().take(3).take_while(same).count();
                    if before + after >= 3 {
                        return None;
                    }
                }
                Some(c.rect[0] - p.rect[2])
            }));
            sc.gaps.sort_by(f32::total_cmp);
            let typical = sc.gaps.get(sc.gaps.len() / 3).copied().unwrap_or(0.0).max(0.0);
            let threshold = (typical + h * WORD_GAP).max(h * WORD_GAP);
            spaces.clear();
            spaces.resize(sc.placed.len(), false);
            for (w, pair) in sc.placed.windows(2).enumerate() {
                let &[p, c] = pair else { continue };
                let gap = glyphs[c].rect[0] - glyphs[p].rect[2];
                let (pt, ct) = (&glyphs[p].text, &glyphs[c].text);
                let cjk = pt.chars().any(is_cjk) && ct.chars().any(is_cjk) && gap < h * 0.5;
                let tight_cluster = (pt.chars().any(is_complex) || ct.chars().any(is_complex)) && gap < h * 0.6;
                let run_start = sc.breaks.binary_search(&c).is_ok();
                if let Some(space) = spaces.get_mut(w + 1) {
                    *space = (gap > threshold || run_start) && !cjk && !tight_cluster && !pt.trim().is_empty() && !ct.trim().is_empty();
                }
            }
            // Right-to-left spans are reversed (visual → logical) with their gaps, and only then is a second copy made.
            let has_rtl = sc.placed.iter().any(|&i| glyphs[i].text.chars().any(is_rtl));
            if has_rtl {
                sc.logical.clear();
                sc.logical.extend_from_slice(&sc.placed);
                reverse_rtl(&glyphs, &mut sc.logical, &mut spaces);
            }
            let idx: &[usize] = if has_rtl { &sc.logical } else { &sc.placed };
            order.extend_from_slice(idx);
            line_of.resize(order.len(), line);
            // `spaces[0]` stays false: the first glyph of a segment never takes a space.
            space_before.extend(spaces.iter().take(idx.len()));
            space_before.resize(order.len(), false);
            line += 1;
        }
    }
    (order.into_iter().map(|i| kept[i]).collect(), line_of, space_before)
}

#[cfg(test)]
mod tests;
