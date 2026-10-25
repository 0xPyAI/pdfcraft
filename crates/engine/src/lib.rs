//! pdfcraft-engine — the façade every frontend talks to (architecture §4).
//!
//! Holds open documents, their edit history and the tool catalogue. Frontends never touch the
//! parsing, rendering or editing crates directly.
//!
//! **Editing model.** Each document keeps a `pdfcraft_cos::Document` (the object graph with a
//! copy-on-write overlay of edits). An edit runs on a clone, and on success the previous state is
//! pushed onto the undo stack (clones share all unchanged data, so this is cheap). After every
//! edit the *working file* is produced by an incremental write — original bytes plus one
//! appended revision — and the view is refreshed from it, so what you see is exactly what Save
//! will write. Saving rebases onto the written bytes, so the next save appends only new edits.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod actions;
pub mod autotag;
pub mod build_info;
pub mod camera;
pub mod camera_clean;
pub mod camera_edges;
pub mod catalog;
pub mod commands;
pub mod compare;
mod copy_image;
pub mod dates;
mod document;
mod edit;
mod error;
pub mod export;
pub mod field_table;
mod history;
pub mod js;
pub mod links;
pub mod ocr;
mod open;
pub mod optimizer;
pub mod print_raster;
pub mod printcheck;
mod protection;
mod reexports;
mod scan_colours;
mod scan_fields;
mod session;
pub mod signature_image;
pub mod signature_scan;
mod stamp_preview;
pub mod text_fonts;
mod text_preview;
pub mod word_count;
pub mod xfa;

pub use signature_image::{ImageSignaturePreview, SignatureImage};
pub use signature_scan::{PhotoRect, ScanError, SignaturePhoto, Turn};
pub use stamp_preview::StampPreview;
pub use text_preview::TextEditPreview;

pub use reexports::*;

/// A change to an existing page image.
#[derive(Clone, Debug, PartialEq)]
pub enum ImageEdit {
    /// Move and resize it to this box (user space).
    Move([f64; 4]),
    /// Quarter turns clockwise about its centre.
    Rotate(i32),
    /// Any angle in degrees clockwise about its centre (the unit is degrees, not quarter turns).
    RotateBy(f64),
    Flip {
        horizontal: bool,
    },
    /// Show only this box of it (user space), the pixels staying where they are.
    Crop([f64; 4]),
    /// Draw this image file (PNG, JPEG, TIFF, GIF, BMP, JPEG 2000) in its place.
    Replace {
        name: String,
        bytes: Arc<Vec<u8>>,
    },
    Delete,
}

/// Fill & Sign: `text` in the script font as a typed signature, its left edge at `at` (user
/// space, vertically centred) and `height` points tall. `None` for text with no outlines.
///
/// Left, centred and upright are as displayed on a page turned by `rotation` (its `/Rotate`):
/// the box runs along the displayed axes, like an image signature's
/// ([`SignatureImage::rect`]), and the outlines are turned back so the name reads across.
pub fn typed_signature_shape(at: [f64; 2], text: &str, height: f64, rotation: i64) -> Option<Shape> {
    let o = script_outline(text);
    let [left, bottom, right, top] = o.bounds();
    let span = (top - bottom).max(0.1);
    let width = right - left;
    if o.contours.is_empty() || o.width <= 0.0 || !(width > 0.0 && height > 0.0) {
        return None;
    }
    let k = height / span;
    let rect = signature_image::upright_box(rotation, at, width * k, height)?;
    // Displayed right and up as user-space unit vectors: a point at (nx, ny) of the displayed box
    // is at this fraction of the user-space box. Exact for an unturned page.
    let [a, b, c, d, ..] = pdfcraft_model::view_matrix_for(rotation, [0.0; 4]);
    let to_user = |nx: f64, ny: f64| [a * nx + c * ny + (-a).max(0.0) + (-c).max(0.0), b * nx + d * ny + (-b).max(0.0) + (-d).max(0.0)];
    let contours = o.contours.iter().map(|c| c.iter().map(|p| to_user((p[0] - left) / width, (p[1] - bottom) / span)).collect()).collect();
    Some(Shape::TypedSignature { rect, contours })
}
/// `at` moved `right` and `up` points as displayed on a page turned by `rotation` (its `/Rotate`).
pub fn displayed_offset(rotation: i64, at: [f64; 2], right: f64, up: f64) -> [f64; 2] {
    let [a, b, c, d, ..] = pdfcraft_model::view_matrix_for(rotation, [0.0; 4]);
    [at[0] + a * right + c * up, at[1] + b * right + d * up]
}

/// The user-space box `width` x `height` points as displayed on a page turned by `rotation`,
/// with its displayed top-left corner at `at`: where typed text and text boxes go. `None` for a
/// non-finite point.
pub fn text_box_at(rotation: i64, at: [f64; 2], width: f64, height: f64) -> Option<[f64; 4]> {
    signature_image::upright_box(rotation, displayed_offset(rotation, at, 0.0, -height / 2.0), width, height)
}

/// `text` in the script font as the graphic of a visible digital signature, as Fill & Sign types
/// it. `None` for text with no outlines.
pub fn typed_signature_graphic(text: &str) -> Option<pdfcraft_sign::Graphic> {
    let o = script_outline(text);
    o.contours.iter().any(|c| c.len() > 2).then_some(pdfcraft_sign::Graphic::Outlines(o.contours))
}

pub use links::{
    MAX_REMOTE_DOCUMENT_BYTES, RemoteDocumentPathError, read_remote_document, remote_document_path_is_absolute, resolve_remote_document_path,
};

/// Forms ▸ Merge data files into spreadsheet: the field values of each file (FDF, XFDF or a
/// filled-in PDF form), as CSV with one row per file.
pub fn merge_data_files(files: &[(String, Vec<u8>)]) -> Result<String, String> {
    let rows =
        files.iter().map(|(name, bytes)| pdfcraft_xfdf::data_values(bytes).map_err(|e| format!("{name}: {e}"))).collect::<Result<Vec<_>, _>>()?;
    Ok(pdfcraft_xfdf::merge_csv(&rows))
}

/// One file produced by a split: (1-based first page, last page, PDF bytes).
pub type SplitPart = (usize, usize, Arc<Vec<u8>>);

use std::sync::Arc;

#[cfg(test)]
thread_local! {
    static SECONDARY_SCAN_COUNTS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    static REFRESH_PANICS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn panic_on_refresh(count: usize) {
    REFRESH_PANICS.with(|n| n.set(count));
}

#[cfg(test)]
fn check_refresh_panic() {
    REFRESH_PANICS.with(|n| {
        let rem = n.get();
        if rem > 0 {
            n.set(rem - 1);
            panic!("injected refresh panic");
        }
    });
}

#[cfg(test)]
fn reset_secondary_scan_counts() {
    SECONDARY_SCAN_COUNTS.with(|counts| counts.set((0, 0)));
}

#[cfg(test)]
fn secondary_scan_counts() -> (usize, usize) {
    SECONDARY_SCAN_COUNTS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn record_marker_scan() {
    SECONDARY_SCAN_COUNTS.with(|counts| {
        let (markers, links) = counts.get();
        counts.set((markers + 1, links));
    });
}

#[cfg(test)]
fn record_link_scan() {
    SECONDARY_SCAN_COUNTS.with(|counts| {
        let (markers, links) = counts.get();
        counts.set((markers, links + 1));
    });
}

pub use document::{Document, TextNotice, display_bytes_for, render_password_for};
use document::{display_bytes, info_fields, recovered_render_password, same_pages, shown_under_redactions, undrawn_notices};
use edit::EditCtx;
pub use edit::{BATCH_LABELS, EDIT_LABELS, Edit};
pub use error::{EditError, SourceProblem};
pub use history::Label;
pub use open::{OpenContext, PreparedDocument};
use pdfcraft_render::{Layer, LayerOp, RenderPool};
use protection::unrestricted;
pub use protection::{Changes, Printing, Protection, SecuritySummary};
pub use session::Session;

/// Stable identifier of an open document within a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DocId(pub u64);

/// The highest resolution a document that allows only low-resolution printing (Table 22, bit 12
/// clear) is printed at.
const LOW_RESOLUTION_PRINT_DPI: f64 = 150.0;

fn render_threads() -> usize {
    std::thread::available_parallelism().map(std::num::NonZeroUsize::get).unwrap_or(4).clamp(2, 8) - 1
}

/// Write `layers`' visibility into the default optional-content configuration of `cos` (a copy
/// made for printing): `/BaseState /ON`, `/ON` the visible layers, `/OFF` the hidden ones. Its
/// other entries (`/Order`, `/RBGroups`, `/AS`…) are kept. A document without `/OCProperties`
/// is left alone.
fn set_default_layer_state(cos: &mut pdfcraft_cos::Document, layers: &[Layer]) {
    use pdfcraft_cos::{Dict, ObjRef, Object};
    let Some(root_ref) = cos.trailer().get(b"Root").and_then(Object::as_ref) else { return };
    let Some(mut root) = cos.get(root_ref).as_dict().cloned() else { return };
    let Some(properties) = root.get(b"OCProperties").cloned() else { return };
    let Some(mut properties) = cos.resolve(&properties).as_dict().cloned() else { return };
    let mut config = properties.get(b"D").and_then(|d| cos.resolve(d).as_dict().cloned()).unwrap_or_else(Dict::new);
    let refs =
        |visible: bool| Object::Array(layers.iter().filter(|l| l.visible == visible).map(|l| Object::Ref(ObjRef::new(l.id.0, l.id.1))).collect());
    config.set(b"BaseState".to_vec(), Object::name("ON"));
    config.set(b"ON".to_vec(), refs(true));
    config.set(b"OFF".to_vec(), refs(false));
    properties.set(b"D".to_vec(), Object::Dict(config));
    root.set(b"OCProperties".to_vec(), Object::Dict(properties));
    cos.set(root_ref, Object::Dict(root));
}

/// Render `doc` with the layer visibility in its `info.layers`.
fn use_layer_choices(doc: &mut Document) {
    let overrides: Vec<(i32, i32, bool)> = doc.info.layers.iter().map(|l| (l.id.0 as i32, l.id.1 as i32, l.visible)).collect();
    doc.config.layers = Arc::new(overrides);
    doc.renderer = RenderPool::new(doc.display.clone(), render_threads(), doc.config.clone());
}

/// Apply a set-layer-visibility action to `layers`, one change at a time, so a toggle flips the
/// state the changes before it left. Returns whether any layer ended up changed.
fn apply_layer_state(layers: &mut [Layer], groups: &[Vec<(u32, u16)>], changes: &[(LayerOp, (u32, u16))], preserve_rb: bool) -> bool {
    let before: Vec<bool> = layers.iter().map(|l| l.visible).collect();
    let index: std::collections::HashMap<(u32, u16), usize> = layers.iter().enumerate().map(|(i, l)| (l.id, i)).collect();
    for &(op, ocg) in changes {
        let Some(layer) = index.get(&ocg).and_then(|&i| layers.get_mut(i)) else { continue };
        let on = match op {
            LayerOp::On => true,
            LayerOp::Off => false,
            LayerOp::Toggle => !layer.visible,
        };
        layer.visible = on;
        // Turning a layer off leaves the rest of its groups alone.
        if on && preserve_rb {
            for other in groups.iter().filter(|g| g.contains(&ocg)).flatten().filter(|&&o| o != ocg) {
                if let Some(l) = index.get(other).and_then(|&i| layers.get_mut(i)) {
                    l.visible = false;
                }
            }
        }
    }
    layers.iter().zip(before).any(|(l, was)| l.visible != was)
}

/// A file to combine: its name (the bookmark title), bytes, and page range (`None`: all).
pub type CombineSource = (String, Arc<Vec<u8>>, Option<String>);

/// What a combine produced: the new PDF bytes, and the fields renamed to keep colliding names
/// separate (old fully-qualified name → new one), in order across the sources.
#[derive(Debug)]
pub struct Combined {
    pub bytes: Arc<Vec<u8>>,
    pub renamed_fields: Vec<(String, String)>,
}

/// A bookmark as the bookmark list shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct BookmarkRow {
    /// Child indices from the top level, 0-based, as the edit functions take them.
    pub path: Vec<usize>,
    pub title: String,
    /// Shown expanded (a positive `/Count`).
    pub open: bool,
    /// The 0-based page it goes to.
    pub page: Option<usize>,
    /// Where on that page it goes ([`pdfcraft_render::DestView::Top`] when it doesn't say).
    pub view: pdfcraft_render::DestView,
    /// The title's colour (`/C`), `None` for the default black, and its style (`/F`).
    pub color: Option<[f32; 3]>,
    pub italic: bool,
    pub bold: bool,
}

/// One page of the bookmark list, in outline order.
#[derive(Clone, Debug, PartialEq)]
pub struct BookmarkPage {
    pub rows: Vec<BookmarkRow>,
    /// Where the next page starts, or `None` at the end.
    pub next: Option<usize>,
    /// Set only when the document's structure could not be read for editing: its listing then
    /// comes from the viewer's copy of the outline, which is shorter than the file's.
    pub truncated: bool,
}

/// The rows of a viewer outline, in outline order, with their paths.
fn outline_rows(items: &[pdfcraft_render::OutlineItem]) -> Vec<BookmarkRow> {
    let mut rows = Vec::new();
    // One entry per open level: its items, and how many of them have been listed.
    let mut levels: Vec<(&[pdfcraft_render::OutlineItem], usize)> = vec![(items, 0)];
    while let Some(&(list, next)) = levels.last() {
        let Some(item) = list.get(next) else {
            levels.pop();
            continue;
        };
        if let Some(top) = levels.last_mut() {
            top.1 += 1;
        }
        let path: Vec<usize> = levels.iter().map(|(_, listed)| listed.saturating_sub(1)).collect();
        rows.push(BookmarkRow {
            path,
            title: item.title.clone(),
            open: item.open,
            page: item.page,
            view: item.view,
            color: item.color,
            italic: item.italic,
            bold: item.bold,
        });
        if !item.children.is_empty() {
            levels.push((&item.children, 0));
        }
    }
    rows
}

/// A document's working file captured for crash recovery.
#[derive(Clone, Debug)]
pub struct RecoverySnapshot {
    pub doc: DocId,
    pub name: String,
    pub path: Option<String>,
    pub bytes: Arc<Vec<u8>>,
    /// The snapshot is encrypted (recovering it asks for the password again).
    pub encrypted: bool,
}

/// The comment list, read from the object graph (as `inspect` lists comments).
fn comment_list(doc: &pdfcraft_cos::Document) -> Vec<pdfcraft_render::Annotation> {
    let mut list: Vec<pdfcraft_render::Annotation> = pdfcraft_annot::summaries(doc)
        .into_iter()
        .map(|s| pdfcraft_render::Annotation {
            page: s.page,
            subtype: s.subtype,
            author: s.author,
            contents: s.contents,
            modified: s.modified.map(|m| pdfcraft_render::pretty_date(&m)),
            name: s.name,
            in_reply_to: s.in_reply_to,
            parent: s.parent,
            grouped: s.grouped,
            rect: s.rect,
            color: s.color,
            index: s.index,
            state: s.state,
            quads: s.quads,
            locked: s.locked,
            intent: s.intent,
            fill_sign: s.fill_sign,
            no_rotate: s.no_rotate,
            points: s.points,
        })
        .collect();
    pdfcraft_render::link_replies(&mut list);
    list
}

/// Seconds to add to UTC for local time (0 where unknown).
fn local_utc_offset() -> i64 {
    #[cfg(not(target_arch = "wasm32"))]
    return i64::from(chrono::Local::now().offset().local_minus_utc());
    #[cfg(target_arch = "wasm32")]
    return 0;
}

/// (year, month, day) from a PDF date `D:YYYYMMDD…`.
fn parse_ymd(d: &str) -> Option<(i64, u32, u32)> {
    let d = d.strip_prefix("D:").unwrap_or(d);
    Some((d.get(0..4)?.parse().ok()?, d.get(4..6)?.parse().ok()?, d.get(6..8)?.parse().ok()?))
}

/// The recovered user password of an R2–R4 file as text for the renderer, which tries a password
/// as its UTF-8 bytes and then in PDFDocEncoding: UTF-8 bytes (as some writers store passwords
/// PDFDocEncoding can't hold, such as "şifre") stay UTF-8, and other bytes are read as
/// PDFDocEncoding, so either way the renderer gets back exactly these bytes.
fn renderer_password(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => bytes.iter().map(|b| pdfcraft_cos::pdfdoc_char(*b)).collect(),
    }
}

/// The document as it may be written to a file (#3926, #3944): a copy without the values of text
/// fields with the Password flag (or XFA `<passwordEdit/>` fields in the datasets), or `cos`
/// itself when it holds none. ISO 32000-2 §12.7.5.3, Table 231 (a note to the Password flag):
/// "it is imperative that PDF processors never store the value of the text field in the PDF file
/// if this flag is set". The open document keeps them in memory.
fn writable(cos: &pdfcraft_cos::Document) -> Result<std::borrow::Cow<'_, pdfcraft_cos::Document>, EditError> {
    let has_form_pw = pdfcraft_forms::has_password_values(cos);
    let has_xfa_pw = pdfcraft_xfa::has_password_values(cos);
    if !has_form_pw && !has_xfa_pw {
        return Ok(std::borrow::Cow::Borrowed(cos));
    }
    let mut copy = cos.clone();
    if has_form_pw {
        pdfcraft_forms::clear_password_values(&mut copy).map_err(|e| EditError::Write(e.to_string()))?;
    }
    if has_xfa_pw {
        pdfcraft_xfa::clear_password_values(&mut copy).map_err(|e| EditError::Write(e.to_string()))?;
    }
    Ok(std::borrow::Cow::Owned(copy))
}

/// The last-resort guard (AGENTS.md §4): run `f`, turning a panic that escapes it into an error
/// message, so one bad file or edit can't take the app and its other documents down. It is a
/// safety net for bugs, not a substitute for returning errors.
pub fn guard<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|p| {
        p.downcast_ref::<&str>().map(|s| (*s).to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown error".into())
    })
}

/// Parse another PDF to copy pages from.
pub(crate) fn open_source(name: &str, bytes: &Arc<Vec<u8>>) -> Result<pdfcraft_cos::Document, EditError> {
    open_source_with(name, bytes, None)
}

/// [`open_source`], authenticating with `password` (user or owner) for an encrypted file.
fn open_source_with(name: &str, bytes: &Arc<Vec<u8>>, password: Option<&str>) -> Result<pdfcraft_cos::Document, EditError> {
    source_document(bytes, password).map_err(|p| EditError::Source(format!("{name}: {p}")))
}

fn source_document(bytes: &Arc<Vec<u8>>, password: Option<&str>) -> Result<pdfcraft_cos::Document, SourceProblem> {
    match std::panic::catch_unwind(|| pdfcraft_cos::Document::open_with_password(bytes.clone(), password)) {
        Ok(Ok(d)) if d.permissions().is_some_and(|p| !p.assemble()) => Err(SourceProblem::NotPermitted),
        Ok(Ok(d)) => Ok(d),
        Ok(Err(pdfcraft_cos::CosError::NeedsPassword)) => Err(SourceProblem::Password),
        Ok(Err(pdfcraft_cos::CosError::WrongPassword)) => Err(SourceProblem::WrongPassword),
        Ok(Err(e)) => Err(SourceProblem::Unreadable(e.to_string())),
        Err(_) => Err(SourceProblem::Unreadable("the file could not be read".into())),
    }
}

/// Whether `bytes` can be combined, opened with `password` (user or owner) if given, and if not
/// why: what Combine would refuse it for, so the Combine files list can say so before Combine
/// is pressed. The owner (permissions) password lifts the restriction on copying pages.
pub fn combine_source_check(bytes: &Arc<Vec<u8>>, password: Option<&str>) -> Result<(), SourceProblem> {
    source_document(bytes, password).map(|_| ())
}

/// The number of pages of `bytes`, opened with `password` (user or owner) if given, whatever
/// its permissions allow; `None` when it can't be read.
pub fn source_page_count(bytes: &Arc<Vec<u8>>, password: Option<&str>) -> Option<usize> {
    let doc = std::panic::catch_unwind(|| pdfcraft_cos::Document::open_with_password(bytes.clone(), password)).ok()?.ok()?;
    pdfcraft_organize::page_count(&doc).ok()
}

/// Lay a dynamic XFA form out (pages and fields) and give its widgets appearances.
fn xfa_layout(doc: &mut pdfcraft_cos::Document) -> Result<XfaLayout, String> {
    let mut report = pdfcraft_xfa::render_into(doc).map_err(|e| e.to_string())?;
    xfa_redraw(doc)?;
    // The form's own values: one its field's font can't draw is drawn with `?` and reported.
    report.warnings.extend(undrawn_field_warnings(doc));
    Ok(report)
}

/// Give the fields of a laid-out XFA form their appearances. A signed signature box keeps the
/// picture its signer's application drew.
pub(crate) fn xfa_redraw(doc: &mut pdfcraft_cos::Document) -> Result<(), String> {
    for f in pdfcraft_forms::fields(doc) {
        if f.kind == pdfcraft_forms::FieldKind::Signature && doc.get(f.obj).as_dict().is_some_and(|d| d.contains(b"V")) {
            continue;
        }
        pdfcraft_forms::redraw_field(doc, &f.name).map_err(|e| format!("{}: {e}", f.name))?;
    }
    Ok(())
}

/// A warning for each field whose appearance PdfCraft drew with `?` for a character its font
/// can't draw (laying out an XFA form's own values).
fn undrawn_field_warnings(doc: &pdfcraft_cos::Document) -> Vec<String> {
    pdfcraft_forms::undrawn_fields(doc)
        .into_iter()
        .map(|(name, c)| {
            format!("form field {name:?} holds \"{}\" (U+{:04X}), which its font can't draw: it shows ? in its place", c.escape_debug(), u32::from(c))
        })
        .collect()
}

/// The form's fields as the XFA data layer wants them.
fn xfa_field_data(doc: &pdfcraft_cos::Document) -> Vec<pdfcraft_xfa::FieldDatum> {
    use pdfcraft_forms::FieldKind as K;
    pdfcraft_forms::fields(doc)
        .into_iter()
        .map(|f| {
            let data = match f.kind {
                K::Text | K::Combo | K::List => pdfcraft_xfa::FieldData::Text(f.value.join("\n")),
                K::CheckBox => pdfcraft_xfa::FieldData::Check(!f.value.is_empty()),
                K::Radio => pdfcraft_xfa::FieldData::Radio(f.value.first().cloned()),
                K::PushButton | K::Signature => pdfcraft_xfa::FieldData::None,
            };
            pdfcraft_xfa::FieldDatum { obj: f.obj, name: f.name, data }
        })
        .collect()
}

/// Keep the XFA datasets packet in step with the fields after an edit. Returns what could not
/// be written.
/// `datasets`: the stream this edit already wrote, replaced in place rather than added again
/// (and set to the one written).
/// `tpl`: the form's parsed template when the document has one cached (else it is parsed).
fn xfa_sync_datasets(
    doc: &mut pdfcraft_cos::Document,
    tpl: Option<&pdfcraft_xfa::model::Template>,
    datasets: &mut Option<pdfcraft_cos::ObjRef>,
) -> Result<Vec<String>, String> {
    let data = xfa_field_data(doc);
    let r = pdfcraft_xfa::write_datasets_with(doc, tpl, &data, *datasets).map_err(|e| e.to_string())?;
    if r.stream.is_some() {
        *datasets = r.stream;
    }
    Ok(r.warnings)
}

/// What a document whose form script ran away is told.
const XFA_SCRIPTS_OFF: &str =
    "A script of this form ran too long and was abandoned; the form's scripts are off for this document (reopen it to run them again)";

/// Most XFA warnings kept per document.
const MAX_XFA_WARNINGS: usize = 50;

/// Add `new` to `list` (no repeats, at most [`MAX_XFA_WARNINGS`]).
fn note_warnings(list: &mut Vec<String>, new: &[String]) {
    for w in new {
        if !list.contains(w) && list.len() < MAX_XFA_WARNINGS {
            list.push(w.clone());
        }
    }
}

/// A data file's text: UTF-8 or UTF-16 (by a byte order mark, or how `<` is written), or
/// Latin-1 when its XML declaration says so. `None`: not text at all (FDF); `Err`: an
/// encoding this can't read.
pub(crate) fn decode_data_file(bytes: &[u8]) -> Option<Result<String, String>> {
    if bytes.starts_with(b"%FDF") {
        return None;
    }
    let declared = |text: &str| -> Option<String> {
        let head = text.get(..text.len().min(256))?;
        let decl = head.strip_prefix('\u{feff}').unwrap_or(head).trim_start().strip_prefix("<?xml")?;
        let decl = decl.split_once("?>")?.0;
        let enc = decl.split_once("encoding=")?.1;
        let quote = enc.chars().next()?;
        Some(enc.get(1..)?.split(quote).next()?.trim().to_ascii_lowercase())
    };
    if let Some((text, _)) = pdfcraft_xfa::packets::decode(bytes) {
        match declared(&text).as_deref() {
            None | Some("utf-8" | "utf8" | "utf-16" | "utf-16le" | "utf-16be" | "us-ascii" | "ascii") => return Some(Ok(text)),
            Some("iso-8859-1" | "latin1" | "latin-1" | "windows-1252" | "cp1252") if !bytes.starts_with(&[0xEF, 0xBB, 0xBF]) => {
                // Each byte is the character it names (the 0x80–0x9F block of Windows-1252
                // differs, and is rare in form data).
                return Some(Ok(bytes.iter().map(|b| char::from(*b)).collect()));
            }
            Some(other) => return Some(Err(format!("the data file's encoding {other:?} is not supported (UTF-8, UTF-16 and ISO-8859-1 are)"))),
        }
    }
    // Not valid UTF-8/UTF-16: Latin-1 by declaration, else refused.
    let latin1: String = bytes.iter().map(|b| char::from(*b)).collect();
    match declared(&latin1).as_deref() {
        Some("iso-8859-1" | "latin1" | "latin-1" | "windows-1252" | "cp1252") => Some(Ok(latin1)),
        _ => Some(Err("the data file is not UTF-8, UTF-16 or ISO-8859-1 text".into())),
    }
}

/// Take the XFA out of `doc` and note what went with it: the packets, data no field showed,
/// buttons that did nothing but run scripts, and fields whose value differs from the form's
/// data (data PdfCraft could not read into them, now lost).
pub(crate) fn strip_xfa(doc: &mut pdfcraft_cos::Document, cx: &mut EditCtx) -> Result<(), EditError> {
    let fields = pdfcraft_forms::fields(doc);
    let data = xfa_field_data(doc);
    let differ = pdfcraft_xfa::read_values(doc, &data)
        .into_iter()
        .filter(|(name, value)| {
            let Some(f) = fields.iter().find(|f| f.name == *name) else { return false };
            match value {
                pdfcraft_xfa::FieldData::Text(t) => f.value.join("\n") != *t && !t.trim().is_empty(),
                pdfcraft_xfa::FieldData::Check(on) => f.value.is_empty() == *on,
                pdfcraft_xfa::FieldData::Radio(sel) => f.value.first() != sel.as_ref(),
                pdfcraft_xfa::FieldData::None => false,
            }
        })
        .count();
    let r = pdfcraft_xfa::strip(doc).map_err(|e| EditError::Write(e.to_string()))?;
    cx.xfa_removed.extend(r.notes());
    if differ > 0 {
        cx.xfa_removed.push(format!(
            "{differ} field(s) held a value other than the form's XFA data (data PdfCraft could not read into them); the data's version is gone"
        ));
    }
    Ok(())
}

/// Keep the document's XFA state in step with its file after an edit: a script may have laid
/// the form out again (rows added, subforms shown), an edit may have taken the XFA out
/// (flattened, or made static), or an undo put it back.
fn sync_xfa_state(doc: &mut Document) {
    let Some(cos) = doc.editor.as_ref().map(|e| &e.cos) else { return };
    match pdfcraft_xfa::existing_layout(cos) {
        Some(report) => {
            if doc.xfa.is_none() {
                // Back after an undo: the XFA's own warnings apply again, the strip's don't.
                doc.xfa_warnings.retain(|w| !w.starts_with(XFA_REMOVED_PREFIX));
                if doc.xfa_template.is_none() && !doc.xfa_scripts_off {
                    doc.xfa_template = xfa::template(cos);
                }
            }
            doc.xfa = Some(report);
        }
        None => {
            doc.xfa = None;
            doc.xfa_template = None;
            // Only what the strip said stays (the XFA's own warnings no longer apply).
            doc.xfa_warnings.retain(|w| w.starts_with(XFA_REMOVED_PREFIX));
        }
    }
}

/// How the warnings of a strip start, so they can be told from the XFA's own.
const XFA_REMOVED_PREFIX: &str = "XFA removed: ";

/// Is this an XFA data file (an XDP, a datasets document, a data element or a data document),
/// rather than XFDF, Acrobat's `<fields>` XML or a table? By its root element.
pub(crate) fn is_xfa_data_file(text: &str) -> bool {
    let text = text.get(..text.len().min(65_536)).unwrap_or(text);
    let mut rest = text.trim_start_matches('\u{feff}').trim_start();
    // Past the XML declaration, comments and processing instructions.
    let mut guard = 0;
    while guard < 100 {
        guard += 1;
        if let Some(r) = rest.strip_prefix("<?") {
            rest = r.split_once("?>").map_or("", |(_, r)| r).trim_start();
        } else if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.split_once("-->").map_or("", |(_, r)| r).trim_start();
        } else if let Some(r) = rest.strip_prefix("<!") {
            rest = r.split_once('>').map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    let Some(tag) = rest.strip_prefix('<') else { return false };
    let name = tag.split(|c: char| c.is_whitespace() || c == '>' || c == '/').next().unwrap_or("");
    let local = name.rsplit(':').next().unwrap_or(name);
    !local.is_empty() && !matches!(local, "xfdf" | "fields" | "html" | "body")
}

/// The content type a picture is saved under, from its bytes: the kinds a laid-out form shows.
pub(crate) fn image_content_type(bytes: &[u8]) -> Option<&'static str> {
    match pdfcraft_xfa::image::kind(bytes)? {
        pdfcraft_xfa::image::Kind::Jpeg => Some("image/jpeg"),
        pdfcraft_xfa::image::Kind::Png => Some("image/png"),
        pdfcraft_xfa::image::Kind::Gif => Some("image/gif"),
        pdfcraft_xfa::image::Kind::Bmp => Some("image/bmp"),
        pdfcraft_xfa::image::Kind::Tiff => Some("image/tiff"),
    }
}

/// After the XFA data changed outside the fields (an import, a picture): a laid-out form is
/// laid out again from the data (then, when `calculate`, its calculations run), and the
/// fields take the data's values.
pub(crate) fn xfa_after_data_change(doc: &mut pdfcraft_cos::Document, cx: &mut EditCtx, calculate: bool) -> Result<(), EditError> {
    if let Some(tpl) = cx.xfa.clone() {
        let mut run =
            xfa::XfaRun { page_count: cx.pages, out: &mut cx.xfa_out, datasets: cx.xfa_datasets, ran_away: false, time_scale: cx.xfa_time_scale };
        let r = guard(|| xfa::on_data_replaced(doc, &tpl, calculate, &mut run))
            .unwrap_or_else(|m| Err(format!("laying the form out again failed unexpectedly ({m})")));
        cx.xfa_datasets = run.datasets;
        cx.xfa_ran_away |= run.ran_away;
        if let Err(e) = r {
            cx.xfa_out.errors.push(e);
        }
    }
    let taken = xfa_values_from_datasets(doc).map_err(EditError::Write)?;
    if !taken.is_empty() {
        cx.xfa_out.console.push(format!("{} field value(s) taken from the imported data", taken.len()));
    }
    // Not laid out again (a static form, or scripts off): fields the data no longer holds are
    // emptied, so the data replaced is the data shown.
    if cx.xfa.is_none() && calculate {
        let data = xfa_field_data(doc);
        let present: std::collections::HashSet<String> = pdfcraft_xfa::read_values(doc, &data).into_iter().map(|(n, _)| n).collect();
        for f in pdfcraft_forms::fields(doc) {
            if present.contains(&f.name) || f.value.is_empty() {
                continue;
            }
            use pdfcraft_forms::FieldKind as K;
            let empty = match f.kind {
                K::Text => FieldValue::Text(String::new()),
                K::CheckBox => FieldValue::Check(false),
                K::Radio => FieldValue::Radio(None),
                K::Combo | K::List => FieldValue::Choice(Vec::new()),
                K::PushButton | K::Signature => continue,
            };
            if let Err(e) = pdfcraft_forms::set_value(doc, &f.name, &empty) {
                cx.xfa_out.errors.push(format!("{}: not emptied by the import: {e}", f.name));
            }
        }
    }
    Ok(())
}

/// Give the fields the values the XFA datasets hold (a form filled in another viewer). Returns
/// the names of the fields that changed.
fn xfa_values_from_datasets(doc: &mut pdfcraft_cos::Document) -> Result<Vec<String>, String> {
    let data = xfa_field_data(doc);
    let fields = pdfcraft_forms::fields(doc);
    // The first field of each name, as a lookup by name would find it.
    let mut by_name = std::collections::HashMap::new();
    for f in &fields {
        by_name.entry(f.name.as_str()).or_insert(f);
    }
    let mut updates = Vec::new();
    for (name, value) in pdfcraft_xfa::read_values(doc, &data) {
        let Some(f) = by_name.get(name.as_str()) else { continue };
        let new = match value {
            pdfcraft_xfa::FieldData::Text(t) if matches!(f.kind, pdfcraft_forms::FieldKind::Combo | pdfcraft_forms::FieldKind::List) => {
                // Saved values, one per line for a multi-select list; shown text stands for
                // its saved value (an older viewer may have written it).
                let saved = |v: &str| f.options.iter().find(|(e, s)| e == v || s == v).map_or(v.to_string(), |(e, _)| e.clone());
                let picked: Vec<String> = if f.has(pdfcraft_forms::flags::MULTI_SELECT) {
                    t.lines().map(str::trim).filter(|l| !l.is_empty()).map(saved).collect()
                } else {
                    vec![saved(t.trim())]
                };
                (f.value != picked).then_some(FieldValue::Choice(picked))
            }
            pdfcraft_xfa::FieldData::Text(t) => (f.value.join("\n") != t).then_some(FieldValue::Text(t)),
            pdfcraft_xfa::FieldData::Check(on) => (f.value.is_empty() == on).then_some(FieldValue::Check(on)),
            pdfcraft_xfa::FieldData::Radio(sel) => (f.value.first() != sel.as_ref()).then_some(FieldValue::Radio(sel)),
            pdfcraft_xfa::FieldData::None => None,
        };
        if let Some(v) = new {
            updates.push((name, v));
        }
    }
    // All at once: one by one, each value would read the whole form again, which took minutes
    // on a form with thousands of fields.
    let Ok(results) = pdfcraft_forms::set_values(doc, &updates) else { return Ok(Vec::new()) };
    Ok(updates.into_iter().zip(results).filter(|(_, r)| r.is_ok()).map(|((name, _), _)| name).collect())
}

/// Validate the signature fields of `cos` (written as `bytes`).
fn signatures_of(cos: &pdfcraft_cos::Document, bytes: &[u8], trust: &TrustStore, cache: &pdfcraft_sign::DigestCache) -> Vec<SignatureInfo> {
    pdfcraft_sign::pdf::list_cached(cos, bytes, trust, cache)
}

/// A picture for a watermark or background (Acrobat: Source ▸ File): an image, or `page`
/// (0-based) of a PDF.
#[derive(Clone, Debug, PartialEq)]
pub struct MarkFile {
    pub name: String,
    pub bytes: Arc<Vec<u8>>,
    pub page: usize,
}

/// Embed an image that is placed on a page, turned the way its EXIF Orientation says.
pub(crate) fn embed_image(
    doc: &mut pdfcraft_cos::Document,
    name: &str,
    bytes: &[u8],
) -> Result<(pdfcraft_cos::ObjRef, (f64, f64)), pdfcraft_create::CreateError> {
    pdfcraft_create::image_xobject(doc, name, bytes)
}

/// The largest box of `w`:`h` shape inside `rect`, centred in it: a stamp's picture keeps its
/// shape in a box of any other shape. A box that is already that shape (to within rounding) is
/// kept exactly, so callers that size it themselves get their box back. An empty or non-finite
/// box, or a picture with no size, is returned as it is, for the annotation's own check to refuse.
pub(crate) fn fit_picture(rect: [f64; 4], w: f64, h: f64) -> [f64; 4] {
    if !rect.iter().all(|v| v.is_finite()) {
        return rect;
    }
    let rect = [rect[0].min(rect[2]), rect[1].min(rect[3]), rect[0].max(rect[2]), rect[1].max(rect[3])];
    let (bw, bh) = (rect[2] - rect[0], rect[3] - rect[1]);
    let usable = |v: f64| v.is_finite() && v > 0.0;
    if !(usable(bw) && usable(bh) && usable(w) && usable(h)) {
        return rect;
    }
    let k = (bw / w).min(bh / h);
    let (fw, fh) = (w * k, h * k);
    if (fw - bw).abs() <= bw * 1e-9 && (fh - bh).abs() <= bh * 1e-9 {
        return rect;
    }
    let (cx, cy) = (rect[0] + bw / 2.0, rect[1] + bh / 2.0);
    [cx - fw / 2.0, cy - fh / 2.0, cx + fw / 2.0, cy + fh / 2.0]
}

/// Bring a mark's picture into `doc`: a PDF page as a form XObject, or an image.
pub(crate) fn mark_source(doc: &mut pdfcraft_cos::Document, f: &MarkFile) -> Result<pdfcraft_edit::MarkSource, EditError> {
    let head = &f.bytes[..f.bytes.len().min(1024)];
    if head.windows(5).any(|w| w == b"%PDF-") {
        let src = pdfcraft_cos::Document::open(f.bytes.clone()).map_err(|e| EditError::Source(format!("{}: {e}", f.name)))?;
        let (xobject, size) = pdfcraft_organize::page_as_form(doc, &src, f.page)?;
        Ok(pdfcraft_edit::MarkSource { xobject, size, image: false })
    } else {
        let (xobject, size) = embed_image(doc, &f.name, &f.bytes)?;
        Ok(pdfcraft_edit::MarkSource { xobject, size, image: true })
    }
}

/// What Optimize PDF did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OptimizeReport {
    pub optimize: optimize::Report,
    /// Identical objects merged.
    pub merged: usize,
    /// Remove Hidden Information categories discarded, with counts.
    pub discarded: Vec<(Hidden, usize)>,
}

/// Order of Summarize Comments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SummarySort {
    #[default]
    Page,
    Author,
    Date,
    Type,
}

impl SummarySort {
    pub const ALL: [SummarySort; 4] = [SummarySort::Page, SummarySort::Author, SummarySort::Date, SummarySort::Type];
    pub fn name(self) -> &'static str {
        match self {
            SummarySort::Page => "Page",
            SummarySort::Author => "Author",
            SummarySort::Date => "Date",
            SummarySort::Type => "Type",
        }
    }
    pub fn parse(s: &str) -> Option<SummarySort> {
        Self::ALL.into_iter().find(|k| k.name().eq_ignore_ascii_case(s))
    }
}

/// Acrobat's name for a comment type, as comment lists show it.
pub fn comment_type_name(subtype: &str) -> &str {
    match subtype {
        "Text" => "Sticky Note",
        "FreeText" => "Text Box",
        "Highlight" => "Highlight",
        "Underline" => "Underline",
        "StrikeOut" => "Strikethrough",
        "Squiggly" => "Squiggly",
        "Square" => "Rectangle",
        "Circle" => "Oval",
        "Line" => "Line",
        "Ink" => "Pencil",
        "Polygon" => "Polygon",
        "PolyLine" => "Polygonal Line",
        "Stamp" => "Stamp",
        "Caret" => "Inserted Text",
        "FileAttachment" => "File Attachment",
        "Sound" => "Sound",
        "Redact" => "Redaction",
        other => other,
    }
}

/// [`comment_type_name`], telling callouts, clouds and typewriter text apart by `/IT`.
pub fn comment_kind(a: &pdfcraft_render::Annotation) -> &str {
    match a.intent.as_deref() {
        Some("FreeTextCallout") => "Callout",
        Some("PolygonCloud") => "Cloud",
        Some("FreeTextTypeWriter") => "Typewriter",
        _ => comment_type_name(&a.subtype),
    }
}

/// The text of a comment summary: one block per comment (replies indented under it), with a
/// "Page N" heading when sorted by page. Checkmarks and status replies are left out, as are
/// pop-ups; the number is the comment's position on its page.
pub fn comment_summary(name: &str, all: &[pdfcraft_render::Annotation], sort: SummarySort) -> String {
    use std::fmt::Write;
    let top: Vec<&pdfcraft_render::Annotation> = all.iter().filter(|a| a.is_comment()).collect();
    // Each reply under the annotation its `/IRT` points at, in document order, gathered once.
    type Children<'a> = std::collections::HashMap<(usize, usize), Vec<&'a pdfcraft_render::Annotation>>;
    let mut children = Children::new();
    for r in all.iter().filter(|r| r.state.is_none()) {
        if let Some(parent) = r.parent {
            children.entry(parent).or_default().push(r);
        }
    }
    // Write a comment's replies, indenting each level. A reply to a reply keeps its `/IRT` on the
    // direct parent, so following one level would leave the deeper replies out of the summary.
    fn write_replies(out: &mut String, children: &Children<'_>, parent: &pdfcraft_render::Annotation, depth: usize) {
        use std::fmt::Write;
        if depth > 64 {
            return;
        }
        for r in children.get(&(parent.page, parent.index)).into_iter().flatten() {
            let indent = "    ".repeat(depth + 1);
            let _ =
                writeln!(out, "{indent}Author: {}  Subject: Reply  Date: {}", r.author.as_deref().unwrap_or(""), r.modified.as_deref().unwrap_or(""));
            for line in r.contents.as_deref().unwrap_or("").lines() {
                let _ = writeln!(out, "{indent}{line}");
            }
            write_replies(out, children, r, depth + 1);
        }
    }
    let mut numbered: Vec<(usize, &pdfcraft_render::Annotation)> = Vec::new();
    let mut last = usize::MAX;
    let mut n = 0;
    for a in &top {
        if a.page != last {
            last = a.page;
            n = 0;
        }
        n += 1;
        numbered.push((n, a));
    }
    match sort {
        SummarySort::Page => {}
        SummarySort::Author => numbered.sort_by(|x, y| x.1.author.cmp(&y.1.author)),
        SummarySort::Date => numbered.sort_by(|x, y| x.1.modified.cmp(&y.1.modified)),
        SummarySort::Type => numbered.sort_by(|x, y| comment_kind(x.1).cmp(comment_kind(y.1))),
    }
    let mut out = format!("Summary of Comments on {name}\n\n");
    if numbered.is_empty() {
        out.push_str("This document has no comments.\n");
        return out;
    }
    let mut heading = usize::MAX;
    for (n, a) in numbered {
        if sort == SummarySort::Page && a.page != heading {
            heading = a.page;
            let _ = writeln!(out, "Page: {}", a.page + 1);
        }
        let _ = write!(out, "Number: {n}  Author: {}  Subject: {}", a.author.as_deref().unwrap_or(""), comment_kind(a));
        if sort != SummarySort::Page {
            let _ = write!(out, "  Page: {}", a.page + 1);
        }
        let _ = writeln!(out, "  Date: {}", a.modified.as_deref().unwrap_or(""));
        if let Some(c) = a.contents.as_deref().filter(|c| !c.is_empty()) {
            let _ = writeln!(out, "{c}");
        }
        write_replies(&mut out, &children, a, 0);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests;

/// Page filters of Acrobat's Rotate Pages and page selection: even/odd page numbers and
/// orientation (as displayed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PageParity {
    #[default]
    Both,
    Even,
    Odd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PageOrientation {
    #[default]
    Both,
    Landscape,
    Portrait,
}

/// The pages of `pages` (0-based) that pass the filters.
pub fn filter_pages(info: &pdfcraft_render::DocInfo, pages: &[usize], parity: PageParity, orientation: PageOrientation) -> Vec<usize> {
    pages
        .iter()
        .copied()
        .filter(|p| match parity {
            PageParity::Both => true,
            // Page numbers are 1-based: index 1 is page 2, an even page.
            PageParity::Even => p % 2 == 1,
            PageParity::Odd => p % 2 == 0,
        })
        .filter(|p| {
            let Some(pg) = info.pages.get(*p) else { return false };
            match orientation {
                PageOrientation::Both => true,
                PageOrientation::Landscape => pg.width > pg.height,
                PageOrientation::Portrait => pg.width <= pg.height,
            }
        })
        .collect()
}
