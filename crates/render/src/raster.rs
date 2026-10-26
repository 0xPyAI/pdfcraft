//! Background page rasterization.
//!
//! A `RenderPool` parses lazily once and shares that immutable parser between workers. Render
//! caches stay worker-local. Requests carry a caller tag that is echoed back.
//!
//! **Robustness:** every page render runs under `catch_unwind`. A panic inside the renderer on a
//! malformed page yields an error and permanently retires the pool's shared parser generation.
//! Workers discard its caches/results and fall back to private parsers. There is no shared
//! rebuild loop, and a timed-out shared initialization is never started again in that pool.

use std::panic::{AssertUnwindSafe, catch_unwind};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{Receiver, sync_channel};

#[cfg(test)]
mod radial_softmask_tests;
mod work_queue;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use work_queue::{ResultReceiver, WorkQueue};

use hayro::hayro_interpret::font::{FontData, FontQuery};
use hayro::hayro_interpret::hayro_cmap::CidFamily;
use hayro::hayro_interpret::{InterpreterSettings, InterpreterWarning};
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::AlphaColor;
use hayro::vello_cpu::color::palette::css::{TRANSPARENT, WHITE};
use hayro::{RenderCache, RenderSettings, render_into, render_size};

use crate::{Pixels, ReadingColors};

/// Monotonic-ish timer that is safe on wasm32 (where `std::time::Instant` panics).
#[derive(Clone, Copy)]
struct Stopwatch(#[cfg(not(target_arch = "wasm32"))] std::time::Instant);

impl Stopwatch {
    fn start() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        return Self(std::time::Instant::now());
        #[cfg(target_arch = "wasm32")]
        return Self();
    }
    fn millis(self) -> u32 {
        #[cfg(not(target_arch = "wasm32"))]
        return self.0.elapsed().as_millis() as u32;
        #[cfg(target_arch = "wasm32")]
        return 0;
    }
}

/// Per-document rendering configuration.
#[derive(Clone, Debug, Default)]
pub struct RenderConfig {
    /// User or owner password for encrypted documents.
    pub password: Option<Arc<str>>,
    /// Layer (optional content group) visibility overrides: (object number, generation, visible).
    pub layers: Arc<Vec<(i32, i32, bool)>>,
    /// View ▸ Hide all comments: markup annotations aren't drawn (fields and links still are).
    pub hide_comments: bool,
    /// Refuse a whole-page raster that would be reduced by the renderer's hard size caps.
    /// Tiled requests remain available for pages that exceed those caps. Both modes refuse
    /// a raster whose rounded integer dimensions exceed the final pixel-allocation budget.
    pub reject_oversize: bool,
    /// View ▸ Reading colours: solid text and line-art paints are remapped for readability and
    /// the page clears to the setting's background. Display only; the document never changes.
    pub reading_colors: ReadingColors,
    /// The page clears to transparent instead of white (or the reading background): for a page
    /// holding only what is laid over another raster, such as a comment while it is dragged.
    pub transparent: bool,
}

impl RenderConfig {
    fn settings(&self) -> InterpreterSettings {
        let standard = InterpreterSettings::default().font_resolver;
        InterpreterSettings {
            ocg_overrides: self.layers.clone(),
            hide_comments: self.hide_comments,
            font_resolver: Arc::new(move |query| japanese_fallback(query).or_else(|| standard(query))),
            ..InterpreterSettings::default()
        }
    }
}

/// A Japanese face from craft-fonts for a CID font of the Adobe-Japan1 collection that the PDF
/// doesn't embed (`HeiseiMin-W3`, `KozGoPro-Medium`, …). hayro's own substitutes for fonts that
/// aren't embedded are the Latin standard 14, so such text drew nothing. `None` for every other
/// font, and when PdfCraft was built without craft-fonts. Only Japanese: no other CJK face is
/// embedded whole.
fn japanese_fallback(query: &FontQuery) -> Option<(FontData, u32)> {
    let FontQuery::Fallback(f) = query else { return None };
    if f.character_collection.as_ref()?.family != CidFamily::AdobeJapan1 {
        return None;
    }
    let face = match japanese_face(f.post_script_name.as_deref().unwrap_or_default(), f.is_serif, f.is_bold || f.font_weight >= 600) {
        // BIZ UDMincho before the document face (Shippori Mincho): it covers half-width katakana
        // (U+FF61–U+FF9F), which Shippori Mincho lacks, and a character the substitute lacks is
        // drawn as .notdef.
        JapaneseFace::Mincho => pdfcraft_fonts::ui_japanese_fonts()
            .into_iter()
            .find(|c| c.family == "BIZ UDMincho" && c.style == "Regular")
            .or_else(pdfcraft_fonts::document_japanese_font),
        JapaneseFace::Gothic { bold } => {
            let faces = pdfcraft_fonts::ui_japanese_fonts();
            let style = if bold { "Bold" } else { "Regular" };
            faces.iter().find(|c| c.family == "BIZ UDPGothic" && c.style == style).or(faces.first()).copied()
        }
    }?;
    Some((Arc::new(face.bytes), 0))
}

#[derive(Debug, PartialEq)]
enum JapaneseFace {
    Mincho,
    Gothic { bold: bool },
}

/// Which kind of Japanese face stands in for the font named `name`: Mincho names (`HeiseiMin`,
/// `KozMin`, `Ryumin`, `MS-Mincho`) a serif Mincho; Gothic names (`…Gothic…`, `HeiseiKakuGo`,
/// `KozGo`, `…Maru…`) a sans Gothic; any other name by the font descriptor's serif flag.
fn japanese_face(name: &str, serif: bool, bold: bool) -> JapaneseFace {
    let name = name.to_ascii_lowercase();
    if name.contains("min") {
        JapaneseFace::Mincho
    } else if ["goth", "kakugo", "kozgo", "kaku", "maru"].iter().any(|k| name.contains(k)) || !serif {
        JapaneseFace::Gothic { bold }
    } else {
        JapaneseFace::Mincho
    }
}

/// Hard cap on a rendered side, to bound memory at extreme zoom levels (tiling arrives in M3.3).
pub const MAX_SIDE: f32 = 8192.0;
/// Hard cap on rendered pixels per page (~64 MP ≈ 256 MB RGBA).
pub const MAX_PIXELS: f32 = 64.0e6;

/// What a request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum RequestKind {
    /// A raster of the page.
    #[default]
    Pixels,
    /// The page's text layer (glyphs with Unicode and boxes); `scale` is ignored.
    Text,
}

/// A region of a scaled page, in device pixels (for tiled rendering of large pages).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Tile {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct RenderRequest {
    pub page: usize,
    pub kind: RequestKind,
    /// Render only this region (device pixels at `scale`). `None` renders the whole page.
    pub tile: Option<Tile>,
    /// Device pixels per PDF point.
    pub scale: f32,
    /// Caller-defined tag, echoed back (used for zoom generations / thumbnail vs page).
    pub tag: u64,
    /// Low-detail render for previews (page backdrops, thumbnails): images covering only a
    /// few device pixels are skipped. Never set it for a final render (tiles, whole pages,
    /// print, export): previews may differ by those pixels.
    pub preview: bool,
}

/// The tag bit of a print-preview raster. Such requests always render the document's true
/// colours, ignoring `RenderConfig::reading_colors`: a preview's job is fidelity to the
/// printout, and the print itself — imposed from the document — is never recoloured.
pub const PRINT_TAG: u64 = 1 << 60;

/// The reading colours a request renders with: print previews ([`PRINT_TAG`]) ignore the
/// document's setting and render with reading colours off.
fn request_reading(config: &RenderConfig, req: &RenderRequest) -> ReadingColors {
    if req.tag & PRINT_TAG != 0 { ReadingColors::default() } else { config.reading_colors }
}

/// What a render that panicked or overran the watchdog is remembered by: its page and kind. Not
/// its scale, tile or tag: the canvas tags every zoom level and tile anew, so a wider key would
/// let one pathological page tie up a worker for the full watchdog limit per zoom and per tile
/// (exhausting worker replacements) and re-parse the document for every new request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct RequestFailureKey {
    page: usize,
    kind: RequestKind,
}

impl From<RenderRequest> for RequestFailureKey {
    fn from(req: RenderRequest) -> Self {
        Self { page: req.page, kind: req.kind }
    }
}

#[derive(Debug)]
pub struct RenderedPage {
    pub request: RenderRequest,
    pub width: u32,
    pub height: u32,
    /// Premultiplied RGBA8, row-major. Empty when `error` is set.
    pub rgba: Pixels,
    /// Why the page could not be rendered (renderer panic, empty page box, …).
    pub error: Option<String>,
    /// For `RequestKind::Text`.
    pub text: Option<Arc<crate::text::PageText>>,
    /// Non-fatal conditions that made the result partial or otherwise noteworthy.
    pub warnings: Vec<RenderWarning>,
    pub millis: u32,
}

/// A non-fatal condition reported while interpreting a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderWarning {
    /// Content was skipped after the per-page decoded-content budget was exhausted.
    ContentTruncated,
    /// A colour could not be computed (a colour space that cannot be read, or a Separation or
    /// DeviceN tint transform that cannot be evaluated) and was painted in a fallback colour.
    ColourFallback,
}

/// Counters for work performed by a renderer or render pool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderStats {
    /// Number of parser construction attempts.
    pub parser_builds: u64,
    /// Number of pixel page interpretations, including tiled requests.
    pub page_interpretations: u64,
    /// Number of render requests accepted for processing.
    pub render_requests: u64,
    /// Number of requests that render only a tile.
    pub tile_requests: u64,
    /// Number of text-layer interpretations.
    pub text_interpretations: u64,
    /// Number of pixel rasterizations.
    pub rasterizations: u64,
}

#[derive(Default)]
struct AtomicRenderStats {
    parser_builds: AtomicU64,
    page_interpretations: AtomicU64,
    render_requests: AtomicU64,
    tile_requests: AtomicU64,
    text_interpretations: AtomicU64,
    rasterizations: AtomicU64,
}

impl AtomicRenderStats {
    fn record_request(&self, req: RenderRequest) {
        self.render_requests.fetch_add(1, Ordering::Relaxed);
        if req.tile.is_some() {
            self.tile_requests.fetch_add(1, Ordering::Relaxed);
        }
        match req.kind {
            RequestKind::Pixels => {
                self.page_interpretations.fetch_add(1, Ordering::Relaxed);
                self.rasterizations.fetch_add(1, Ordering::Relaxed);
            }
            RequestKind::Text => {
                self.text_interpretations.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn snapshot(&self) -> RenderStats {
        RenderStats {
            parser_builds: self.parser_builds.load(Ordering::Relaxed),
            page_interpretations: self.page_interpretations.load(Ordering::Relaxed),
            render_requests: self.render_requests.load(Ordering::Relaxed),
            tile_requests: self.tile_requests.load(Ordering::Relaxed),
            text_interpretations: self.text_interpretations.load(Ordering::Relaxed),
            rasterizations: self.rasterizations.load(Ordering::Relaxed),
        }
    }
}

/// Clamp a requested scale so the output respects `MAX_SIDE` and `MAX_PIXELS`.
pub fn effective_scale(width_pt: f32, height_pt: f32, scale: f32) -> f32 {
    let (w, h) = (width_pt.max(1.0), height_pt.max(1.0));
    let by_side = MAX_SIDE / w.max(h);
    let by_area = (MAX_PIXELS / (w * h)).sqrt();
    // The caps always win: a floor here once let a 934-million-point-wide page (a fuzzed file)
    // render 9 million pixels wide. Only guard against a zero or non-finite scale.
    let capped = scale.min(by_side).min(by_area);
    if capped.is_finite() && capped > 0.0 { capped } else { by_side.min(by_area).max(f32::MIN_POSITIVE) }
}

/// Device pixels that cover `pt` points at `scale`: rounded up, as poppler's `pdftoppm` does, so
/// the last partial row or column of a page is drawn (anti-aliased against the background) rather
/// than cut off. Float noise up to 1/100 px is not rounded up, so 612 pt at 150 dpi stays 1275 px.
/// At least 1; saturates instead of overflowing.
pub fn device_pixels(pt: f32, scale: f32) -> u32 {
    let px = pt * scale - 0.01;
    if px.is_finite() { px.ceil().max(1.0) as u32 } else { 1 }
}

/// Render one page with a caller-owned parser and cache. Panics inside the renderer are caught
/// and reported as `Err((message, panicked))`.
type Output = (u32, u32, Pixels, Option<Arc<crate::text::PageText>>);

fn render_page<'a>(
    pdf: &'a Pdf,
    cache: &RenderCache<'a>,
    settings: &InterpreterSettings,
    config: &RenderConfig,
    req: RenderRequest,
) -> Result<Output, (String, bool)> {
    let reading = request_reading(config, &req);
    if req.kind == RequestKind::Text {
        return match catch_unwind(AssertUnwindSafe(|| crate::text::extract_page(pdf, req.page, settings))) {
            Ok(Some(t)) => Ok((0, 0, Pixels::default(), Some(Arc::new(t)))),
            Ok(None) => Err((format!("page {} does not exist", req.page + 1), false)),
            Err(panic) => Err((format!("text extraction crashed on page {}: {}", req.page + 1, panic_message(&panic)), true)),
        };
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let pages = pdf.pages();
        let page = pages.get(req.page).ok_or_else(|| format!("page {} does not exist", req.page + 1))?;
        let (w, h) = page.render_dimensions();
        if !(w.is_finite() && h.is_finite()) || w < 0.5 || h < 0.5 {
            return Err(format!("page {} has an empty or invalid page box ({w}×{h} pt)", req.page + 1));
        }
        // Reading colours: the page clears to the setting's background, and solid text and
        // line-art paints are remapped (images and patterns never are). Off renders as before.
        let (bg_color, color_replacement) = match reading.params {
            Some(p) => (AlphaColor::from_rgb8(p.background[0], p.background[1], p.background[2]), Some(hayro::ColorReplacement::from(p))),
            None => (WHITE, None),
        };
        let bg_color = if config.transparent { TRANSPARENT } else { bg_color };
        let rs = match req.tile {
            // Tiles are bounded by construction, so the page itself may be arbitrarily large.
            Some(t) => {
                let scale = req.scale.clamp(0.01, 400.0);
                let (tw, th) = (t.w.clamp(1, 4096), t.h.clamp(1, 4096));
                RenderSettings {
                    x_scale: scale,
                    y_scale: scale,
                    width: Some(tw as u16),
                    height: Some(th as u16),
                    x_offset: t.x as f32,
                    y_offset: t.y as f32,
                    bg_color,
                    color_replacement,
                }
            }
            None => {
                let scale = effective_scale(w, h, req.scale);
                if config.reject_oversize && (!(req.scale.is_finite() && req.scale > 0.0) || req.scale > scale) {
                    return Err(format!(
                        "requested page {} raster scale {req_scale} exceeds renderer limits (maximum scale {scale})",
                        req.page + 1,
                        req_scale = req.scale
                    ));
                }
                // hayro floors the size when none is given, losing the partial edge pixels. The
                // scale keeps each side within MAX_SIDE (give or take float noise), so it fits u16.
                let side = |pt: f32| device_pixels(pt, scale).min(MAX_SIDE as u32) as u16;
                RenderSettings {
                    x_scale: scale,
                    y_scale: scale,
                    width: Some(side(w)),
                    height: Some(side(h)),
                    bg_color,
                    color_replacement,
                    ..Default::default()
                }
            }
        };
        // Rendered straight into a buffer of whole pixels, which the GUI takes over as is.
        let (w, h) = render_size(page, &rs);
        if u64::from(w).checked_mul(u64::from(h)).is_none_or(|pixels| pixels > MAX_PIXELS as u64) {
            return Err(format!("page {} raster size {w}x{h} exceeds renderer pixel limit", req.page + 1));
        }
        let mut pixels = Pixels::zeroed(w.into(), h.into()).ok_or_else(|| format!("page {} is too large to render", req.page + 1))?;
        // Previews render at low detail (tiny images are skipped); finals always draw all.
        let mut preview_settings = settings.clone();
        preview_settings.preview = req.preview;
        render_into(page, cache, &preview_settings, &rs, pixels.bytes_mut());
        Ok((w.into(), h.into(), pixels, None))
    }));
    match result {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err((e, false)),
        Err(panic) => Err((format!("renderer crashed on page {}: {}", req.page + 1, panic_message(&panic)), true)),
    }
}

fn finish(req: RenderRequest, start: Stopwatch, r: Result<Output, (String, bool)>, warnings: Vec<RenderWarning>) -> RenderedPage {
    let millis = start.millis();
    match r {
        Ok((width, height, rgba, text)) => RenderedPage { request: req, width, height, rgba, error: None, text, warnings, millis },
        Err((e, _)) => RenderedPage { request: req, width: 0, height: 0, rgba: Pixels::default(), error: Some(e), text: None, warnings, millis },
    }
}

fn warning_settings(settings: &InterpreterSettings, warnings: Arc<Mutex<Vec<RenderWarning>>>) -> InterpreterSettings {
    let target = warnings;
    InterpreterSettings {
        warning_sink: Arc::new(move |warning| {
            // Nested forms, Type 3 glyphs and patterns are interpreted again and each reports the
            // exhausted budget, or the colours that fell back: once per page is enough.
            let warning = match warning {
                InterpreterWarning::ContentTruncated => RenderWarning::ContentTruncated,
                InterpreterWarning::ColorFallback => RenderWarning::ColourFallback,
                InterpreterWarning::UnsupportedFont | InterpreterWarning::ImageDecodeFailure => return,
            };
            let mut warnings = lock(&target);
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }),
        ..settings.clone()
    }
}

fn take_warnings(warnings: &Mutex<Vec<RenderWarning>>) -> Vec<RenderWarning> {
    std::mem::take(&mut *lock(warnings))
}

/// A single-threaded renderer over one parsed document (used by the CLI and tests).
pub struct PageRenderer {
    bytes: Arc<Vec<u8>>,
    config: RenderConfig,
    pdf: Option<Pdf>,
    settings: InterpreterSettings,
    stats: RenderStats,
}

impl PageRenderer {
    pub fn new(bytes: Arc<Vec<u8>>, config: RenderConfig) -> Self {
        let pdf = parse(&bytes, config.password.as_deref());
        let settings = config.settings();
        Self { bytes, config, pdf, settings, stats: RenderStats { parser_builds: 1, ..RenderStats::default() } }
    }

    pub fn page_count(&self) -> usize {
        self.pdf.as_ref().map(|p| p.pages().len()).unwrap_or(0)
    }

    /// Snapshot the work counters collected by this renderer.
    pub fn stats(&self) -> RenderStats {
        self.stats
    }

    /// Extract one page for statistics, with semantic exclusions. Never rasterizes or panics.
    pub fn filtered_text(&mut self, page: usize, filter: crate::text::TextFilter) -> Result<crate::PageText, String> {
        let pdf = self.pdf.as_ref().ok_or("the document could not be parsed")?;
        self.stats.text_interpretations = self.stats.text_interpretations.saturating_add(1);
        catch_unwind(AssertUnwindSafe(|| crate::text::extract_page_filtered(pdf, page, &self.settings, filter)))
            .map_err(|e| format!("text extraction failed: {}", panic_message(&e)))?
    }

    /// Render one page. Never panics.
    pub fn render(&mut self, req: RenderRequest) -> RenderedPage {
        let start = Stopwatch::start();
        if self.pdf.is_some() {
            self.stats.render_requests += 1;
            if req.tile.is_some() {
                self.stats.tile_requests += 1;
            }
            match req.kind {
                RequestKind::Pixels => {
                    self.stats.page_interpretations += 1;
                    self.stats.rasterizations += 1;
                }
                RequestKind::Text => self.stats.text_interpretations += 1,
            }
        }
        let Some(pdf) = self.pdf.as_ref() else {
            return finish(req, start, Err(("the document could not be parsed".into(), false)), Vec::new());
        };
        let cache = RenderCache::new();
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let settings = warning_settings(&self.settings, warnings.clone());
        let r = render_page(pdf, &cache, &settings, &self.config, req);
        let warnings = take_warnings(&warnings);
        if matches!(r, Err((_, true))) {
            self.pdf = parse(&self.bytes, self.config.password.as_deref());
            self.stats.parser_builds += 1;
        }
        finish(req, start, r, warnings)
    }
}

fn parse(bytes: &Arc<Vec<u8>>, password: Option<&str>) -> Option<Pdf> {
    catch_unwind(AssertUnwindSafe(|| Pdf::new_with_password(bytes.clone(), password.unwrap_or("")).ok())).ok().flatten()
}

pub(crate) fn panic_message(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>().map(ToString::to_string).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown panic".into())
}

/// How long one page may render before the pool gives up on it (the watchdog).
pub const STUCK_AFTER: std::time::Duration = std::time::Duration::from_secs(20);

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum BusyStage {
    /// Parsing the document, shared or private. Not timed: a huge or damaged file is slow to
    /// open, not stuck, and giving up on it would fail every page of the pool. (Parsing has
    /// its own limits.) Rendering is timed from the moment parsing ends.
    Parsing,
    SharedRender,
    Private,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
struct Busy {
    request: RenderRequest,
    since: std::time::Instant,
    stage: BusyStage,
    cancelled: Arc<AtomicBool>,
    valid: Arc<AtomicBool>,
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[derive(Clone)]
struct BusyPublishGate {
    request: RenderRequest,
    entered: std::sync::mpsc::Sender<()>,
    release: Arc<Mutex<Receiver<()>>>,
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[derive(Clone)]
struct RenderGate {
    page: usize,
    shared_generation: bool,
    claimed: Arc<std::sync::atomic::AtomicBool>,
    entered: Arc<std::sync::Barrier>,
    release: Arc<std::sync::Barrier>,
    finished: Arc<std::sync::Barrier>,
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[derive(Clone)]
struct ParseGate {
    entered: Arc<std::sync::Barrier>,
    release: Arc<std::sync::Barrier>,
}

/// State shared by the pool and its workers.
#[derive(Default)]
struct Shared {
    /// Desired requests, active jobs and bounded completed results share one deduplication key.
    queue: Arc<WorkQueue>,
    /// Per worker id: the request it is rendering and since when.
    #[cfg(not(target_arch = "wasm32"))]
    busy: Mutex<Vec<Option<Busy>>>,
    /// Exact requests the watchdog gave up on: answered with an error at once, so a pathological
    /// request cannot trap every worker in turn while other scales, tiles, or generations remain usable.
    stuck: Mutex<std::collections::HashSet<RequestFailureKey>>,
    stats: AtomicRenderStats,
    /// Per worker id: set to stop that worker's render at its next content operator once nobody
    /// can receive its answer (the pool was dropped, or the watchdog gave up on the render).
    /// Lock `busy` first when holding both.
    stop: Mutex<Vec<Arc<std::sync::atomic::AtomicBool>>>,
    /// Test hook: hold one worker at the render boundary until the test releases it.
    #[cfg(all(test, not(target_arch = "wasm32")))]
    render_gates: Mutex<Vec<RenderGate>>,
    /// Test hook: hold parser initialization until the test releases it.
    #[cfg(all(test, not(target_arch = "wasm32")))]
    parse_gate: Mutex<Option<ParseGate>>,
    /// Test hook: make rendering this page take this long (cancellable).
    #[cfg(all(test, not(target_arch = "wasm32")))]
    slow_page: Mutex<Option<(usize, std::time::Duration)>>,
    /// Test hook: make parser initialization take this long.
    #[cfg(all(test, not(target_arch = "wasm32")))]
    slow_parse: Mutex<Option<std::time::Duration>>,
    /// Test hook: count parser initializations, including failed parses.
    #[cfg(all(test, not(target_arch = "wasm32")))]
    parses: std::sync::atomic::AtomicUsize,
    #[cfg(not(target_arch = "wasm32"))]
    parser: PoolParser,
    #[cfg(all(test, not(target_arch = "wasm32")))]
    panic_page: Mutex<Option<usize>>,
    #[cfg(all(test, not(target_arch = "wasm32")))]
    render_started: std::sync::atomic::AtomicUsize,
    #[cfg(all(test, not(target_arch = "wasm32")))]
    busy_publish_gate: Mutex<Option<BusyPublishGate>>,
    #[cfg(all(test, not(target_arch = "wasm32")))]
    parser_stage_entries: Mutex<Vec<RenderRequest>>,
    #[cfg(all(test, not(target_arch = "wasm32")))]
    shared_render_delay: Mutex<Option<std::time::Duration>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(not(target_arch = "wasm32"))]
fn same_request(a: RenderRequest, b: RenderRequest) -> bool {
    // `preview` is excluded on purpose: it names the same work for cancellation, only the
    // detail level of the result differs.
    a.page == b.page && a.kind == b.kind && a.tile == b.tile && a.scale.to_bits() == b.scale.to_bits() && a.tag == b.tag
}

#[cfg(not(target_arch = "wasm32"))]
impl Shared {
    fn cancel_in_flight(&self, obsolete: &[RenderRequest]) {
        let busy = lock(&self.busy);
        for busy in busy.iter().flatten() {
            if obsolete.iter().copied().any(|request| same_request(request, busy.request)) {
                busy.cancelled.store(true, Ordering::Release);
                busy.valid.store(false, Ordering::Release);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn set_busy_stage(shared: &Shared, id: usize, stage: BusyStage) -> bool {
    let mut slots = lock(&shared.busy);
    let Some(Some(busy)) = slots.get_mut(id) else { return false };
    if busy.stage != stage {
        busy.stage = stage;
        busy.since = std::time::Instant::now();
    }
    true
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
enum ParserState {
    #[default]
    Empty,
    Loading,
    Ready(Option<Arc<Pdf>>),
    /// A page failed after initialization: isolate subsequent work per worker.
    Private,
    /// Retired before it became ready (defensive: parsing is not timed, and only a shared
    /// render retires the parser). Retrying it on every worker cannot help.
    Failed,
}

#[cfg(not(target_arch = "wasm32"))]
struct PoolParser {
    state: Mutex<ParserState>,
    ready: std::sync::Condvar,
    valid: Arc<AtomicBool>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for PoolParser {
    fn default() -> Self {
        Self { state: Mutex::default(), ready: std::sync::Condvar::new(), valid: Arc::new(AtomicBool::new(true)) }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl PoolParser {
    /// `None` means use a private parser. `Some(None)` is a terminal parse failure.
    fn acquire(&self, load: impl FnOnce() -> Option<Pdf>, cancelled: impl Fn() -> bool) -> Option<Option<Arc<Pdf>>> {
        let mut state = lock(&self.state);
        loop {
            if cancelled() {
                return Some(None);
            }
            match &*state {
                ParserState::Ready(pdf) => return Some(pdf.clone()),
                ParserState::Private => return None,
                ParserState::Failed => return Some(None),
                ParserState::Loading => {
                    // Drop never joins a parser. A waiting worker must still notice
                    // its pool's disconnected wake channel while another thread is
                    // stuck in initialization and cannot signal this condition.
                    state = self.ready.wait_timeout(state, std::time::Duration::from_millis(50)).unwrap_or_else(std::sync::PoisonError::into_inner).0;
                }
                ParserState::Empty => {
                    *state = ParserState::Loading;
                    break;
                }
            }
        }
        drop(state);
        // Parsing cannot hold the lifecycle mutex: watchdog/drop must remain nonblocking.
        // Even a panic in initialization publishes one terminal failure and wakes waiters.
        let pdf = catch_unwind(AssertUnwindSafe(load)).ok().flatten().map(Arc::new);
        let mut state = lock(&self.state);
        let result = match &*state {
            ParserState::Loading => {
                *state = ParserState::Ready(pdf.clone());
                Some(pdf)
            }
            ParserState::Private => None,
            _ => Some(None), // the watchdog retired initialization while it ran
        };
        self.ready.notify_all();
        result
    }

    fn abandon(&self) {
        self.retire();
    }

    /// After a shared render failed or timed out: later work parses privately, per worker
    /// (for the rest of the pool's life: the old one-parser-per-worker memory use). A parser
    /// that never became ready stays failed.
    fn retire(&self) {
        let mut state = lock(&self.state);
        self.valid.store(false, Ordering::Release);
        *state = match &*state {
            ParserState::Loading | ParserState::Empty | ParserState::Failed => ParserState::Failed,
            _ => ParserState::Private,
        };
        self.ready.notify_all();
    }
}

/// Renders pages on worker threads, most urgent request first.
///
/// **Watchdog:** a render running longer than `stuck_after` (default [`STUCK_AFTER`]) is given
/// up on. A timed-out *shared* render retires that parser and retries still-desired work
/// privately: the page may only have been waiting behind another page's decode. A private
/// timeout is reported as an error for that page and the page is not attempted again. Threads
/// cannot be killed: the stuck worker is told to stop at its next content operator, exits when
/// its render returns, and its late result is dropped; a single long operator, such as decoding
/// a huge image, still runs to its end. At most `threads` replacements are started per pool, so
/// a document full of pathological pages cannot spawn threads without bound. If every worker is
/// retired after that budget is spent, pending and future requests fail explicitly instead of
/// waiting forever.
pub struct RenderPool {
    shared: Arc<Shared>,
    wake: Mutex<Vec<SyncSender<()>>>,
    results: ResultReceiver,
    results_tx: Arc<WorkQueue>,
    bytes: Arc<Vec<u8>>,
    config: RenderConfig,
    _workers: Mutex<Vec<JoinHandle<()>>>,
    stuck_after: std::time::Duration,
    replacements_left: Mutex<usize>,
    retired_workers: Mutex<usize>,
    /// Used when threads are unavailable (wasm32 without atomics, or spawn failure): requests are
    /// rendered on the calling thread inside `try_recv`, one per call.
    inline: Option<std::cell::RefCell<PageRenderer>>,
}

impl RenderPool {
    pub fn new(bytes: Arc<Vec<u8>>, threads: usize, config: RenderConfig) -> Self {
        Self::with_mode(bytes, threads, config, false)
    }

    fn with_mode(bytes: Arc<Vec<u8>>, threads: usize, config: RenderConfig, inline: bool) -> Self {
        let threads = if inline || cfg!(target_arch = "wasm32") { 0 } else { threads.max(1) };
        let shared = Arc::new(Shared::default());
        let results_tx = shared.queue.clone();
        let results = ResultReceiver(shared.queue.clone());
        let mut pool = Self {
            shared,
            wake: Mutex::new(Vec::new()),
            results,
            results_tx,
            bytes: bytes.clone(),
            config: config.clone(),
            _workers: Mutex::new(Vec::new()),
            stuck_after: STUCK_AFTER,
            replacements_left: Mutex::new(threads),
            retired_workers: Mutex::new(0),
            inline: None,
        };
        for _ in 0..threads {
            pool.spawn_worker();
        }
        if lock(&pool._workers).is_empty() {
            pool.inline = Some(std::cell::RefCell::new(PageRenderer::new(bytes, config)));
        }
        pool
    }

    /// Start one worker thread; returns whether it started.
    fn spawn_worker(&self) -> bool {
        #[cfg(target_arch = "wasm32")]
        return false;
        #[cfg(not(target_arch = "wasm32"))]
        {
            let (id, stop) = {
                let mut busy = lock(&self.shared.busy);
                busy.push(None);
                let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                lock(&self.shared.stop).push(stop.clone());
                (busy.len() - 1, stop)
            };
            let (wtx, wrx) = sync_channel::<()>(1);
            let (shared, out, bytes, config) = (self.shared.clone(), self.results_tx.clone(), self.bytes.clone(), self.config.clone());
            match std::thread::Builder::new().name(format!("pdfcraft-render-{id}")).spawn(move || worker(id, bytes, config, shared, wrx, out, stop)) {
                Ok(h) => {
                    lock(&self.wake).push(wtx);
                    lock(&self._workers).push(h);
                    true
                }
                Err(e) => {
                    log::error!("could not spawn render worker {id}: {e}");
                    false
                }
            }
        }
    }

    /// A pool without worker threads: renders inside `try_recv` (the web path; also used in tests).
    pub fn new_inline(bytes: Arc<Vec<u8>>, config: RenderConfig) -> Self {
        Self::with_mode(bytes, 0, config, true)
    }

    /// `true` when rendering happens on the caller's thread (no worker threads available).
    pub fn is_inline(&self) -> bool {
        self.inline.is_some()
    }

    /// Snapshot the work counters collected by this pool.
    pub fn stats(&self) -> RenderStats {
        self.inline.as_ref().map_or_else(|| self.shared.stats.snapshot(), |renderer| renderer.borrow().stats())
    }

    /// Change the watchdog limit (tests and benchmarks).
    pub fn set_stuck_after(&mut self, limit: std::time::Duration) {
        self.stuck_after = limit;
    }

    /// Replace desired work (most urgent first), retaining matching in-flight/completed jobs.
    /// Obsolete results are released before they reach the caller, and active obsolete renders
    /// are asked to stop at their next cancellation point.
    ///
    /// The contract: a result is delivered only while its request is in the latest queue.
    /// A caller still waiting for a page (or text, or a thumbnail) must keep listing it in
    /// every call, or the finished result is dropped to save memory.
    pub fn set_queue(&self, requests: Vec<RenderRequest>) {
        #[cfg(not(target_arch = "wasm32"))]
        let obsolete = self.shared.queue.replace(requests);
        #[cfg(target_arch = "wasm32")]
        let _ = self.shared.queue.replace(requests);
        #[cfg(not(target_arch = "wasm32"))]
        self.shared.cancel_in_flight(&obsolete);
        self.wake_workers();
    }

    fn wake_workers(&self) {
        for w in lock(&self.wake).iter() {
            // One pending wakeup is sufficient; UI polling must not grow wake channels.
            let _ = w.try_send(());
        }
    }

    pub fn try_recv(&self) -> Option<RenderedPage> {
        if let Some(r) = &self.inline {
            let req = self.shared.queue.pop()?;
            let page = r.borrow_mut().render(req);
            self.shared.queue.finish_inline(req);
            return Some(page);
        }
        self.watchdog();
        let page = self.results.try_recv().ok();
        // Consuming a result releases byte/count capacity. An invalid parser generation
        // can also requeue work without returning a page, so wake in both cases.
        self.wake_workers();
        page.or_else(|| {
            let workers = lock(&self._workers).len();
            if workers == 0 || *lock(&self.retired_workers) < workers {
                return None;
            }
            // Count successful handles, not reserved busy slots: thread spawning
            // can fail. No worker remains able to service these pending requests.
            let request = self.shared.queue.pop()?;
            self.shared.queue.finish_inline(request);
            Some(RenderedPage {
                request,
                width: 0,
                height: 0,
                rgba: Pixels::default(),
                error: Some("all render workers exceeded their time limits; close and reopen the document to try again".into()),
                text: None,
                warnings: Vec::new(),
                millis: 0,
            })
        })
    }

    /// Give up on renders that exceeded `stuck_after` (see the type docs).
    fn watchdog(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut gave_up = Vec::new();
            {
                let mut busy = lock(&self.shared.busy);
                // Lock order: `busy`, then `stop`.
                let stop = lock(&self.shared.stop);
                for (id, slot) in busy.iter_mut().enumerate() {
                    if let Some(busy) = slot.clone()
                        && busy.stage != BusyStage::Parsing
                        && busy.since.elapsed() > self.stuck_after
                    {
                        *slot = None; // the worker sees this and exits when it returns
                        // Told to stop at its next operator; its replacement retries the page.
                        busy.cancelled.store(true, Ordering::Release);
                        busy.valid.store(false, Ordering::Release);
                        if let Some(s) = stop.get(id) {
                            s.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        gave_up.push(busy);
                    }
                }
            }
            {
                let mut retired = lock(&self.retired_workers);
                *retired = retired.saturating_add(gave_up.len());
            }
            for busy in gave_up {
                let req = busy.request;
                self.shared.parser.retire();
                if busy.stage == BusyStage::SharedRender {
                    // The shared cold-decode gate can make a healthy page wait
                    // behind another page. Only its private retry can attribute
                    // the timeout to this page; do not blacklist it here.
                    self.shared.queue.retry(req);
                } else {
                    lock(&self.shared.stuck).insert(req.into());
                    let what = if req.kind == RequestKind::Text { "text extraction for page" } else { "page" };
                    let error = format!(
                        "{what} {} took longer than {:.0} s and was skipped; the page may be damaged or extremely complex",
                        req.page + 1,
                        self.stuck_after.as_secs_f32().max(1.0)
                    );
                    self.shared.queue.abandon(RenderedPage {
                        request: req,
                        width: 0,
                        height: 0,
                        rgba: Pixels::default(),
                        error: Some(error),
                        text: None,
                        warnings: Vec::new(),
                        millis: 0,
                    });
                }
                let mut left = lock(&self.replacements_left);
                if *left > 0 && self.spawn_worker() {
                    *left -= 1;
                }
            }
        }
    }
}

/// A pool's threads after [`RenderPool::retire`]. Idle ones exit at once; one still rendering is
/// told to stop at its next drawing operator and exits when its render returns, which a single
/// long operator (such as decoding a huge image) can delay.
pub struct Retiring(Vec<JoinHandle<()>>);

impl Retiring {
    /// The threads that haven't exited yet.
    pub fn running(&mut self) -> usize {
        self.0.retain(|h| !h.is_finished());
        self.0.len()
    }

    /// Whether every thread has exited.
    pub fn exited(&self) -> bool {
        self.0.iter().all(JoinHandle::is_finished)
    }
}

impl RenderPool {
    /// No replacement threads: when the watchdog gives up on a render, the request (and any
    /// later one) is answered with an error instead of starting another thread. For a caller that
    /// bounds how many render threads exist at once: the stuck thread keeps running until its
    /// operator returns, and a replacement would run beside it.
    pub fn without_replacements(self) -> Self {
        *lock(&self.replacements_left) = 0;
        self
    }

    /// This pool's threads that haven't exited (workers and replacements; 0 when rendering inline).
    pub fn threads(&self) -> usize {
        lock(&self._workers).iter().filter(|h| !h.is_finished()).count()
    }

    /// Drop the pool but keep its threads' handles, for a caller that bounds how many render
    /// threads run at once: a render the watchdog gave up on can outlive its pool.
    pub fn retire(self) -> Retiring {
        let handles = std::mem::take(&mut *lock(&self._workers));
        drop(self);
        Retiring(handles)
    }
}

impl Drop for RenderPool {
    fn drop(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        for busy in lock(&self.shared.busy).iter().flatten() {
            busy.cancelled.store(true, Ordering::Release);
            busy.valid.store(false, Ordering::Release);
        }
        for s in lock(&self.shared.stop).iter() {
            s.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// A worker's interpreter settings: the document's, stopping once the request is cancelled.
#[cfg(not(target_arch = "wasm32"))]
fn worker_settings(config: &RenderConfig, stop: &Arc<std::sync::atomic::AtomicBool>) -> InterpreterSettings {
    InterpreterSettings { cancelled: Some(stop.clone()), ..config.settings() }
}

#[cfg(not(target_arch = "wasm32"))]
fn worker(
    id: usize,
    bytes: Arc<Vec<u8>>,
    config: RenderConfig,
    shared: Arc<Shared>,
    wake: Receiver<()>,
    out: Arc<WorkQueue>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    let warnings = Arc::new(Mutex::new(Vec::new()));
    // A local cell keeps Pdf borrows used by RenderCache stable. Drop both on an
    // epoch change/panic before publishing an error or taking another request.
    loop {
        let reset_result = {
            let parsed = std::cell::OnceCell::new();
            loop {
                if parsed.get().is_some_and(|(_, _, shared_generation)| *shared_generation) && !shared.parser.valid.load(Ordering::Acquire) {
                    break None;
                }
                let next = shared.queue.pop();
                let Some(req) = next else {
                    if wake.recv().is_err() {
                        return; // pool dropped
                    }
                    continue;
                };
                if shared.queue.cancel_obsolete(req) {
                    continue;
                }
                #[cfg(all(test, not(target_arch = "wasm32")))]
                if let Some(gate) = lock(&shared.busy_publish_gate).clone().filter(|gate| same_request(gate.request, req)) {
                    let _ = gate.entered.send(());
                    let _ = lock(&gate.release).recv();
                }
                let request_cancelled = Arc::new(AtomicBool::new(false));
                let request_valid = Arc::new(AtomicBool::new(true));
                if lock(&shared.stuck).contains(&req.into()) {
                    let error = format!("page {} was skipped earlier because rendering failed or took too long", req.page + 1);
                    let page = RenderedPage {
                        request: req,
                        width: 0,
                        height: 0,
                        rgba: Pixels::default(),
                        error: Some(error),
                        text: None,
                        warnings: Vec::new(),
                        millis: 0,
                    };
                    if out.send_valid(page, None).is_err() {
                        return;
                    }
                    continue;
                }
                let start = Stopwatch::start();
                shared.stats.record_request(req);
                let stage = parsed.get().map_or(
                    BusyStage::Parsing,
                    |(_, _, shared_generation)| {
                        if *shared_generation { BusyStage::SharedRender } else { BusyStage::Private }
                    },
                );
                if let Some(slot) = lock(&shared.busy).get_mut(id) {
                    *slot = Some(Busy {
                        request: req,
                        since: std::time::Instant::now(),
                        stage,
                        cancelled: request_cancelled.clone(),
                        valid: request_valid.clone(),
                    });
                }
                if shared.queue.cancel_obsolete(req) {
                    if lock(&shared.busy).get_mut(id).is_none_or(|slot| slot.take().is_none()) {
                        return;
                    }
                    continue;
                }
                #[cfg(all(test, not(target_arch = "wasm32")))]
                lock(&shared.parser_stage_entries).push(req);
                // A request cancelled while it waited for the parser must not leave its abort
                // behind as this worker's parse: every later request would then fail as unparsable.
                let aborted = || request_cancelled.load(Ordering::Acquire) || stop.load(Ordering::Acquire);
                if parsed.get().is_none() {
                    let init = {
                        let load = || {
                            #[cfg(test)]
                            {
                                shared.parses.fetch_add(1, Ordering::Relaxed);
                                if let Some(gate) = lock(&shared.parse_gate).clone() {
                                    gate.entered.wait();
                                    gate.release.wait();
                                }
                                let delay = *lock(&shared.slow_parse);
                                if let Some(delay) = delay {
                                    std::thread::sleep(delay);
                                }
                            }
                            shared.stats.parser_builds.fetch_add(1, Ordering::Relaxed);
                            parse(&bytes, config.password.as_deref())
                        };
                        match shared.parser.acquire(load, || {
                            request_cancelled.load(Ordering::Acquire)
                                || stop.load(Ordering::Acquire)
                                || matches!(wake.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected))
                        }) {
                            Some(pdf) => {
                                let shared_generation = pdf.is_some();
                                (pdf, RenderCache::new(), shared_generation)
                            }
                            None => {
                                let pdf = if !request_cancelled.load(Ordering::Acquire)
                                    && !stop.load(Ordering::Acquire)
                                    && set_busy_stage(&shared, id, BusyStage::Parsing)
                                {
                                    load().map(Arc::new)
                                } else {
                                    None
                                };
                                (pdf, RenderCache::new(), false)
                            }
                        }
                    };
                    if aborted() {
                        if lock(&shared.busy).get_mut(id).is_none_or(|slot| slot.take().is_none()) {
                            return;
                        }
                        // Release the active key, retrying if demand returned during parsing.
                        shared.queue.retry(req);
                        continue;
                    }
                    let _ = parsed.set(init);
                }
                // Set just above when it was empty.
                let Some((pdf, cache, shared_generation)) = parsed.get() else {
                    if lock(&shared.busy).get_mut(id).is_none_or(|slot| slot.take().is_none()) {
                        return;
                    }
                    continue;
                };
                // Acquisition may have waited or parsed privately after shared
                // fallback. Retire before any render work if the watchdog gave
                // this request to a replacement while initialization ran.
                let stage = if *shared_generation { BusyStage::SharedRender } else { BusyStage::Private };
                if !set_busy_stage(&shared, id, stage) {
                    return;
                }
                if shared.queue.cancel_obsolete(req) {
                    if lock(&shared.busy).get_mut(id).is_none_or(|slot| slot.take().is_none()) {
                        return;
                    }
                    continue;
                }
                if *shared_generation && !shared.parser.valid.load(Ordering::Acquire) {
                    if lock(&shared.busy).get_mut(id).is_none_or(|slot| slot.take().is_none()) {
                        return;
                    }
                    break Some((
                        finish(req, start, Err(("the shared parser was retired".into(), false)), Vec::new()),
                        vec![request_valid, shared.parser.valid.clone()],
                    ));
                }
                #[cfg(test)]
                {
                    shared.render_started.fetch_add(1, Ordering::Relaxed);
                    let shared_delay = *lock(&shared.shared_render_delay);
                    if *shared_generation && let Some(delay) = shared_delay {
                        std::thread::sleep(delay);
                    }
                    // Copy out first: a guard held in the `if let` would block the other workers.
                    let slow = *lock(&shared.slow_page);
                    if let Some((page, delay)) = slow
                        && page == req.page
                    {
                        let deadline = std::time::Instant::now() + delay;
                        while std::time::Instant::now() < deadline && !request_cancelled.load(Ordering::Acquire) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                    }
                }
                #[cfg(all(test, not(target_arch = "wasm32")))]
                let render_gate = lock(&shared.render_gates)
                    .iter()
                    .find(|gate| {
                        gate.page == req.page
                            && gate.shared_generation == *shared_generation
                            && gate.claimed.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok()
                    })
                    .cloned();
                #[cfg(all(test, not(target_arch = "wasm32")))]
                if let Some(gate) = &render_gate {
                    gate.entered.wait();
                    gate.release.wait();
                }
                let settings = warning_settings(&worker_settings(&config, &request_cancelled), warnings.clone());
                lock(&warnings).clear();
                let r = catch_unwind(AssertUnwindSafe(|| {
                    #[cfg(test)]
                    if *lock(&shared.panic_page) == Some(req.page) {
                        panic!("injected page panic");
                    }
                    match pdf {
                        Some(pdf) => render_page(pdf, cache, &settings, &config, req),
                        None => Err(("the document could not be parsed".into(), false)),
                    }
                }))
                .unwrap_or_else(|panic| Err((format!("renderer crashed on page {}: {}", req.page + 1, panic_message(&panic)), true)));
                let request_warnings = take_warnings(&warnings);
                // If the watchdog cleared our slot meanwhile, it already answered for this request
                // and started a replacement: drop the late result and retire.
                let abandoned = lock(&shared.busy).get_mut(id).is_none_or(|slot| slot.take().is_none());
                if abandoned {
                    #[cfg(all(test, not(target_arch = "wasm32")))]
                    if let Some(gate) = render_gate {
                        gate.finished.wait();
                    }
                    return;
                }
                let panicked = matches!(r, Err((_, true)));
                if panicked {
                    shared.parser.abandon();
                    lock(&shared.stuck).insert(req.into());
                    break Some((finish(req, start, r, request_warnings), vec![request_valid]));
                }
                let mut valid = vec![request_valid];
                if *shared_generation {
                    valid.push(shared.parser.valid.clone());
                }
                if *shared_generation && !shared.parser.valid.load(Ordering::Acquire) {
                    break Some((finish(req, start, r, request_warnings), valid));
                }
                if out.send_valid_with_tokens(finish(req, start, r, request_warnings), valid).is_err() {
                    return;
                }
            }
        }; // drop the parser and its borrowing cache before delivering reset_result
        if let Some((page, valid)) = reset_result
            && out.send_valid_with_tokens(page, valid).is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
