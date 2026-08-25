//! What a window is made of, on the side that knows how it is drawn.
//!
//! **The seam, and the first phase of the terminal frontend rather than the last.** bru's shared
//! state holds concrete Views types — a `Window`, a `BoxLayout`, a `Panel` — and every week of
//! feature work deepens that. A windowless browser has none of them: it is not a `BrowserView`, it
//! goes into no panel, and nothing lays it out. So the types have to come apart before a second
//! frontend can exist, and the extraction pays for itself even if that frontend never arrives,
//! because it is what stops `state.rs` from being a Views file wearing a neutral name.
//!
//! ## An enum and not a trait, deliberately
//!
//! `BruState` lives behind an `Arc<Mutex<…>>` and is reached from every handler, so a `dyn Shell`
//! would have to promise `Send + Sync` — which the CEF view wrappers are not going to. And a closed
//! set of two gains nothing from dynamic dispatch: bru is one crate, an exhaustive `match` costs
//! nothing, and **every place the terminal shell has not implemented yet is a compile error rather
//! than a `todo!()` nobody finds.** That is exactly the property a port of this size wants.
//!
//! ## What lives here and what does not
//!
//! Only what one frontend owns and the other cannot have. A tab's url, title, pinned flag and
//! browser id are the browser's business and stay in `tabs.rs`; the `BrowserView` it is drawn in is
//! the Views frontend's and belongs here.

use cef::*;

/// How a window is drawn.
pub enum Shell {
    /// CEF's Views framework: a real window, laid out by CEF, with the chrome strips and the page
    /// as sibling `BrowserView`s. Everything bru has today.
    Views(ViewsShell),
}

impl Shell {
    /// A Views window, before `on_window_created` has given it anything.
    pub fn views() -> Shell {
        Shell::Views(ViewsShell::default())
    }

    /// The Views half, when this is one.
    ///
    /// Answers `None` for a shell that is not Views — which is how a Views-only path announces
    /// itself once there is a second variant, instead of quietly doing nothing.
    pub fn as_views(&self) -> Option<&ViewsShell> {
        match self {
            Shell::Views(views) => Some(views),
        }
    }

    pub fn as_views_mut(&mut self) -> Option<&mut ViewsShell> {
        match self {
            Shell::Views(views) => Some(views),
        }
    }
}

/// The handles a Views window is made of. All `Option`, because a slot is allocated before the
/// window exists — `window.rs` needs the id to build the delegates that fill these in.
#[derive(Default)]
pub struct ViewsShell {
    /// The top-level window, kept from `on_window_created` so views can be added to it later.
    pub window: Option<Window>,
    /// The window's vertical box layout, kept so a tab opened later can be given flex 1 like the
    /// ones that were there when the window was built.
    pub layout: Option<BoxLayout>,
    /// The panel holding this window's tab views, and its horizontal layout.
    ///
    /// **Every window has one, whether or not anything is docked beside the pages.** It could have
    /// been made only when `devtools.position right` was first used, and that would have meant
    /// re-parenting every live tab view at that moment — the one operation this codebase has
    /// measured as fatal in a neighbouring case (`tabs.rs`, closing a tab). Made with the window
    /// instead, once, and never moved.
    pub pages: Option<Panel>,
    pub pages_layout: Option<BoxLayout>,
    /// The inspector docked under this window's pages, if one is.
    ///
    /// **Views-only by construction, which is why it is here and not beside the tabs.** A docked
    /// inspector is a `BrowserView` in a layout and a divider strip that resizes it by dragging;
    /// the terminal frontend has neither a view to dock nor a pointer to drag with, and will answer
    /// the same question a different way or not at all. See `devtools.rs`.
    pub devtools: Option<Docked>,
}

// --- src/devtools.rs --------------------------------------------------------------------------
/// A docked inspector: the view in the window, and who it is for.
pub struct Docked {
    pub view: BrowserView,
    /// The browser being inspected. `follow_tab` compares it with the tab on screen.
    pub inspects: i32,
    /// The inspector's *own* browser, learned from the view the moment it is docked. It is what
    /// `on_before_close` sees when the panel is closed by its own button rather than by `:devtools`.
    pub browser_id: Option<i32>,
    /// The strip above it that resizes it, made with it and going with it.
    ///
    /// Held here rather than beside it so the two cannot drift: every place that shows, hides or
    /// removes the inspector has the divider in the same hand, and there is no window state in
    /// which one exists without the other.
    pub divider: Option<BrowserView>,
    /// The inspector's height when the pointer went down on the divider, in DIP.
    ///
    /// The drag reports how far the pointer has moved **from where it started**, so the height it
    /// asks for is this number minus that distance — which is why the start has to be kept. `None`
    /// when no drag is in progress; a `drag` phase that arrives without a `start` is ignored rather
    /// than guessed at.
    pub drag_from: Option<i32>,
}
// --- end src/devtools.rs ----------------------------------------------------------------------

// --- src/tabs.rs ------------------------------------------------------------------------------
/// What a tab is drawn in.
///
/// **The plan had this moved out of `Tab` and into `ViewsShell`, keyed by index. It is here
/// instead, and the reason is `tab-give`.** A tab is handed between windows *whole* — `Tab` carries
/// the pin, the mute and the browser id across, because a `BrowserView` alone would lose them
/// (`tabs.rs`, `detach_active_tab_in`). A parallel `Vec` on the shell would have to be moved
/// between two windows' shells in step with that, and a pair of vectors that must agree is a pair
/// of vectors that will one day disagree — silently, with a tab drawing another tab's page.
///
/// Naming the type here achieves what the seam is for: `tabs.rs` and `state.rs` stop saying
/// `BrowserView` in a struct both frontends share. What they say instead is "whatever this shell
/// draws a tab in", and the terminal's answer is that there is nothing to hold — a windowless tab
/// is its browser, and the compositor decides where it lands.
pub enum TabSurface {
    Views(BrowserView),
}

impl TabSurface {
    /// The `BrowserView`, when this tab is drawn in one.
    pub fn view(&self) -> Option<&BrowserView> {
        match self {
            TabSurface::Views(view) => Some(view),
        }
    }

    /// The same, given up — for the paths that hand a view back to a caller who will re-parent it.
    pub fn into_view(self) -> Option<BrowserView> {
        match self {
            TabSurface::Views(view) => Some(view),
        }
    }
}
// --- end src/tabs.rs --------------------------------------------------------------------------
