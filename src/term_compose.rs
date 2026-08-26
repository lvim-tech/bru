//! C5: laying the surfaces CEF paints — the page, the two chrome strips, the panel, and the
//! popup layer — into one picture. Pure buffer work.
//!
//! **This module touches no CEF type, no terminal and no file descriptor, and that is deliberate.**
//! Everything above it is untestable without a browser and a kitty: `on_paint` fires when Chromium
//! feels like it, the pane's size comes out of an escape-sequence round trip, and a wrong number
//! anywhere in that chain shows up as a picture that looks slightly off. What *is* testable is the
//! arithmetic — where each surface goes, which bytes move, and which of them are allowed to move —
//! so it lives here on its own, with the buffers small enough to check by hand.
//!
//! It is also the one module in the terminal path where a mistake is memory corruption rather than
//! a wrong picture. `on_paint` hands over a raw pointer whose length is *not* in its type (see
//! `term_frontend.rs`, where the `unsafe` slice is built and commented); by the time anything reaches
//! here the length has been committed to, and [`Surface::new`] refuses a buffer that does not match
//! its dimensions rather than trusting the caller twice. Below that every copy goes through
//! `get`/`get_mut` on a slice: there is no `unsafe` in this file and there is no reason for one.
//!
//! ## What the compositor is asked to produce
//!
//! One RGB buffer the size of the pane, kept across frames, plus the rectangle of it that changed.
//!
//! - **RGB and not RGBA**, because that is what the presenter sends: kitty is told `f=24`, three
//!   bytes to the pixel, and CEF hands over BGRA. The swizzle happens exactly once, inside the row
//!   copy, and it is written down where it happens — a red/blue swap is the classic bug of this
//!   path and it is cheaper to name it than to look at it.
//! - **Kept across frames**, because CEF only repaints what changed. A frame in which nothing but
//!   the status bar moved copies twenty-four rows, not the pane.
//! - **Plus the damage rectangle**, because that is what lets the presenter send a strip instead of
//!   a screenful. [`compose`] answers the union, in output coordinates, of everything it actually
//!   wrote — not of what it was asked to write, which after clipping can be less or nothing at all.
//!
//! ## The four surfaces, and the fifth
//!
//! The Views frontend gives a window a tab strip, a page, a completion panel and a status bar, in
//! that order down the window (`window.rs`). The terminal frontend has the same four, as four
//! windowless browsers loading the same three `bru://chrome/` documents, and [`layout`] is where
//! their heights become pixel rectangles in the pane.
//!
//! The fifth is not a browser. A `<select>` dropdown arrives through `on_paint` with
//! `PET_POPUP` and its own rectangle, and if nobody composites it the menu does not exist anywhere
//! on screen — it is not drawn into the page's surface, it is a second surface CEF expects the host
//! to put on top. [`popup_layer`] places it, in the page's coordinates, clipped to the page.

// **The whole module is dead until C5 attaches it to a browser, and that is the phase it is waiting
// for.** Every item here is written against the shape `on_paint` hands over — the surfaces, the
// layout, the damage — and none of it has a caller yet; the tests are the only ones. Rather than
// thirty `#[allow(dead_code)]` attributes saying the same sentence thirty times, it is said once,
// here, and comes off in one line when the OSR host is wired up.
#![allow(dead_code)]

/// The tab strip's height, in logical pixels.
///
/// A copy of `window.rs`'s `TOP_HEIGHT`, which is private to it. It is copied rather than shared
/// because the two frontends are peers: the Views window is not this module's owner, and a term
/// pane that wanted a different strip height would change it here without touching the window.
/// They are the same number today because the same chrome document is laid out in both.
pub const TOP_HEIGHT: i32 = 40;

/// The status bar's height, in logical pixels. `window.rs`'s `BOTTOM_HEIGHT`; see [`TOP_HEIGHT`].
pub const BOTTOM_HEIGHT: i32 = 24;

/// The strip between the page and a docked inspector, in logical pixels.
///
/// Unlike the two above, this one *is* public in `window.rs`, so the copy is checked against it at
/// compile time instead of being kept in step by hand. Twelve pixels of which the eye sees two —
/// the other ten are the drag's headroom, and `window.rs::DIVIDER_HEIGHT` explains why.
pub const DIVIDER_HEIGHT: i32 = 12;
const _: () = assert!(DIVIDER_HEIGHT == crate::window::DIVIDER_HEIGHT);

/// The largest pane edge, in device pixels, that this module will lay out or allocate for.
///
/// **This is a guard against an answer, not against a screen.** The pane's size arrives from a
/// `CSI 14t` round trip or an ioctl, and a terminal that answers wrongly — or a parse that reads
/// the wrong field — produces a number, not an error. 65536 is eight times the widest display
/// anyone owns and small enough that every `width * height * 4` below stays inside `i32` and every
/// allocation stays inside a machine's memory.
const MAX_EDGE: i32 = 1 << 16;

/// The largest frame this module will allocate, in bytes: 256 MiB, which at three bytes to the
/// pixel is a pane of about 9500x9500. Past that [`Frame::new`] answers `None` rather than asking
/// the allocator for something a bad size query invented.
const MAX_FRAME_BYTES: usize = 256 << 20;

/// An axis-aligned rectangle in pixels, in whichever space the thing holding it says.
///
/// The field names are CEF's `Rect`'s, so the conversion at the boundary is four assignments and
/// no thinking; the type is not CEF's because nothing in this file should need CEF to be tested.
///
/// A rectangle with a width or a height of zero or less is **empty**, and every operation here
/// treats every empty rectangle as the same nothing: an empty rectangle intersects to nothing,
/// unions to the other operand, and paints nothing. That is why [`Rect::ZERO`] can stand for "no
/// rectangle" without an `Option` around it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    /// The empty rectangle at the origin — what every operation that finds nothing answers.
    pub const ZERO: Rect = Rect { x: 0, y: 0, width: 0, height: 0 };

    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect { x, y, width, height }
    }

    /// A rectangle of this size at the origin.
    pub const fn of_size(width: i32, height: i32) -> Rect {
        Rect { x: 0, y: 0, width, height }
    }

    /// Nothing to paint, and nothing to clip against.
    pub const fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    /// One past the last column. Saturating, because a rectangle that came from outside — a dirty
    /// rect, a popup rect — is allowed to be nonsense, and an overflow panic in a paint callback is
    /// a dead browser rather than a skipped rectangle.
    pub const fn right(&self) -> i32 {
        self.x.saturating_add(self.width)
    }

    /// One past the last row. See [`Rect::right`].
    pub const fn bottom(&self) -> i32 {
        self.y.saturating_add(self.height)
    }

    /// The overlap, or [`Rect::ZERO`] when there is none.
    ///
    /// The edges are computed in `i64` and clamped back: two rectangles far apart in opposite
    /// directions have a difference that does not fit in `i32`, and that is reachable from a
    /// garbage dirty rect without anything else being wrong.
    pub fn intersect(&self, other: &Rect) -> Rect {
        if self.is_empty() || other.is_empty() {
            return Rect::ZERO;
        }
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = i64::from(self.right().min(other.right()));
        let bottom = i64::from(self.bottom().min(other.bottom()));
        let width = (right - i64::from(x)).clamp(0, i64::from(i32::MAX)) as i32;
        let height = (bottom - i64::from(y)).clamp(0, i64::from(i32::MAX)) as i32;
        if width == 0 || height == 0 {
            return Rect::ZERO;
        }
        Rect::new(x, y, width, height)
    }

    /// The smallest rectangle holding both. An empty operand contributes nothing, so this is also
    /// the accumulator [`union_all`] folds with.
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return if other.is_empty() { Rect::ZERO } else { *other };
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = i64::from(self.right().max(other.right()));
        let bottom = i64::from(self.bottom().max(other.bottom()));
        let width = (right - i64::from(x)).clamp(0, i64::from(i32::MAX)) as i32;
        let height = (bottom - i64::from(y)).clamp(0, i64::from(i32::MAX)) as i32;
        Rect::new(x, y, width, height)
    }

    /// The same rectangle moved. Saturating, for the reason [`Rect::right`] gives.
    pub const fn translate(&self, dx: i32, dy: i32) -> Rect {
        Rect {
            x: self.x.saturating_add(dx),
            y: self.y.saturating_add(dy),
            width: self.width,
            height: self.height,
        }
    }

    /// Whether every pixel of `other` is inside this one. An empty `other` is contained by
    /// anything, including nothing — it is no pixels.
    pub fn contains(&self, other: &Rect) -> bool {
        if other.is_empty() {
            return true;
        }
        !self.is_empty()
            && other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }
}

/// The union of a list of rectangles, or `None` when every one of them is empty.
///
/// `None` and not [`Rect::ZERO`], because at this level the difference matters: a frame in which
/// nothing changed must not be presented at all, and a zero-sized rectangle at the origin is
/// indistinguishable from one that was clipped away.
pub fn union_all(rects: &[Rect]) -> Option<Rect> {
    let mut answer: Option<Rect> = None;
    for rect in rects {
        if rect.is_empty() {
            continue;
        }
        answer = Some(match answer {
            Some(so_far) => so_far.union(rect),
            None => *rect,
        });
    }
    answer
}

/// Which of the pane's rows a rectangle belongs to. The order of the variants is the order down
/// the pane, which is also the z-order from the bottom up for everything except the popup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceKind {
    /// `bru://chrome/top.html` — the tab strip, along the top.
    Top,
    /// The page. The only surface whose height is what is left over.
    Page,
    /// The drag strip above a docked inspector. Empty unless one is docked.
    Divider,
    /// A docked inspector, under the page. Empty unless one is docked (C5.5).
    Inspector,
    /// `bru://chrome/panel.html` — completion and prompt, directly above the status bar.
    Panel,
    /// `bru://chrome/bottom.html` — the status bar, along the bottom.
    Bottom,
}

/// What the pane looks like and what is open in it — everything [`layout`] needs.
///
/// The pane's own size is in **device** pixels, because that is what the terminal measured and what
/// the presenter will send. The chrome heights are in **logical** pixels, because that is what the
/// chrome documents are written in and what `window.rs` states them in; [`LayoutRequest::scale`] is
/// the one place the two meet.
#[derive(Clone, Copy, Debug)]
pub struct LayoutRequest {
    /// The pane's width in device pixels.
    pub width: i32,
    /// The pane's height in device pixels.
    pub height: i32,
    /// The device scale factor — the same number `get_screen_info` reports to CEF, or the two
    /// disagree about what a pixel is and the chrome comes out at half size. 1.0 for the MVP.
    pub scale: f32,
    /// `tabs.show`. A hidden tab strip is zero rows, not a hidden view.
    pub top_visible: bool,
    /// `statusbar.show`. See [`LayoutRequest::panel_height`] for what it drags with it.
    pub bottom_visible: bool,
    /// The panel's height in logical pixels, as `prompt.rs`/`completers.rs` compute it; zero when
    /// nothing is open, which is the usual case and costs no rows.
    ///
    /// **Ignored when the status bar is hidden**, which is `window.rs`'s rule and its reason: a
    /// panel over a hidden status line is a block floating on the page with nothing under it.
    pub panel_height: i32,
    /// A docked inspector's height in logical pixels, zero when none is docked. The divider is
    /// added above it automatically — a docked inspector without its drag strip cannot be resized.
    pub inspector_height: i32,
    /// **Docked to the right instead of under the page.** `inspector_height` then names the
    /// inspector's *width*, because it is the same setting read against the other axis — which is
    /// what `devtools::inspector_size` already does in a window. The divider becomes a vertical
    /// strip between the page and the panel rather than a horizontal one above it.
    pub inspector_side: bool,
}

impl Default for LayoutRequest {
    fn default() -> LayoutRequest {
        LayoutRequest {
            width: 0,
            height: 0,
            scale: 1.0,
            top_visible: true,
            bottom_visible: true,
            panel_height: 0,
            inspector_height: 0,
            inspector_side: false,
        }
    }
}

/// Where every surface goes, in device pixels, with the pane's top-left at the origin.
///
/// **The rows tile the pane exactly**: their heights sum to the pane's height and none of them
/// overlaps another. That is not a nicety — the presenter places each surface as its own image, and
/// a one-pixel gap between two of them is a line of whatever was on the terminal before bru
/// started. [`Layout::tiles_exactly`] is the assertion, and the tests hold every case to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub top: Rect,
    pub page: Rect,
    pub divider: Rect,
    pub inspector: Rect,
    pub panel: Rect,
    pub bottom: Rect,
    /// The pane itself — the bounds every one of the above is clipped to.
    pub pane: Rect,
    /// Whether the page, the divider and the inspector share one band rather than taking a row
    /// each. Kept on the layout because [`Layout::tiles_exactly`] cannot tell the two apart from
    /// the rectangles alone — a bottom dock with a zero-width page looks the same.
    pub side: bool,
}

impl Layout {
    /// The rectangle for one surface, for a caller holding a [`SurfaceKind`] rather than a field.
    pub fn rect_of(&self, kind: SurfaceKind) -> Rect {
        match kind {
            SurfaceKind::Top => self.top,
            SurfaceKind::Page => self.page,
            SurfaceKind::Divider => self.divider,
            SurfaceKind::Inspector => self.inspector,
            SurfaceKind::Panel => self.panel,
            SurfaceKind::Bottom => self.bottom,
        }
    }

    /// Every row, top to bottom. Empty ones are included: a caller stepping over them wants to see
    /// that they are empty rather than to guess which are missing.
    pub fn rows(&self) -> [(SurfaceKind, Rect); 6] {
        [
            (SurfaceKind::Top, self.top),
            (SurfaceKind::Page, self.page),
            (SurfaceKind::Divider, self.divider),
            (SurfaceKind::Inspector, self.inspector),
            (SurfaceKind::Panel, self.panel),
            (SurfaceKind::Bottom, self.bottom),
        ]
    }

    /// The invariant: the rows are stacked, in order, with no gap and no overlap, and together they
    /// are exactly the pane. Held by every path through [`layout`], including the ones that had to
    /// throw rows away to fit.
    pub fn tiles_exactly(&self) -> bool {
        if self.side {
            // Three rows, one of which is three rectangles side by side. The band is covered left
            // to right and the rows stack, which is the same promise stated against two axes.
            let band = [self.page, self.divider, self.inspector];
            let mut x = self.pane.x;
            for rect in band {
                if rect.width < 0 || rect.x != x || rect.y != self.page.y {
                    return false;
                }
                if rect.height != self.page.height {
                    return false;
                }
                x = x.saturating_add(rect.width);
            }
            if x != self.pane.right() {
                return false;
            }
            let mut y = self.pane.y;
            for rect in [self.top, self.page, self.panel, self.bottom] {
                if rect.height < 0 || rect.y != y {
                    return false;
                }
                y = y.saturating_add(rect.height);
            }
            return y == self.pane.bottom();
        }
        let mut y = self.pane.y;
        for (_, rect) in self.rows() {
            if rect.x != self.pane.x || rect.width != self.pane.width {
                return false;
            }
            if rect.height < 0 || rect.y != y {
                return false;
            }
            y = y.saturating_add(rect.height);
        }
        y == self.pane.bottom()
    }
}

/// A logical measurement in device pixels.
///
/// The clamps are not decoration. A scale of zero — which is what a `get_screen_info` that never
/// got an answer would leave behind — collapses the chrome to nothing, and the symptom is a page
/// with no tab strip, which reads as a bug in the chrome documents rather than in the query. NaN
/// does the same more quietly. Both become 1.0, which is at worst the wrong size and at best right.
fn device(logical: i32, scale: f32) -> i32 {
    let scale = if scale.is_finite() && scale > 0.0 { scale.clamp(0.25, 8.0) } else { 1.0 };
    let pixels = (f64::from(logical.max(0)) * f64::from(scale)).round();
    pixels.clamp(0.0, f64::from(MAX_EDGE)) as i32
}

/// Turn the pane's size and what is open in it into six rectangles.
///
/// # When the pane is too short for the chrome
///
/// A pane of three text rows cannot hold a 40-pixel tab strip, a 24-pixel status bar and a page.
/// The page shrinking to nothing is the first answer and is enough almost always; past that
/// something has to be dropped, and the order is **panel, then tab strip, then status bar**.
///
/// The panel goes first because it is transient — it is a completion table, and a completion table
/// nobody can see is a keystroke away from not existing. The tab strip goes next because its
/// content is recoverable: `:tab-select` says the same thing the strip does. The status bar goes
/// last because it is where a refusal, an error and the current URL are written, and a browser that
/// silently drops the one surface that could explain itself is a browser that looks broken.
pub fn layout(request: &LayoutRequest) -> Layout {
    let width = request.width.clamp(0, MAX_EDGE);
    let height = request.height.clamp(0, MAX_EDGE);
    let pane = Rect::of_size(width, height);
    // **A pane of no pixels needs no special case**, and giving it one is how the tiling invariant
    // gets broken: six rectangles at the origin do not tile a pane six hundred rows tall and zero
    // columns wide. The arithmetic below already answers a zero-width pane with six zero-width rows
    // in the right places, and a zero-height pane by trimming every row to nothing.
    let mut top = if request.top_visible { device(TOP_HEIGHT, request.scale) } else { 0 };
    let mut bottom = if request.bottom_visible { device(BOTTOM_HEIGHT, request.scale) } else { 0 };
    let mut panel = if request.bottom_visible {
        device(request.panel_height, request.scale)
    } else {
        0
    };

    // Nothing below can go negative, so the trim happens before anything is placed. Each `min` is
    // what is left to give back, so a row is never trimmed past zero and `over` never goes under.
    let mut over = top.saturating_add(bottom).saturating_add(panel) - height;
    for row in [&mut panel, &mut top, &mut bottom] {
        if over <= 0 {
            break;
        }
        let given_back = (*row).min(over);
        *row -= given_back;
        over -= given_back;
    }

    // What is left for the page, before the inspector takes its share out of it.
    let page_area = (height - top - bottom - panel).max(0);
    let wanted_divider = if request.inspector_height > 0 {
        device(DIVIDER_HEIGHT, request.scale)
    } else {
        0
    };
    let wanted_inspector = device(request.inspector_height, request.scale);
    // The divider is kept whole in preference to the inspector: a dock the drag strip has been
    // trimmed off is a dock that cannot be resized back, which is a corner with no way out of it.
    let (divider, inspector) = if wanted_divider + wanted_inspector <= page_area {
        (wanted_divider, wanted_inspector)
    } else {
        let divider = wanted_divider.min(page_area);
        (divider, page_area - divider)
    };
    let page = page_area - divider - inspector;

    // **Side docking splits the page's row rather than taking a row of its own**, so the tiling
    // invariant is unchanged: the same six rectangles, with three of them sharing one band. The
    // strips above and below are full width either way — a status bar that stopped at the
    // inspector's edge would be a status bar with a hole in it.
    if request.inspector_side {
        let band = page_area;
        let wanted_divider = if request.inspector_height > 0 {
            device(DIVIDER_HEIGHT, request.scale)
        } else {
            0
        };
        let wanted_inspector = device(request.inspector_height, request.scale);
        let (divider_width, inspector_width) = if wanted_divider + wanted_inspector <= width {
            (wanted_divider, wanted_inspector)
        } else {
            let divider = wanted_divider.min(width);
            (divider, width - divider)
        };
        let page_width = width - divider_width - inspector_width;
        // Written out rather than through a closure: the band's own `y` has to be read between
        // two rows, and a closure that borrows `y` mutably is a closure nothing else may read it
        // through.
        let top_rect = Rect::new(0, 0, width, top);
        let band_y = top;
        let panel_y = band_y + band;
        let panel_rect = Rect::new(0, panel_y, width, panel);
        let bottom_rect = Rect::new(0, panel_y + panel, width, bottom);
        let (top, panel, bottom) = (top_rect, panel_rect, bottom_rect);
        return Layout {
            top,
            page: Rect::new(0, band_y, page_width, band),
            divider: Rect::new(page_width, band_y, divider_width, band),
            inspector: Rect::new(page_width + divider_width, band_y, inspector_width, band),
            panel,
            bottom,
            pane,
            side: true,
        };
    }

    let mut y = 0;
    let mut row = |rect_height: i32| {
        let rect = Rect::new(0, y, width, rect_height);
        y += rect_height;
        rect
    };
    Layout {
        top: row(top),
        page: row(page),
        divider: row(divider),
        inspector: row(inspector),
        panel: row(panel),
        bottom: row(bottom),
        pane,
        side: false,
    }
}

/// One surface's pixels as CEF painted them: BGRA, `width * height * 4` bytes, top row first.
///
/// **The length is checked once, here, and never assumed again.** `on_paint` gives a pointer and
/// two integers; the slice built from them is the only `unsafe` in the terminal path and it lives
/// at the call site, next to CEF's contract. What this constructor adds is the second opinion: if
/// the caller's arithmetic and the caller's dimensions disagree, the surface does not exist, and
/// every loop below can index by row without wondering whether the last one is short.
///
/// A surface of zero width or zero height is legal and paints nothing — CEF does hand those over,
/// briefly, while a view is being resized.
#[derive(Clone, Copy, Debug)]
pub struct Surface<'a> {
    bgra: &'a [u8],
    width: i32,
    height: i32,
}

impl<'a> Surface<'a> {
    /// A surface, or `None` when the buffer is not exactly the size the dimensions claim.
    ///
    /// Exactly, not at least: a caller with a buffer longer than its dimensions has computed one of
    /// the two wrongly, and the interesting case — a buffer built from a stale `width` after a
    /// resize — is one where "at least" happily reads a frame's worth of the wrong pixels.
    pub fn new(bgra: &'a [u8], width: i32, height: i32) -> Option<Surface<'a>> {
        if width < 0 || height < 0 || width > MAX_EDGE || height > MAX_EDGE {
            return None;
        }
        let needed = i64::from(width) * i64::from(height) * 4;
        if needed != bgra.len() as i64 {
            return None;
        }
        Some(Surface { bgra, width, height })
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn bgra(&self) -> &'a [u8] {
        self.bgra
    }

    /// The surface's own rectangle, at its own origin. What a dirty rect is clipped against.
    pub fn bounds(&self) -> Rect {
        Rect::of_size(self.width, self.height)
    }
}

/// One surface, where it goes, and how much of it changed.
///
/// The three are separate on purpose. `x`/`y` say where the surface's own `(0, 0)` lands, `clip`
/// says which output pixels may be written, and for an ordinary surface the two agree — its
/// rectangle. They stop agreeing for the popup, which is positioned relative to the page and
/// clipped to it, and that is exactly the case where conflating them puts a dropdown over the
/// status bar.
#[derive(Clone, Copy, Debug)]
pub struct Layer<'a> {
    pub surface: Surface<'a>,
    /// Where the surface's `(0, 0)` lands in the output.
    pub x: i32,
    /// See [`Layer::x`].
    pub y: i32,
    /// No output pixel outside this is written, whatever the dirty rects say.
    pub clip: Rect,
    /// What changed, in the surface's own coordinates.
    ///
    /// `None` means all of it — a first frame, a resize, or a surface that has just been placed.
    /// `Some(&[])` means nothing changed and the layer is skipped, which is not the same thing and
    /// is worth the `Option` to keep apart: the difference is a full repaint against a no-op.
    pub dirty: Option<&'a [Rect]>,
}

impl<'a> Layer<'a> {
    /// A surface filling its row: positioned at the rectangle's corner, clipped to the rectangle,
    /// wholly dirty. The three chrome strips and the page are all made this way.
    pub fn at(surface: Surface<'a>, dest: Rect) -> Layer<'a> {
        Layer { surface, x: dest.x, y: dest.y, clip: dest, dirty: None }
    }

    /// Repaint only these, in the surface's coordinates — `on_paint`'s `dirty_rects` verbatim.
    pub fn with_dirty(mut self, dirty: &'a [Rect]) -> Layer<'a> {
        self.dirty = Some(dirty);
        self
    }

    /// Narrow the clip further, without moving the surface.
    pub fn clipped_to(mut self, clip: Rect) -> Layer<'a> {
        self.clip = self.clip.intersect(&clip);
        self
    }
}

/// The `PET_POPUP` surface, placed over the page.
///
/// CEF reports the popup's rectangle through `on_popup_size` in the **view's** coordinates — the
/// page browser's, not the pane's — so the page's own offset is added here and nowhere else. The
/// clip is the overlap with the page: a dropdown opened from a `<select>` near the bottom of a
/// short page is taller than the room under it, and Chromium is content to hand over a rectangle
/// that runs off the end. Clipped, it stops at the page's edge; unclipped it would be painted over
/// the status bar, which is a surface the page has no business writing into.
pub fn popup_layer<'a>(surface: Surface<'a>, popup_in_view: Rect, page: Rect) -> Layer<'a> {
    let dest = popup_in_view.translate(page.x, page.y);
    Layer { surface, x: dest.x, y: dest.y, clip: dest.intersect(&page), dirty: None }
}

/// The pane's pixels, in the presenter's format: RGB, three bytes to the pixel, top row first.
///
/// Kept across frames and written into in place — see the module header. Allocation is the only
/// thing that resizes it: a pane that changed size gets a new `Frame`, because a `Frame` that
/// reinterpreted its own bytes at a new stride would be showing a sheared copy of the last one.
#[derive(Clone, Debug)]
pub struct Frame {
    rgb: Vec<u8>,
    width: i32,
    height: i32,
}

impl Frame {
    /// A black frame of this size, or `None` when the size is not one a pane can have — negative,
    /// past [`MAX_EDGE`], or more than [`MAX_FRAME_BYTES`] once multiplied out.
    ///
    /// A zero-sized frame is allowed and holds no bytes: a pane can genuinely be measured at zero
    /// while a split is being dragged, and refusing it there would turn a transient into an error.
    pub fn new(width: i32, height: i32) -> Option<Frame> {
        if width < 0 || height < 0 || width > MAX_EDGE || height > MAX_EDGE {
            return None;
        }
        let bytes = i64::from(width) * i64::from(height) * 3;
        if bytes > MAX_FRAME_BYTES as i64 {
            return None;
        }
        Some(Frame { rgb: vec![0u8; bytes as usize], width, height })
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    /// The whole buffer, ready for the transfer into shared memory.
    pub fn rgb(&self) -> &[u8] {
        &self.rgb
    }

    pub fn bounds(&self) -> Rect {
        Rect::of_size(self.width, self.height)
    }

    /// Bytes in one row of the frame.
    pub fn stride(&self) -> usize {
        (self.width.max(0) as usize) * 3
    }

    /// Paint a flat colour, clipped to the frame.
    ///
    /// **What this is for is the gap.** [`compose`] writes only what a surface covers, so a surface
    /// smaller than its row — a chrome browser that has not caught up with a resize — leaves the
    /// rest of the row holding the last frame's pixels. The host paints those once, in the chrome
    /// background colour, for the same reason `window.rs` sets `background_color` on the chrome
    /// browsers: the alternative is the page showing through a strip it is not behind.
    pub fn fill(&mut self, rect: Rect, colour: [u8; 3]) {
        let rect = rect.intersect(&self.bounds());
        if rect.is_empty() {
            return;
        }
        let stride = self.stride();
        for row in 0..rect.height as usize {
            let start = (rect.y as usize + row) * stride + rect.x as usize * 3;
            let end = start + rect.width as usize * 3;
            let Some(slice) = self.rgb.get_mut(start..end) else {
                continue;
            };
            for pixel in slice.as_chunks_mut::<3>().0.iter_mut() {
                *pixel = colour;
            }
        }
    }

    /// A tight copy of one rectangle of the frame, rows packed end to end.
    ///
    /// This is what the presenter sends when only part of the pane changed: kitty is given the
    /// damage rectangle's pixels and told where to put them, so a status-bar update costs its own
    /// twenty-four rows rather than the pane's. An empty or off-frame rectangle answers an empty
    /// buffer, which the caller should read as "nothing to send".
    pub fn sub_rect_rgb(&self, rect: Rect) -> Vec<u8> {
        let rect = rect.intersect(&self.bounds());
        if rect.is_empty() {
            return Vec::new();
        }
        let stride = self.stride();
        let row_bytes = rect.width as usize * 3;
        let mut out = Vec::with_capacity(row_bytes * rect.height as usize);
        for row in 0..rect.height as usize {
            let start = (rect.y as usize + row) * stride + rect.x as usize * 3;
            let end = start + row_bytes;
            if let Some(slice) = self.rgb.get(start..end) {
                out.extend_from_slice(slice);
            }
        }
        out
    }
}

/// Lay the layers into the frame, bottom first, and answer what changed.
///
/// The layers are painted in the order given, so the caller's order *is* the z-order: the four
/// rows do not overlap and could go in any order, but the popup must come after the page or a menu
/// is drawn and then covered by the page it belongs to.
///
/// The answer is the union, in the frame's coordinates, of the pixels actually written — `None`
/// when nothing was. That is the difference between a frame the presenter should send and one it
/// should not, and it is computed from what survived clipping rather than from what was asked for:
/// a popup entirely off the page and a dirty rect entirely off its surface both write nothing, and
/// a present tick for either would be a screenful of escape sequences carrying no news.
pub fn compose(frame: &mut Frame, layers: &[Layer<'_>]) -> Option<Rect> {
    let mut damage: Option<Rect> = None;
    for layer in layers {
        let painted = paint_layer(frame, layer);
        if let Some(rect) = painted {
            damage = Some(match damage {
                Some(so_far) => so_far.union(&rect),
                None => rect,
            });
        }
    }
    damage
}

/// One layer's contribution, or `None` when it wrote nothing.
fn paint_layer(frame: &mut Frame, layer: &Layer<'_>) -> Option<Rect> {
    let bounds = layer.surface.bounds();
    let whole = [bounds];
    let dirty: &[Rect] = match layer.dirty {
        Some(rects) => rects,
        None => &whole,
    };
    let mut damage: Option<Rect> = None;
    for rect in dirty {
        let painted = blit(frame, layer, *rect);
        if let Some(rect) = painted {
            damage = Some(match damage {
                Some(so_far) => so_far.union(&rect),
                None => rect,
            });
        }
    }
    damage
}

/// Copy one dirty rectangle of one surface into the frame, converting BGRA to RGB on the way.
///
/// The clipping is done once, on rectangles, before a single byte moves — which is what makes the
/// inner loop's indices provably in range rather than defensively so. Three clips apply, and the
/// order matters only in that the last one is the frame itself:
///
/// 1. the dirty rectangle against the surface, so a rectangle CEF reported for a size the surface
///    no longer has cannot read past its buffer;
/// 2. the result, moved into output coordinates, against the layer's clip — its row, or for the
///    popup the page;
/// 3. and against the frame, so a row that outlived a shrink writes nothing outside the buffer.
///
/// The surface rectangle is then recovered from the *clipped* destination, so the two always name
/// the same pixels and neither can walk off its end.
fn blit(frame: &mut Frame, layer: &Layer<'_>, dirty: Rect) -> Option<Rect> {
    let source = dirty.intersect(&layer.surface.bounds());
    if source.is_empty() {
        return None;
    }
    let destination = source
        .translate(layer.x, layer.y)
        .intersect(&layer.clip)
        .intersect(&frame.bounds());
    if destination.is_empty() {
        return None;
    }
    // Back into the surface's coordinates. Inside `source` by construction, because `destination`
    // is a subset of `source` moved, and moving it back cannot leave what it came from.
    let source = destination.translate(-layer.x, -layer.y);

    let surface_stride = layer.surface.width().max(0) as usize * 4;
    let frame_stride = frame.stride();
    let source_bytes = layer.surface.bgra();
    let row_pixels = destination.width as usize;
    for row in 0..destination.height as usize {
        let source_start = (source.y as usize + row) * surface_stride + source.x as usize * 4;
        let destination_start =
            (destination.y as usize + row) * frame_stride + destination.x as usize * 3;
        // `get`/`get_mut` rather than indexing: the arithmetic above is checked, but this is the
        // module where a wrong number is a memory bug, and a skipped row is a cheaper way to be
        // wrong than a panic inside a paint callback.
        let Some(source_row) = source_bytes.get(source_start..source_start + row_pixels * 4) else {
            continue;
        };
        let Some(destination_row) =
            frame.rgb.get_mut(destination_start..destination_start + row_pixels * 3)
        else {
            continue;
        };
        // **BGRA in, RGB out — this is the swizzle, and it happens exactly here.** CEF's buffer is
        // little-endian BGRA, so byte 0 is blue and byte 2 is red; kitty is told `f=24` and wants
        // red first. Alpha is dropped rather than blended: every surface CEF hands over is a
        // finished, opaque widget — the popup included, which is its own window and not a
        // translucent overlay — and the terminal has no alpha to blend against anyway. If a
        // surface ever does need blending, it needs it here and nowhere else.
        let source_pixels = source_row.as_chunks::<4>().0;
        let destination_pixels = destination_row.as_chunks_mut::<3>().0;
        for (out, pixel) in destination_pixels.iter_mut().zip(source_pixels) {
            out[0] = pixel[2];
            out[1] = pixel[1];
            out[2] = pixel[0];
        }
    }
    Some(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BGRA surface buffer whose pixels are whatever `colour` says, so a test can name the pixel
    /// it expects to find at a coordinate instead of counting bytes.
    fn bgra(width: i32, height: i32, colour: impl Fn(i32, i32) -> [u8; 4]) -> Vec<u8> {
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                out.extend_from_slice(&colour(x, y));
            }
        }
        out
    }

    /// A flat surface: one BGRA value everywhere.
    fn flat(width: i32, height: i32, pixel: [u8; 4]) -> Vec<u8> {
        bgra(width, height, |_, _| pixel)
    }

    /// The RGB pixel at a coordinate of a frame.
    fn pixel_at(frame: &Frame, x: i32, y: i32) -> [u8; 3] {
        let start = (y as usize) * frame.stride() + (x as usize) * 3;
        let slice = &frame.rgb()[start..start + 3];
        [slice[0], slice[1], slice[2]]
    }

    const BLACK: [u8; 3] = [0, 0, 0];

    // --- rectangles ----------------------------------------------------------------------------

    #[test]
    fn an_empty_rectangle_is_nothing_whichever_way_it_is_empty() {
        assert!(Rect::new(0, 0, 0, 10).is_empty());
        assert!(Rect::new(0, 0, 10, 0).is_empty());
        assert!(Rect::new(0, 0, -1, 10).is_empty());
        assert!(!Rect::new(-5, -5, 1, 1).is_empty());
    }

    #[test]
    fn intersecting_answers_the_overlap_and_nothing_when_there_is_none() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), Rect::new(5, 5, 5, 5));
        assert_eq!(a.intersect(&Rect::new(20, 20, 5, 5)), Rect::ZERO);
        // Touching edges share no pixel.
        assert_eq!(a.intersect(&Rect::new(10, 0, 5, 5)), Rect::ZERO);
        assert_eq!(a.intersect(&Rect::ZERO), Rect::ZERO);
    }

    /// The `i64` in [`Rect::intersect`] earns its place here: `i32::MAX - i32::MIN` is not an
    /// `i32`, and a garbage dirty rect can reach it without anything else being wrong.
    #[test]
    fn rectangles_at_the_ends_of_the_number_line_do_not_overflow() {
        let huge = Rect::new(i32::MIN / 2, i32::MIN / 2, i32::MAX, i32::MAX);
        let other = Rect::new(0, 0, i32::MAX, i32::MAX);
        let overlap = huge.intersect(&other);
        assert!(!overlap.is_empty());
        assert!(overlap.width > 0 && overlap.height > 0);
        let both = huge.union(&other);
        assert!(both.width > 0 && both.height > 0);
        // Saturating, not wrapping: the far edge cannot come out to the left of the near one.
        assert!(Rect::new(i32::MAX - 1, 0, 100, 1).right() == i32::MAX);
    }

    #[test]
    fn a_union_ignores_the_empty_and_a_union_of_nothing_is_none() {
        assert_eq!(union_all(&[]), None);
        assert_eq!(union_all(&[Rect::ZERO, Rect::new(1, 1, 0, 5)]), None);
        assert_eq!(
            union_all(&[Rect::ZERO, Rect::new(2, 3, 4, 5), Rect::new(10, 1, 1, 1)]),
            Some(Rect::new(2, 1, 9, 7))
        );
    }

    // --- layout --------------------------------------------------------------------------------

    #[test]
    fn the_four_rows_tile_the_pane_with_the_page_taking_what_is_left() {
        let plan = layout(&LayoutRequest { width: 800, height: 600, ..Default::default() });
        assert_eq!(plan.top, Rect::new(0, 0, 800, 40));
        assert_eq!(plan.page, Rect::new(0, 40, 800, 536));
        assert_eq!(plan.panel, Rect::new(0, 576, 800, 0));
        assert_eq!(plan.bottom, Rect::new(0, 576, 800, 24));
        assert!(plan.tiles_exactly());
    }

    #[test]
    fn an_open_panel_comes_out_of_the_page_and_sits_on_the_status_bar() {
        let plan = layout(&LayoutRequest {
            width: 800,
            height: 600,
            panel_height: 120,
            ..Default::default()
        });
        assert_eq!(plan.page, Rect::new(0, 40, 800, 416));
        assert_eq!(plan.panel, Rect::new(0, 456, 800, 120));
        assert_eq!(plan.bottom, Rect::new(0, 576, 800, 24));
        assert!(plan.tiles_exactly());
    }

    /// `window.rs`'s rule: no status bar, no panel — a completion table with nothing under it.
    #[test]
    fn a_hidden_status_bar_takes_the_panel_with_it() {
        let plan = layout(&LayoutRequest {
            width: 800,
            height: 600,
            bottom_visible: false,
            panel_height: 120,
            ..Default::default()
        });
        assert!(plan.panel.is_empty());
        assert!(plan.bottom.is_empty());
        assert_eq!(plan.page, Rect::new(0, 40, 800, 560));
        assert!(plan.tiles_exactly());
    }

    #[test]
    fn hiding_the_tab_strip_gives_its_rows_to_the_page() {
        let plan = layout(&LayoutRequest {
            width: 800,
            height: 600,
            top_visible: false,
            ..Default::default()
        });
        assert!(plan.top.is_empty());
        assert_eq!(plan.page, Rect::new(0, 0, 800, 576));
        assert!(plan.tiles_exactly());
    }

    #[test]
    fn the_scale_factor_is_applied_to_the_logical_heights() {
        let plan =
            layout(&LayoutRequest { width: 1600, height: 1200, scale: 2.0, ..Default::default() });
        assert_eq!(plan.top.height, 80);
        assert_eq!(plan.bottom.height, 48);
        assert_eq!(plan.page, Rect::new(0, 80, 1600, 1072));
        assert!(plan.tiles_exactly());
    }

    /// A scale that never arrived must not collapse the chrome; see [`device`].
    #[test]
    fn a_nonsense_scale_is_treated_as_one_to_one() {
        for scale in [0.0, -3.0, f32::NAN, f32::INFINITY] {
            let plan =
                layout(&LayoutRequest { width: 800, height: 600, scale, ..Default::default() });
            assert_eq!(plan.top.height, TOP_HEIGHT, "scale {scale}");
            assert_eq!(plan.bottom.height, BOTTOM_HEIGHT, "scale {scale}");
        }
    }

    #[test]
    fn a_pane_too_short_for_the_chrome_sheds_the_panel_then_the_strip_then_the_bar() {
        // 100 rows: 40 + 24 + 120 wanted, so the panel gives back 84 and the page is nothing.
        let plan = layout(&LayoutRequest {
            width: 200,
            height: 100,
            panel_height: 120,
            ..Default::default()
        });
        assert_eq!(plan.top.height, 40);
        assert_eq!(plan.panel.height, 36);
        assert_eq!(plan.bottom.height, 24);
        assert_eq!(plan.page.height, 0);
        assert!(plan.tiles_exactly());

        // 30 rows: the panel is gone, the tab strip is gone, and the status bar keeps all of it.
        let plan = layout(&LayoutRequest {
            width: 200,
            height: 30,
            panel_height: 120,
            ..Default::default()
        });
        assert!(plan.panel.is_empty());
        assert_eq!(plan.top.height, 6);
        assert_eq!(plan.bottom.height, 24);
        assert!(plan.tiles_exactly());

        // 10 rows: only the status bar is left, and it is trimmed rather than overflowing.
        let plan = layout(&LayoutRequest { width: 200, height: 10, ..Default::default() });
        assert!(plan.top.is_empty());
        assert!(plan.page.is_empty());
        assert_eq!(plan.bottom.height, 10);
        assert!(plan.tiles_exactly());
    }

    #[test]
    fn a_pane_of_no_pixels_lays_out_to_nothing_at_all() {
        for request in [
            LayoutRequest { width: 0, height: 600, ..Default::default() },
            LayoutRequest { width: 800, height: 0, ..Default::default() },
            LayoutRequest { width: -10, height: -10, ..Default::default() },
        ] {
            let plan = layout(&request);
            for (_, rect) in plan.rows() {
                assert!(rect.is_empty());
            }
            assert!(plan.tiles_exactly());
        }
    }

    #[test]
    fn a_docked_inspector_takes_the_bottom_of_the_page_with_its_drag_strip_above_it() {
        let plan = layout(&LayoutRequest {
            width: 800,
            height: 600,
            inspector_height: 200,
            ..Default::default()
        });
        assert_eq!(plan.page, Rect::new(0, 40, 800, 324));
        assert_eq!(plan.divider, Rect::new(0, 364, 800, DIVIDER_HEIGHT));
        assert_eq!(plan.inspector, Rect::new(0, 376, 800, 200));
        assert_eq!(plan.bottom, Rect::new(0, 576, 800, 24));
        assert!(plan.tiles_exactly());
    }

    /// An inspector asked for more than the page area has: the drag strip survives whole, because a
    /// dock that cannot be dragged back is a corner with no way out.
    #[test]
    fn an_oversized_inspector_keeps_its_drag_strip_and_gives_up_its_own_rows() {
        let plan = layout(&LayoutRequest {
            width: 800,
            height: 200,
            inspector_height: 1000,
            ..Default::default()
        });
        assert_eq!(plan.page.height, 0);
        assert_eq!(plan.divider.height, DIVIDER_HEIGHT);
        assert_eq!(plan.inspector.height, 200 - 40 - 24 - DIVIDER_HEIGHT);
        assert!(plan.tiles_exactly());
    }

    #[test]
    fn the_kinds_answer_the_same_rectangles_as_the_fields() {
        let plan = layout(&LayoutRequest {
            width: 800,
            height: 600,
            panel_height: 60,
            inspector_height: 100,
            ..Default::default()
        });
        for (kind, rect) in plan.rows() {
            assert_eq!(plan.rect_of(kind), rect, "{kind:?}");
        }
    }

    // --- surfaces ------------------------------------------------------------------------------

    /// The check the whole module rests on: dimensions and buffer must agree, or there is no
    /// surface. A short buffer is the read out of bounds this exists to make impossible.
    #[test]
    fn a_surface_whose_buffer_is_the_wrong_length_is_refused() {
        let four_by_four = flat(4, 4, [1, 2, 3, 255]);
        assert!(Surface::new(&four_by_four, 4, 4).is_some());
        // One pixel short.
        assert!(Surface::new(&four_by_four[..60], 4, 4).is_none());
        // One pixel long.
        let mut long = four_by_four.clone();
        long.extend_from_slice(&[0, 0, 0, 0]);
        assert!(Surface::new(&long, 4, 4).is_none());
        // The dimensions themselves lying.
        assert!(Surface::new(&four_by_four, 8, 2).is_some());
        assert!(Surface::new(&four_by_four, 5, 4).is_none());
        assert!(Surface::new(&four_by_four, -4, 4).is_none());
        assert!(Surface::new(&four_by_four, 4, MAX_EDGE + 1).is_none());
    }

    #[test]
    fn a_surface_of_no_pixels_exists_and_paints_nothing() {
        let empty: [u8; 0] = [];
        let surface = Surface::new(&empty, 0, 0).expect("a zero-sized surface is legal");
        assert!(surface.bounds().is_empty());
        let mut frame = Frame::new(4, 4).expect("frame");
        let damage = compose(&mut frame, &[Layer::at(surface, Rect::new(0, 0, 4, 4))]);
        assert_eq!(damage, None);
        assert!(frame.rgb().iter().all(|byte| *byte == 0));
    }

    // --- composing -----------------------------------------------------------------------------

    /// Byte for byte, on a frame small enough to write out: BGRA in, RGB out, alpha dropped.
    #[test]
    fn a_surface_filling_the_frame_is_copied_with_red_and_blue_the_right_way_round() {
        let pixels = flat(2, 2, [0x10, 0x20, 0x30, 0xff]);
        let surface = Surface::new(&pixels, 2, 2).expect("surface");
        let mut frame = Frame::new(2, 2).expect("frame");
        let damage = compose(&mut frame, &[Layer::at(surface, Rect::new(0, 0, 2, 2))]);
        assert_eq!(damage, Some(Rect::new(0, 0, 2, 2)));
        // B=0x10 G=0x20 R=0x30 becomes R,G,B.
        assert_eq!(frame.rgb(), &[0x30, 0x20, 0x10].repeat(4));
    }

    /// Three rows composed into one frame, at heights small enough to check pixel by pixel. The
    /// rectangles are hand-built rather than taken from [`layout`]: a pane this small trims the
    /// chrome away entirely, and what is under test here is the copying, not the trimming.
    #[test]
    fn each_surface_lands_at_its_own_row_and_the_damage_is_their_union() {
        let top_pixels = flat(8, 1, [1, 1, 1, 255]);
        let page_pixels = flat(8, 4, [2, 2, 2, 255]);
        let bar_pixels = flat(8, 1, [3, 3, 3, 255]);
        let mut frame = Frame::new(8, 6).expect("frame");
        let damage = compose(
            &mut frame,
            &[
                Layer::at(Surface::new(&top_pixels, 8, 1).unwrap(), Rect::new(0, 0, 8, 1)),
                Layer::at(Surface::new(&page_pixels, 8, 4).unwrap(), Rect::new(0, 1, 8, 4)),
                Layer::at(Surface::new(&bar_pixels, 8, 1).unwrap(), Rect::new(0, 5, 8, 1)),
            ],
        );
        assert_eq!(damage, Some(Rect::new(0, 0, 8, 6)));
        assert_eq!(pixel_at(&frame, 0, 0), [1, 1, 1]);
        assert_eq!(pixel_at(&frame, 7, 3), [2, 2, 2]);
        assert_eq!(pixel_at(&frame, 4, 5), [3, 3, 3]);
    }

    /// The point of the damage rectangle: a frame in which one row changed copies one row.
    #[test]
    fn only_the_dirty_rectangle_is_copied_and_only_it_is_reported() {
        let pixels = bgra(4, 4, |x, y| [(x + 1) as u8, (y + 1) as u8, 0x40, 0xff]);
        let surface = Surface::new(&pixels, 4, 4).expect("surface");
        let mut frame = Frame::new(4, 4).expect("frame");
        let dirty = [Rect::new(1, 2, 2, 1)];
        let damage = compose(
            &mut frame,
            &[Layer::at(surface, Rect::new(0, 0, 4, 4)).with_dirty(&dirty)],
        );
        assert_eq!(damage, Some(Rect::new(1, 2, 2, 1)));
        assert_eq!(pixel_at(&frame, 1, 2), [0x40, 3, 2]);
        assert_eq!(pixel_at(&frame, 2, 2), [0x40, 3, 3]);
        // Everything else is untouched — the frame was black and stayed black.
        for y in 0..4 {
            for x in 0..4 {
                if y == 2 && (x == 1 || x == 2) {
                    continue;
                }
                assert_eq!(pixel_at(&frame, x, y), BLACK, "at {x},{y}");
            }
        }
    }

    #[test]
    fn an_explicitly_empty_dirty_list_paints_nothing_which_is_not_the_same_as_all_of_it() {
        let pixels = flat(4, 4, [9, 9, 9, 255]);
        let surface = Surface::new(&pixels, 4, 4).expect("surface");
        let mut frame = Frame::new(4, 4).expect("frame");
        let nothing: [Rect; 0] = [];
        assert_eq!(
            compose(&mut frame, &[Layer::at(surface, Rect::new(0, 0, 4, 4)).with_dirty(&nothing)]),
            None
        );
        assert!(frame.rgb().iter().all(|byte| *byte == 0));
        // The same layer with `None` paints all of it.
        assert_eq!(
            compose(&mut frame, &[Layer::at(surface, Rect::new(0, 0, 4, 4))]),
            Some(Rect::new(0, 0, 4, 4))
        );
    }

    /// A surface larger than the row it was given — a chrome browser that has not caught up with a
    /// resize. Everything past the row's edge is dropped, not wrapped onto the next row.
    #[test]
    fn a_surface_bigger_than_its_row_is_clipped_to_the_row() {
        let pixels = flat(8, 8, [5, 6, 7, 255]);
        let surface = Surface::new(&pixels, 8, 8).expect("surface");
        let mut frame = Frame::new(4, 4).expect("frame");
        let damage = compose(&mut frame, &[Layer::at(surface, Rect::new(0, 0, 4, 2))]);
        assert_eq!(damage, Some(Rect::new(0, 0, 4, 2)));
        assert_eq!(pixel_at(&frame, 3, 1), [7, 6, 5]);
        assert_eq!(pixel_at(&frame, 3, 2), BLACK);
    }

    /// And the other way: a surface smaller than its row leaves the rest of the row alone, which is
    /// what [`Frame::fill`] exists to paint over.
    #[test]
    fn a_surface_smaller_than_its_row_leaves_the_rest_of_the_row_alone() {
        let pixels = flat(2, 2, [5, 6, 7, 255]);
        let surface = Surface::new(&pixels, 2, 2).expect("surface");
        let mut frame = Frame::new(4, 4).expect("frame");
        frame.fill(Rect::new(0, 0, 4, 4), [0x11, 0x22, 0x33]);
        let damage = compose(&mut frame, &[Layer::at(surface, Rect::new(0, 0, 4, 4))]);
        assert_eq!(damage, Some(Rect::new(0, 0, 2, 2)));
        assert_eq!(pixel_at(&frame, 1, 1), [7, 6, 5]);
        assert_eq!(pixel_at(&frame, 2, 2), [0x11, 0x22, 0x33]);
    }

    /// A row that outlived a shrink: nothing may be written outside the frame, and the report must
    /// say so rather than claiming a rectangle the presenter would then try to send.
    #[test]
    fn a_row_that_runs_off_the_frame_writes_only_what_is_inside_it() {
        let pixels = flat(4, 4, [8, 8, 8, 255]);
        let surface = Surface::new(&pixels, 4, 4).expect("surface");
        let mut frame = Frame::new(4, 4).expect("frame");
        let damage = compose(&mut frame, &[Layer::at(surface, Rect::new(2, 2, 4, 4))]);
        assert_eq!(damage, Some(Rect::new(2, 2, 2, 2)));
        assert_eq!(pixel_at(&frame, 3, 3), [8, 8, 8]);
        assert_eq!(pixel_at(&frame, 1, 1), BLACK);

        // Entirely outside: nothing written, nothing reported.
        let mut frame = Frame::new(4, 4).expect("frame");
        assert_eq!(compose(&mut frame, &[Layer::at(surface, Rect::new(40, 40, 4, 4))]), None);
        assert!(frame.rgb().iter().all(|byte| *byte == 0));

        // And to the top-left, where the destination goes negative.
        let mut frame = Frame::new(4, 4).expect("frame");
        let damage = compose(&mut frame, &[Layer::at(surface, Rect::new(-2, -2, 4, 4))]);
        assert_eq!(damage, Some(Rect::new(0, 0, 2, 2)));
        assert_eq!(pixel_at(&frame, 0, 0), [8, 8, 8]);
        assert_eq!(pixel_at(&frame, 2, 2), BLACK);
    }

    /// Dirty rectangles arrive from Chromium and are not this module's to trust: one for a size the
    /// surface no longer has must clip, not read.
    #[test]
    fn a_dirty_rectangle_outside_its_surface_is_clipped_away() {
        let pixels = flat(4, 4, [8, 8, 8, 255]);
        let surface = Surface::new(&pixels, 4, 4).expect("surface");
        let mut frame = Frame::new(4, 4).expect("frame");
        let dirty = [Rect::new(10, 10, 4, 4), Rect::new(-4, -4, 2, 2), Rect::new(3, 3, 100, 100)];
        let damage =
            compose(&mut frame, &[Layer::at(surface, Rect::new(0, 0, 4, 4)).with_dirty(&dirty)]);
        // Only the third overlaps the surface, and only its last pixel does.
        assert_eq!(damage, Some(Rect::new(3, 3, 1, 1)));
        assert_eq!(pixel_at(&frame, 3, 3), [8, 8, 8]);
        assert_eq!(pixel_at(&frame, 0, 0), BLACK);
    }

    // --- the popup layer -----------------------------------------------------------------------

    #[test]
    fn a_popup_is_placed_in_the_pages_coordinates_and_over_the_page() {
        let page = Rect::new(0, 2, 8, 4);
        let page_pixels = flat(8, 4, [1, 1, 1, 255]);
        let popup_pixels = flat(2, 2, [9, 9, 9, 255]);
        let mut frame = Frame::new(8, 8).expect("frame");
        let popup =
            popup_layer(Surface::new(&popup_pixels, 2, 2).unwrap(), Rect::new(1, 1, 2, 2), page);
        let damage = compose(
            &mut frame,
            &[Layer::at(Surface::new(&page_pixels, 8, 4).unwrap(), page), popup],
        );
        assert_eq!(damage, Some(Rect::new(0, 2, 8, 4)));
        // The popup's (0,0) is at the page's (1,1), which is the frame's (1,3).
        assert_eq!(pixel_at(&frame, 1, 3), [9, 9, 9]);
        assert_eq!(pixel_at(&frame, 2, 4), [9, 9, 9]);
        assert_eq!(pixel_at(&frame, 0, 3), [1, 1, 1]);
        // And the order is the order: the page first, the menu on top of it.
        assert_ne!(pixel_at(&frame, 1, 3), [1, 1, 1]);
    }

    /// The case the clip exists for: a dropdown opened near the bottom of a short page is taller
    /// than the room under it, and Chromium hands over the rectangle anyway. Unclipped it would be
    /// painted over the status bar.
    #[test]
    fn a_popup_hanging_off_the_page_stops_at_the_pages_edge() {
        let page = Rect::new(0, 1, 8, 3);
        let bar_row = 4;
        let popup_pixels = flat(4, 6, [9, 9, 9, 255]);
        let mut frame = Frame::new(8, 8).expect("frame");
        frame.fill(Rect::new(0, bar_row, 8, 4), [7, 7, 7]);
        let popup =
            popup_layer(Surface::new(&popup_pixels, 4, 6).unwrap(), Rect::new(2, 1, 4, 6), page);
        let damage = compose(&mut frame, &[popup]);
        // Clipped to the page: two rows of it fit, and the width fits.
        assert_eq!(damage, Some(Rect::new(2, 2, 4, 2)));
        assert_eq!(pixel_at(&frame, 2, 3), [9, 9, 9]);
        // The status bar is untouched.
        assert_eq!(pixel_at(&frame, 2, bar_row), [7, 7, 7]);
    }

    #[test]
    fn a_popup_entirely_off_the_page_paints_nothing() {
        let page = Rect::new(0, 1, 4, 2);
        let popup_pixels = flat(2, 2, [9, 9, 9, 255]);
        let mut frame = Frame::new(8, 8).expect("frame");
        let popup =
            popup_layer(Surface::new(&popup_pixels, 2, 2).unwrap(), Rect::new(20, 20, 2, 2), page);
        assert_eq!(compose(&mut frame, &[popup]), None);
        assert!(frame.rgb().iter().all(|byte| *byte == 0));
    }

    // --- the frame itself ----------------------------------------------------------------------

    #[test]
    fn a_frame_refuses_a_size_no_pane_has() {
        assert!(Frame::new(-1, 10).is_none());
        assert!(Frame::new(10, -1).is_none());
        assert!(Frame::new(MAX_EDGE + 1, 1).is_none());
        // Inside both edge limits and still far past what may be allocated.
        assert!(Frame::new(MAX_EDGE, MAX_EDGE).is_none());
        let empty = Frame::new(0, 0).expect("a zero-sized pane is a transient, not an error");
        assert!(empty.rgb().is_empty());
    }

    #[test]
    fn filling_is_clipped_to_the_frame() {
        let mut frame = Frame::new(4, 4).expect("frame");
        frame.fill(Rect::new(-2, -2, 4, 4), [1, 2, 3]);
        assert_eq!(pixel_at(&frame, 0, 0), [1, 2, 3]);
        assert_eq!(pixel_at(&frame, 1, 1), [1, 2, 3]);
        assert_eq!(pixel_at(&frame, 2, 2), BLACK);
        frame.fill(Rect::new(10, 10, 4, 4), [4, 5, 6]);
        assert_eq!(pixel_at(&frame, 3, 3), BLACK);
    }

    /// What the presenter actually sends when only a strip changed: the damage rectangle's rows,
    /// packed, with the frame's stride left behind.
    #[test]
    fn a_sub_rectangle_comes_out_packed_and_clipped() {
        let mut frame = Frame::new(4, 4).expect("frame");
        frame.fill(Rect::new(1, 1, 2, 2), [1, 2, 3]);
        let strip = frame.sub_rect_rgb(Rect::new(1, 1, 2, 2));
        assert_eq!(strip, [1, 2, 3].repeat(4));
        assert_eq!(frame.sub_rect_rgb(Rect::new(10, 10, 2, 2)), Vec::<u8>::new());
        assert_eq!(frame.sub_rect_rgb(Rect::ZERO), Vec::<u8>::new());
        // A rectangle half off the frame answers only the half that is on it.
        assert_eq!(frame.sub_rect_rgb(Rect::new(3, 3, 4, 4)).len(), 3);
    }

    /// **A side dock splits the page's band and does not take a band of its own**, so the same six
    /// rectangles still tile the pane. The strips above and below stay full width: a status bar
    /// that stopped at the inspector's edge would be a status bar with a hole in it.
    #[test]
    fn an_inspector_docked_to_the_right_splits_the_page_row() {
        let side = layout(&LayoutRequest {
            width: 1000,
            height: 600,
            scale: 1.0,
            top_visible: true,
            bottom_visible: true,
            panel_height: 0,
            inspector_height: 300,
            inspector_side: true,
        });
        assert_eq!(side.top.width, 1000, "the strip spans the pane");
        assert_eq!(side.bottom.width, 1000);
        // page | divider | inspector, left to right, sharing one band.
        assert_eq!(side.page.y, side.inspector.y);
        assert_eq!(side.page.height, side.inspector.height);
        assert_eq!(side.page.right(), side.divider.x);
        assert_eq!(side.divider.right(), side.inspector.x);
        assert_eq!(side.inspector.right(), 1000);
        assert_eq!(side.inspector.width, 300);
        assert!(side.tiles_exactly(), "the pane is still covered exactly");

        // The same numbers docked at the bottom take a row instead, and the page keeps the width.
        let under = layout(&LayoutRequest { inspector_side: false, ..LayoutRequest {
            width: 1000,
            height: 600,
            scale: 1.0,
            top_visible: true,
            bottom_visible: true,
            panel_height: 0,
            inspector_height: 300,
            inspector_side: true,
        } });
        assert_eq!(under.page.width, 1000);
        assert_eq!(under.inspector.height, 300);
        assert!(under.tiles_exactly());
    }
}
