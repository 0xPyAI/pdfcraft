//! Combine files as a grid: a thumbnail of the first page each file contributes, drawn as a
//! stack when it contributes more than one, with its name below. Cards are selected and moved
//! with the same [`RowAction`]s as the table's rows, so both views share the selection, the
//! order and the undo history.
//!
//! Thumbnails render off the UI thread through [`RenderPool`] (its panic guard, size caps and
//! watchdog), one small pool per thumbnail. Only missing ones are asked for, those in view first.
//! At most [`MAX_THREADS`] render threads exist at once, counted as threads, not pools: the pools
//! start no watchdog replacements ([`RenderPool::without_replacements`]), and a pool's threads
//! count until they have actually exited ([`RenderPool::retire`]), so a render the watchdog gave
//! up on, or one whose file left the list, holds its place until its thread is gone. A thread
//! stuck for good in one step that can't be interrupted keeps its place for good: threads can't
//! be killed safely. On the web the pool renders on the UI thread inside `try_recv`, with no
//! watchdog: there, at most one thumbnail per frame, only one waits at a time (its pool holds the
//! parsed document), and a very slow page still stalls that frame (as in the document view).

use std::collections::HashMap;
use std::sync::Arc;

use egui::{Align2, Color32, CornerRadius, Rect, Sense, Stroke, TextureHandle, Vec2, pos2, vec2};
use pdfcraft_render::{RenderConfig, RenderPool, RenderRequest, Retiring};

use crate::PdfCraftApp;
use crate::combine_ui::{CombineFile, ERROR, Lock, RowAction, WARNING};
use crate::icons;
use crate::theme::{self, Tokens};

/// Render threads for thumbnails at once, counting those of retired pools until they exit.
const MAX_THREADS: usize = 3;
/// Results taken per frame, and the bytes they may upload.
const UPLOADS_PER_FRAME: usize = 4;
const UPLOAD_BYTES_PER_FRAME: usize = 8 << 20;
/// Thumbnail textures kept, in bytes. Those out of view go first, longest unseen first; those in
/// view are kept even over budget (at most a screenful of thumbnails of at most [`MAX_SIDE`]).
const TEXTURE_BUDGET: usize = 32 << 20;
/// The longest side of a thumbnail, in pixels.
/// (Room for a Letter page at 500% on a screen of 1.5 pixels per point; sharper screens upscale a
/// little at the largest sizes.) This only binds near the top of the zoom range: a card below it
/// asks for less than this and costs no more than it did. Near the top one thumbnail is a few
/// megabytes, so [`TEXTURE_BUDGET`] and [`UPLOAD_BYTES_PER_FRAME`] keep the cache at a screenful
/// plus one frame's uploads — and at the top of the range a screenful is a card or two, because a
/// card is wider than most windows.
const MAX_SIDE: f32 = 1536.0;
/// Thumbnail sizes round up to this many pixels, so a slightly different layout reuses them.
const SIZE_STEP: u32 = 16;
/// The box a page is fitted in at 100%, and the room the card adds around it (for the hover bar
/// and the stack above it, the name and a note below), which doesn't scale.
const PAPER: Vec2 = vec2(132.0, 172.0);
const AROUND: Vec2 = vec2(52.0, 90.0);
/// From the top of a card to its page box: room for the hover bar, which sits mostly above the
/// page.
const ABOVE: f32 = 32.0;
/// The hover bar's height; a quarter of it overlaps the page.
const BAR_H: f32 = 34.0;
/// The grid's zoom range, and the sizes its buttons and keys step through (the slider, the wheel
/// and a pinch go anywhere between).
pub(crate) const ZOOM_RANGE: std::ops::RangeInclusive<f32> = 0.6..=5.0;
const ZOOM_STEPS: [f32; 11] = [0.6, 0.8, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 5.0];

/// The next size up (or down) from `zoom`; the end of the range stays put.
pub(crate) fn zoom_step(zoom: f32, larger: bool) -> f32 {
    let next =
        if larger { ZOOM_STEPS.iter().copied().find(|s| *s > zoom + 0.001) } else { ZOOM_STEPS.iter().rev().copied().find(|s| *s < zoom - 0.001) };
    next.unwrap_or(if larger { *ZOOM_RANGE.end() } else { *ZOOM_RANGE.start() })
}
/// Space above the first row of cards.
const TOP: f32 = 12.0;
/// A thumbnail's place: the file (by id) and the page (0-based, in the file). A file's card and
/// the card of its first page when expanded show the same thumbnail.
type Slot = (u64, usize);
/// The page shown large (a card's magnifier) is kept beside the thumbnails under this slot, so it
/// shares their threads, their limits and their checks.
const PREVIEW: Slot = (u64::MAX, 0);
/// The longest side of the page shown large, in pixels.
const PREVIEW_SIDE: f32 = 2048.0;
/// A file whose render is done keeps its document open this long (seconds), so the next page of
/// the same file renders without reading the whole file again.
const KEEP_OPEN: f64 = 2.0;
/// The most pages one file can be spread out into (each is a card, laid out every frame).
pub(crate) const MAX_EXPAND: usize = 2000;
/// The shape drawn for a page whose size is unknown (US Letter); nothing is rendered for it.
const FALLBACK_PAGE: (f32, f32) = (612.0, 792.0);
/// Icons drawn on the (always white) paper, in either theme: the light theme's muted text, which
/// keeps its contrast on white where the dark theme's doesn't.
const ON_PAPER: Color32 = Color32::from_rgb(0x5E, 0x5E, 0x66);

/// What the grid has for a file (tests and the control channel).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThumbState {
    /// Nothing yet: not in view, or nothing to draw (locked, unreadable, a bad page range, a page
    /// whose size is unknown).
    None,
    /// Being rendered.
    Pending,
    /// A thumbnail of page `page` (0-based), `width` × `height` pixels.
    Ready { page: usize, width: u32, height: u32 },
    /// The page couldn't be drawn.
    Failed(String),
}

/// A card being dragged: which file, and the list as it was (see [`RowAction::DropAt`]).
#[derive(Clone, Copy)]
struct GridDrag {
    file: u64,
    revision: u64,
    /// A page card's page (entry id, place among its pages): that page moves, not its file.
    page: Option<(u64, usize)>,
}

/// Everything a thumbnail depends on: the file's bytes, how it was read (its password), the page
/// and the size. A cached thumbnail is used only while all of them still match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    /// The address of the file's shared bytes: the same file, even after undo brings it back.
    source: usize,
    auth: u64,
    page: usize,
    px: [u32; 2],
}

impl Key {
    fn same_page(&self, other: &Key) -> bool {
        (self.source, self.auth, self.page) == (other.source, other.auth, other.page)
    }
}

struct Entry {
    key: Key,
    tex: Option<TextureHandle>,
    error: Option<String>,
    /// The frame the card last showed it.
    seen: u64,
    bytes: usize,
}

struct Job {
    slot: Slot,
    key: Key,
    pool: RenderPool,
    tag: u64,
}

/// A pool whose render is done, with its document still open: kept a moment for the next page of
/// the same file, read the same way.
struct Open {
    source: usize,
    auth: u64,
    pool: RenderPool,
    /// When its render finished (seconds).
    since: f64,
}

/// A thumbnail a card needs this frame.
struct Want {
    slot: Slot,
    key: Key,
    bytes: Arc<Vec<u8>>,
    password: Option<Arc<str>>,
    scale: f32,
    visible: bool,
}

/// The grid's thumbnails and the renders under way.
#[derive(Default)]
pub(crate) struct Thumbs {
    entries: HashMap<Slot, Entry>,
    jobs: Vec<Job>,
    /// Pools done rendering, their documents still open (their threads count).
    open: Vec<Open>,
    /// Threads of pools let go of, until they exit.
    retiring: Vec<Retiring>,
    frame: u64,
    next_tag: u64,
    wants: Vec<Want>,
    /// Render on the UI thread, as the web build does (tests).
    #[cfg(test)]
    inline: bool,
}

/// What a card can draw now.
enum Look {
    Ready(egui::TextureId),
    Failed(String),
    /// Not ready; an older thumbnail of the same page may stand in.
    Pending(Option<egui::TextureId>),
}

impl Thumbs {
    /// Forget every thumbnail (the tab closed). Renders under way are let go of: told to stop at
    /// their next drawing operator, and counted until their threads exit.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.wants.clear();
        for job in self.jobs.drain(..) {
            self.retiring.push(job.pool.retire());
        }
        for open in self.open.drain(..) {
            self.retiring.push(open.pool.retire());
        }
    }

    /// Render threads that exist now (real threads, not pools): those of pools at work, of pools
    /// keeping a document open, and of pools let go of.
    fn threads(&mut self) -> usize {
        self.retiring.retain_mut(|r| r.running() > 0);
        let retiring: usize = self.retiring.iter_mut().map(Retiring::running).sum();
        let working: usize = self.jobs.iter().map(|j| j.pool.threads()).sum();
        let open: usize = self.open.iter().map(|o| o.pool.threads()).sum();
        working.saturating_add(open).saturating_add(retiring)
    }

    fn look(&mut self, slot: Slot, key: Key) -> Look {
        let frame = self.frame;
        match self.entries.get_mut(&slot) {
            Some(e) if e.key == key => {
                e.seen = frame;
                match (&e.tex, &e.error) {
                    (Some(tex), _) => Look::Ready(tex.id()),
                    (None, Some(err)) => Look::Failed(err.clone()),
                    (None, None) => Look::Pending(None),
                }
            }
            // Another size of the same page stands in until this one arrives. A page that failed
            // isn't tried again at another size: it would fail (or hang) the same way, each zoom.
            Some(e) if e.key.same_page(&key) => {
                e.seen = frame;
                match &e.error {
                    Some(err) if e.tex.is_none() => Look::Failed(err.clone()),
                    _ => Look::Pending(e.tex.as_ref().map(TextureHandle::id)),
                }
            }
            _ => Look::Pending(None),
        }
    }

    fn state(&self, slot: Slot) -> ThumbState {
        if let Some(e) = self.entries.get(&slot) {
            if let Some(tex) = &e.tex {
                let [w, h] = tex.size();
                return ThumbState::Ready {
                    page: e.key.page,
                    width: u32::try_from(w).unwrap_or(u32::MAX),
                    height: u32::try_from(h).unwrap_or(u32::MAX),
                };
            }
            if let Some(err) = &e.error {
                return ThumbState::Failed(err.clone());
            }
        }
        if self.jobs.iter().any(|j| j.slot == slot) { ThumbState::Pending } else { ThumbState::None }
    }

    /// Once per frame, whatever shows: drop what no longer matches the list, take finished
    /// renders, start those the grid asked for and keep the textures within budget. `current`
    /// says what a thumbnail must match now (looked up, so the cost follows the files and the
    /// thumbnails kept, not the pages of an expanded file).
    fn finish(&mut self, ctx: &egui::Context, current: &dyn Fn(Slot) -> Option<Key>) {
        let now = ctx.input(|i| i.time);
        // Reconcile with the list as it is now (undo, redo, unlock, a new range, removal): a
        // thumbnail of another page, password or file is never shown again.
        self.entries.retain(|slot, e| current(*slot).is_some_and(|k| k.same_page(&e.key)));
        // Renders nobody wants any more are let go of now (on the web, before they would render).
        let (keep, gone): (Vec<Job>, Vec<Job>) = self.jobs.drain(..).partition(|job| current(job.slot).is_some_and(|k| k.same_page(&job.key)));
        self.jobs = keep;
        self.retiring.extend(gone.into_iter().map(|job| job.pool.retire()));
        // Documents kept open past their time are closed.
        let (keep, gone): (Vec<Open>, Vec<Open>) = self.open.drain(..).partition(|o| now - o.since < KEEP_OPEN);
        self.open = keep;
        self.retiring.extend(gone.into_iter().map(|o| o.pool.retire()));

        // Finished renders, a few per frame. A pool without threads (the web) renders inside
        // `try_recv`: at most one of those per frame.
        let gpu_side = ctx.input(|i| i.max_texture_side);
        let max_side = gpu_side.min(2 * MAX_SIDE as usize);
        let (mut taken, mut uploaded, mut inline_used) = (0usize, 0usize, false);
        let mut i = 0;
        while i < self.jobs.len() {
            if taken >= UPLOADS_PER_FRAME || uploaded >= UPLOAD_BYTES_PER_FRAME {
                break;
            }
            let Some(job) = self.jobs.get(i) else { break };
            if job.pool.is_inline() {
                if inline_used {
                    i += 1;
                    continue;
                }
                inline_used = true;
            }
            // Not done yet, or not this job's answer (cannot happen with one request per pool):
            // keep waiting. A pool without threads answers on the call that renders, so there no
            // answer is one that never comes: the thumbnail fails rather than hold up the rest.
            let answer = job.pool.try_recv().filter(|out| out.request.tag == job.tag);
            if answer.is_none() && !job.pool.is_inline() {
                i += 1;
                continue;
            }
            let job = self.jobs.swap_remove(i);
            taken += 1;
            // A pool that rendered (rather than failed or hung) can keep its document open.
            let healthy = answer.as_ref().is_some_and(|out| out.error.is_none());
            let failed = |e: String| Entry { key: job.key, tex: None, error: Some(e), seen: self.frame, bytes: 0 };
            let entry = match answer {
                None => failed("the page wasn't drawn".to_owned()),
                Some(out) => match out.error {
                    Some(e) => failed(e),
                    None => match texture(
                        ctx,
                        job.slot,
                        [out.width, out.height],
                        out.rgba,
                        if job.slot == PREVIEW { gpu_side.min(PREVIEW_SIDE as usize) } else { max_side },
                    ) {
                        Ok((tex, bytes)) => {
                            uploaded = uploaded.saturating_add(bytes);
                            Entry { key: job.key, tex: Some(tex), error: None, seen: self.frame, bytes }
                        }
                        Err(e) => failed(e),
                    },
                },
            };
            self.entries.insert(job.slot, entry);
            // Its document stays open a moment for the file's next page (its thread, idle, still
            // counts); otherwise its thread (idle, or finishing a render the watchdog gave up on)
            // counts until it exits.
            if healthy && job.pool.threads() <= 1 && self.open.len() < MAX_THREADS {
                self.open.push(Open { source: job.key.source, auth: job.key.auth, pool: job.pool, since: now });
            } else {
                self.retiring.push(job.pool.retire());
            }
        }

        // New renders: the cards in view first, in grid order; one per thumbnail at a time.
        let mut wants = std::mem::take(&mut self.wants);
        wants.sort_by_key(|w| (w.slot != PREVIEW, !w.visible));
        // A pool without threads (the web) parses its document when it is made and keeps it until
        // it renders, one per frame: while one waits, nothing else starts.
        // (Nor in the frame that rendered one: one heavy step per frame on the UI thread.)
        let mut threads = if inline_used || self.jobs.iter().any(|j| j.pool.is_inline()) { MAX_THREADS } else { self.threads() };
        let mut waiting = false;
        for w in wants {
            // Wanted for the list as it is now (not changed by this frame's action); one render
            // per thumbnail at a time; and a want made earlier this frame may have just been
            // answered.
            if current(w.slot).is_none_or(|k| !k.same_page(&w.key))
                || self.jobs.iter().any(|j| j.slot == w.slot)
                || self.entries.get(&w.slot).is_some_and(|e| e.key == w.key)
            {
                continue;
            }
            // The same file, read the same way, still open from a render just done: no new read of
            // the file (and its thread is already counted). On the web, not while another waits.
            let inline_waiting = self.jobs.iter().any(|j| j.pool.is_inline());
            if let Some(k) =
                self.open.iter().position(|o| o.source == w.key.source && o.auth == w.key.auth && !(o.pool.is_inline() && inline_waiting))
            {
                let pool = self.open.swap_remove(k).pool;
                self.next_tag = self.next_tag.wrapping_add(1);
                let tag = self.next_tag;
                pool.set_queue(vec![RenderRequest { page: w.key.page, scale: w.scale, tag, ..Default::default() }]);
                if pool.is_inline() {
                    threads = MAX_THREADS;
                }
                self.jobs.push(Job { slot: w.slot, key: w.key, pool, tag });
                continue;
            }
            if threads >= MAX_THREADS {
                // A document kept open for another file gives its thread up: it exits, and this
                // starts then.
                if let Some(k) = self.open.iter().enumerate().min_by(|(_, a), (_, b)| a.since.total_cmp(&b.since)).map(|(k, _)| k) {
                    let open = self.open.swap_remove(k);
                    self.retiring.push(open.pool.retire());
                }
                waiting = true;
                break;
            }
            let config = RenderConfig { password: w.password, ..Default::default() };
            #[cfg(test)]
            let pool = if self.inline { RenderPool::new_inline(w.bytes, config) } else { RenderPool::new(w.bytes, 1, config).without_replacements() };
            #[cfg(not(test))]
            let pool = RenderPool::new(w.bytes, 1, config).without_replacements();
            self.next_tag = self.next_tag.wrapping_add(1);
            let tag = self.next_tag;
            // The request stays in the pool's queue (it is never replaced) until its answer.
            pool.set_queue(vec![RenderRequest { page: w.key.page, scale: w.scale, tag, ..Default::default() }]);
            threads = if pool.is_inline() { MAX_THREADS } else { threads.saturating_add(1) };
            self.jobs.push(Job { slot: w.slot, key: w.key, pool, tag });
        }

        // Within budget: drop the thumbnails out of view longest unseen first.
        // (The page shown large has its own: one texture, gone when it closes.)
        let mut total: usize = self.entries.iter().filter(|(f, _)| **f != PREVIEW).map(|(_, e)| e.bytes).sum();
        if total > TEXTURE_BUDGET {
            let mut old: Vec<(u64, Slot, usize)> = self
                .entries
                .iter()
                .filter(|(f, e)| **f != PREVIEW && e.seen != self.frame && e.bytes > 0)
                .map(|(f, e)| (e.seen, *f, e.bytes))
                .collect();
            old.sort_unstable();
            for (_, slot, bytes) in old {
                if total <= TEXTURE_BUDGET {
                    break;
                }
                self.entries.remove(&slot);
                total = total.saturating_sub(bytes);
            }
        }

        // The cards were drawn before these results arrived: draw them again. Keep polling while
        // renders are under way, and while a thumbnail waits for a thread to exit (not when
        // nothing waits: a thread exiting changes nothing on screen).
        if taken > 0 || self.jobs.iter().any(|j| j.pool.is_inline()) {
            ctx.request_repaint();
        } else if !self.jobs.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        } else if waiting {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        } else if !self.open.is_empty() {
            // To close the documents kept open when their time is up.
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(KEEP_OPEN));
        }
        self.frame = self.frame.wrapping_add(1);
    }
}

/// A rendered thumbnail as a texture, and its size in bytes. A raster that doesn't add up, or is
/// larger than a thumbnail can be (or than the GPU accepts), is refused, never uploaded.
fn texture(
    ctx: &egui::Context,
    slot: Slot,
    size: [u32; 2],
    pixels: pdfcraft_render::Pixels,
    max_side: usize,
) -> Result<(TextureHandle, usize), String> {
    let [w, h] = size.map(|v| v as usize);
    if w == 0 || h == 0 || w > max_side || h > max_side {
        return Err(format!("the thumbnail came back {w}×{h} pixels"));
    }
    let bytes = w.checked_mul(h).and_then(|p| p.checked_mul(4)).ok_or("the thumbnail is too large")?;
    if pixels.len() != bytes {
        return Err("the thumbnail came back incomplete".to_owned());
    }
    let image = crate::canvas::texture_image([w, h], pixels);
    Ok((ctx.load_texture(format!("combine-thumb-{}-{}", slot.0, slot.1), image, egui::TextureOptions::LINEAR), bytes))
}

/// `name` shortened in the middle to what `fits`, keeping its extension ("Pilot Boat Ad….pdf").
pub(crate) fn fit_name(name: &str, fits: impl Fn(&str) -> bool) -> String {
    if fits(name) {
        return name.to_owned();
    }
    // A short extension after the last dot (a dot is one byte, so these are char boundaries).
    let split = name.rfind('.').filter(|p| *p > 0 && name.len() - p <= 6);
    let (stem, ext) = match split {
        Some(p) => (name.get(..p).unwrap_or(name), name.get(p..).unwrap_or("")),
        None => (name, ""),
    };
    let chars: Vec<char> = stem.chars().collect();
    let with = |n: usize| format!("{}…{ext}", chars.iter().take(n).collect::<String>());
    // The longest start of the stem that fits, found by halving.
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(&with(mid)) { lo = mid } else { hi = mid - 1 }
    }
    with(lo)
}

/// The box a page is fitted in at `zoom` (clamped to [`ZOOM_RANGE`]).
fn paper_box(zoom: f32) -> Vec2 {
    let zoom = if zoom.is_finite() { zoom.clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end()) } else { 1.0 };
    PAPER * zoom
}

/// The size to draw a page of `w` × `h` points in `fit`; `None` for a size that isn't one.
fn paper_size(w: f32, h: f32, fit: Vec2) -> Option<Vec2> {
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        return None;
    }
    let s = (fit.x / w).min(fit.y / h);
    s.is_finite().then(|| vec2(w * s, h * s))
}

/// How many cards of `cell` fit across `width` (at least one).
fn columns_in(width: f32, cell: Vec2) -> usize {
    ((width - 16.0) / cell.x).floor().max(1.0) as usize
}

/// What one place in the grid shows: a file's card, or (the file expanded) one of the pages it
/// adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cell {
    /// The file, by its place in the list.
    pub(crate) file: usize,
    /// For a page: its place among the pages the file adds, the page (0-based, in the file) and
    /// how many it adds.
    pub(crate) page: Option<(usize, usize, usize)>,
}

/// The pages an expanded file shows: in its range's order, and sorted (to look one up), for the
/// range and page count they were worked out for.
#[derive(Clone, Debug)]
pub(crate) struct PageList {
    of: (String, usize),
    pub(crate) order: Arc<[usize]>,
    sorted: Arc<[usize]>,
}

/// The pages an expanded file shows, worked out again only when its range or page count
/// changes; `None` when it can't be shown as its pages (locked, unreadable, a bad range, no page,
/// or more than [`MAX_EXPAND`]).
pub(crate) fn pages_shown(cache: &mut HashMap<u64, PageList>, f: &CombineFile) -> Option<PageList> {
    if f.lock.is_some() || f.problem.is_some() {
        return None;
    }
    let list = match cache.get(&f.id) {
        Some(list) if list.of.0 == f.range && list.of.1 == f.pages => list.clone(),
        _ => {
            let order = f.pages_taken().ok()?;
            let mut sorted = order.clone();
            sorted.sort_unstable();
            let list = PageList { of: (f.range.clone(), f.pages), order: Arc::from(order), sorted: Arc::from(sorted) };
            cache.insert(f.id, list.clone());
            list
        }
    };
    // (One page shows as a page card too: a file split into parts may leave one page in a part.)
    (!list.order.is_empty() && list.order.len() <= MAX_EXPAND).then_some(list)
}

/// The grid's places, in order: each file's card, or its pages when it is expanded.
fn layout(app: &mut PdfCraftApp) -> Vec<Cell> {
    let tab = &mut app.combine_tab;
    let mut cells = Vec::with_capacity(app.combine_draft.len());
    for (file, f) in app.combine_draft.iter().enumerate() {
        match tab.expanded.contains(&f.id).then(|| pages_shown(&mut tab.page_lists, f)).flatten() {
            Some(list) => {
                let of = list.order.len();
                cells.extend(list.order.iter().enumerate().map(|(nth, &page)| Cell { file, page: Some((nth, page, of)) }));
            }
            None => cells.push(Cell { file, page: None }),
        }
    }
    cells
}

/// The file gap (0 = before the first file, n = after the last) a drop before place `at` means: a
/// drop inside an expanded file's pages goes to the nearer end of that file.
fn file_gap(cells: &[Cell], at: usize, files: usize) -> usize {
    match cells.get(at) {
        None => files,
        Some(Cell { file, page: Some((nth, _, of)) }) if *nth > 0 => {
            if nth.saturating_mul(2) < *of {
                *file
            } else {
                file.saturating_add(1)
            }
        }
        Some(c) => c.file,
    }
}

/// The grid of files, in the card that holds the list. Returns what the user did.
pub(crate) fn grid(
    app: &mut PdfCraftApp,
    ui: &mut egui::Ui,
    t: &Tokens,
    checks: &[Result<usize, String>],
    selected: &[bool],
    twice: &[bool],
) -> Option<RowAction> {
    let ctx = ui.ctx().clone();
    let ppp = ctx.pixels_per_point();
    let mut action = None;
    let revision = app.combine_tab.revision;
    let drag = egui::DragAndDrop::payload::<GridDrag>(&ctx).map(|d| *d);
    // The card dragged may scroll out of the rows drawn, so its release is watched here.
    let released = drag.is_some() && ctx.input(|i| i.pointer.any_released());
    let n = app.combine_draft.len();
    let places = layout(app);
    // Each file's first place (for the keys and to scroll to it).
    let mut first_place = vec![0usize; n];
    for (at, c) in places.iter().enumerate().rev() {
        if let Some(slot) = first_place.get_mut(c.file) {
            *slot = at;
        }
    }
    app.combine_tab.grid_places = places.iter().map(|c| c.file).collect();
    // Pages counted over the list (an entry that can't be split counting as one, see
    // `moved_pages`): where each place's pages start, for a page dropped before it.
    let counts: Vec<usize> = app
        .combine_draft
        .iter_mut()
        .map(|f| if f.lock.is_none() && f.problem.is_none() { f.selection().ok().filter(|k| *k > 0).unwrap_or(1) } else { 1 })
        .collect();
    let mut page_at = Vec::with_capacity(places.len());
    let mut total_pages = 0usize;
    for c in &places {
        page_at.push(total_pages);
        total_pages = total_pages.saturating_add(if c.page.is_some() { 1 } else { counts.get(c.file).copied().unwrap_or(1) });
    }
    let picked_pages = app.combine_tab.selected_pages.clone();
    let reveal = app.combine_tab.reveal.take();
    let fit = paper_box(app.combine_zoom);
    let cell = fit + AROUND;
    let mut cells: Vec<(usize, Rect)> = Vec::new();
    let viewport = ui.available_rect_before_wrap();
    let pointer = ctx.pointer_latest_pos();
    let files = &mut app.combine_draft;
    let thumbs = &mut app.combine_thumbs;
    let columns = &mut app.combine_tab.grid_columns;

    // Over the grid itself, not over a menu or dialog above it.
    let over_grid = ui.rect_contains_pointer(viewport);
    let mut width = viewport.width();
    let mut focused_card = None;
    // A zoom in the previous frame asks for a new scroll position. 0.37 keeps `State`'s offset
    // private, so the request is applied through the builder and consumed here.
    let pending_offset = ctx.data_mut(|d| d.remove_temp::<Vec2>(pending_offset_id()));
    let mut grid = egui::ScrollArea::vertical().id_salt("combine-grid").auto_shrink([false, false]);
    if let Some(offset) = pending_offset {
        grid = grid.vertical_scroll_offset(offset.y);
    }
    grid.show_viewport(ui, |ui, clip| {
        // Dragging near the top or bottom edge scrolls.
        if drag.is_some()
            && let Some(p) = pointer
            && viewport.contains(p)
        {
            let edge = 40.0;
            let delta = if p.y < viewport.top() + edge {
                12.0
            } else if p.y > viewport.bottom() - edge {
                -12.0
            } else {
                0.0
            };
            if delta != 0.0 {
                ui.scroll_with_delta(vec2(0.0, delta));
                ctx.request_repaint();
            }
        }
        ui.add_space(TOP);
        width = ui.available_width();
        let cols = columns_in(width, cell);
        *columns = cols;
        let rows = places.len().div_ceil(cols);
        let left = ((width - cols as f32 * cell.x) / 2.0).max(0.0);
        let (area, _) = ui.allocate_exact_size(vec2(width, rows as f32 * cell.y), Sense::hover());
        let cell_rect = |i: usize| {
            let (row, col) = (i / cols, i % cols);
            Rect::from_min_size(pos2(area.left() + left + col as f32 * cell.x, area.top() + row as f32 * cell.y), cell)
        };
        if let Some(i) = reveal.and_then(|id| files.iter().position(|f| f.id == id)) {
            ui.scroll_to_rect(cell_rect(first_place.get(i).copied().unwrap_or(0)), None);
        }
        for row in crate::canvas::thumbnail_rows(clip.top() - TOP, clip.bottom() - TOP, cell.y, rows) {
            let first = row.saturating_mul(cols);
            let in_row = places.get(first..first.saturating_add(cols).min(places.len())).unwrap_or_default();
            // An expanded file's pages on this row share a tinted band, so they read as one file.
            let mut run = 0;
            while run < in_row.len() {
                let Some(start) = in_row.get(run).copied() else { break };
                let mut end = run;
                while in_row.get(end.saturating_add(1)).is_some_and(|c| c.file == start.file && c.page.is_some()) {
                    end = end.saturating_add(1);
                }
                if start.page.is_some() {
                    let band = cell_rect(first.saturating_add(run)).union(cell_rect(first.saturating_add(end))).shrink2(vec2(2.0, 4.0));
                    ui.painter().rect_filled(band, CornerRadius::same(10), t.accent_soft.gamma_multiply(0.45));
                }
                run = end.saturating_add(1);
            }
            for (col, place) in in_row.iter().enumerate() {
                let i = first.saturating_add(col);
                let Some(f) = files.get_mut(place.file) else { break };
                let c = cell_rect(i);
                cells.push((i, c));
                let card = Card {
                    i: place.file,
                    page: place.page,
                    rect: c,
                    check: checks.get(place.file).cloned().unwrap_or(Ok(f.pages)),
                    selected: selected.get(place.file).copied().unwrap_or(false)
                        || place.page.is_some_and(|(nth, ..)| picked_pages.contains(&(f.id, nth))),
                    twice: twice.get(place.file).copied().unwrap_or(false),
                    revision,
                    ppp,
                    fit,
                };
                let card_id = card.id(ui, f);
                if let Some(a) = card.show(ui, t, f, thumbs) {
                    action = Some(a);
                }
                if ui.memory(|m| m.has_focus(card_id)) {
                    focused_card = Some((card_id, f.id, place.page.map_or(0, |p| p.0)));
                }
            }
        }
        // While a page card is dragged: the page gap it would go to (any place, between any two
        // pages), drawn at the nearest card's edge, and how many pages go.
        if let Some(d) = drag
            && let Some(page) = d.page
            && let Some(p) = pointer
        {
            let at = crate::canvas::drop_gap(&cells, p);
            let nearest = cells.iter().min_by(|(_, a), (_, b)| a.distance_sq_to_pos(p).total_cmp(&b.distance_sq_to_pos(p)));
            if let Some((_, r)) = nearest {
                let x = if p.x < r.center().x { r.left() + 2.0 } else { r.right() - 2.0 };
                ui.painter().line_segment([pos2(x, r.top() + 10.0), pos2(x, r.bottom() - 30.0)], Stroke::new(3.0, t.accent));
            }
            let moving = if picked_pages.contains(&page) { picked_pages.len() } else { 1 };
            let label = if moving == 1 { tl!("1 page").to_string() } else { crate::i18n::fmt(tl!("{n} pages"), &[("n", &moving.to_string())]) };
            ui.painter().text(p + vec2(14.0, 14.0), Align2::LEFT_TOP, label, theme::medium(12.0), t.accent_text);
            if released
                && viewport.contains(p)
                && let Some(at) = at
            {
                let gap = page_at.get(at).copied().unwrap_or(total_pages);
                action = Some(RowAction::MovePages { page, revision: d.revision, gap });
            }
        } else if let Some(d) = drag
            && let Some(p) = pointer
        {
            // A file's card: the file gap it would go to, drawn as a bar, and how many files go.
            let gap = crate::canvas::drop_gap(&cells, p).map(|at| file_gap(&places, at, n));
            // Drawn at the end of the file before the gap or the start of the one after (an
            // expanded file's pages are never split), whichever is nearer the pointer.
            if let Some(gap) = gap {
                let before = gap
                    .checked_sub(1)
                    .and_then(|f| first_place.get(f.saturating_add(1)).map(|next| next.saturating_sub(1)).or(places.len().checked_sub(1)));
                let after = first_place.get(gap).copied();
                let ends = [before.map(|at| cell_rect(at).right() - 2.0).zip(before), after.map(|at| cell_rect(at).left() + 2.0).zip(after)];
                let nearest = ends.into_iter().flatten().min_by(|(xa, a), (xb, b)| {
                    let da = pos2(*xa, cell_rect(*a).center().y).distance_sq(p);
                    let db = pos2(*xb, cell_rect(*b).center().y).distance_sq(p);
                    da.total_cmp(&db)
                });
                if let Some((x, at)) = nearest {
                    let r = cell_rect(at);
                    ui.painter().line_segment([pos2(x, r.top() + 10.0), pos2(x, r.bottom() - 30.0)], Stroke::new(3.0, t.accent));
                }
            }
            let moving = files
                .iter()
                .position(|f| f.id == d.file)
                .map_or(1, |i| if selected.get(i).copied().unwrap_or(false) { selected.iter().filter(|s| **s).count() } else { 1 });
            let label = if moving == 1 { tl!("1 file").to_string() } else { crate::i18n::fmt(tl!("{n} files"), &[("n", &moving.to_string())]) };
            ui.painter().text(p + vec2(14.0, 14.0), Align2::LEFT_TOP, label, theme::medium(12.0), t.accent_text);
            // Dropped in the grid: there. Anywhere else: nothing moves.
            if released
                && viewport.contains(p)
                && let Some(gap) = gap
            {
                action = Some(RowAction::DropAt { file: d.file, revision: d.revision, gap });
            }
        }
    });

    app.combine_tab.card_focus = focused_card.map(|(id, ..)| id);
    app.combine_tab.card_focus_at = focused_card.map(|(_, file, nth)| (file, nth));

    // Pinch, or Ctrl/⌘ with the wheel, over the grid zooms it.
    let pinch = crate::motion::zoom_delta(&ctx);
    if (pinch - 1.0).abs() > 0.001 && over_grid {
        app.combine_tab.zoom_request = Some(app.combine_zoom * pinch);
    }
    // A zoom asked for this frame (toolbar, keys, pinch) applies now the grid has been laid out:
    // the card under the pointer (or else the first in view) keeps its place on screen. Not while
    // a card is dragged: the grid under it would change.
    if let Some(zoom) = app.combine_tab.zoom_request.take()
        && drag.is_none()
    {
        let before = app.combine_zoom;
        app.set_combine_zoom(zoom);
        if app.combine_zoom != before {
            // The same card all through one gesture (a slider drag, a turn of the wheel): the
            // first in view changes as the cards reflow, and following it would drift. A card is
            // kept by its file and its place among the file's pages.
            let now = ctx.input(|i| i.time);
            let gesture = app.combine_tab.zoom_anchor.filter(|a| now - a.3 < 0.5).and_then(|(id, nth, above, _)| {
                let file = app.combine_draft.iter().position(|f| f.id == id)?;
                Some((first_place.get(file)?.saturating_add(nth).min(places.len().saturating_sub(1)), above))
            });
            // Cards just outside the view are drawn too (under the toolbar, say): the pointer
            // counts only over the grid.
            let under = pointer.filter(|_| over_grid).and_then(|p| cells.iter().find(|(_, r)| r.contains(p)));
            let anchor = gesture
                .or_else(|| under.or_else(|| cells.iter().find(|(_, r)| r.bottom() > viewport.top())).map(|(i, r)| (*i, r.top() - viewport.top())));
            app.combine_tab.zoom_anchor = anchor.and_then(|(i, above)| {
                let place = places.get(i)?;
                Some((app.combine_draft.get(place.file)?.id, place.page.map_or(0, |p| p.0), above, now))
            });
            if let Some((i, above)) = anchor {
                // Where it will be: its row at the new size, as far below the top as it is now;
                // within what can be scrolled to, so zooming out at the bottom shows no gap.
                let cell = paper_box(app.combine_zoom) + AROUND;
                let cols = columns_in(width, cell);
                let content = TOP + places.len().div_ceil(cols) as f32 * cell.y;
                let y = (TOP + (i / cols) as f32 * cell.y - above).min(content - viewport.height()).max(0.0);
                // Applied by the next frame's scroll area, before anything is drawn there: no
                // jump, and no scroll still animating (to a card revealed by the keys, say) can
                // carry it elsewhere. The request is consumed once.
                ctx.data_mut(|d| d.insert_temp(pending_offset_id(), vec2(0.0, y)));
            }
            ctx.request_repaint();
        }
    }

    action
}

/// The scroll position a zoom asked the next frame's grid to start at.
fn pending_offset_id() -> egui::Id {
    egui::Id::unique("pdfcraft-combine-grid-offset")
}

/// One card's place and what it shows.
struct Card {
    /// The file, by its place in the list.
    i: usize,
    /// A page card (the file expanded): its place among the pages the file adds, the page and how
    /// many it adds. `None` for the file's own card.
    page: Option<(usize, usize, usize)>,
    rect: Rect,
    check: Result<usize, String>,
    selected: bool,
    twice: bool,
    revision: u64,
    ppp: f32,
    /// The box the page is fitted in (the zoom).
    fit: Vec2,
}

impl Card {
    fn id(&self, ui: &egui::Ui, f: &CombineFile) -> egui::Id {
        match self.page {
            None => ui.scope_id().with(("combine-card", f.id)),
            Some((nth, ..)) => ui.scope_id().with(("combine-page", f.id, nth)),
        }
    }

    fn show(&self, ui: &mut egui::Ui, t: &Tokens, f: &mut CombineFile, thumbs: &mut Thumbs) -> Option<RowAction> {
        let mut action = None;
        let c = self.rect;
        let id = self.id(ui, f);
        let resp = ui.interact(c.shrink(4.0), id, Sense::click_and_drag());
        let name = f.name.clone();
        // A page card: "Page 3", and the file it belongs to.
        let page_label = self.page.map(|(_, p, _)| crate::i18n::fmt(tl!("Page {label}"), &[("label", &p.saturating_add(1).to_string())]));
        let label = match &page_label {
            Some(page) => format!("{page} · {name}"),
            None => name.clone(),
        };
        // As the organize grid's pages: a check box that is on when selected; and, for screen
        // readers, selected as items in a list are.
        resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, true, self.selected, &label));
        let pages = match self.page {
            // For screen readers: which page of how many.
            Some((_, p, _)) => crate::i18n::fmt(tl!("Page {p} of {n}"), &[("p", &p.saturating_add(1).to_string()), ("n", &f.pages.to_string())]),
            None if f.pages == 1 => tl!("1 page").to_string(),
            None => crate::i18n::fmt(tl!("{n} pages"), &[("n", &f.pages.to_string())]),
        };
        let painter = ui.painter_at(c);

        // Selected and hovered cards.
        if self.selected || resp.hovered() {
            painter.rect_filled(c.shrink(6.0), CornerRadius::same(8), if self.selected { t.accent_soft } else { t.hover });
        }
        if self.selected {
            painter.rect_stroke(c.shrink(6.0), CornerRadius::same(8), Stroke::new(1.5, t.accent), egui::StrokeKind::Inside);
        }

        // The page, fitted in the paper box (top-aligned, room above for the stack).
        let page = match self.page {
            Some((_, p, _)) => Ok(p),
            None => f.first_page(),
        };
        // The page's own size, when it is known and is one.
        let known = page.as_ref().ok().and_then(|p| f.sizes.get(*p).copied()).filter(|(w, h)| paper_size(*w, *h, self.fit).is_some());
        let size_pt = known.unwrap_or(FALLBACK_PAGE);
        let size = paper_size(size_pt.0, size_pt.1, self.fit).unwrap_or(self.fit);
        let top = c.top() + ABOVE;
        let paper = Rect::from_min_size(pos2(c.center().x - size.x / 2.0 - 3.0, top + (self.fit.y - size.y)), size);
        // A file's card that adds more than one page: sheets behind it.
        let takes = self.check.as_ref().map_or(f.pages, |k| *k);
        if self.page.is_none() && takes > 1 {
            for d in [6.0, 3.0] {
                let sheet = paper.translate(vec2(d, -d));
                painter.rect_filled(sheet, CornerRadius::ZERO, Color32::WHITE);
                painter.rect_stroke(sheet, CornerRadius::ZERO, Stroke::new(1.0, t.border), egui::StrokeKind::Outside);
            }
        }
        painter.rect_filled(paper.translate(vec2(0.0, 1.5)), CornerRadius::same(1), t.page_shadow);
        painter.rect_filled(paper, CornerRadius::ZERO, Color32::WHITE);

        // What goes on the paper: the thumbnail, or why there isn't one.
        let mut why: Option<(&str, Color32, String)> = None;
        if let Some(lock) = f.lock {
            let text = match lock {
                Lock::Open => tl!("Password-protected"),
                Lock::Permissions => tl!("Its security settings don't allow copying pages"),
            };
            why = Some(("lock", ON_PAPER, text.to_owned()));
        } else if let Some(p) = &f.problem {
            why = Some(("circle-x", ERROR, p.clone()));
        } else {
            match page {
                Err(e) => why = Some(("circle-x", ERROR, e)),
                // The pages couldn't be counted, or the page's size is unknown (some passwords
                // open a file for combining but not for reading it here): nothing is drawn (the
                // engine decides at Combine), and the card says so rather than look blank.
                Ok(p) if p >= f.pages || known.is_none() => why = Some(("eye-off", ON_PAPER, tl!("No preview").to_owned())),
                Ok(p) => {
                    // Pixels per point: the size the paper is drawn at, on both sides, and never a
                    // side longer than a thumbnail's (a long, thin page fits by its long side).
                    let (w, h) = size_pt;
                    let scale = (size.x * self.ppp / w).min(size.y * self.ppp / h).min(MAX_SIDE / w.max(h));
                    let px = [w * scale, h * scale].map(|v| (v.max(1.0).ceil() as u32).div_ceil(SIZE_STEP).saturating_mul(SIZE_STEP));
                    let key = Key { source: Arc::as_ptr(&f.bytes) as usize, auth: f.auth, page: p, px };
                    let look = thumbs.look((f.group, p), key);
                    let mut want = false;
                    match look {
                        Look::Ready(tex) => {
                            painter.image(tex, paper, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                        }
                        Look::Failed(e) => why = Some(("triangle-alert", WARNING, crate::i18n::fmt(tl!("Preview unavailable: {e}"), &[("e", &e)]))),
                        Look::Pending(stand_in) => {
                            if let Some(tex) = stand_in {
                                painter.image(tex, paper, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                            }
                            want = true;
                        }
                    }
                    if want && scale.is_finite() && scale > 0.0 {
                        thumbs.wants.push(Want {
                            slot: (f.group, p),
                            key,
                            bytes: f.bytes.clone(),
                            password: f.password().map(Arc::from),
                            scale,
                            visible: ui.is_rect_visible(c),
                        });
                    }
                }
            }
        }
        if let Some((icon, colour, _)) = &why {
            icons::paint(ui, Rect::from_center_size(paper.center(), Vec2::splat(28.0)), icon, 28.0, *colour);
        }
        // For screen readers: the pages, and why no page is shown (otherwise only in the tooltip).
        let description = match &why {
            Some((_, _, reason)) => format!("{pages}. {reason}"),
            None => pages.clone(),
        };
        ui.ctx().accesskit_node_builder(id, |node| {
            node.set_selected(self.selected);
            node.set_description(description);
        });
        painter.rect_stroke(paper, CornerRadius::ZERO, Stroke::new(1.0, t.border), egui::StrokeKind::Outside);

        // A warning that doesn't stop it being combined: a badge on the file's card.
        let warned = self.page.is_none() && (!f.notes.is_empty() || self.twice);
        if warned && why.as_ref().is_none_or(|(icon, ..)| *icon == "eye-off") {
            let r = Rect::from_center_size(paper.right_top() + vec2(-2.0, 2.0), Vec2::splat(18.0));
            painter.circle_filled(r.center(), 10.0, t.card);
            icons::paint(ui, r, "triangle-alert", 15.0, WARNING);
        }

        // The name, shortened in the middle to fit, and the pages taken when a range is set; on a
        // page card, the page first and the file's name below it, smaller.
        let name_y = top + self.fit.y + 16.0;
        let max_w = c.width() - 20.0;
        let (font, colour, y) = match &page_label {
            Some(page) => {
                painter.text(pos2(c.center().x, name_y), Align2::CENTER_CENTER, page, theme::semibold(12.5), t.text);
                (theme::regular(11.5), t.text_muted, name_y + 17.0)
            }
            None => (theme::medium(12.5), t.text, name_y),
        };
        let fits = |s: &str| crate::bidi::layout_ltr(ui, s.to_owned(), font.clone(), colour).size().x <= max_w;
        let shown = fit_name(&f.name, fits);
        // A file name is file/path chrome: always left to right, so an Arabic name ends in `.pdf`.
        let name_galley = crate::bidi::layout_ltr(ui, shown, font, colour);
        painter.galley(pos2(c.center().x - name_galley.size().x / 2.0, y - name_galley.size().y / 2.0), name_galley, colour);
        let note = if self.page.is_none() && !f.range.trim().is_empty() {
            match &self.check {
                Ok(k) => Some((crate::i18n::fmt(tl!("Pages: {k} of {n}"), &[("k", &k.to_string()), ("n", &f.pages.to_string())]), t.text_muted)),
                Err(_) => Some((tl!("Check the pages").to_owned(), ERROR)),
            }
        } else {
            None
        };
        if let Some((text, colour)) = note {
            painter.text(pos2(c.center().x, name_y + 17.0), Align2::CENTER_CENTER, text, theme::regular(11.5), colour);
        }

        // On hover, the card's own actions along the top of the page (not while a card is
        // dragged): remove this file only, and show it large when there is a page to show. Drawn
        // over the card, so a click on one doesn't select or move it.
        let previewable = why.is_none();
        // A file that adds more pages than one can be spread out into them.
        let expandable = self.page.is_none() && takes > 1 && takes <= MAX_EXPAND && f.lock.is_none() && f.problem.is_none() && self.check.is_ok();
        // Shown large: the file from its first page, or a page card's page.
        let preview = match self.page {
            Some((nth, ..)) => RowAction::PreviewAt(f.id, nth),
            None => RowAction::Preview(f.id),
        };
        // The card's own actions, in this order: expand (or, on a page, collapse), remove (the file,
        // or the page), show large.
        let mut buttons: Vec<(&str, String, RowAction)> = Vec::new();
        if expandable {
            buttons.push(("maximize-2", tl!("Expand").to_owned(), RowAction::Expand(f.id, true)));
        }
        if let Some((nth, ..)) = self.page {
            buttons.push(("minimize-2", tl!("Collapse").to_owned(), RowAction::Expand(f.id, false)));
            buttons.push(("trash-2", tl!("Remove page").to_owned(), RowAction::RemovePages(Some((f.id, nth)))));
        } else {
            buttons.push(("trash-2", crate::i18n::fmt(tl!("Remove {name}"), &[("name", &f.name)]), RowAction::RemoveFile(f.id)));
        }
        if previewable {
            buttons.push(("search", tl!("Preview").to_owned(), preview));
        }
        // Nothing of the card shows over a dialog (the preview, the password box).
        let covered = ui.ctx().memory(|m| m.top_modal_layer().is_some());
        let mut over_bar = false;
        if !covered && ui.rect_contains_pointer(c.shrink(4.0)) && !egui::DragAndDrop::has_any_payload(ui.ctx()) {
            let count = buttons.len() as f32;
            // Mostly above the page (a quarter over its top edge), so it hides little of it, and
            // never the name below.
            let y = paper.top() + BAR_H / 4.0 - BAR_H / 2.0;
            let bar = Rect::from_center_size(pos2(paper.center().x, y), vec2(count * 26.0 + (count - 1.0).max(0.0) * 2.0 + 8.0, BAR_H));
            over_bar = ui.rect_contains_pointer(bar);
            painter.rect(bar, CornerRadius::same(8), t.card, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
            let mut bar_ui = ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink(4.0)).layout(egui::Layout::left_to_right(egui::Align::Center)));
            bar_ui.spacing_mut().item_spacing.x = 2.0;
            for (icon, label, act) in &buttons {
                if icons::button(&mut bar_ui, icon, 26.0, false, label).clicked() {
                    action = Some(act.clone());
                }
            }
        }

        // The full name and everything known about the file, on hover (on a page card, which page).
        let mut tip = match &page_label {
            Some(page) => format!("{}\n{page}", f.name),
            None => format!("{}\n{pages} · {}", f.name, crate::panels::human_size(f.bytes.len())),
        };
        for line in why.iter().map(|(_, _, s)| s).chain(&f.notes) {
            tip.push('\n');
            tip.push_str(line);
        }
        if self.twice {
            tip.push('\n');
            tip.push_str(tl!("Added more than once"));
        }
        // Page ranges are typed in the list.
        if self.page.is_none() && self.check.is_err() {
            tip.push('\n');
            tip.push_str(tl!("Switch to List view to change the pages it takes."));
        }
        // (Over the bar, its buttons say what they do instead.)
        let resp = if covered || over_bar { resp } else { resp.on_hover_text(tip) };

        // The same actions on a right click, show large first (and Unlock… for a locked file).
        let at = resp.rect.left_bottom();
        let locked = f.lock.is_some();
        resp.context_menu(|ui| {
            let item = |ui: &mut egui::Ui, icon: &str, label: &str| {
                ui.add(egui::Button::image_and_text(icons::image(icon, 15.0, ui.visuals().text_color()), label)).clicked()
            };
            for (icon, label, act) in buttons.iter().rev() {
                // (Remove names its file, as the trash button does.)
                if item(ui, icon, label) {
                    action = Some(act.clone());
                    ui.close();
                }
            }
            if locked && item(ui, "lock-open", tl!("Unlock…")) {
                action = Some(RowAction::Unlock(Some(self.i), at));
                ui.close();
            }
        });
        if resp.drag_started() {
            egui::DragAndDrop::set_payload(ui.ctx(), GridDrag { file: f.id, revision: self.revision, page: self.page.map(|(nth, ..)| (f.id, nth)) });
        }
        if resp.clicked() {
            let mods = resp.ctx.input(|i| i.modifiers);
            action = Some(match self.page {
                Some((nth, ..)) => RowAction::ClickPage(f.id, nth, mods),
                None => RowAction::Click(self.i, mods),
            });
        }
        action
    }
}

/// The file a card's magnifier opened, shown large over the page: one of the pages it adds at a
/// time, flipped with the buttons or the Left and Right keys; Esc, the close button or a click
/// outside closes it. It closes by itself when the file leaves the list or has nothing to show.
pub(crate) fn preview(app: &mut PdfCraftApp, ctx: &egui::Context, t: &Tokens) {
    // The command palette goes over everything: the preview waits under it (drawn again after).
    if app.palette_open {
        return;
    }
    let Some(p) = app.combine_tab.preview.as_mut() else { return };
    let Some(f) = app.combine_draft.iter().find(|f| f.id == p.file).filter(|f| f.lock.is_none() && f.problem.is_none()) else {
        app.combine_tab.preview = None;
        return;
    };
    // The pages it adds, worked out again only when its range changes.
    let pages = match &p.pages {
        Some(((range, count), pages)) if *range == f.range && *count == f.pages => pages.clone(),
        _ => match f.pages_taken() {
            Ok(pages) => {
                let pages: Arc<[usize]> = Arc::from(pages);
                p.pages = Some(((f.range.clone(), f.pages), pages.clone()));
                pages
            }
            Err(_) => {
                app.combine_tab.preview = None;
                return;
            }
        },
    };
    let n = pages.len();
    p.at = p.at.min(n.saturating_sub(1));
    let Some(&page) = pages.get(p.at) else {
        app.combine_tab.preview = None;
        return;
    };
    p.page = Some(page);
    let at = p.at;
    // Flipped with the keys (the grid's arrows wait while this shows), unless a text field has
    // them.
    let typing = ctx.text_edit_focused();
    let (back, on) = ctx.input_mut(|i| {
        for k in [egui::Key::Plus, egui::Key::Equals, egui::Key::Minus, egui::Key::Num0] {
            i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, k));
        }
        if typing {
            return (false, false);
        }
        (
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) | i.consume_key(egui::Modifiers::NONE, egui::Key::PageUp),
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) | i.consume_key(egui::Modifiers::NONE, egui::Key::PageDown),
        )
    });
    let (mut step, mut close) = (i64::from(on) - i64::from(back), false);
    let screen = ctx.content_rect();
    let width = (screen.width() * 0.86).max(320.0);
    let body_h = (screen.height() * 0.86 - 110.0).max(200.0);
    let ppp = ctx.pixels_per_point();
    // No larger than the GPU takes, or the render would be refused.
    let longest = ctx.input(|i| i.max_texture_side as f32).min(PREVIEW_SIDE);
    let thumbs = &mut app.combine_thumbs;
    let modal = egui::Modal::new(egui::Id::unique("combine-preview")).show(ctx, |ui| {
        ui.set_width(width);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(f.name.clone()).font(theme::semibold(15.0)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icons::button(ui, "x", 30.0, false, tl!("Close")).clicked() {
                    close = true;
                }
                let of = crate::i18n::fmt(tl!("Page {p} of {n}"), &[("p", &(at + 1).to_string()), ("n", &n.to_string())]);
                ui.label(egui::RichText::new(of).color(t.text_muted));
            });
        });
        ui.add_space(8.0);
        // The page, as large as fits.
        let (area, _) = ui.allocate_exact_size(vec2(width, body_h), Sense::hover());
        let known = f.sizes.get(page).copied().filter(|(w, h)| paper_size(*w, *h, area.size()).is_some());
        let (w, h) = known.unwrap_or(FALLBACK_PAGE);
        let size = paper_size(w, h, area.size() - vec2(8.0, 8.0)).unwrap_or(area.size());
        let paper = Rect::from_center_size(area.center(), size);
        let painter = ui.painter_at(area);
        painter.rect_filled(paper.translate(vec2(0.0, 2.0)), CornerRadius::same(1), t.page_shadow);
        painter.rect_filled(paper, CornerRadius::ZERO, Color32::WHITE);
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        if known.is_some() {
            let scale = (size.x * ppp / w).min(size.y * ppp / h).min(longest / w.max(h));
            let px = [w * scale, h * scale].map(|v| (v.max(1.0).ceil() as u32).div_ceil(SIZE_STEP).saturating_mul(SIZE_STEP));
            let key = Key { source: Arc::as_ptr(&f.bytes) as usize, auth: f.auth, page, px };
            match thumbs.look(PREVIEW, key) {
                Look::Ready(tex) => {
                    painter.image(tex, paper, uv, Color32::WHITE);
                }
                Look::Failed(e) => {
                    let text = crate::i18n::fmt(tl!("Preview unavailable: {e}"), &[("e", &e)]);
                    painter.text(paper.center(), Align2::CENTER_CENTER, text, theme::regular(13.0), ON_PAPER);
                }
                Look::Pending(stand_in) => {
                    // A smaller render of this page stands in: the last size shown, or its card's.
                    let card = thumbs.entries.get(&(f.group, page)).filter(|e| e.key.auth == f.auth).and_then(|e| e.tex.as_ref());
                    if let Some(tex) = stand_in.or(card.map(TextureHandle::id)) {
                        painter.image(tex, paper, uv, Color32::WHITE);
                    }
                    if scale.is_finite() && scale > 0.0 {
                        thumbs.wants.push(Want {
                            slot: PREVIEW,
                            key,
                            bytes: f.bytes.clone(),
                            password: f.password().map(Arc::from),
                            scale,
                            visible: true,
                        });
                    }
                }
            }
        } else {
            icons::paint(ui, Rect::from_center_size(paper.center(), Vec2::splat(36.0)), "eye-off", 36.0, ON_PAPER);
        }
        painter.rect_stroke(paper, CornerRadius::ZERO, Stroke::new(1.0, t.border), egui::StrokeKind::Outside);
        ui.add_space(8.0);
        // Previous and next, centred below.
        ui.horizontal(|ui| {
            ui.add_space((width - 2.0 * 30.0 - 6.0) / 2.0);
            if ui.add_enabled_ui(at > 0, |ui| icons::button(ui, "chevron-left", 30.0, false, tl!("Previous page"))).inner.clicked() {
                step = -1;
            }
            if ui.add_enabled_ui(at + 1 < n, |ui| icons::button(ui, "chevron-right", 30.0, false, tl!("Next page"))).inner.clicked() {
                step = 1;
            }
        });
    });
    if close || modal.should_close() {
        app.combine_tab.preview = None;
    } else if let Some(p) = app.combine_tab.preview.as_mut() {
        p.at = match step {
            s if s < 0 => p.at.saturating_sub(1),
            s if s > 0 => (p.at + 1).min(n.saturating_sub(1)),
            _ => p.at,
        };
    }
}

impl PdfCraftApp {
    /// The file shown large, the page on screen (0-based, in the file) and its render, while it
    /// shows (tests and the control channel).
    pub fn combine_preview(&self) -> Option<(String, usize, ThumbState)> {
        let p = self.combine_tab.preview.as_ref()?;
        let f = self.combine_draft.iter().find(|f| f.id == p.file)?;
        Some((f.name.clone(), p.page?, self.combine_thumbs.state(PREVIEW)))
    }

    /// Size the Combine grid's cards (1.0 = the usual); out-of-range values are clamped and
    /// nonsense is ignored.
    pub fn set_combine_zoom(&mut self, zoom: f32) {
        if zoom.is_finite() {
            self.combine_zoom = zoom.clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end());
        }
    }

    /// Every frame, whatever shows: take finished thumbnails, start those the grid asked for (only
    /// while it is drawn), and let go of what no longer matches the list, so renders under way
    /// finish and release their threads in the list view, on Home or in a document tab too.
    pub(crate) fn combine_thumbs_frame(&mut self, ctx: &egui::Context) {
        // What each thumbnail must match now (cards out of view included): each file's first page,
        // and an expanded file's pages (looked up in its sorted list, never listed one by one).
        // (By file: the parts a file is split into share its thumbnails.)
        struct Shown {
            source: usize,
            auth: u64,
            pages: Vec<usize>,
        }
        let mut shown: HashMap<u64, Shown> = HashMap::with_capacity(self.combine_draft.len());
        let tab = &mut self.combine_tab;
        for f in self.combine_draft.iter_mut() {
            let Ok(first) = f.first_page() else { continue };
            let sorted = tab.expanded.contains(&f.id).then(|| pages_shown(&mut tab.page_lists, f)).flatten().map(|l| l.sorted);
            let entry = shown.entry(f.group).or_insert_with(|| Shown { source: Arc::as_ptr(&f.bytes) as usize, auth: f.auth, pages: Vec::new() });
            entry.pages.push(first);
            entry.pages.extend(sorted.iter().flat_map(|s| s.iter().copied()));
        }
        for s in shown.values_mut() {
            s.pages.sort_unstable();
            s.pages.dedup();
        }
        // The page shown large, while it shows: leaving the tab closes it.
        if !self.combine_showing() {
            self.combine_tab.preview = None;
        }
        let preview = self.combine_tab.preview.as_ref().and_then(|p| {
            let page = p.page?;
            let f = self.combine_draft.iter().find(|f| f.id == p.file)?;
            Some(Key { source: Arc::as_ptr(&f.bytes) as usize, auth: f.auth, page, px: [0, 0] })
        });
        let current = |slot: Slot| -> Option<Key> {
            if slot == PREVIEW {
                return preview;
            }
            let (id, page) = slot;
            let s = shown.get(&id)?;
            s.pages.binary_search(&page).is_ok().then_some(Key { source: s.source, auth: s.auth, page, px: [0, 0] })
        };
        self.combine_thumbs.finish(ctx, &current);
    }

    /// The single pages selected: each one's entry (by place in the list) and page (0-based, in
    /// the file), in list order (tests and the control channel).
    pub fn combine_selected_pages(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (i, f) in self.combine_draft.iter().enumerate() {
            let pages = f.pages_taken().unwrap_or_default();
            for (nth, page) in pages.iter().enumerate() {
                if self.combine_tab.selected_pages.contains(&(f.id, nth)) {
                    out.push((i, *page));
                }
            }
        }
        out
    }

    /// Each entry's name and the pages it takes (0-based, in order): a file split into parts is
    /// several entries (tests and the control channel).
    pub fn combine_parts_shown(&self) -> Vec<(String, Vec<usize>)> {
        self.combine_draft.iter().map(|f| (f.name.clone(), f.pages_taken().unwrap_or_default())).collect()
    }

    /// The files shown as their pages in the grid, by place in the list (tests and the control
    /// channel).
    pub fn combine_expanded(&self) -> Vec<usize> {
        self.combine_draft.iter().enumerate().filter(|(_, f)| self.combine_tab.expanded.contains(&f.id)).map(|(i, _)| i).collect()
    }

    /// An expanded file's page cards: each page (0-based, in the file) and its thumbnail; empty
    /// for a file shown as its card (tests and the control channel).
    pub fn combine_page_thumbnails(&self, file: usize) -> Vec<(usize, ThumbState)> {
        let Some(f) = self.combine_draft.get(file).filter(|f| self.combine_tab.expanded.contains(&f.id)) else { return Vec::new() };
        let pages = f.pages_taken().unwrap_or_default();
        pages.into_iter().map(|p| (p, self.combine_thumbs.state((f.group, p)))).collect()
    }

    /// Each listed file's grid thumbnail, in list order (tests and the control channel).
    pub fn combine_thumbnails(&self) -> Vec<ThumbState> {
        self.combine_draft.iter().map(|f| f.first_taken().map_or(ThumbState::None, |p| self.combine_thumbs.state((f.group, p)))).collect()
    }

    /// Thumbnail renders under way (tests: never more than a few).
    pub fn combine_thumbnails_rendering(&self) -> usize {
        self.combine_thumbs.jobs.len()
    }

    /// Render threads for thumbnails that exist now (counted from the threads themselves),
    /// including those of finished or abandoned renders that haven't exited yet (tests: never
    /// more than three).
    pub fn combine_thumbnail_threads(&mut self) -> usize {
        self.combine_thumbs.threads()
    }

    /// Whether no thumbnail render is under way and every render thread has exited (tests).
    pub fn combine_thumbnail_threads_exited(&self) -> bool {
        self.combine_thumbs.jobs.is_empty() && self.combine_thumbs.retiring.iter().all(Retiring::exited)
    }

    /// Thumbnail textures held (tests: none once the tab is closed).
    pub fn combine_thumbnail_textures(&self) -> usize {
        self.combine_thumbs.entries.values().filter(|e| e.tex.is_some()).count()
    }
}

#[cfg(test)]
mod tests;
