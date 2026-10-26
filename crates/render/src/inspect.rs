//! Document inspection: everything panels need that is not pixels.
//!
//! Page geometry comes from hayro (which resolves inheritance and rotation); the rest comes from
//! the lazy COS reader through a read-only lopdf object adapter. The original lopdf loader
//! remains the compatibility fallback. When neither can inspect a renderable file, panels
//! are empty and `warnings` says why.

use std::cell::OnceCell;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use std::panic::{AssertUnwindSafe, catch_unwind};

use hayro::hayro_syntax::DecryptionError;
use hayro::hayro_syntax::LoadPdfError;
use hayro::hayro_syntax::Pdf;
use lopdf::{Dictionary, Document, LoadOptions, Object, ObjectId};
use pdfcraft_cos::page_labels::{
    MAX_LABEL_BYTES, MAX_LABEL_TOTAL_BYTES, MAX_LABEL_TREE_DEPTH, MAX_LABEL_TREE_WORK, MAX_PREFIX_BYTES, MAX_PREFIX_TOTAL_BYTES, alpha, roman,
};

use crate::OpenError;
use crate::structure::{LazyStructure, Structure};

/// lopdf decodes object and cross-reference streams while it loads, with no limit unless one is
/// set: a few hundred bytes of nested FlateDecode then inflate to gigabytes. Real object and
/// xref streams are far below this.
const LOAD_STREAM_LIMIT: usize = 256 << 20;

/// At most this many `/State` entries of a set-layer-visibility action are read.
const MAX_LAYER_STATE: usize = 1024;

/// Decoded bytes read from an XFA form's configuration packet (or a whole-XDP `/XFA` stream) to
/// find `dynamicRender`. Designer's packets are a few kilobytes; a whole XDP can hold the template.
const MAX_XFA_CONFIG_BYTES: usize = 16 << 20;

/// File specifications retained from a remote-Go-To action are deliberately small.
const MAX_REMOTE_FILE_SPEC_BYTES: usize = 4096;
const MAX_REMOTE_DESTINATION_NAME_BYTES: usize = 1024;

/// Text and geometry `DocInfo::annotations` keeps, as counted by [`retained`]. Past it the rest
/// are not listed: real comments stay far below, many small ones sharing one long string do not.
const MAX_ANNOTATION_BYTES: usize = 64 << 20;

/// Web-address text the link listing, and name, description and embedded-file key text the
/// attachment listing, keep together. Many links or attachments can share one long string, which
/// each would otherwise copy. Past it, a link to a web address lists as a URI action without its
/// address, an attachment under its generic name without a description, and an embedded file whose
/// key does not fit is not listed.
const MAX_LINK_ATTACHMENT_TEXT_BYTES: usize = 16 << 20;

/// The bytes a listed annotation keeps.
fn retained(a: &Annotation) -> usize {
    let text = [&a.author, &a.contents, &a.modified, &a.name, &a.in_reply_to, &a.state, &a.intent];
    text.into_iter()
        .map(|t| t.as_ref().map_or(0, String::len))
        .chain([
            a.subtype.len(),
            std::mem::size_of_val(a.quads.as_slice()),
            std::mem::size_of_val(a.points.as_slice()),
            std::mem::size_of::<Annotation>(),
        ])
        .fold(0, usize::saturating_add)
}

/// At most this many layers are read from all of `/RBGroups` together.
const MAX_LAYER_GROUP_ENTRIES: usize = 4096;

/// `DocInfo::outline` holds the first this many bookmarks in outline order. Opening a document
/// reads only this window; the rest are listed by page through `pdfcraft_organize`.
const OUTLINE_WINDOW: usize = 10_000;

/// Deepest level `DocInfo::outline` nests (the top level is 0); deeper bookmarks are left out.
const OUTLINE_DEPTH: usize = 32;

fn load_options(password: Option<&str>) -> LoadOptions {
    LoadOptions { password: password.map(str::to_owned), max_decompressed_size: Some(LOAD_STREAM_LIMIT), ..LoadOptions::default() }
}

#[derive(Clone, Debug, Default)]
pub struct DocInfo {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Option<String>,
    pub creator: Option<String>,
    pub producer: Option<String>,
    /// Custom document properties: `/Info` entries with a key the spec does not name and a text
    /// value, as (name, text), in the dictionary's order (at most 256).
    pub custom: Vec<(String, String)>,
    pub pdf_version: String,
    pub file_size: usize,
    pub encrypted: bool,
    pub tagged: bool,
    pub has_javascript: bool,
    /// The form is (also) an XFA form, which isn't read yet (`form.xfa-*` in the parity list).
    pub xfa: Option<Xfa>,
    pub pages: Vec<PageInfo>,
    pub outline: Vec<OutlineItem>,
    /// The outline has bookmarks that `outline` leaves out (see `OUTLINE_WINDOW`, `OUTLINE_DEPTH`).
    pub outline_more: bool,
    pub annotations: Vec<Annotation>,
    pub fields: Vec<Field>,
    pub links: Vec<Link>,
    pub layers: Vec<Layer>,
    /// Radio-button layer groups (`/OCProperties /D /RBGroups`): turning one layer of a group on
    /// turns the others off.
    pub layer_groups: Vec<Vec<(u32, u16)>>,
    pub fonts: Vec<FontInfo>,
    pub attachments: Vec<Attachment>,
    /// Pages with a presentation transition (`/Trans`) or display time (`/Dur`), in page order.
    pub transitions: Vec<PageTransition>,
    /// Where the document opens (`/OpenAction`): the (0-based page, view) of its destination,
    /// named destinations resolved. `None` without one, for an action that goes nowhere in this
    /// document (a script, say), or for one that doesn't resolve to a page of it.
    pub open_dest: Option<(usize, DestView)>,
    pub warnings: Vec<String>,
}

/// What kind of XFA form a document has (`/AcroForm /XFA`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Xfa {
    /// The pages and fields are ordinary PDF; the XFA packets describe the same form (and hold
    /// its data, which Acrobat shows in preference to the fields).
    Static,
    /// The form is laid out from the XFA packets at open (`/NeedsRendering`, or no fields and a
    /// configuration that doesn't forbid dynamic rendering): the PDF pages are only a placeholder.
    Dynamic,
}

/// A page's presentation transition and display time (ISO 32000-2 §12.4.4): how the page comes
/// into view in a full-screen presentation, and how long it stays before advancing. Only pages
/// carrying `/Trans` or `/Dur` are listed. Values outside the spec's ranges fall back to the
/// defaults rather than being refused.
#[derive(Clone, Debug, PartialEq)]
pub struct PageTransition {
    pub page: usize,
    /// `/Trans /S`; `None` when the page has only `/Dur`.
    pub style: Option<TransitionStyle>,
    /// `/D`: how long the transition takes, in seconds (default 1).
    pub duration: f32,
    /// `/Dm /H` (true) or `/V` (Split, Blinds).
    pub horizontal: bool,
    /// `/M /I` (true) or `/O` (Split, Box, Fly).
    pub inward: bool,
    /// `/Di`: 0, 90, 180, 270 or 315 degrees (Wipe, Glitter, Fly, Cover, Uncover, Push); `None`
    /// for `/None` (Fly only).
    pub direction: Option<u16>,
    /// `/SS`: the Fly scale (default 1).
    pub scale: f32,
    /// `/B`: Fly with an opaque backdrop.
    pub opaque: bool,
    /// `/Dur`: seconds the page shows before the presentation advances; `None` waits for the
    /// presenter.
    pub display: Option<f32>,
}

/// The transition styles of ISO 32000-2 table 164 (`/Trans /S`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionStyle {
    Split,
    Blinds,
    Box,
    Wipe,
    Dissolve,
    Glitter,
    /// `/R`: the next page replaces the old one with no effect (the default).
    Replace,
    Fly,
    Push,
    Cover,
    Uncover,
    Fade,
}

impl TransitionStyle {
    /// The style a `/S` name means; an unknown name is `Replace`, the spec's default.
    pub fn from_name(name: &str) -> Self {
        match name {
            "Split" => Self::Split,
            "Blinds" => Self::Blinds,
            "Box" => Self::Box,
            "Wipe" => Self::Wipe,
            "Dissolve" => Self::Dissolve,
            "Glitter" => Self::Glitter,
            "Fly" => Self::Fly,
            "Push" => Self::Push,
            "Cover" => Self::Cover,
            "Uncover" => Self::Uncover,
            "Fade" => Self::Fade,
            _ => Self::Replace,
        }
    }

    /// The name as written in a PDF (`/S`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Split => "Split",
            Self::Blinds => "Blinds",
            Self::Box => "Box",
            Self::Wipe => "Wipe",
            Self::Dissolve => "Dissolve",
            Self::Glitter => "Glitter",
            Self::Replace => "R",
            Self::Fly => "Fly",
            Self::Push => "Push",
            Self::Cover => "Cover",
            Self::Uncover => "Uncover",
            Self::Fade => "Fade",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PageInfo {
    /// Displayed size in points, after `/Rotate` and `/UserUnit`.
    pub width: f32,
    pub height: f32,
    /// Page label (`/PageLabels`), falling back to the 1-based page number.
    pub label: String,
    /// Effective crop box in user space [x0, y0, x1, y1] (the visible region).
    pub crop: [f32; 4],
    /// Clockwise page rotation in degrees (0, 90, 180, 270).
    pub rotation: u16,
}

impl PageInfo {
    /// A point in view space (points, y down, after `/Rotate`; what the text layer uses) →
    /// PDF user space.
    pub fn view_to_user(&self, x: f32, y: f32) -> [f32; 2] {
        let [cx0, cy0, cx1, cy1] = self.crop;
        let (a, b) = (x / self.width.max(1e-3), y / self.height.max(1e-3));
        let (u, v) = match self.rotation {
            90 => (b, 1.0 - a),
            180 => (1.0 - a, 1.0 - b),
            270 => (1.0 - b, a),
            _ => (a, b),
        };
        [cx0 + u * (cx1 - cx0), cy1 - v * (cy1 - cy0)]
    }

    /// PDF user space → view space (the inverse of [`Self::view_to_user`]).
    pub fn user_to_view(&self, x: f32, y: f32) -> [f32; 2] {
        let [cx0, cy0, cx1, cy1] = self.crop;
        let (u, v) = ((x - cx0) / (cx1 - cx0).max(1e-3), (cy1 - y) / (cy1 - cy0).max(1e-3));
        let (a, b) = match self.rotation {
            90 => (1.0 - v, u),
            180 => (1.0 - u, 1.0 - v),
            270 => (v, 1.0 - u),
            _ => (u, v),
        };
        [a * self.width, b * self.height]
    }

    /// A point as displayed (points from the page's top-left, y down, before any view rotation)
    /// → the `/XYZ` position `[left, top]` in user space that puts it at the window's top-left.
    /// Only a coordinate that is given is pinned: a bookmark made at the top of the window
    /// leaves the horizontal position alone. On a page turned a quarter, the displayed `y` decides
    /// the user `left` and the displayed `x` the user `top` (the inverse of [`Self::dest_fraction`]).
    /// A value that is not finite counts as not given.
    pub fn xyz_for_view(&self, x: Option<f32>, y: Option<f32>) -> [Option<f32>; 2] {
        let (x, y) = (x.filter(|v| v.is_finite()), y.filter(|v| v.is_finite()));
        let [ux, uy] = self.view_to_user(x.unwrap_or(0.0), y.unwrap_or(0.0));
        if self.rotation.is_multiple_of(180) { [x.map(|_| ux), y.map(|_| uy)] } else { [y.map(|_| ux), x.map(|_| uy)] }
    }

    /// A destination's user-space point (each coordinate possibly unspecified) → the same point
    /// as a fraction of the displayed page `[x, y]` (0..1 from its top-left corner, y down),
    /// after `/Rotate` and a further clockwise `view_rotation` (0, 90, 180 or 270). On a page
    /// turned a quarter, the user `x` decides the displayed `y` and vice versa, so each output
    /// is `None` when the input it comes from is (or is not finite). Points off the page are
    /// clamped to its edge.
    pub fn dest_fraction(&self, x: Option<f32>, y: Option<f32>, view_rotation: u16) -> [Option<f32>; 2] {
        let [cx0, cy0, cx1, cy1] = self.crop;
        let frac = |v: f32| if v.is_finite() { Some(v.clamp(0.0, 1.0)) } else { None };
        // Unrotated fractions across (u, from the crop's left) and down (v, from its top).
        let u = x.and_then(|x| frac((x - cx0) / (cx1 - cx0).max(1e-3)));
        let v = y.and_then(|y| frac((cy1 - y) / (cy1 - cy0).max(1e-3)));
        let flip = |f: Option<f32>| f.map(|f| 1.0 - f);
        match (u32::from(self.rotation) + u32::from(view_rotation)) % 360 {
            90 => [flip(v), u],
            180 => [flip(u), flip(v)],
            270 => [v, flip(u)],
            _ => [u, v],
        }
    }

    /// A view-space rectangle → the normalized user-space rectangle covering the same area of the
    /// page (`[x0, y0, x1, y1]`, `x0 <= x1`, `y0 <= y1`). Under `/Rotate` the corners swap roles,
    /// so a view rectangle's top-left is not in general the user rectangle's `[x0, y1]`.
    pub fn view_rect_to_user(&self, r: [f32; 4]) -> [f32; 4] {
        let (a, b) = (self.view_to_user(r[0], r[1]), self.view_to_user(r[2], r[3]));
        [a[0].min(b[0]), a[1].min(b[1]), a[0].max(b[0]), a[1].max(b[1])]
    }

    /// A view-space rectangle as a text-markup quad in user space: top-left, top-right,
    /// bottom-left, bottom-right as read on screen.
    pub fn view_rect_to_quad(&self, r: [f32; 4]) -> [f64; 8] {
        let mut q = [0.0; 8];
        for (i, (x, y)) in [(r[0], r[1]), (r[2], r[1]), (r[0], r[3]), (r[2], r[3])].into_iter().enumerate() {
            let [ux, uy] = self.view_to_user(x, y);
            q[2 * i] = ux as f64;
            q[2 * i + 1] = uy as f64;
        }
        q
    }
}

/// A clickable link annotation.
#[derive(Clone, Debug)]
pub struct Link {
    pub page: usize,
    /// Zero-based position in the page's raw `/Annots` array.
    pub index: usize,
    pub rect: [f32; 4],
    pub target: LinkTarget,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LinkTarget {
    /// A destination in this document: the 0-based page and where on it the view goes.
    Page(usize, DestView),
    Uri(String),
    /// A set-layer-visibility action (`SetOCGState`, ISO 32000-2 §12.6.4.13): each change in
    /// order, naming the layer by its optional content group. With `preserve_rb`, a layer turned
    /// on turns off the other layers of its radio-button groups.
    SetLayers {
        changes: Vec<(LayerOp, (u32, u16))>,
        preserve_rb: bool,
    },
    /// A local-file remote Go-To action. The caller resolves `file` relative to the source path.
    /// `launch` marks a Launch action that names a PDF file (see `launch_pdf`), opened the same
    /// way: as a document in PdfCraft, never by the operating system.
    RemoteGoTo {
        launch: bool,
        file: String,
        destination: RemoteDestination,
        new_window: Option<bool>,
    },
    Other(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum RemoteDestination {
    Default,
    Page { page: usize, view: DestView },
    Named(String),
}

/// What a set-layer-visibility action does to a layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerOp {
    On,
    Off,
    Toggle,
}

/// Where an explicit destination (ISO 32000-2 §12.3.2.2, Table 149) places its page in the
/// window. Coordinates are PDF user space; `None` is a `null` (or unusable) operand, which keeps
/// that part of the view as it is.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum DestView {
    /// The top of the page at the current zoom: a destination with no position, or one whose
    /// type is missing or unknown.
    #[default]
    Top,
    /// `/XYZ left top zoom`: the point (left, top) at the window's upper-left corner, at `zoom`
    /// (1.0 = 100%; 0 or null keeps the current zoom).
    Xyz { left: Option<f32>, top: Option<f32>, zoom: Option<f32> },
    /// `/Fit`, and `/FitB` (fit the page's content box; read as the whole page).
    Fit,
    /// `/FitH top`, and `/FitBH`: fit the width, with `top` at the window's top edge.
    FitH { top: Option<f32> },
    /// `/FitV left`, and `/FitBV`: fit the height, with `left` at the window's left edge.
    FitV { left: Option<f32> },
    /// `/FitR left bottom right top`: fit the rectangle `[x0, y0, x1, y1]` (normalized).
    FitR { rect: [f32; 4] },
}

impl DestView {
    /// The view an explicit destination array asks for, from its elements after the page
    /// (`/XYZ left top zoom`, `/FitH top`, …). Hostile input never fails: a missing or unknown
    /// type, or a `/FitR` without four usable numbers, is [`DestView::Top`]; a non-numeric,
    /// non-finite or missing operand is `None`; a zoom that is not positive is `None`.
    pub fn from_operands(kind: Option<&[u8]>, operands: &[Option<f32>]) -> Self {
        let n = |i: usize| operands.get(i).copied().flatten().filter(|v| v.is_finite());
        match kind {
            Some(b"XYZ") => Self::Xyz { left: n(0), top: n(1), zoom: n(2).filter(|z| *z > 0.0) },
            Some(b"Fit" | b"FitB") => Self::Fit,
            Some(b"FitH" | b"FitBH") => Self::FitH { top: n(0) },
            Some(b"FitV" | b"FitBV") => Self::FitV { left: n(0) },
            Some(b"FitR") => match (n(0), n(1), n(2), n(3)) {
                (Some(x0), Some(y0), Some(x1), Some(y1)) => Self::FitR { rect: [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)] },
                _ => Self::Top,
            },
            _ => Self::Top,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OutlineItem {
    pub title: String,
    pub page: Option<usize>,
    /// Where on `page` the bookmark goes ([`DestView::Top`] for a bookmark without a page).
    pub view: DestView,
    pub children: Vec<OutlineItem>,
    /// Whether the bookmark starts open: only a positive `/Count` says so (ISO 32000-2 §12.3.3,
    /// Table 151). A negative one is closed, and so is a missing one, which a bookmark with
    /// children should not have.
    pub open: bool,
    /// The title's colour (`/C`, DeviceRGB, ISO 32000-2 §12.3.3 Table 151), or `None` for the
    /// default black or a value that isn't three numbers.
    pub color: Option<[f32; 3]>,
    /// The title's style (`/F`, Table 152): bit 1 italic, bit 2 bold.
    pub italic: bool,
    pub bold: bool,
}

#[derive(Clone, Debug)]
pub struct Annotation {
    pub page: usize,
    pub subtype: String,
    pub author: Option<String>,
    pub contents: Option<String>,
    pub modified: Option<String>,
    /// `/NM` of this annotation, and of the one it replies to (`/IRT`).
    pub name: Option<String>,
    pub in_reply_to: Option<String>,
    /// The page and `/Annots` position of the comment `/IRT` points at, found by object
    /// reference. Threads follow this rather than `/NM`, which need not be unique: PyMuPDF
    /// names the first comment on every page `fitz-A0`. `None` when `/IRT` leads to no listed
    /// comment ([`link_replies`]).
    pub parent: Option<(usize, usize)>,
    /// `/RT /Group`: grouped with the annotation `/IRT` points at (Replace Text's caret with its
    /// strikeout) rather than a reply to it.
    pub grouped: bool,
    /// Rect in PDF user space: [x0, y0, x1, y1].
    pub rect: [f32; 4],
    pub color: Option<[f32; 3]>,
    /// Position in the page's `/Annots` (how edits address the comment).
    pub index: usize,
    /// For a status reply (`/State`, Acrobat's "Set status"): the state it sets.
    pub state: Option<String>,
    /// Text markup quadrilaterals (`/QuadPoints`), 8 numbers each.
    pub quads: Vec<[f32; 8]>,
    /// The Locked flag (`/F` bit 8): the comment can't be moved, resized, restyled or deleted.
    pub locked: bool,
    /// `/IT`, the intent: `FreeTextCallout`, `PolygonCloud`, `FreeTextTypeWriter`…
    pub intent: Option<String>,
    /// A Fill & Sign mark: typed text, a check, cross, dot or line, or a typed, drawn or picture
    /// signature or initials (the rules of `pdfcraft_annot::is_fill_sign`).
    pub fill_sign: bool,
    /// The NoRotate flag (`/F` bit 5), set on sticky notes and attachments: on a turned page the
    /// appearance stays upright, turned about the upper-left corner of `rect`
    /// ([`Self::shown_rect`]).
    pub no_rotate: bool,
    /// The points that shape a line (its two ends, `/L`) or connected lines, a polygon or a
    /// cloud (their corners, `/Vertices`), in user space; empty for other comments.
    pub points: Vec<[f32; 2]>,
}

impl Annotation {
    /// A checkmark reply (`/StateModel /Marked`): "Marked" or "Unmarked" rather than a review status.
    pub fn is_mark(&self) -> bool {
        matches!(self.state.as_deref(), Some("Marked" | "Unmarked"))
    }

    /// A reply (or status reply) to another comment rather than a comment of its own.
    pub fn is_reply(&self) -> bool {
        self.parent.is_some()
    }

    /// A comment of its own: not a reply, and not a status or checkmark. One whose comment can't
    /// be found (deleted from another page, or never listed) has nothing to show, so it is listed
    /// nowhere: not as a card, on the page or in a count.
    pub fn is_comment(&self) -> bool {
        !self.is_reply() && self.state.is_none()
    }

    /// Whether this annotation's `/IRT` points at `other`.
    pub fn replies_to(&self, other: &Annotation) -> bool {
        self.parent == Some((other.page, other.index))
    }

    /// Where the comment is drawn on a page turned by `rotation` (`PageInfo::rotation`), in user
    /// space: `rect`, or for a NoRotate comment `rect` turned upright about its upper-left corner
    /// (see [`Self::no_rotate`]). Hit-testing, selection and anything placed beside the comment
    /// use this, so they match the drawing.
    pub fn shown_rect(&self, rotation: u16) -> [f32; 4] {
        self.shown(self.rect.map(f64::from), rotation).map(|v| v as f32)
    }

    /// [`Self::shown_rect`] for this comment with its `/Rect` at `rect` (one being moved or
    /// resized).
    pub fn shown(&self, rect: [f64; 4], rotation: u16) -> [f64; 4] {
        if self.no_rotate { upright_rect(rect, rotation) } else { rect }
    }

    /// The `/Rect` that shows this comment in the user-space box `shown`: the inverse of
    /// [`Self::shown`], for a box resized as drawn.
    pub fn rect_for_shown(&self, shown: [f64; 4], rotation: u16) -> [f64; 4] {
        if self.no_rotate { rect_from_upright(shown, rotation) } else { shown }
    }
}

/// The user-space box a NoRotate annotation with `/Rect` `rect` is drawn in on a page turned
/// clockwise by `rotation` (ISO 32000-2 §12.5.3): `rect` turned as far counterclockwise about its
/// upper-left corner `(x0, y1)`, which stays where it is on the page. As displayed, the box's
/// top-left is that corner, the rectangle's width runs across and its height down, as on an
/// unturned page. `rect` itself for 0 or anything that isn't a quarter turn. The vendored
/// renderer draws the appearance there (hayro-interpret patch 37).
fn upright_rect(rect: [f64; 4], rotation: u16) -> [f64; 4] {
    let [x0, y0, x1, y1] = [rect[0].min(rect[2]), rect[1].min(rect[3]), rect[0].max(rect[2]), rect[1].max(rect[3])];
    let (w, h) = (x1 - x0, y1 - y0);
    match rotation {
        90 => [x0, y1, x0 + h, y1 + w],
        180 => [x0 - w, y1, x0, y1 + h],
        270 => [x0 - h, y1 - w, x0, y1],
        _ => [x0, y0, x1, y1],
    }
}

/// The `/Rect` whose [`upright_rect`] is `shown`: `shown` turned back about the corner that
/// stays put.
fn rect_from_upright(shown: [f64; 4], rotation: u16) -> [f64; 4] {
    let [s0, s1, s2, s3] = [shown[0].min(shown[2]), shown[1].min(shown[3]), shown[0].max(shown[2]), shown[1].max(shown[3])];
    let (sw, sh) = (s2 - s0, s3 - s1);
    // The upper-left corner of `/Rect` and the rectangle's width and height.
    let ((x0, y1), w, h) = match rotation {
        90 => ((s0, s1), sh, sw),
        180 => ((s2, s1), sw, sh),
        270 => ((s2, s3), sh, sw),
        _ => return [s0, s1, s2, s3],
    };
    [x0, y1 - h, x0 + w, y1]
}

/// Settle the reply links of a document's annotations: a reply keeps its `parent` only when
/// following the links reaches a comment of its own within 64 steps. An `/IRT` to an unlisted
/// object, a pop-up, the annotation itself or a loop leaves it a comment of its own, so it stays
/// in view instead of hanging off nothing. Both comment readers (inspect, and the engine's list
/// after an edit) run this.
///
/// Only comments can be parents: both readers leave links, form widgets, pop-ups and other
/// non-markup annotations out of `annotations` (`pdfcraft_annot::is_comment_subtype`), so an
/// `/IRT` to one of those finds no parent here. Pop-ups are skipped again in case a caller's list
/// has them.
pub fn link_replies(annotations: &mut [Annotation]) {
    let at: HashMap<(usize, usize), usize> =
        annotations.iter().enumerate().filter(|(_, a)| a.subtype != "Popup").map(|(i, a)| ((a.page, a.index), i)).collect();
    let links: Vec<Option<usize>> =
        annotations.iter().enumerate().map(|(i, a)| a.parent.and_then(|p| at.get(&p).copied()).filter(|&j| j != i)).collect();
    for (i, a) in annotations.iter_mut().enumerate() {
        let mut up = links.get(i).copied().flatten();
        let mut settled = false;
        for _ in 0..64 {
            let Some(j) = up else { break };
            up = links.get(j).copied().flatten();
            if up.is_none() {
                settled = true;
                break;
            }
        }
        if !settled {
            a.parent = None;
        }
    }
}

/// The comment each reply's thread starts from, by the reply's page and `/Annots` position: a
/// reply to a reply files under that comment too. Expects links [`link_replies`] settled.
pub fn thread_tops(annotations: &[Annotation]) -> HashMap<(usize, usize), (usize, usize)> {
    let parents: HashMap<(usize, usize), (usize, usize)> = annotations.iter().filter_map(|a| Some(((a.page, a.index), a.parent?))).collect();
    parents
        .iter()
        .map(|(&reply, &first)| {
            let mut top = first;
            for _ in 0..64 {
                let Some(&up) = parents.get(&top) else { break };
                top = up;
            }
            (reply, top)
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    CheckBox,
    Radio,
    PushButton,
    Combo,
    List,
    Signature,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    pub kind: FieldKind,
    pub value: Option<String>,
    pub page: Option<usize>,
    pub tooltip: Option<String>,
    pub has_actions: bool,
    /// Rect of the first widget, in user space.
    pub rect: Option<[f32; 4]>,
}

#[derive(Clone, Debug)]
pub struct Layer {
    /// Object number and generation of the optional content group.
    pub id: (u32, u16),
    pub name: String,
    pub visible: bool,
    /// Shown as the file opens: the default configuration (`/OCProperties /D`).
    pub initial: bool,
}

#[derive(Clone, Debug)]
pub struct Attachment {
    pub name: String,
    pub description: Option<String>,
    pub size: Option<usize>,
    pub source: AttachmentSource,
}

/// Where an attachment lives (so its bytes can be fetched on demand).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentSource {
    /// Document-level: `/Names /EmbeddedFiles` entry with this key.
    Document { key: String },
    /// A FileAttachment annotation: page index and position in the page's `/Annots`.
    Annotation { page: usize, index: usize },
}

/// A font used by the document (Document Properties ▸ Fonts).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontInfo {
    /// BaseFont without the subset tag.
    pub name: String,
    /// Type1, TrueType, Type0, Type3, MMType1, CIDFontType0/2 …
    pub kind: String,
    pub embedded: bool,
    pub subset: bool,
    pub encoding: Option<String>,
    /// The face standing in for a program the document does not embed. `None` when the font is
    /// embedded (nothing stands in) or when no face was available at all -- `substitution` then
    /// says which of the two it is.
    pub substitute: Option<String>,
    /// Why a face was substituted, as a sentence for whoever has to act on it. `None` only when
    /// the document's own glyphs are being shown.
    pub substitution: Option<String>,
}

/// Check that `bytes` still open as the same document: the parser reads them and the page
/// count matches. Content-only edits skip the full [`inspect`] (seconds on very large files)
/// but must still catch an edit that produced something the viewer cannot display, so the
/// caller can roll it back. Never panics: parser crashes are caught and reported.
pub fn check_pages(bytes: Arc<Vec<u8>>, password: Option<&str>, expected: usize) -> Result<(), OpenError> {
    let pdf = catch_unwind(AssertUnwindSafe(|| Pdf::new_with_password(bytes, password.unwrap_or(""))))
        .map_err(|p| OpenError::Invalid(format!("the parser crashed: {}", crate::raster::panic_message(&p))))?
        .map_err(|e| match e {
            LoadPdfError::Decryption(DecryptionError::PasswordProtected) => {
                if password.is_some() {
                    OpenError::WrongPassword
                } else {
                    OpenError::NeedsPassword
                }
            }
            LoadPdfError::Decryption(other) => OpenError::Unsupported(format!("encryption not supported: {other:?}")),
            LoadPdfError::Invalid => OpenError::Invalid("no readable page tree or cross-reference data was found".into()),
        })?;
    let pages = catch_unwind(AssertUnwindSafe(|| pdf.pages().len()))
        .map_err(|p| OpenError::Invalid(format!("the page tree could not be read: {}", crate::raster::panic_message(&p))))?;
    if pages != expected {
        return Err(OpenError::Invalid(format!("the edit changed the page count from {expected} to {pages}")));
    }
    Ok(())
}

/// Inspect a document. Fails only if the renderer itself cannot open the file (or needs a
/// password). Never panics: parser crashes are caught and reported.
pub fn inspect(bytes: Arc<Vec<u8>>, password: Option<&str>) -> Result<DocInfo, OpenError> {
    let pdf = catch_unwind(AssertUnwindSafe(|| Pdf::new_with_password(bytes.clone(), password.unwrap_or(""))))
        .map_err(|p| OpenError::Invalid(format!("the parser crashed: {}", crate::raster::panic_message(&p))))?
        .map_err(|e| match e {
            LoadPdfError::Decryption(DecryptionError::PasswordProtected) => {
                if password.is_some() {
                    OpenError::WrongPassword
                } else {
                    OpenError::NeedsPassword
                }
            }
            LoadPdfError::Decryption(other) => OpenError::Unsupported(format!("encryption not supported: {other:?}")),
            LoadPdfError::Invalid => OpenError::Invalid("no readable page tree or cross-reference data was found".into()),
        })?;
    let mut info = DocInfo { file_size: bytes.len(), pdf_version: version_label(&format!("{:?}", pdf.version())), ..Default::default() };
    let pages = catch_unwind(AssertUnwindSafe(|| {
        let mut out = Vec::new();
        for (i, page) in pdf.pages().iter().enumerate() {
            let (w, h) = page.render_dimensions();
            let c = page.intersected_crop_box();
            let rotation = match page.rotation() {
                hayro::hayro_syntax::page::Rotation::None => 0,
                hayro::hayro_syntax::page::Rotation::Horizontal => 90,
                hayro::hayro_syntax::page::Rotation::Flipped => 180,
                hayro::hayro_syntax::page::Rotation::FlippedHorizontal => 270,
            };
            let crop = [c.x0 as f32, c.y0 as f32, c.x1 as f32, c.y1 as f32];
            // Degenerate boxes still get a usable placeholder size so layout never divides by zero.
            let (w, h) = if w.is_finite() && h.is_finite() && w >= 1.0 && h >= 1.0 { (w, h) } else { (612.0, 792.0) };
            out.push(PageInfo { width: w, height: h, label: (i + 1).to_string(), crop, rotation });
        }
        out
    }));
    match pages {
        Ok(p) => info.pages = p,
        Err(p) => return Err(OpenError::Invalid(format!("the page tree could not be read: {}", crate::raster::panic_message(&p)))),
    }
    let options = load_options(password);
    // Geometry no longer needs the renderer parser. Do not overlap its allocations with
    // structural inspection, including the compatibility fallback for damaged files.
    drop(pdf);
    inspect_structure(&mut info, |tmp| {
        // The lazy reader first; lopdf's repair/tolerance path when it can't expose the same
        // page tree or an object a panel needs. No partial result is published.
        let lazy = catch_unwind(AssertUnwindSafe(|| {
            let doc = LazyStructure::new(bytes.clone(), password).ok()?;
            let mut t = DocInfo { pages: tmp.pages.clone(), ..Default::default() };
            let inspector = Inspector::new(&doc);
            if inspector.page_index.len() != t.pages.len() {
                return None;
            }
            inspector.fill(&mut t);
            (!doc.failed()).then_some(t)
        }))
        .ok()
        .flatten();
        if let Some(t) = lazy {
            *tmp = t;
            return Ok(());
        }
        let doc = Document::load_mem_with_options(&bytes, options).map_err(|e| e.to_string())?;
        Inspector::new(&doc).fill(tmp);
        Ok(())
    });
    if info.pages.is_empty() {
        return Err(OpenError::Invalid("the document has no pages".into()));
    }
    Ok(info)
}

// Commit auxiliary metadata only after inspection succeeds; renderer geometry is the fallback.
fn inspect_structure(info: &mut DocInfo, fill: impl FnOnce(&mut DocInfo) -> Result<(), String>) {
    let structure = catch_unwind(AssertUnwindSafe(|| {
        let mut tmp = DocInfo { pages: info.pages.clone(), ..Default::default() };
        fill(&mut tmp)?;
        Ok::<_, String>(tmp)
    }));
    match structure {
        Ok(Ok(mut filled)) => {
            filled.file_size = info.file_size;
            filled.pdf_version = std::mem::take(&mut info.pdf_version);
            *info = filled;
        }
        Ok(Err(e)) => info.warnings.push(format!("Some document structure (bookmarks, comments, fields) could not be read: {e}")),
        Err(p) => {
            info.warnings.push(format!("Document structure inspection crashed and was skipped: {}", crate::raster::panic_message(&p)));
        }
    }
}

struct Inspector<'a> {
    doc: &'a dyn Structure,
    page_ids: Vec<ObjectId>,
    page_index: HashMap<ObjectId, usize>,
    /// Named destinations (`/Names /Dests` tree), keyed by raw string bytes, built once.
    /// Looking each name up by walking the tree was O(links × names): minutes on manuals.
    named: HashMap<Vec<u8>, &'a Object>,
    /// The page of each annotation (the first page that lists it), built when a field's widget
    /// has no `/P`. Searching every page's `/Annots` for each such field was O(fields ×
    /// annotations): a second on a form of 8,000 fields.
    annotation_pages: OnceCell<HashMap<ObjectId, usize>>,
}

impl<'a> Inspector<'a> {
    fn new(doc: &'a dyn Structure) -> Self {
        let page_ids = doc.page_ids();
        let page_index = page_ids.iter().copied().enumerate().map(|(index, id)| (id, index)).collect();
        let mut me = Self { doc, page_ids, page_index, named: HashMap::new(), annotation_pages: OnceCell::new() };
        let mut named = HashMap::new();
        if let Some(tree) =
            doc.catalog().ok().and_then(|c| c.get(b"Names").ok()).and_then(|o| me.dict(o)).and_then(|n| n.get(b"Dests").ok()).and_then(|o| me.dict(o))
        {
            me.name_tree_raw(tree, &mut HashSet::new(), 0, &mut named);
        }
        me.named = named;
        me
    }

    fn fill(&self, info: &mut DocInfo) {
        // lopdf drops /Encrypt from the trailer once it has decrypted the file.
        info.encrypted = self.doc.encrypted();
        if let Some(d) = self.doc.trailer().get(b"Info").ok().and_then(|o| self.dict(o)) {
            info.title = self.text(d, b"Title");
            info.author = self.text(d, b"Author");
            info.subject = self.text(d, b"Subject");
            info.keywords = self.text(d, b"Keywords");
            info.creator = self.text(d, b"Creator");
            info.producer = self.text(d, b"Producer");
            const STANDARD: [&[u8]; 9] =
                [b"Title", b"Author", b"Subject", b"Keywords", b"Creator", b"Producer", b"CreationDate", b"ModDate", b"Trapped"];
            // The same rule as the engine's list after an edit (`pdfcraft_organize::custom_info`,
            // trimmed and without empty values): text strings only, so a name value is not a
            // custom property, and the cap counts listed entries.
            info.custom = d
                .iter()
                .filter(|(k, _)| !STANDARD.contains(&k.as_slice()))
                .filter_map(|(k, v)| match self.resolve(v) {
                    Object::String(bytes, _) => {
                        let v = text_string(bytes).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string();
                        (!v.is_empty()).then(|| (String::from_utf8_lossy(k).into_owned(), v))
                    }
                    _ => None,
                })
                .take(256)
                .collect();
        }
        let Ok(catalog) = self.doc.catalog() else { return };
        info.tagged = catalog
            .get(b"MarkInfo")
            .ok()
            .and_then(|o| self.dict(o))
            .and_then(|m| m.get(b"Marked").ok())
            // Any value may be written as an indirect reference, `/Marked 9 0 R` included, so the
            // flag is resolved before it is read as a boolean (#3671).
            .map(|o| self.resolve(o))
            .and_then(|o| o.as_bool().ok())
            .unwrap_or(false);
        match self.page_labels(catalog, &mut info.pages) {
            Ok(notes) => info.warnings.extend(notes.into_iter().map(|note| format!("Page labels: {note}."))),
            Err(reason) => info.warnings.push(format!("Page labels were skipped: {reason}; using physical page numbers.")),
        }
        if let Some(first) = catalog.get(b"Outlines").ok().and_then(|o| self.dict(o)).and_then(|d| d.get(b"First").ok()) {
            (info.outline, info.outline_more) = self.outline_window(first);
        }
        let mut text_budget = MAX_LINK_ATTACHMENT_TEXT_BYTES;
        self.annotations(info, &mut text_budget);
        self.transitions(info);
        let mut field_seen = HashSet::new();
        if let Some(form) = catalog.get(b"AcroForm").ok().and_then(|o| self.dict(o))
            && let Ok(fields) = form.get(b"Fields").and_then(|o| self.resolve(o).as_array())
        {
            for f in fields {
                self.field(f, None, &mut info.fields, &mut field_seen, 0);
            }
        }
        // Widgets no `/Fields` entry reaches are fields too (Acrobat and the browsers fill them);
        // each is adopted through its topmost unlisted `/Parent`, so a split field stays one.
        for &pid in &self.page_ids {
            let Ok(page_dict) = self.doc.get_dictionary(pid) else { continue };
            let Ok(Object::Array(annots)) = page_dict.get(b"Annots").map(|o| self.resolve(o)) else { continue };
            for a in annots {
                let Some(d) = self.dict(a) else { continue };
                if self.name(d, b"Subtype").as_deref() != Some("Widget")
                    || (d.get(b"FT").is_err() && d.get(b"T").is_err() && d.get(b"Parent").is_err())
                {
                    continue;
                }
                if let Object::Reference(id) = a
                    && let Some(root) = self.unlisted_field(*id, &field_seen)
                {
                    self.field(&Object::Reference(root), None, &mut info.fields, &mut field_seen, 0);
                }
            }
        }
        info.has_javascript |= info.fields.iter().any(|f| f.has_actions);
        let needs_rendering = catalog.get(b"NeedsRendering").ok().and_then(|o| self.resolve(o).as_bool().ok()).unwrap_or(false);
        if let Some(xfa) = catalog.get(b"AcroForm").ok().and_then(|o| self.dict(o)).and_then(|form| form.get(b"XFA").ok()) {
            // A form with no fields is taken to be laid out from its template, unless its
            // configuration says it is a static form (#3180): Designer writes
            // `<dynamicRender>forbidden</dynamicRender>` for one, whose pages are the form.
            let static_form = self.xfa_dynamic_render(xfa).as_deref() == Some("forbidden");
            info.xfa = Some(if needs_rendering || (info.fields.is_empty() && !static_form) { Xfa::Dynamic } else { Xfa::Static });
        }
        self.layers(catalog, &mut info.layers);
        info.layer_groups = self.layer_groups(catalog);
        self.fonts(&mut info.fonts);
        if let Some(names) = catalog.get(b"Names").ok().and_then(|o| self.dict(o)) {
            if let Some(ef) = names.get(b"EmbeddedFiles").ok().and_then(|o| self.dict(o)) {
                let mut seen = HashSet::new();
                for (name, spec) in self.name_tree(ef, &mut seen, 0, &mut text_budget) {
                    // The key is kept a second time when it is also the name.
                    let fallback = |budget: &mut usize| spend(name.as_bytes(), budget, |_| name.clone());
                    info.attachments.push(self.attachment(spec, AttachmentSource::Document { key: name.clone() }, &mut text_budget, fallback));
                }
            }
            info.has_javascript |= names.get(b"JavaScript").is_ok();
        }
        if let Ok(action) = catalog.get(b"OpenAction")
            && let Some(dict) = self.dict(action)
            && dict.get(b"S").ok().and_then(|o| self.resolve(o).as_name().ok()) == Some(&b"JavaScript"[..])
        {
            info.has_javascript = true;
        }
        info.open_dest = catalog.get(b"OpenAction").ok().and_then(|o| self.open_dest(o));
    }

    /// The destination `/OpenAction` opens at (ISO 32000-2 §7.7.2 Table 29): an explicit
    /// destination array, a `/GoTo` action's `/D`, or (leniently, since Table 29 asks for an array
    /// or an action) a destination name or string; a name or string is looked up as a named
    /// destination (§12.3.2.4). Any other action (a script, a remote go-to) has none here, and
    /// neither has a page number past the end of the document.
    fn open_dest(&self, action: &Object) -> Option<(usize, DestView)> {
        let dest = match self.resolve(action) {
            Object::Dictionary(d) => {
                if d.get(b"S").ok().and_then(|o| self.resolve(o).as_name().ok()) != Some(&b"GoTo"[..]) {
                    return None;
                }
                d.get(b"D").ok()?
            }
            other => other,
        };
        self.dest(dest, 0).filter(|(page, _)| *page < self.page_ids.len())
    }

    // ── object helpers ──────────────────────────────────────────────────────────────────────

    /// `/Trans` and `/Dur` of every page (ISO 32000-2 §12.4.4), in page order. Lenient: a value
    /// outside the spec's choices takes that entry's default, and a page with neither entry is
    /// not listed. Bounded by the page count.
    fn transitions(&self, info: &mut DocInfo) {
        for (page, &pid) in self.page_ids.iter().enumerate() {
            let Ok(d) = self.doc.get_dictionary(pid) else { continue };
            let display = d.get(b"Dur").ok().and_then(|o| self.resolve(o).as_float().ok()).filter(|s| s.is_finite() && *s >= 0.0);
            let trans = d.get(b"Trans").ok().and_then(|o| self.dict(o));
            if trans.is_none() && display.is_none() {
                continue;
            }
            let mut t = PageTransition {
                page,
                style: None,
                duration: 1.0,
                horizontal: true,
                inward: true,
                direction: Some(0),
                scale: 1.0,
                opaque: false,
                display,
            };
            if let Some(tr) = trans {
                t.style = Some(self.name(tr, b"S").map_or(TransitionStyle::Replace, |n| TransitionStyle::from_name(&n)));
                let num = |key: &[u8]| tr.get(key).ok().and_then(|o| self.resolve(o).as_float().ok()).filter(|v| v.is_finite());
                t.duration = num(b"D").filter(|d| *d >= 0.0).unwrap_or(1.0);
                t.horizontal = self.name(tr, b"Dm").as_deref() != Some("V");
                t.inward = self.name(tr, b"M").as_deref() != Some("O");
                t.direction = match tr.get(b"Di").map(|o| self.resolve(o)) {
                    Ok(Object::Name(n)) if n.as_slice() == b"None" => None,
                    Ok(o) => match o.as_i64().ok() {
                        Some(0) => Some(0),
                        Some(90) => Some(90),
                        Some(180) => Some(180),
                        Some(270) => Some(270),
                        Some(315) => Some(315),
                        _ => Some(0),
                    },
                    Err(_) => Some(0),
                };
                t.scale = num(b"SS").filter(|s| *s > 0.0).unwrap_or(1.0);
                t.opaque = matches!(tr.get(b"B").map(|o| self.resolve(o)), Ok(Object::Boolean(true)));
            }
            info.transitions.push(t);
        }
    }

    /// The value of `dynamicRender` in an XFA form's configuration packet (`config`, under
    /// `acrobat/acrobat7`): `required` for a dynamic form, `forbidden` for a static one. `/XFA` is
    /// either one stream holding the whole XDP or an array of packet names and streams (ISO
    /// 32000-1 §12.7.8). Lenient: `None` when the packet is missing, unreadable or over
    /// `MAX_XFA_CONFIG_BYTES` decoded, or has no such element.
    fn xfa_dynamic_render(&self, xfa: &'a Object) -> Option<String> {
        let stream = match self.resolve(xfa) {
            Object::Array(items) => {
                let at = items.iter().position(|o| matches!(self.resolve(o), Object::String(name, _) if name.as_slice() == b"config"))?;
                self.resolve(items.get(at.checked_add(1)?)?)
            }
            whole => whole,
        };
        let stream = stream.as_stream().ok()?;
        let bytes = if stream.dict.get(b"Filter").is_ok() {
            stream.decompressed_content_with_limit(MAX_XFA_CONFIG_BYTES).ok()?
        } else {
            stream.content.get(..stream.content.len().min(MAX_XFA_CONFIG_BYTES))?.to_vec()
        };
        xfa_config_dynamic_render(&xfa_packet_text(&bytes))
    }

    fn resolve(&self, o: &'a Object) -> &'a Object {
        self.doc.dereference(o).map(|(_, o)| o).unwrap_or(o)
    }

    /// The object a reference ends at after following any chain of references; `None` for a
    /// direct object or a broken reference.
    fn resolved_id(&self, o: &Object) -> Option<ObjectId> {
        self.doc.dereference(o).ok().and_then(|(id, _)| id)
    }

    fn dict(&self, o: &'a Object) -> Option<&'a Dictionary> {
        match self.resolve(o) {
            Object::Dictionary(d) => Some(d),
            Object::Stream(s) => Some(&s.dict),
            _ => None,
        }
    }

    fn text(&self, d: &Dictionary, key: &[u8]) -> Option<String> {
        let s = trimmed_text(self.resolve(d.get(key).ok()?))?;
        (!s.is_empty()).then_some(s)
    }

    /// [`Self::text`], converted and spent from `budget` as [`spend`] does, so text that trims
    /// short or to nothing still spends its raw length.
    fn budgeted_text(&self, d: &Dictionary, key: &[u8], budget: &mut usize) -> Option<String> {
        let o = self.resolve(d.get(key).ok()?);
        let (Object::String(bytes, _) | Object::Name(bytes)) = o else { return None };
        let text = spend(bytes, budget, |_| trimmed_text(o).unwrap_or_default())?;
        (!text.is_empty()).then_some(text)
    }

    fn name(&self, d: &Dictionary, key: &[u8]) -> Option<String> {
        self.resolve(d.get(key).ok()?).as_name().ok().map(|n| String::from_utf8_lossy(n).into_owned())
    }

    /// Whether an annotation is a Fill & Sign mark. Keep in step with
    /// `pdfcraft_annot::is_fill_sign`, which the engine's comment list uses.
    fn is_fill_sign(&self, d: &Dictionary, subtype: &str) -> bool {
        let yes = |key: &[u8]| matches!(d.get(key).map(|o| self.resolve(o)), Ok(Object::Boolean(true)));
        if yes(b"PCFillSign") {
            return true;
        }
        match subtype {
            "FreeText" => self.name(d, b"IT").as_deref() == Some("FreeTextTypeWriter"),
            "Stamp" => match self.name(d, b"Name").as_deref() {
                Some("PCCheck" | "PCCross" | "PCDot" | "PCLine" | "PCTypedSignature") => true,
                Some("PCCustomSignature" | "PCCustomInitials") => yes(b"PCPictureImage"),
                _ => false,
            },
            "Ink" => self.text(d, b"Subj").as_deref() == Some("Signature"),
            _ => false,
        }
    }

    fn page_of(&self, o: &Object) -> Option<usize> {
        match o {
            Object::Reference(id) => self.page_index.get(id).copied(),
            // Some producers write page *numbers* in remote-style destinations.
            Object::Integer(n) => usize::try_from(*n).ok(),
            _ => None,
        }
    }

    // ── destinations ────────────────────────────────────────────────────────────────────────

    /// The page and view of a destination: an explicit array (`[page /XYZ left top zoom]`, …),
    /// a dictionary with `/D`, or a named destination that resolves to one.
    fn dest(&self, dest: &Object, depth: u32) -> Option<(usize, DestView)> {
        if depth > 8 {
            return None;
        }
        match self.resolve(dest) {
            Object::Array(a) => {
                let page = a.first().and_then(|p| self.page_of(p))?;
                let kind = a.get(1).map(|k| self.resolve(k)).and_then(|k| k.as_name().ok());
                let operands: Vec<Option<f32>> = a.iter().skip(2).take(4).map(|o| self.resolve(o).as_float().ok()).collect();
                Some((page, DestView::from_operands(kind, &operands)))
            }
            Object::Dictionary(d) => d.get(b"D").ok().and_then(|d| self.dest(d, depth + 1)),
            Object::String(key, _) | Object::Name(key) => self.named_dest(key).and_then(|d| self.dest(d, depth + 1)),
            _ => None,
        }
    }

    fn named_dest(&self, key: &[u8]) -> Option<&'a Object> {
        let catalog = self.doc.catalog().ok()?;
        if let Some(v) = self.named.get(key) {
            return Some(v);
        }
        // PDF 1.1 style /Dests dictionary.
        catalog.get(b"Dests").ok().and_then(|o| self.dict(o)).and_then(|d| d.get(key).ok())
    }

    /// A name tree's leaves keyed by their raw key bytes (first occurrence wins).
    fn name_tree_raw(&self, node: &'a Dictionary, seen: &mut HashSet<*const Dictionary>, depth: u32, out: &mut HashMap<Vec<u8>, &'a Object>) {
        if depth > 32 || !seen.insert(node as *const _) {
            return;
        }
        if let Ok(Object::Array(pairs)) = node.get(b"Names").map(|o| self.resolve(o)) {
            for pair in pairs.chunks(2) {
                if let [k, v] = pair {
                    let key = match self.resolve(k) {
                        Object::String(s, _) => s.clone(),
                        Object::Name(n) => n.clone(),
                        _ => continue,
                    };
                    out.entry(key).or_insert(v);
                }
            }
        }
        if let Ok(Object::Array(kids)) = node.get(b"Kids").map(|o| self.resolve(o)) {
            for k in kids {
                if let Some(d) = self.dict(k) {
                    self.name_tree_raw(d, seen, depth + 1, out);
                }
            }
        }
    }

    /// The entries of an embedded-files name tree. Each string key is spent from `text_budget`
    /// (see [`MAX_LINK_ATTACHMENT_TEXT_BYTES`]), and an entry whose key does not fit is left out.
    fn name_tree(
        &self,
        node: &'a Dictionary,
        seen: &mut HashSet<*const Dictionary>,
        depth: u32,
        text_budget: &mut usize,
    ) -> Vec<(String, &'a Object)> {
        let mut out = Vec::new();
        if depth > 32 || !seen.insert(node as *const _) {
            return out;
        }
        if let Ok(Object::Array(pairs)) = node.get(b"Names").map(|o| self.resolve(o)) {
            for pair in pairs.chunks(2) {
                if let [k, v] = pair {
                    let key = match self.resolve(k) {
                        Object::String(bytes, _) => match spend(bytes, text_budget, text_string) {
                            Some(key) => key,
                            None => continue,
                        },
                        _ => String::new(),
                    };
                    out.push((key, v));
                }
            }
        }
        if let Ok(Object::Array(kids)) = node.get(b"Kids").map(|o| self.resolve(o)) {
            for k in kids {
                if let Some(d) = self.dict(k) {
                    out.extend(self.name_tree(d, seen, depth + 1, text_budget));
                }
            }
        }
        out
    }

    /// The first entry of an embedded-files name tree whose key is `key`, in the order
    /// [`Self::name_tree`] lists them, with no key budget. It keeps no key, and converts a key
    /// object or `/Names` array that several places reference only once (a second visit cannot
    /// match, since the first would have returned).
    fn name_tree_find(&self, node: &'a Dictionary, key: &str, seen: &mut NameTreeSeen, depth: u32) -> Option<&'a Object> {
        if depth > 32 || !seen.nodes.insert(node as *const _) {
            return None;
        }
        let names = node.get(b"Names").ok();
        if let Some(names) = names
            && self.resolved_id(names).is_none_or(|id| seen.objects.insert(id))
            && let Object::Array(pairs) = self.resolve(names)
        {
            for pair in pairs.chunks(2) {
                if let [k, v] = pair {
                    if self.resolved_id(k).is_some_and(|id| !seen.objects.insert(id)) {
                        continue;
                    }
                    let found = match self.resolve(k) {
                        Object::String(bytes, _) => text_string(bytes) == key,
                        _ => key.is_empty(),
                    };
                    if found {
                        return Some(v);
                    }
                }
            }
        }
        if let Ok(Object::Array(kids)) = node.get(b"Kids").map(|o| self.resolve(o)) {
            for k in kids {
                if let Some(d) = self.dict(k)
                    && let Some(v) = self.name_tree_find(d, key, seen, depth + 1)
                {
                    return Some(v);
                }
            }
        }
        None
    }

    // ── outline ─────────────────────────────────────────────────────────────────────────────

    /// The window `DocInfo::outline` keeps: the first `OUTLINE_WINDOW` bookmarks in outline order,
    /// nested no deeper than `OUTLINE_DEPTH`, and whether any bookmark was left out. Iterative, and
    /// each bookmark is read once, so the work and memory stay within the window.
    fn outline_window(&self, first: &'a Object) -> (Vec<OutlineItem>, bool) {
        let mut seen = HashSet::new();
        // The listed bookmarks in outline order, with their depth.
        let mut listed: Vec<(usize, OutlineItem)> = Vec::new();
        // One entry per open level: its next sibling, or `None` once that run of siblings has ended.
        let mut levels: Vec<Option<&'a Object>> = vec![Some(first)];
        let mut more = false;
        while let Some(depth) = levels.len().checked_sub(1) {
            let Some(slot) = levels.get_mut(depth) else { break };
            let Some(o) = slot.take() else {
                levels.pop();
                continue;
            };
            if let Object::Reference(id) = o
                && !seen.insert(*id)
            {
                continue; // cycle: this run of siblings ends here
            }
            let Some(d) = self.dict(o) else { continue };
            if listed.len() >= OUTLINE_WINDOW {
                more = true;
                break;
            }
            *slot = d.get(b"Next").ok();
            let dest = d
                .get(b"Dest")
                .ok()
                .and_then(|dest| self.dest(dest, 0))
                .or_else(|| d.get(b"A").ok().and_then(|a| self.dict(a)).and_then(|a| a.get(b"D").ok()).and_then(|dest| self.dest(dest, 0)));
            let (page, view) = dest.map_or((None, DestView::Top), |(p, v)| (Some(p), v));
            match d.get(b"First").ok() {
                Some(child) if depth < OUTLINE_DEPTH => levels.push(Some(child)),
                Some(_) => more = true,
                None => {}
            }
            let open = d.get(b"Count").ok().and_then(|c| c.as_i64().ok()).is_some_and(|c| c > 0);
            let title = self.text(d, b"Title").unwrap_or_default();
            let color = match d.get(b"C").map(|o| self.resolve(o)) {
                Ok(Object::Array(c)) => outline_color(c.iter().map(|v| self.resolve(v).as_float().ok())),
                _ => None,
            };
            let flags = d.get(b"F").ok().and_then(|f| self.resolve(f).as_i64().ok()).unwrap_or(0);
            let (italic, bold) = (flags & 1 != 0, flags & 2 != 0);
            listed.push((depth, OutlineItem { title, page, view, children: Vec::new(), open, color, italic, bold }));
        }
        (nest_outline(listed), more)
    }

    // ── page labels (ISO 32000-2 §12.4.2) ───────────────────────────────────────────────────

    fn label_object(&self, object: &'a Object) -> Result<&'a Object, &'static str> {
        self.doc.dereference(object).map(|(_, o)| o).map_err(|_| "a number tree reference is missing or cyclic")
    }

    fn label_dict(&self, object: &'a Object) -> Result<Option<&'a Dictionary>, &'static str> {
        Ok(match self.label_object(object)? {
            Object::Dictionary(d) => Some(d),
            Object::Stream(s) => Some(&s.dict),
            _ => None,
        })
    }

    /// Apply `/PageLabels` to `pages`. `Err`: the tree is unusable and no label is applied.
    /// `Ok(notes)`: labels applied, with what was repaired on the way (a duplicated start page,
    /// a range whose labels are too long to show).
    fn page_labels(&self, catalog: &Dictionary, pages: &mut [PageInfo]) -> Result<Vec<String>, &'static str> {
        let mut notes = Vec::new();
        let Ok(tree) = catalog.get(b"PageLabels") else { return Ok(notes) };
        let Some(tree) = self.label_dict(tree)? else { return Ok(notes) };
        let mut ranges: BTreeMap<usize, &Dictionary> = BTreeMap::new();
        let mut stack = vec![(tree, 0usize)];
        let mut seen = HashSet::new();
        let mut left = MAX_LABEL_TREE_WORK;
        while let Some((node, depth)) = stack.pop() {
            label_work(&mut left, 1)?;
            if depth > MAX_LABEL_TREE_DEPTH {
                return Err("the number tree is too deep");
            }
            if !seen.insert(node as *const Dictionary) {
                return Err("the number tree repeats a node or contains a cycle");
            }
            if let Ok(nums) = node.get(b"Nums")
                && let Object::Array(nums) = self.label_object(nums)?
            {
                label_work(&mut left, nums.len().div_ceil(2))?;
                for pair in nums.chunks(2) {
                    if let [k, v] = pair
                        && let (Ok(start), Some(d)) = (self.label_object(k)?.as_i64(), self.label_dict(v)?)
                        && let Ok(start) = usize::try_from(start)
                        && ranges.insert(start, d).is_some()
                    {
                        // The later range wins, as in readers that overwrite.
                        notes.push(format!("a range starts on page {} twice; the later one is used", start.saturating_add(1)));
                    }
                }
            }
            if let Ok(kids) = node.get(b"Kids")
                && let Object::Array(kids) = self.label_object(kids)?
            {
                label_work(&mut left, kids.len())?;
                if kids.len() > MAX_LABEL_TREE_WORK.saturating_sub(stack.len()) {
                    return Err("the number tree has too many pending nodes");
                }
                if !kids.is_empty() && depth == MAX_LABEL_TREE_DEPTH {
                    return Err("the number tree is too deep");
                }
                for kid in kids {
                    if let Some(d) = self.label_dict(kid)? {
                        stack.push((d, depth + 1));
                    }
                }
            }
        }
        // Decode each bounded prefix once, rather than once for every page in its range.
        let mut specs = BTreeMap::new();
        let mut prefix_total = 0usize;
        for (start, d) in ranges {
            let first = d.get(b"St").ok().and_then(|o| self.resolve(o).as_i64().ok()).unwrap_or(1).max(1) as u64;
            let prefix = match d.get(b"P").ok().map(|o| self.resolve(o)) {
                Some(Object::String(raw, _)) => {
                    if raw.len() > MAX_PREFIX_BYTES {
                        return Err("a raw prefix exceeds 2050 bytes");
                    }
                    // A label prefix is literal text, not trimmed document metadata.
                    text_string(raw).trim_matches('\0').to_string()
                }
                Some(Object::Name(raw)) => {
                    if raw.len() > MAX_PREFIX_BYTES {
                        return Err("a raw prefix exceeds 2050 bytes");
                    }
                    // Retain the existing name compatibility and boundary-NUL handling.
                    String::from_utf8_lossy(raw).trim_matches('\0').to_string()
                }
                _ => String::new(),
            };
            if prefix.len() > MAX_LABEL_BYTES {
                return Err("a decoded prefix exceeds 1024 UTF-8 bytes");
            }
            // A prefix object shared by many ranges is decoded once per range: count every copy.
            prefix_total = prefix_total.saturating_add(prefix.len());
            if prefix_total > MAX_PREFIX_TOTAL_BYTES {
                return Err("decoded prefixes exceed 4 MiB in total");
            }
            let style = d.get(b"S").ok().and_then(|o| self.resolve(o).as_name().ok());
            specs.insert(start, (first, prefix, style));
        }
        let mut staged = Vec::new();
        let mut total = 0usize;
        for (i, _) in pages.iter().enumerate() {
            let Some((&start, (first, prefix, style))) = specs.range(..=i).next_back() else { continue };
            let n = if matches!(*style, Some(b"D" | b"R" | b"r" | b"A" | b"a")) {
                let offset = u64::try_from(i - start).map_err(|_| "a page number is too large")?;
                first.checked_add(offset).ok_or("a page number overflows")?
            } else {
                0 // /St is unused when the label consists only of its prefix.
            };
            let remaining = MAX_LABEL_BYTES - prefix.len();
            let number = match *style {
                Some(b"D") => Some(n.to_string()),
                Some(b"R" | b"r") => roman(n, remaining),
                Some(b"A" | b"a") => alpha(n, remaining),
                _ => Some(String::new()),
            }
            .filter(|number| number.len() <= remaining);
            // A label too long to show keeps that page's physical number; other ranges stay.
            let Some(mut number) = number else {
                let note = format!(
                    "labels from page {} exceed {MAX_LABEL_BYTES} UTF-8 bytes; those pages show their physical page numbers",
                    start.saturating_add(1)
                );
                if !notes.contains(&note) {
                    notes.push(note);
                }
                continue;
            };
            if matches!(*style, Some(b"R" | b"A")) {
                number.make_ascii_uppercase();
            }
            let mut label = prefix.clone();
            label.push_str(&number);
            if label.is_empty() {
                continue; // Keep the existing physical number for an empty custom label.
            }
            total = total.checked_add(label.len()).ok_or("the total label size overflows")?;
            if total > MAX_LABEL_TOTAL_BYTES {
                return Err("custom labels exceed 4 MiB in total");
            }
            staged.push((i, label));
        }
        // A malformed later range must not leave a partially applied custom label sequence.
        for (i, label) in staged {
            if let Some(page) = pages.get_mut(i) {
                page.label = label;
            }
        }
        Ok(notes)
    }

    // ── annotations ─────────────────────────────────────────────────────────────────────────

    /// `text_budget` is what is left of [`MAX_LINK_ATTACHMENT_TEXT_BYTES`].
    fn annotations(&self, info: &mut DocInfo, text_budget: &mut usize) {
        let mut pages: Vec<(ObjectId, usize)> = self.page_index.iter().map(|(&id, &page)| (id, page)).collect();
        pages.sort_unstable_by_key(|&(_, page)| page);
        // An annotation, or a whole `/Annots` array, that several entries or pages list is on the
        // first of them (§12.5.2 allows one page per annotation). Both are told apart by the object
        // a reference resolves to, since an indirect object may itself hold a reference. Where each
        // annotation is listed is collected before any is read, so a reply's `/IRT` finds its
        // parent by reference even when the parent comes later.
        let mut seen_arrays = HashSet::new();
        let mut slots: HashMap<ObjectId, (usize, usize)> = HashMap::new();
        let mut entries = Vec::new();
        for (id, page) in pages {
            let Ok(page_dict) = self.doc.get_dictionary(id) else { continue };
            if let Ok(annots) = page_dict.get(b"Annots")
                && let Some(r) = self.resolved_id(annots)
                && !seen_arrays.insert(r)
            {
                continue;
            }
            let Ok(Object::Array(annots)) = page_dict.get(b"Annots").map(|o| self.resolve(o)) else { continue };
            for (index, a) in annots.iter().enumerate() {
                if let Some(r) = self.resolved_id(a) {
                    let Entry::Vacant(slot) = slots.entry(r) else { continue };
                    slot.insert((page, index));
                }
                entries.push((page, index, a));
            }
        }
        let mut kept = 0usize;
        // Once the comment budget is spent, comments are no longer built, but links and attachments
        // are still listed.
        let mut comments_full = false;
        for (page, index, a) in entries {
            let Some(d) = self.dict(a) else { continue };
            let Some(subtype) = self.name(d, b"Subtype") else { continue };
            if subtype == "FileAttachment"
                && let Ok(fs) = d.get(b"FS")
            {
                let fallback = |budget: &mut usize| self.budgeted_text(d, b"Contents", budget);
                info.attachments.push(self.attachment(fs, AttachmentSource::Annotation { page, index }, text_budget, fallback));
            }
            if subtype == "Link" {
                if let Some(link) = self.link(page, index, d, text_budget) {
                    if matches!(&link.target, LinkTarget::Other(s) if s == "JavaScript") {
                        info.has_javascript = true;
                    }
                    info.links.push(link);
                }
                continue;
            }
            // Not comments: form widgets, pop-ups and non-markup annotations such as the
            // Screen annotation that drives a LaTeX `animate` player (keep in step with
            // `pdfcraft_annot::is_comment_subtype`).
            if matches!(subtype.as_str(), "Widget" | "Popup" | "Screen" | "Movie" | "RichMedia" | "3D" | "PrinterMark" | "TrapNet" | "Watermark") {
                continue;
            }
            if comments_full {
                continue;
            }
            let rect = rect4(self.resolve(d.get(b"Rect").unwrap_or(&Object::Null)));
            let mut color = match d.get(b"C").map(|o| self.resolve(o)) {
                Ok(Object::Array(c)) if c.len() == 3 => {
                    let f = |i: usize| c[i].as_float().unwrap_or(0.0);
                    Some([f(0), f(1), f(2)])
                }
                _ => None,
            };
            // A text box's /C is its background; its text colour is in /DA.
            if subtype == "FreeText"
                && let Some(da) = self.text(d, b"DA")
            {
                let t: Vec<&str> = da.split_whitespace().collect();
                if let Some(i) = t.iter().position(|x| *x == "rg")
                    && i >= 3
                {
                    let f = |k: usize| t[k].parse::<f32>().unwrap_or(0.0);
                    color = Some([f(i - 3), f(i - 2), f(i - 1)]);
                }
            }
            let quads = match d.get(b"QuadPoints").map(|o| self.resolve(o)) {
                Ok(Object::Array(q)) => {
                    let v: Vec<f32> = q.iter().map(|o| o.as_float().unwrap_or(0.0)).collect();
                    v.as_chunks::<8>().0.to_vec()
                }
                _ => Vec::new(),
            };
            let in_reply_to = d.get(b"IRT").ok().and_then(|o| self.dict(o)).and_then(|p| self.text(p, b"NM"));
            let parent = d.get(b"IRT").ok().and_then(|o| self.resolved_id(o)).and_then(|r| slots.get(&r).copied());
            let fill_sign = self.is_fill_sign(d, &subtype);
            // Keep in step with `pdfcraft_annot::summaries`.
            let key: &[u8] = if subtype == "Line" { b"L" } else { b"Vertices" };
            let points = match d.get(key).map(|o| self.resolve(o)) {
                Ok(Object::Array(v)) if matches!(subtype.as_str(), "Line" | "PolyLine" | "Polygon") => {
                    let v: Vec<f32> = v.iter().map(|o| o.as_float().unwrap_or(0.0)).collect();
                    let pts: Vec<[f32; 2]> = v.as_chunks::<2>().0.to_vec();
                    if subtype == "Line" && pts.len() != 2 { Vec::new() } else { pts }
                }
                _ => Vec::new(),
            };
            let annotation = Annotation {
                page,
                subtype,
                author: self.text(d, b"T"),
                // Acrobat ends lines with CR, which the interface would draw as nothing (keep
                // in step with `pdfcraft_annot::summaries`).
                contents: self.text(d, b"Contents").map(|s| s.replace("\r\n", "\n").replace('\r', "\n")),
                modified: self.text(d, b"M").map(|m| pretty_date(&m)),
                name: self.text(d, b"NM"),
                in_reply_to,
                parent,
                grouped: self.name(d, b"RT").as_deref() == Some("Group"),
                rect,
                color,
                index,
                state: self.text(d, b"State"),
                quads,
                locked: d.get(b"F").ok().and_then(|f| self.resolve(f).as_i64().ok()).unwrap_or(0) & 128 != 0,
                intent: self.name(d, b"IT"),
                fill_sign,
                no_rotate: d.get(b"F").is_ok_and(|f| draw_flags(self.resolve(f)) & 16 != 0),
                points,
            };
            kept = kept.saturating_add(retained(&annotation));
            if kept > MAX_ANNOTATION_BYTES {
                comments_full = true;
                continue;
            }
            info.annotations.push(annotation);
        }
        info.annotations.sort_by(|a, b| a.page.cmp(&b.page).then(b.rect[3].total_cmp(&a.rect[3])));
        link_replies(&mut info.annotations);
    }

    fn link(&self, page: usize, index: usize, d: &Dictionary, text_budget: &mut usize) -> Option<Link> {
        let rect = rect4(self.resolve(d.get(b"Rect").ok()?));
        let target = if let Ok(dest) = d.get(b"Dest") {
            let (page, view) = self.dest(dest, 0)?;
            LinkTarget::Page(page, view)
        } else {
            let a = d.get(b"A").ok().and_then(|a| self.dict(a))?;
            match self.name(a, b"S").as_deref() {
                Some("GoTo") => {
                    let (page, view) = self.dest(a.get(b"D").ok()?, 0)?;
                    LinkTarget::Page(page, view)
                }
                Some("URI") => {
                    let uri = a.get(b"URI").ok().and_then(|u| self.resolve(u).as_str().ok())?;
                    match spend(uri, text_budget, |b| String::from_utf8_lossy(b).into_owned()) {
                        Some(uri) => LinkTarget::Uri(uri),
                        None => LinkTarget::Other("URI".into()),
                    }
                }
                Some("SetOCGState") => self.layer_state(a),
                Some("GoToR") => self.remote_go_to(a).unwrap_or_else(|| LinkTarget::Other("GoToR".into())),
                Some("Launch") => self.launch_pdf(a).unwrap_or_else(|| LinkTarget::Other("Launch".into())),
                Some(other) => LinkTarget::Other(other.to_string()),
                None => return None,
            }
        };
        Some(Link { page, index, rect, target })
    }

    fn remote_go_to(&self, action: &Dictionary) -> Option<LinkTarget> {
        let file = self.action_file(action)?;
        let destination = match action.get(b"D") {
            Err(_) => RemoteDestination::Default,
            Ok(o) => match self.resolve(o) {
                Object::Array(items) => {
                    let page = items.first().and_then(|o| self.resolve(o).as_i64().ok()).and_then(|n| usize::try_from(n).ok())?;
                    let kind = items.get(1).map(|o| self.resolve(o)).and_then(|o| o.as_name().ok());
                    let operands: [Option<f32>; 4] = std::array::from_fn(|i| items.get(i + 2).and_then(|o| self.resolve(o).as_float().ok()));
                    RemoteDestination::Page { page, view: DestView::from_operands(kind, &operands) }
                }
                Object::Name(name) if name.len() <= MAX_REMOTE_DESTINATION_NAME_BYTES => {
                    let name = String::from_utf8_lossy(name).into_owned();
                    if name.is_empty() || name.len() > MAX_REMOTE_DESTINATION_NAME_BYTES {
                        return None;
                    }
                    RemoteDestination::Named(name)
                }
                Object::String(bytes, _) if bytes.len() <= MAX_REMOTE_DESTINATION_NAME_BYTES => {
                    let name = text_string(bytes);
                    if name.is_empty() || name.len() > MAX_REMOTE_DESTINATION_NAME_BYTES {
                        return None;
                    }
                    RemoteDestination::Named(name)
                }
                _ => return None,
            },
        };
        Some(LinkTarget::RemoteGoTo { launch: false, file, destination, new_window: self.new_window(action) })
    }

    /// The non-empty local file an action's `/F` names (a string or a file specification), shared
    /// by GoToR and Launch. A URL file system (`/FS /URL`) names no local file.
    fn action_file(&self, action: &Dictionary) -> Option<String> {
        if self.name(action, b"FS").as_deref() == Some("URL") {
            return None;
        }
        let file = match self.resolve(action.get(b"F").ok()?) {
            Object::String(bytes, _) => limited_pdf_string(bytes, MAX_REMOTE_FILE_SPEC_BYTES)?,
            Object::Dictionary(spec) => {
                if self.name(spec, b"FS").as_deref() == Some("URL") {
                    return None;
                }
                spec.get(b"UF")
                    .ok()
                    .and_then(|o| self.pdf_text(o, MAX_REMOTE_FILE_SPEC_BYTES))
                    .or_else(|| spec.get(b"F").ok().and_then(|o| self.pdf_text(o, MAX_REMOTE_FILE_SPEC_BYTES)))?
            }
            _ => return None,
        };
        (!file.is_empty()).then_some(file)
    }

    fn new_window(&self, action: &Dictionary) -> Option<bool> {
        match action.get(b"NewWindow").map(|o| self.resolve(o)) {
            Ok(Object::Boolean(value)) => Some(*value),
            _ => None,
        }
    }

    /// A Launch action (ISO 32000-2 §12.6.4.6, Table 207) whose `/F` names a `.pdf` file, read as
    /// the remote Go-To to that file's first page that it amounts to (#4168). Launching anything
    /// else, or with the deprecated `/Win` launch parameters (an application, an operation, its
    /// arguments), is not followed: a document must not start programs. `\` separators, which
    /// Windows writers put in such paths, are read as `/` (§7.11.2.1 separates components with
    /// `/`) when the path has no `/` of its own. Launch has no `/D`, so one present (written
    /// for GoToR, or junk) is ignored rather than checked as a destination.
    fn launch_pdf(&self, action: &Dictionary) -> Option<LinkTarget> {
        if action.has(b"Win") {
            return None;
        }
        let file = self.action_file(action)?;
        let new_window = self.new_window(action);
        let file = if file.contains('/') { file } else { file.replace('\\', "/") };
        let pdf = file.rsplit('/').next().is_some_and(|name| name.len() > 4 && name.to_ascii_lowercase().ends_with(".pdf"));
        pdf.then_some(LinkTarget::RemoteGoTo { launch: true, file, destination: RemoteDestination::Default, new_window })
    }

    fn pdf_text(&self, o: &Object, max_bytes: usize) -> Option<String> {
        match self.resolve(o) {
            Object::String(bytes, _) => limited_pdf_string(bytes, max_bytes),
            _ => None,
        }
    }

    /// A set-OCG-state action: `/State` is `ON`, `OFF` or `Toggle`, each followed by the groups
    /// it applies to; `/PreserveRB` is true unless it is `false`.
    fn layer_state(&self, a: &Dictionary) -> LinkTarget {
        let mut changes = Vec::new();
        if let Ok(Object::Array(items)) = a.get(b"State").map(|o| self.resolve(o)) {
            let mut op = None;
            for item in items.iter().take(MAX_LAYER_STATE) {
                match item {
                    Object::Name(n) => {
                        op = match n.as_slice() {
                            b"ON" => Some(LayerOp::On),
                            b"OFF" => Some(LayerOp::Off),
                            b"Toggle" => Some(LayerOp::Toggle),
                            _ => None,
                        }
                    }
                    Object::Reference(id) => changes.extend(op.map(|op| (op, *id))),
                    _ => {}
                }
            }
        }
        let preserve_rb = !matches!(a.get(b"PreserveRB").map(|o| self.resolve(o)), Ok(Object::Boolean(false)));
        LinkTarget::SetLayers { changes, preserve_rb }
    }

    // ── form fields ─────────────────────────────────────────────────────────────────────────

    fn field(
        &self,
        o: &'a Object,
        parent: Option<(&str, Option<String>, Option<i64>)>,
        out: &mut Vec<Field>,
        seen: &mut HashSet<ObjectId>,
        depth: u32,
    ) {
        if depth > 32 {
            return;
        }
        if let Object::Reference(id) = o
            && !seen.insert(*id)
        {
            return;
        }
        let Some(d) = self.dict(o) else { return };
        let (parent_name, parent_ft, parent_ff) = parent.unwrap_or(("", None, None));
        let partial = self.text(d, b"T");
        let full = match (&partial, parent_name.is_empty()) {
            (Some(t), true) => t.clone(),
            (Some(t), false) => format!("{parent_name}.{t}"),
            (None, _) => parent_name.to_string(),
        };
        let ft = self.name(d, b"FT").or(parent_ft);
        let ff = d.get(b"Ff").ok().and_then(|x| x.as_i64().ok()).or(parent_ff);
        // Non-terminal: has kids that are fields (they carry /T). Widgets-only kids are terminal.
        let kids = match d.get(b"Kids").map(|k| self.resolve(k)) {
            Ok(Object::Array(k)) => k.as_slice(),
            _ => &[],
        };
        let field_kids: Vec<_> = kids.iter().filter(|k| self.dict(k).is_some_and(|kd| kd.get(b"T").is_ok())).collect();
        if !field_kids.is_empty() {
            for k in field_kids {
                self.field(k, Some((&full, ft.clone(), ff)), out, seen, depth + 1);
            }
            return;
        }
        let flags = ff.unwrap_or(0);
        let kind = match ft.as_deref() {
            Some("Tx") => FieldKind::Text,
            Some("Btn") if flags & (1 << 16) != 0 => FieldKind::PushButton,
            Some("Btn") if flags & (1 << 15) != 0 => FieldKind::Radio,
            Some("Btn") => FieldKind::CheckBox,
            Some("Ch") if flags & (1 << 17) != 0 => FieldKind::Combo,
            Some("Ch") => FieldKind::List,
            Some("Sig") => FieldKind::Signature,
            _ => FieldKind::Unknown,
        };
        let value = match d.get(b"V").map(|v| self.resolve(v)) {
            Ok(Object::Name(n)) => Some(String::from_utf8_lossy(n).into_owned()),
            Ok(Object::String(bytes, _)) => Some(text_string(bytes)),
            Ok(Object::Array(a)) => Some(
                a.iter()
                    .filter_map(|x| match self.resolve(x) {
                        Object::String(bytes, _) => Some(text_string(bytes)),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            Ok(Object::Dictionary(_)) if kind == FieldKind::Signature => Some("signed".into()),
            _ => None,
        };
        // The widget is either this dict (merged field/widget) or its first kid.
        let widget = if d.get(b"Rect").is_ok() { Some(d) } else { kids.first().and_then(|k| self.dict(k)) };
        let page = widget.and_then(|w| w.get(b"P").ok()).and_then(|p| self.page_of(p)).or_else(|| self.find_widget_page(o, kids));
        let has_actions = d.get(b"AA").is_ok() || widget.is_some_and(|w| w.get(b"AA").is_ok() || w.get(b"A").is_ok());
        let rect = widget.and_then(|w| w.get(b"Rect").ok()).map(|r| rect4(self.resolve(r)));
        out.push(Field { name: full, kind, value, page, tooltip: self.text(d, b"TU"), has_actions, rect });
    }

    /// The first page whose `/Annots` lists the field or one of its kids.
    fn find_widget_page(&self, field: &Object, kids: &[Object]) -> Option<usize> {
        let pages = self.annotation_pages.get_or_init(|| self.annotation_pages());
        std::iter::once(field).chain(kids).filter_map(|o| if let Object::Reference(id) = o { pages.get(id).copied() } else { None }).min()
    }

    /// Each annotation listed in a page's `/Annots`, with the first page that lists it.
    fn annotation_pages(&self) -> HashMap<ObjectId, usize> {
        let mut pages = HashMap::new();
        for (idx, &pid) in self.page_ids.iter().enumerate() {
            let Some(annots) = self.doc.get_dictionary(pid).ok().and_then(|d| d.get(b"Annots").ok()) else { continue };
            let Ok(annots) = self.resolve(annots).as_array() else { continue };
            for a in annots {
                if let Object::Reference(id) = a {
                    pages.entry(*id).or_insert(idx);
                }
            }
        }
        pages
    }

    /// The topmost field above widget `id` (itself without a `/Parent`), or `None` when it or an
    /// ancestor is already reachable from the `/Fields` tree, or a `/Parent` loop hides the top.
    fn unlisted_field(&self, id: ObjectId, seen: &HashSet<ObjectId>) -> Option<ObjectId> {
        let mut top = id;
        let mut visited = HashSet::from([id]);
        loop {
            let o = Object::Reference(top);
            let d = self.dict(&o)?;
            let parent = match d.get(b"Parent") {
                Ok(Object::Reference(p)) => Some(*p),
                _ => None,
            };
            let Some(p) = parent else {
                return if seen.contains(&top) { None } else { Some(top) };
            };
            if !visited.insert(p) || visited.len() > 64 {
                return None;
            }
            if seen.contains(&p) {
                return None;
            }
            top = p;
        }
    }

    // ── optional content ────────────────────────────────────────────────────────────────────

    fn layers(&self, catalog: &Dictionary, out: &mut Vec<Layer>) {
        let Some(props) = catalog.get(b"OCProperties").ok().and_then(|o| self.dict(o)) else { return };
        let config = props.get(b"D").ok().and_then(|o| self.dict(o));
        let off: HashSet<ObjectId> = config
            .and_then(|c| c.get(b"OFF").ok())
            .and_then(|o| self.resolve(o).as_array().ok())
            .map(|a| a.iter().filter_map(|x| x.as_reference().ok()).collect())
            .unwrap_or_default();
        let base_off = config.and_then(|c| self.name(c, b"BaseState")).as_deref() == Some("OFF");
        let on: HashSet<ObjectId> = config
            .and_then(|c| c.get(b"ON").ok())
            .and_then(|o| self.resolve(o).as_array().ok())
            .map(|a| a.iter().filter_map(|x| x.as_reference().ok()).collect())
            .unwrap_or_default();
        let Ok(Object::Array(ocgs)) = props.get(b"OCGs").map(|o| self.resolve(o)) else { return };
        for g in ocgs {
            let Some(d) = self.dict(g) else { continue };
            let id = g.as_reference().ok();
            let visible = match id {
                Some(id) if off.contains(&id) => false,
                Some(id) if on.contains(&id) => true,
                _ => !base_off,
            };
            let Some(id) = id else { continue };
            out.push(Layer { id, name: self.text(d, b"Name").unwrap_or_else(|| "Layer".into()), visible, initial: visible });
        }
    }

    /// The default configuration's radio-button groups (`/RBGroups`) of two or more layers.
    fn layer_groups(&self, catalog: &Dictionary) -> Vec<Vec<ObjectId>> {
        let groups = catalog
            .get(b"OCProperties")
            .ok()
            .and_then(|o| self.dict(o))
            .and_then(|p| p.get(b"D").ok())
            .and_then(|o| self.dict(o))
            .and_then(|c| c.get(b"RBGroups").ok())
            .and_then(|o| self.resolve(o).as_array().ok());
        let mut left = MAX_LAYER_GROUP_ENTRIES;
        let mut out = Vec::new();
        for g in groups.into_iter().flatten() {
            let Ok(members) = self.resolve(g).as_array() else { continue };
            let group: Vec<ObjectId> = members.iter().filter_map(|x| x.as_reference().ok()).take(left).collect();
            left = left.saturating_sub(group.len());
            if group.len() > 1 {
                out.push(group);
            }
            if left == 0 {
                break;
            }
        }
        out
    }

    fn fonts(&self, out: &mut Vec<FontInfo>) {
        let mut seen = HashSet::new();
        for &pid in &self.page_ids {
            let Ok(fonts) = self.doc.get_page_fonts(pid) else { continue };
            for (_, f) in fonts {
                let base = f
                    .get(b"BaseFont")
                    .ok()
                    .and_then(|o| o.as_name().ok())
                    .map(|n| String::from_utf8_lossy(n).into_owned())
                    .unwrap_or_else(|| "(unnamed)".into());
                let kind = self.name(f, b"Subtype").unwrap_or_default();
                let encoding = match f.get(b"Encoding").map(|o| self.resolve(o)) {
                    Ok(Object::Name(n)) => Some(String::from_utf8_lossy(n).into_owned()),
                    Ok(Object::Dictionary(_)) => Some("Custom".into()),
                    Ok(Object::Stream(_)) => Some("Embedded CMap".into()),
                    _ => None,
                };
                // Embedded: FontFile/2/3 in the descriptor (for Type0, in the descendant font).
                let descriptor_of = |d: &'a Dictionary| d.get(b"FontDescriptor").ok().and_then(|o| self.dict(o));
                let descendant =
                    f.get(b"DescendantFonts").ok().and_then(|o| self.resolve(o).as_array().ok()).and_then(|a| a.first()).and_then(|o| self.dict(o));
                let desc = descriptor_of(f).or_else(|| descendant.and_then(descriptor_of));
                let embedded =
                    kind == "Type3" || desc.is_some_and(|d| d.get(b"FontFile").is_ok() || d.get(b"FontFile2").is_ok() || d.get(b"FontFile3").is_ok());
                // What the substitution chain needs beyond the name: the descriptor flags, and for
                // a composite font the script its descendant declares (ISO 32000-2 9.7.3).
                let flags = desc
                    .and_then(|d| d.get(b"Flags").ok())
                    .and_then(|o| self.resolve(o).as_i64().ok())
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0);
                let ordering =
                    descendant.and_then(|d| d.get(b"CIDSystemInfo").ok()).and_then(|o| self.dict(o)).and_then(|c| self.text(c, b"Ordering"));
                let subset = base.len() > 7 && base.as_bytes()[6] == b'+' && base[..6].bytes().all(|b| b.is_ascii_uppercase());
                let name = if subset { base[7..].to_string() } else { base };
                let chosen = pdfcraft_fonts::fallback::substitute(&pdfcraft_fonts::fallback::Request {
                    base_font: &name,
                    embedded,
                    flags,
                    ordering: ordering.as_deref(),
                });
                let info = FontInfo {
                    name,
                    kind,
                    embedded,
                    subset,
                    encoding,
                    substitute: chosen.face_name(),
                    substitution: (!chosen.why.is_faithful()).then(|| chosen.why.note().to_owned()),
                };
                if seen.insert((info.name.clone(), info.kind.clone(), info.embedded)) {
                    out.push(info);
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// The attachment's name is the file specification's, else `fallback`'s, else a generic one.
    fn attachment(
        &self,
        spec: &Object,
        source: AttachmentSource,
        text_budget: &mut usize,
        fallback: impl FnOnce(&mut usize) -> Option<String>,
    ) -> Attachment {
        let d = self.dict(spec);
        let size =
            d.and_then(|d| d.get(b"EF").ok()).and_then(|o| self.dict(o)).and_then(|ef| ef.get(b"F").ok().or_else(|| ef.get(b"UF").ok())).and_then(
                |f| match self.resolve(f) {
                    Object::Stream(s) => s
                        .dict
                        .get(b"Params")
                        .ok()
                        .and_then(|p| self.dict(p))
                        .and_then(|p| p.get(b"Size").ok())
                        .and_then(|s| s.as_i64().ok())
                        .map(|s| s as usize)
                        .or(Some(s.content.len())),
                    _ => None,
                },
            );
        let display = d
            .and_then(|d| self.budgeted_text(d, b"UF", text_budget).or_else(|| self.budgeted_text(d, b"F", text_budget)))
            .or_else(|| fallback(text_budget))
            .unwrap_or_else(|| "attachment".into());
        Attachment { name: display, description: d.and_then(|d| self.budgeted_text(d, b"Desc", text_budget)), size, source }
    }
}

/// A text string's value, decoded by the same rules as the engine's object model
/// ([`pdfcraft_cos::PdfString::to_text`]) so the panels and outline editing agree.
fn text_string(bytes: &[u8]) -> String {
    pdfcraft_cos::PdfString::literal(bytes).to_text()
}

/// What [`Inspector::name_tree_find`] has visited: node dictionaries, and the key and `/Names`
/// array objects it reached by reference.
#[derive(Default)]
struct NameTreeSeen {
    nodes: HashSet<*const Dictionary>,
    objects: HashSet<ObjectId>,
}

/// A string or name value as text, without surrounding NULs and white space.
fn trimmed_text(o: &Object) -> Option<String> {
    let s = match o {
        Object::String(bytes, _) => text_string(bytes),
        Object::Name(n) => String::from_utf8_lossy(n).into_owned(),
        _ => return None,
    };
    Some(s.trim_matches('\0').trim().to_string())
}

/// `convert(bytes)` when the result fits in `budget`, which it then spends by the longer of the raw
/// and converted lengths. Bytes longer than the budget are not converted. A conversion that comes
/// out too long spends the raw length, so a string many entries share is converted only until the
/// budget runs out.
fn spend(bytes: &[u8], budget: &mut usize, convert: impl FnOnce(&[u8]) -> String) -> Option<String> {
    if bytes.len() > *budget {
        return None;
    }
    let text = convert(bytes);
    if text.len() > *budget {
        *budget = budget.saturating_sub(bytes.len());
        return None;
    }
    // The conversion's work follows the raw length, even when the text it keeps is shorter.
    *budget = budget.saturating_sub(text.len().max(bytes.len()));
    Some(text)
}

fn limited_pdf_string(bytes: &[u8], max_bytes: usize) -> Option<String> {
    if bytes.len() > max_bytes {
        return None;
    }
    let text = text_string(bytes);
    (text.len() <= max_bytes).then_some(text)
}

/// A bookmark title's colour from its `/C` entries, each kept within 0–1 (ISO 32000-2 §12.3.3,
/// Table 151: "An array of three numbers in the range 0.0 to 1.0, representing the components in
/// the DeviceRGB colour space"). Black, the default, and anything that isn't three numbers give
/// `None`.
pub(crate) fn outline_color(values: impl Iterator<Item = Option<f32>>) -> Option<[f32; 3]> {
    let v: Vec<Option<f32>> = values.take(4).collect();
    let [Some(r), Some(g), Some(b)] = v.as_slice() else { return None };
    // A number past the range, however large, is kept at its edge.
    let rgb = [*r, *g, *b].map(|c| c.clamp(0.0, 1.0));
    (rgb != [0.0; 3]).then_some(rgb)
}

/// Nests a pre-order list of `(depth, item)` into the outline tree.
fn nest_outline(listed: Vec<(usize, OutlineItem)>) -> Vec<OutlineItem> {
    let mut roots = Vec::new();
    // The open ancestors, outermost first; each collects its children until it closes.
    let mut open: Vec<(usize, OutlineItem)> = Vec::new();
    for (depth, item) in listed {
        while open.last().is_some_and(|(d, _)| *d >= depth) {
            close_outline(&mut open, &mut roots);
        }
        open.push((depth, item));
    }
    while !open.is_empty() {
        close_outline(&mut open, &mut roots);
    }
    roots
}

/// Closes the innermost open bookmark: it joins its parent's children, or the top level.
fn close_outline(open: &mut Vec<(usize, OutlineItem)>, roots: &mut Vec<OutlineItem>) {
    if let Some((_, item)) = open.pop() {
        match open.last_mut() {
            Some((_, parent)) => parent.children.push(item),
            None => roots.push(item),
        }
    }
}

/// An annotation's `/F` as the renderer reads it: a real is truncated, and a value that isn't a
/// `u32` counts as no flags.
fn draw_flags(o: &Object) -> u32 {
    let f = match o {
        Object::Integer(i) => *i,
        Object::Real(r) => *r as i64,
        _ => 0,
    };
    u32::try_from(f).unwrap_or(0)
}

fn rect4(o: &Object) -> [f32; 4] {
    match o {
        Object::Array(a) if a.len() == 4 => {
            let v: Vec<f32> = a.iter().map(|x| x.as_float().unwrap_or(0.0)).collect();
            [v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])]
        }
        _ => [0.0; 4],
    }
}

fn label_work(left: &mut usize, count: usize) -> Result<(), &'static str> {
    *left = left.checked_sub(count).ok_or("the number tree has too many entries")?;
    Ok(())
}

/// `D:20260930104512-04'00'` → `2026-09-30 10:45`.
/// A PDF date (`D:20261001123000Z`) as "2026-10-01 12:30"; other strings unchanged.
pub fn pretty_date(s: &str) -> String {
    let d = s.trim_start_matches("D:");
    if let Some(d) = d.get(..12).filter(|d| d.bytes().all(|b| b.is_ascii_digit())) {
        // Twelve ASCII digits make every slice below a UTF-8 character boundary.
        format!("{}-{}-{} {}:{}", &d[0..4], &d[4..6], &d[6..8], &d[8..10], &d[10..12])
    } else {
        s.to_string()
    }
}

/// Fetch an attachment's bytes (decoded). Capped at 1 GiB to defuse decompression bombs.
pub fn attachment_data(bytes: &[u8], password: Option<&str>, att: &Attachment) -> Result<Vec<u8>, String> {
    let run = || -> Result<Vec<u8>, String> {
        let options = load_options(password);
        let doc = Document::load_mem_with_options(bytes, options).map_err(|e| e.to_string())?;
        let insp = Inspector::new(&doc);
        let spec: &Object = match &att.source {
            AttachmentSource::Document { key } => {
                let names = doc.catalog().ok().and_then(|c| c.get(b"Names").ok()).and_then(|o| insp.dict(o)).ok_or("no /Names")?;
                let ef = names.get(b"EmbeddedFiles").ok().and_then(|o| insp.dict(o)).ok_or("no /EmbeddedFiles")?;
                insp.name_tree_find(ef, key, &mut NameTreeSeen::default(), 0).ok_or("attachment not found")?
            }
            AttachmentSource::Annotation { page, index } => {
                let (&pid, _) = insp.page_index.iter().find(|(_, i)| **i == *page).ok_or("page not found")?;
                let annots = doc.get_dictionary(pid).map_err(|e| e.to_string())?.get(b"Annots").map_err(|e| e.to_string())?;
                let a = insp.resolve(annots).as_array().map_err(|e| e.to_string())?.get(*index).ok_or("annotation not found")?;
                insp.dict(a).and_then(|d| d.get(b"FS").ok()).ok_or("annotation has no file")?
            }
        };
        let fs = insp.dict(spec).ok_or("bad file specification")?;
        let ef = fs.get(b"EF").ok().and_then(|o| insp.dict(o)).ok_or("the file is not embedded (external reference)")?;
        let stream = ef.get(b"UF").ok().or_else(|| ef.get(b"F").ok()).map(|o| insp.resolve(o)).ok_or("no embedded stream")?;
        let stream = stream.as_stream().map_err(|e| e.to_string())?;
        if stream.dict.get(b"Filter").is_ok() {
            stream.decompressed_content_with_limit(1 << 30).map_err(|e| e.to_string())
        } else {
            Ok(stream.content.clone())
        }
    };
    catch_unwind(AssertUnwindSafe(run)).unwrap_or_else(|p| Err(format!("reading the attachment crashed: {}", crate::raster::panic_message(&p))))
}

/// An XFA packet's bytes as text: UTF-16 (little- or big-endian, told by a byte order mark or by
/// how `<` is written) or UTF-8, as `pdfcraft_xfa::packets::decode` reads them. Lossy: only one
/// element is looked for.
fn xfa_packet_text(bytes: &[u8]) -> String {
    let utf16 = |b: &[u8], from: fn([u8; 2]) -> u16| {
        let units = b.as_chunks::<2>().0.iter().map(|c| from(*c));
        char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect::<String>()
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        [b'<', 0, ..] => utf16(bytes, u16::from_le_bytes),
        [0, b'<', ..] => utf16(bytes, u16::from_be_bytes),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// One markup tag found by [`next_tag`]: where it starts (`<`) and ends (after `>`), whether it
/// closes (`</x>`) or closes itself (`<x/>`), and its name without a namespace prefix.
struct Tag<'a> {
    start: usize,
    end: usize,
    close: bool,
    empty: bool,
    local: &'a str,
}

/// The next element tag in `xml` at or after `from`, skipping comments, CDATA sections,
/// processing instructions and declarations. A byte scan, not an XML parser: enough to find one
/// known element in an XFA packet, whose tags Designer writes with a line break before each `>`.
fn next_tag(xml: &str, mut from: usize) -> Option<Tag<'_>> {
    loop {
        let lt = from + xml.get(from..)?.find('<')?;
        let rest = xml.get(lt..)?;
        let skip_to = |end: &str| rest.find(end).map(|i| lt + i + end.len());
        if rest.starts_with("<!--") {
            from = skip_to("-->")?;
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            from = skip_to("]]>")?;
            continue;
        }
        if rest.starts_with("<?") || rest.starts_with("<!") {
            from = skip_to(">")?;
            continue;
        }
        let gt = lt + rest.find('>')?;
        let inner = xml.get(lt + 1..gt)?;
        let (close, inner) = match inner.strip_prefix('/') {
            Some(i) => (true, i),
            None => (false, inner),
        };
        let empty = !close && inner.ends_with('/');
        let name = inner.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or_default();
        let local = name.rsplit(':').next().unwrap_or(name);
        return Some(Tag { start: lt, end: gt + 1, close, empty, local });
    }
}

/// What `<name>…</name>` holds: the content of the first top-level element of `xml` called `name`
/// (any prefix). Elements nested in other elements are not looked at. `None` when there is none,
/// it closes itself, or it is never closed.
fn xml_child_body<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let mut at = 0;
    // The elements open around `at`, innermost last: a closing tag must name the innermost one,
    // or the markup is malformed and has no value.
    let mut open_tags: Vec<&str> = Vec::new();
    // Where the wanted element's content starts, once its opening tag is seen at the top level.
    let mut open = None;
    loop {
        let tag = next_tag(xml, at)?;
        at = tag.end;
        if tag.close {
            if open_tags.pop()? != tag.local {
                return None;
            }
            if open_tags.is_empty()
                && let Some(start) = open
            {
                return xml.get(start..tag.start);
            }
        } else if !tag.empty {
            if open_tags.is_empty() && tag.local == name {
                open = Some(tag.end);
            }
            open_tags.push(tag.local);
        } else if open_tags.is_empty() && tag.local == name {
            return None;
        }
    }
}

/// The trimmed text at `path` (element names from the outside in, any prefix, each a child of the
/// one before) in `xml`, if it is plain text.
fn xml_path_text(xml: &str, path: &[&str]) -> Option<String> {
    let body = path.iter().try_fold(xml, |within, name| xml_child_body(within, name))?;
    let text = body.trim();
    (!text.is_empty() && !text.contains('<')).then(|| text.to_owned())
}

/// `dynamicRender` at `config/acrobat/acrobat7` in the configuration packet: the `config` child
/// of the `xdp` root in a whole-XDP stream, or the `config` packet itself. The same names inside
/// the form data or the template are not the configuration.
fn xfa_config_dynamic_render(packet: &str) -> Option<String> {
    const IN_CONFIG: [&str; 4] = ["config", "acrobat", "acrobat7", "dynamicRender"];
    xml_path_text(packet, &["xdp", IN_CONFIG[0], IN_CONFIG[1], IN_CONFIG[2], IN_CONFIG[3]]).or_else(|| xml_path_text(packet, &IN_CONFIG))
}

#[cfg(test)]
mod tests;

/// "1.7" from the parser's version name ("Pdf17", "V1_7", …).
fn version_label(name: &str) -> String {
    let digits: Vec<char> = name.chars().filter(char::is_ascii_digit).collect();
    match digits[..] {
        [a, b] => format!("{a}.{b}"),
        _ => name.to_owned(),
    }
}

#[cfg(test)]
mod version_tests {
    #[test]
    fn version_labels_read_as_numbers() {
        assert_eq!(super::version_label("Pdf17"), "1.7");
        assert_eq!(super::version_label("V2_0"), "2.0");
        assert_eq!(super::version_label("Unknown"), "Unknown");
    }
}

#[cfg(test)]
mod substitution_tests {
    use std::sync::Arc;

    use super::{FontInfo, inspect};

    /// One page naming four fonts the document does not embed: a Chinese CID font (Adobe-GB1), a
    /// Japanese one (Adobe-Japan1), an unknown serif-flagged simple font, and an aliased
    /// standard-14 name.
    const NOT_EMBEDDED: &[u8] = b"%PDF-1.7
1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj
2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj
3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R /F2 6 0 R /F3 8 0 R /F4 9 0 R /F5 11 0 R >> >> >> endobj
4 0 obj << /Type /Font /Subtype /Type0 /BaseFont /STSong-Light /Encoding /UniGB-UCS2-H /DescendantFonts [5 0 R] >> endobj
5 0 obj << /Type /Font /Subtype /CIDFontType0 /BaseFont /STSong-Light /CIDSystemInfo << /Registry (Adobe) /Ordering (GB1) /Supplement 2 >> >> endobj
6 0 obj << /Type /Font /Subtype /Type0 /BaseFont /KozMinPro-Regular /Encoding /UniJIS-UCS2-H /DescendantFonts [7 0 R] >> endobj
7 0 obj << /Type /Font /Subtype /CIDFontType0 /BaseFont /KozMinPro-Regular /CIDSystemInfo << /Registry (Adobe) /Ordering (Japan1) /Supplement 4 >> >> endobj
8 0 obj << /Type /Font /Subtype /TrueType /BaseFont /SomeHouseSerif /FontDescriptor 10 0 R >> endobj
9 0 obj << /Type /Font /Subtype /TrueType /BaseFont /ArialMT >> endobj
10 0 obj << /Type /FontDescriptor /FontName /SomeHouseSerif /Flags 34 >> endobj
11 0 obj << /Type /Font /Subtype /TrueType /BaseFont /HouseGrotesk /FontDescriptor 12 0 R >> endobj
12 0 obj << /Type /FontDescriptor /FontName /HouseGrotesk /Flags 33 >> endobj
trailer << /Size 13 /Root 1 0 R >>
%%EOF";

    fn font<'a>(fonts: &'a [FontInfo], name: &str) -> &'a FontInfo {
        fonts.iter().find(|f| f.name == name).unwrap_or_else(|| panic!("{name} listed; got {:?}", fonts.iter().map(|f| &f.name).collect::<Vec<_>>()))
    }

    #[test]
    fn doc_info_says_which_face_stands_in_and_why() {
        let info = inspect(Arc::new(NOT_EMBEDDED.to_vec()), None).expect("opens");

        // The one that matters most to a user: a Chinese PDF that draws nothing. Before this,
        // doc_info said only `embedded: false` and left them to guess.
        let cjk = font(&info.fonts, "STSong-Light");
        assert!(!cjk.embedded);
        assert_eq!(cjk.substitute, None, "no licensed Chinese face, so nothing stands in");
        let why = cjk.substitution.as_deref().unwrap_or_default();
        assert!(why.contains("no licensed face") && why.contains("rather than the wrong glyphs"), "{why}");

        // An aliased standard-14 name resolves to the real face, whose metrics are exact.
        let arial = font(&info.fonts, "ArialMT");
        assert_eq!(arial.substitute.as_deref(), Some("Helvetica"));
        assert!(arial.substitution.as_deref().unwrap_or_default().contains("standard 14"));

        // A house font whose name merely contains "Serif" gets Times -- reported as the guess it
        // is, not as a standard-14 name, which would be untrue of a font nobody has heard of.
        let serif = font(&info.fonts, "SomeHouseSerif");
        assert_eq!(serif.substitute.as_deref(), Some("Times-Roman"));
        assert!(serif.substitution.as_deref().unwrap_or_default().contains("hints at one"), "{serif:?}");

        // No hint in the name at all, so the descriptor flags decide: 33 = fixed pitch + nonsymbolic.
        let grotesk = font(&info.fonts, "HouseGrotesk");
        assert_eq!(grotesk.substitute.as_deref(), Some("Courier"));
        assert!(grotesk.substitution.as_deref().unwrap_or_default().contains("descriptor flags"), "{grotesk:?}");

        // Japanese depends on the optional craft-fonts build input; either way it is reported,
        // and never as a Latin face.
        let jpan = font(&info.fonts, "KozMinPro-Regular");
        assert!(jpan.substitution.is_some(), "a substitution is always explained");
        match &jpan.substitute {
            Some(face) => {
                assert!(!face.starts_with("Helvetica") && !face.starts_with("Times"), "a Latin face is never a Japanese substitute: {face}")
            }
            None => assert!(jpan.substitution.as_deref().unwrap_or_default().contains("no licensed face")),
        }

        // Nothing claims to be embedded, so every font here carries a reason.
        assert!(info.fonts.iter().all(|f| !f.embedded && f.substitution.is_some()), "{:?}", info.fonts);
    }

    /// An embedded font is reported as faithful: nothing stands in, and there is nothing to explain.
    const EMBEDDED: &[u8] = b"%PDF-1.7
1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj
2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj
3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> >> >> endobj
4 0 obj << /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+SomeHouseSans /FontDescriptor 5 0 R >> endobj
5 0 obj << /Type /FontDescriptor /FontName /ABCDEF+SomeHouseSans /Flags 32 /FontFile2 6 0 R >> endobj
6 0 obj << /Length 4 >> stream
test
endstream endobj
trailer << /Size 7 /Root 1 0 R >>
%%EOF";

    #[test]
    fn an_embedded_font_reports_no_substitution_at_all() {
        let info = inspect(Arc::new(EMBEDDED.to_vec()), None).expect("opens");
        let f = font(&info.fonts, "SomeHouseSans");
        assert!(f.embedded && f.subset, "{f:?}");
        assert_eq!((f.substitute.as_deref(), f.substitution.as_deref()), (None, None), "the document's own glyphs are shown");
    }
}

#[cfg(test)]
mod open_action_javascript_tests {
    use std::sync::Arc;

    use super::inspect;

    /// One-page PDF whose catalog points its /OpenAction at object 4 (written by the caller).
    fn pdf_with_open_action(action: &[u8]) -> Vec<u8> {
        let objs: Vec<&[u8]> = vec![
            b"<< /Type /Catalog /Pages 2 0 R /OpenAction 4 0 R >>",
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
            action,
        ];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(o);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        out
    }

    #[test]
    fn open_action_javascript_sets_has_javascript() {
        let bytes = pdf_with_open_action(b"<< /S /JavaScript /JS (app.alert('hi')) >>");
        let info = inspect(Arc::new(bytes), None).expect("inspects");
        assert!(info.has_javascript, "an /OpenAction /JavaScript is JavaScript");
    }

    #[test]
    fn open_action_goto_does_not_set_has_javascript() {
        let bytes = pdf_with_open_action(b"<< /S /GoTo /D [3 0 R /Fit] >>");
        let info = inspect(Arc::new(bytes), None).expect("inspects");
        assert!(!info.has_javascript, "a /GoTo /OpenAction is not JavaScript");
    }
}

#[cfg(test)]
mod open_dest_tests {
    use std::sync::Arc;

    use super::{DestView, inspect};

    /// Three 612×792 pages (objects 3, 4, 5); `catalog` is added to the catalog dictionary and
    /// `extra` become objects 6, 7, ….
    fn pdf(catalog: &str, extra: &[String]) -> Vec<u8> {
        let mut objs =
            vec![format!("<< /Type /Catalog /Pages 2 0 R {catalog} >>"), "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>".to_string()];
        for _ in 0..3 {
            objs.push("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_string());
        }
        objs.extend(extra.iter().cloned());
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        out
    }

    fn open_dest(catalog: &str, extra: &[String]) -> Option<(usize, DestView)> {
        inspect(Arc::new(pdf(catalog, extra)), None).expect("inspects").open_dest
    }

    const XYZ: DestView = DestView::Xyz { left: Some(100.0), top: Some(500.0), zoom: Some(2.0) };

    #[test]
    fn an_explicit_destination_opens_at_its_page_and_position() {
        assert_eq!(open_dest("/OpenAction [5 0 R /XYZ 100 500 2]", &[]), Some((2, XYZ)));
        assert_eq!(open_dest("/OpenAction [4 0 R /FitH 400]", &[]), Some((1, DestView::FitH { top: Some(400.0) })));
        // Indirect, and a zoom of 0 keeps the zoom (Table 149).
        assert_eq!(
            open_dest("/OpenAction 6 0 R", &["[4 0 R /XYZ 0 792 0]".into()]),
            Some((1, DestView::Xyz { left: Some(0.0), top: Some(792.0), zoom: None }))
        );
    }

    #[test]
    fn a_go_to_action_opens_at_its_destination_explicit_or_named() {
        assert_eq!(open_dest("/OpenAction << /S /GoTo /D [5 0 R /XYZ 100 500 2] >>", &[]), Some((2, XYZ)));
        // A name, through the PDF 1.1 catalog /Dests dictionary (§12.3.2.4).
        assert_eq!(open_dest("/OpenAction << /S /GoTo /D /there >> /Dests << /there [5 0 R /XYZ 100 500 2] >>", &[]), Some((2, XYZ)));
        // A byte string, through the /Names /Dests name tree, whose value is a dictionary with /D.
        assert_eq!(
            open_dest(
                "/OpenAction << /S /GoTo /D (there) >> /Names << /Dests 6 0 R >>",
                &["<< /Kids [7 0 R] >>".into(), "<< /Names [(here) [3 0 R /Fit] (there) << /D [5 0 R /XYZ 100 500 2] >>] >>".into()]
            ),
            Some((2, XYZ))
        );
    }

    #[test]
    fn an_open_action_given_as_a_name_or_string_is_looked_up() {
        assert_eq!(open_dest("/OpenAction /there /Dests << /there [4 0 R /Fit] >>", &[]), Some((1, DestView::Fit)));
        assert_eq!(
            open_dest("/OpenAction (there) /Names << /Dests << /Names [(there) [5 0 R /FitV 30]] >> >>", &[]),
            Some((2, DestView::FitV { left: Some(30.0) }))
        );
    }

    #[test]
    fn an_open_action_that_goes_nowhere_here_has_no_destination() {
        for catalog in [
            "",
            "/OpenAction << /S /JavaScript /JS (app.alert(1)) >>",
            // A remote go-to's /D is a page of another file.
            "/OpenAction << /S /GoToR /F (other.pdf) /D [1 /Fit] >>",
            "/OpenAction << /D [4 0 R /Fit] >>",
            // An unknown name, and no name dictionaries at all.
            "/OpenAction << /S /GoTo /D /missing >> /Dests << /there [4 0 R /Fit] >>",
            "/OpenAction /missing",
            // A page number past the end, and a page reference that is no page.
            "/OpenAction [7 /Fit]",
            "/OpenAction [2 0 R /Fit]",
            "/OpenAction 42",
        ] {
            assert_eq!(open_dest(catalog, &[]), None, "{catalog}");
        }
    }

    #[test]
    fn cyclic_and_deep_named_destinations_end() {
        // Names that refer to each other.
        assert_eq!(open_dest("/OpenAction /a /Dests << /a /b /b /a >>", &[]), None);
        assert_eq!(open_dest("/OpenAction /a /Dests << /a << /D /a >> >>", &[]), None);
        // A name tree whose node is its own kid.
        assert_eq!(open_dest("/OpenAction (x) /Names << /Dests 6 0 R >>", &["<< /Kids [6 0 R] >>".into()]), None);
        // A name tree deeper than the depth bound: the leaf past it isn't read.
        let depth = 40;
        let mut nodes: Vec<String> = (0..depth).map(|i| format!("<< /Kids [{} 0 R] >>", 7 + i)).collect();
        nodes.push("<< /Names [(x) [4 0 R /Fit]] >>".into());
        assert_eq!(open_dest("/OpenAction (x) /Names << /Dests 6 0 R >>", &nodes), None);
        // A shallow one is read.
        let shallow = ["<< /Kids [7 0 R] >>".to_string(), "<< /Kids [8 0 R] >>".into(), "<< /Names [(x) [4 0 R /Fit]] >>".into()];
        assert_eq!(open_dest("/OpenAction (x) /Names << /Dests 6 0 R >>", &shallow), Some((1, DestView::Fit)));
    }
}
