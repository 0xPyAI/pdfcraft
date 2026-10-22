//! The pointer as the pages see it, read once per frame from the page view's content widget.
//!
//! egui knows what is drawn over the pages (a dialog, a menu, the palette, the quick bar, the
//! chrome over a page scrolled under it) and keeps a press with the widget it began on until
//! release. egui's raw pointer state knows neither, and its hover position is gone on the frame
//! a touch or a Windows pen lifts: egui-winit sends `PointerGone` with every touch release, and
//! winit reports Windows pen input as touch. So page modules read [`PagePointer`], never
//! `hover_pos`, `latest_pos` or `interact_pos`.

use egui::{Event, PointerButton, Pos2, Rect};

/// A primary-button press or release, and where it happened.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Edge {
    Press(Pos2),
    Release(Pos2),
}

/// How many of a frame's presses and releases are kept. A frame holds one or two (a click, a
/// double-click in a slow frame), but one after a stall can hold any number.
const MAX_EDGES: usize = 8;

/// One frame's pointer, as the pages see it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PagePointer {
    /// Where the pointer is, while the pages are what egui has under it, or while they hold a
    /// press (until the frame of its release, past a page's edge included). It is egui's
    /// `interact_pos`, which still has the position on the frame a touch lifts.
    pub pos: Option<Pos2>,
    /// Where the press the pages hold began, also on the frame of its release when the press
    /// came in that same frame (egui has forgotten it by then). `None` for a press that began on
    /// something else.
    pub origin: Option<Pos2>,
    /// This frame's primary presses and releases of a press the pages hold, in order.
    pub edges: Vec<Edge>,
}

impl PagePointer {
    /// The pointer for `resp`, the content widget that holds every page, allocated in `ui`.
    pub(crate) fn read(ui: &egui::Ui, resp: &egui::Response) -> Self {
        let (at, origin, edges) = ui.input(|i| (i.pointer.interact_pos(), i.pointer.press_origin(), edges(&i.events)));
        // egui's own answer to "do the pages hold a press?": from the press on until the frame
        // of its click, release or drag stop. A drag can outlive egui's press while the
        // pointer is outside the window.
        let held = resp.dragged() || resp.interact_pointer_pos().is_some();
        // `contains_pointer` alone also holds a few points under the edge of an Area over the
        // pages (egui's interact radius), so the top layer there must be the pages' own.
        let layer = ui.layer_id();
        let on_top = |p: &Pos2| resp.contains_pointer() && ui.ctx().layer_id_at(*p).is_none_or(|l| l == layer);
        let pos = at.filter(|p| held || on_top(p));
        if !held {
            return Self { pos, origin: None, edges: Vec::new() };
        }
        Self { pos, origin: origin.or_else(|| first_press(&edges)), edges }
    }

    /// Where the pointer is, when it is inside `r`.
    pub(crate) fn within(&self, r: Rect) -> Option<Pos2> {
        self.pos.filter(|p| r.contains(*p))
    }

    /// Where this frame's first primary press was.
    pub(crate) fn pressed(&self) -> Option<Pos2> {
        first_press(&self.edges)
    }
}

fn first_press(edges: &[Edge]) -> Option<Pos2> {
    edges.iter().find_map(|e| match e {
        Edge::Press(p) => Some(*p),
        Edge::Release(_) => None,
    })
}

/// The first [`MAX_EDGES`] primary presses and releases among `events`, in order.
fn edges(events: &[Event]) -> Vec<Edge> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::PointerButton { pos, button: PointerButton::Primary, pressed, .. } if pos.is_finite() => {
                Some(if *pressed { Edge::Press(*pos) } else { Edge::Release(*pos) })
            }
            _ => None,
        })
        .take(MAX_EDGES)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Context, Modifiers, Sense, TouchDeviceId, TouchId, TouchPhase, pos2, vec2};

    /// The pages' widget: x 0..200, y 0..200 of a 400×400 screen.
    const PAGES: Rect = Rect { min: Pos2::ZERO, max: Pos2 { x: 200.0, y: 200.0 } };
    /// An Area drawn over the pages' lower right corner.
    const OVER: Rect = Rect { min: Pos2 { x: 120.0, y: 120.0 }, max: Pos2 { x: 260.0, y: 260.0 } };

    fn button(pos: Pos2, button: PointerButton, pressed: bool) -> Event {
        Event::PointerButton { pos, button, pressed, modifiers: Modifiers::NONE }
    }

    fn touch(phase: TouchPhase, pos: Pos2) -> Event {
        Event::Touch { device_id: TouchDeviceId(1), id: TouchId(1), phase, pos, force: None }
    }

    /// Runs one frame of the pages' widget (and the Area over it, with `area`) and returns what
    /// the pages see.
    fn frame(ctx: &Context, events: Vec<Event>, area: bool) -> PagePointer {
        let mut seen = PagePointer::default();
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 400.0))), events, ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| {
            let (_, resp) = ui.allocate_exact_size(PAGES.size(), Sense::click_and_drag());
            seen = PagePointer::read(ui, &resp);
            if area {
                egui::Area::new(egui::Id::new("over")).fixed_pos(OVER.min).show(ui.ctx(), |ui| {
                    ui.allocate_exact_size(OVER.size(), Sense::hover());
                });
            }
        });
        // An input-only test has no renderer to take the font texture.
        output.textures_delta.clear();
        seen
    }

    #[test]
    fn a_touch_tap_keeps_its_position_on_the_frame_it_lifts() {
        let ctx = Context::default();
        let p = pos2(50.0, 60.0);
        frame(&ctx, vec![], false);
        let down = frame(&ctx, vec![touch(TouchPhase::Start, p), Event::PointerMoved(p), button(p, PointerButton::Primary, true)], false);
        assert_eq!((down.pos, down.origin, down.pressed()), (Some(p), Some(p), Some(p)));
        // egui-winit ends a touch with the release and `PointerGone` in one frame.
        let up = frame(&ctx, vec![touch(TouchPhase::End, p), button(p, PointerButton::Primary, false), Event::PointerGone], false);
        assert_eq!(ctx.input(|i| i.pointer.hover_pos()), None, "egui's hover position is gone");
        assert_eq!(up.pos, Some(p), "the pages still know where the finger lifted");
        assert_eq!(up.edges, [Edge::Release(p)]);
        assert_eq!(frame(&ctx, vec![], false), PagePointer::default(), "and forget it the frame after");
    }

    #[test]
    fn a_press_and_release_in_one_frame_keep_the_press() {
        let ctx = Context::default();
        let p = pos2(50.0, 60.0);
        frame(&ctx, vec![Event::PointerMoved(p)], false);
        let click = frame(&ctx, vec![button(p, PointerButton::Primary, true), button(p, PointerButton::Primary, false)], false);
        assert_eq!(ctx.input(|i| i.pointer.press_origin()), None, "egui has forgotten the press");
        assert_eq!(click.origin, Some(p));
        assert_eq!(click.edges, [Edge::Press(p), Edge::Release(p)]);
    }

    #[test]
    fn something_drawn_over_the_pages_hides_the_pointer() {
        let ctx = Context::default();
        // The Area and the widget rects egui hit-tests against come from the frame before.
        frame(&ctx, vec![], true);
        frame(&ctx, vec![], true);
        let beside = pos2(100.0, 100.0);
        assert_eq!(frame(&ctx, vec![Event::PointerMoved(beside)], true).pos, Some(beside));
        // Inside the Area, and just inside its edge where egui's interact radius still finds the
        // pages' widget.
        for p in [pos2(160.0, 160.0), pos2(122.0, 150.0)] {
            assert_eq!(frame(&ctx, vec![Event::PointerMoved(p)], true).pos, None, "{p:?}");
        }
        // Off the pages altogether.
        assert_eq!(frame(&ctx, vec![Event::PointerMoved(pos2(300.0, 50.0))], true).pos, None);
        // A press on the Area is not the pages' press.
        let p = pos2(160.0, 160.0);
        let press = frame(&ctx, vec![button(p, PointerButton::Primary, true)], true);
        assert_eq!(press, PagePointer::default());
    }

    #[test]
    fn a_press_on_the_pages_keeps_the_pointer_until_its_release() {
        let ctx = Context::default();
        frame(&ctx, vec![], true);
        frame(&ctx, vec![], true);
        let start = pos2(40.0, 40.0);
        frame(&ctx, vec![Event::PointerMoved(start), button(start, PointerButton::Primary, true)], true);
        // Dragged over the Area and off the pages: still theirs.
        for p in [pos2(160.0, 160.0), pos2(300.0, 50.0)] {
            let held = frame(&ctx, vec![Event::PointerMoved(p)], true);
            assert_eq!((held.pos, held.origin), (Some(p), Some(start)), "{p:?}");
        }
        let end = pos2(300.0, 60.0);
        let up = frame(&ctx, vec![Event::PointerMoved(end), button(end, PointerButton::Primary, false)], true);
        assert_eq!((up.pos, up.edges.as_slice()), (Some(end), [Edge::Release(end)].as_slice()), "the release is the pages' too");
        assert_eq!(frame(&ctx, vec![], true).pos, None, "then off the pages, the pointer is not theirs");
    }

    #[test]
    fn edges_are_primary_finite_in_order_and_capped() {
        let p = pos2(1.0, 2.0);
        let q = pos2(3.0, 4.0);
        let events = vec![
            button(p, PointerButton::Secondary, true),
            button(p, PointerButton::Primary, true),
            Event::PointerMoved(q),
            button(pos2(f32::NAN, 0.0), PointerButton::Primary, false),
            button(q, PointerButton::Primary, false),
        ];
        assert_eq!(edges(&events), [Edge::Press(p), Edge::Release(q)]);
        let many: Vec<Event> = (0..100).map(|i| button(p, PointerButton::Primary, i % 2 == 0)).collect();
        assert_eq!(edges(&many).len(), MAX_EDGES);
        assert_eq!(first_press(&[Edge::Release(q), Edge::Press(p), Edge::Press(q)]), Some(p));
    }
}
