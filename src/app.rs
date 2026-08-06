//! The application object, and the one callback that matters at startup.

use cef::*;
use std::sync::{Arc, Mutex};

use crate::keys::BruClient;
use crate::state::{schedule_close, BruState};

/// Where bru goes with no argument, and what `open` with no URL opens, unless `config.lua` says
/// otherwise — see `bru.set("start_page", ...)` and [`crate::open::start_page`], which is what
/// everything should ask. This is qutebrowser's `url.default_page` default (configdata.yml :2569).
pub const HOME: &str = "https://start.duckduckgo.com/";

/// TEMPORARY (M5): a page for a tab opened by the stand-in `t` key, distinct enough that a
/// screenshot shows which tab is on. Goes when `:open` lands in M9.
pub fn placeholder_tab(index: usize) -> String {
    const HUES: [&str; 6] = ["#1f7a3d", "#7a1f5e", "#7a5e1f", "#1f3d7a", "#7a1f1f", "#1f7a7a"];
    let hue = HUES[index % HUES.len()];
    data_uri(
        &format!(
            r#"<!doctype html><meta charset="utf-8"><body style="margin:0;height:100vh;display:flex;align-items:center;justify-content:center;background:{hue};color:#fff;font:64px/1 monospace">tab {index}</body>"#
        ),
        "text/html",
    )
    .to_string()
}

/// A page CEF can load without a scheme handler behind it.
fn data_uri(content: &str, mime_type: &str) -> CefString {
    let encoded = CefString::from(&base64_encode(Some(content.as_bytes())));
    let escaped = CefString::from(&uriencode(Some(&encoded), 0)).to_string();
    CefString::from(format!("data:{mime_type};base64,{escaped}").as_str())
}

/// Add one name to a comma-separated Chromium switch, keeping whatever is already there.
///
/// **Never `append_switch_with_value` on its own.** Chromium reads a single value for each of these,
/// so a second append replaces the first — and CEF sets them itself. Measured 2026-08-06: on this
/// machine `--disable-features` already arrives holding
/// `GlicActorUi,AutofillActorMode,LensOverlay,KillOnInvalidNavigationHeaders`, and overwriting it
/// would turn those four back **on** in exactly the subprocesses CEF had turned them off in — a
/// failure that would surface somewhere else entirely, as something else entirely.
fn add_to_switch(command_line: &mut CommandLine, switch: &str, name: &str) {
    let spelling = switch;
    let switch = CefString::from(switch);
    let existing = CefString::from(&command_line.switch_value(Some(&switch))).to_string();
    if existing.split(',').any(|present| present.trim() == name) {
        return;
    }
    let value = if existing.is_empty() {
        name.to_string()
    } else {
        format!("{existing},{name}")
    };
    if std::env::var_os("BRU_DEBUG_SWITCHES").is_some() {
        eprintln!(
            "bru[switches]: {spelling} {existing:?} -> {value:?}"
        );
    }
    command_line.append_switch_with_value(Some(&switch), Some(&CefString::from(value.as_str())));
}

wrap_app! {
    pub struct BruApp {
        state: Arc<Mutex<BruState>>,
    }

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(BruBrowserProcessHandler::new(self.state.clone()))
        }

        // Chromium's own command line, before Chromium reads it. bru had no hook here at all until
        // a crash needed one.
        //
        // **A soft navigation crashes bru, and this is where it is turned off.** Measured
        // 2026-08-06 on
        // `youtube.com/watch?v=…`: clicking a link inside the page killed bru with SIGSEGV in **two
        // of three runs**, and the core shows the fault is entirely inside libcef, with no bru frame
        // above the message loop:
        //
        // ```
        // tabs::TabInterface::GetFromContents(content::WebContents*)   <- SEGV_MAPERR
        //   <- ReadAnythingSoftNavigationObserver::OnSoftNavigation()
        //   <- page_load_metrics::PageLoadTracker::OnSoftNavigation()
        //   <- MetricsWebContentsObserver::OnTimingUpdated
        // ```
        //
        // `tabs::TabInterface` is a *Chrome browser* concept — a WebContents owned by a
        // `TabStripModel`. CEF's browsers are not in one, so `GetFromContents` answers null and the
        // observer dereferences it. It needs a soft navigation to fire, which is why only a
        // single-page app reaches it: loading a watch page directly is a full navigation and
        // survived 2/2, and it was clicking a link *within* the page that crashed. YouTube is simply
        // the SPA this user opens; the bug is not YouTube's and not the ad blocker's — bru has no
        // filter lists on this machine and `adblock` appears nowhere in the trace.
        //
        // The observer is what is broken, but it is Chromium's, so the only end reachable from
        // here is the detection that wakes it. What that costs is a metric nothing in bru reads.
        //
        // The switch belongs on every process, so no `process_type` test: the renderer computes the
        // soft-navigation metrics that the browser process then dispatches into the crash.
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(command_line) = command_line else {
                return;
            };
            // **The one that is measured to engage.** Blink's heuristic is what notices a soft
            // navigation at all, so with it off the renderer reports none and the browser process
            // never reaches the observer. Measured 2026-08-06 by asking a page what it supports:
            //
            // ```
            // no switch                                          -> soft-nav supported: true
            // --disable-features=SoftNavigationHeuristics         -> soft-nav supported: true
            // --disable-blink-features=SoftNavigationHeuristics   -> soft-nav supported: false
            // ```
            add_to_switch(command_line, "disable-blink-features", "SoftNavigationHeuristics");

            // A hedge, and named as one. It aims at the browser-side feature that owns the observer
            // that actually faults, which is the more direct target — but **nothing here proves it
            // does anything**: with `--disable-features=SoftNavigationDetection` and nothing else,
            // the page still reported `soft-navigation` as a supported entry type, so its only
            // evidence is that `SoftNavigationDetection` appears as a bare string in libcef, i.e.
            // that it is a real feature name. It stays because it costs one word in a list and
            // covers the case where something other than Blink's heuristic reaches
            // `PageLoadTracker::OnSoftNavigation`; it is not what the fix rests on.
            add_to_switch(command_line, "disable-features", "SoftNavigationDetection");
        }

        // --- M2 --------------------------------------------------------------------------------
        // Runs in every process: browser, renderer, GPU, zygote. Keep it pure — there is no state
        // to reach for out here, and a renderer that missed this call refuses to load bru:// at all.
        fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
            crate::chrome::register_scheme(registrar);
        }

        // --- M4 --------------------------------------------------------------------------------
        // Renderer process only. It must not touch browser-process state — same binary, different
        // process — and everything it needs is the renderer half of the message router.
        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(BruRenderProcessHandler::new())
        }
    }
}

// --- M4 ----------------------------------------------------------------------------------------
// The renderer side of the message router: three forwards, exactly as its trait documents. The
// router is a per-process singleton inside ipc.rs, because CEF may ask for this handler more than
// once and two routers in one renderer would each answer half the queries.
wrap_render_process_handler! {
    struct BruRenderProcessHandler;

    impl RenderProcessHandler {
        fn on_context_created(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            // --- src/greasemonkey.rs ----------------------------------------------------------
            // Kept before the router takes the frame, and *used* after it: userscripts must see
            // the same window the page sees, `window.cefQuery` included, or the check that a
            // userscript is refused exactly as a hostile page is would be testing nothing.
            let gm_frame = frame.as_deref().cloned();
            // --- end src/greasemonkey.rs ------------------------------------------------------
            crate::ipc::renderer_on_context_created(browser, frame, context);
            // --- src/greasemonkey.rs ----------------------------------------------------------
            // The one injection point for userscripts, and it is here rather than in a load
            // handler because this is `@run-at document-start`: the document's V8 context exists
            // and none of the page's own scripts have run. `document-end` and `document-idle` are
            // waited for inside the wrapper, which is what makes them right in a subframe too.
            crate::greasemonkey::renderer_on_context_created(gm_frame.as_ref());
            // --- end src/greasemonkey.rs ------------------------------------------------------
        }

        fn on_context_released(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            crate::ipc::renderer_on_context_released(browser, frame, context);
        }

        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> ::std::os::raw::c_int {
            // --- src/greasemonkey.rs ----------------------------------------------------------
            // Two messages the browser process sends this renderer: drop the script cache, and
            // evaluate a probe expression. Claimed here rather than in `ipc.rs` because neither is
            // a query a page could have started, so neither goes near the message router or the
            // `bru://`-only check that guards it.
            if crate::greasemonkey::renderer_on_message(frame.as_deref(), message.as_deref()) {
                return 1;
            }
            // --- end src/greasemonkey.rs ------------------------------------------------------
            crate::ipc::renderer_on_process_message_received(
                browser,
                frame,
                source_process,
                message,
            )
        }
    }
}

wrap_browser_process_handler! {
    struct BruBrowserProcessHandler {
        state: Arc<Mutex<BruState>>,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let command_line = command_line_get_global().expect("no global command line");

            // The factory has to exist before anything can ask for a bru:// URL, and the two chrome
            // views below are the first things to ask.
            crate::chrome::register_factory();

            // The bindings, before any browser exists to press a key at. `Config::load` compiles in
            // qutebrowser's defaults, then runs ~/.config/bru/config.lua over them if it is there.
            // The Lua state lives and dies inside that call: what comes back is plain tries of
            // parsed commands, and nothing Lua-shaped survives into the key path.
            let config = crate::config::Config::load();
            {
                let mut state = self.state.lock().expect("state mutex poisoned");
                // Kept whole as well as compiled into tries: `bru://help` lists what is bound, and
                // a trie cannot be read backwards.
                state.set_bindings(config.bindings.clone());
                state.set_parsers(config.into_parsers());
            }

            // --- src/settings.rs (merge: this block belongs to the settings workstream) ---------
            // What config.lua set, pushed into Chromium. `Config::into_parsers` above only stored
            // it — that function is run by unit tests with no browser process behind them, and a
            // content-settings call there would go through libcef before `initialize`. Here it is
            // safe, it is the UI thread, and it is still before the first tab exists: a start page
            // with JavaScript switched off has to load that way rather than load and be corrected.
            crate::settings::apply_at_startup();
            // --- end src/settings.rs ----------------------------------------------------------

            // The completion's four sources, bound to the modules that own them. After the line
            // above, which is what installs the search engines from config.lua.
            crate::completion::install(Box::new(crate::data::DataSources));

            // Four modules that were built in parallel and each left a hole where another one
            // belongs. Each hole was deliberate — a second `wl-copy` or a second download path
            // would have been the wrong kind of duplication — so this is where they are introduced
            // to each other, once, before any browser exists to press a key at.
            crate::hints::install_clipboard(Box::new(crate::clip::HintClipboard));
            crate::hints::install_downloads(Box::new(crate::clip::HintDownloads));
            crate::completers::install_clipboard(crate::clip::yank_plain);
            // The fifth hole, and the one that was still open: `spawn.rs` left a sink for whoever
            // built the message line, and nobody ever called it — so `:spawn`'s "started …", its
            // failures, and everything `-m` collected went to stderr, where nobody running a
            // browser is looking. `message.rs` is that message line, and this is the one line that
            // joins them.
            crate::spawn::set_message_sink(crate::message::info);
            // A cancelled popup becomes a tab, and the window it lands in is the window of the page
            // that asked — not whichever window happens to be in front. `popups.rs` cannot ask that
            // question itself (it knows only the opener browser's id, and `state.rs` is not its),
            // so it left this hook and `state.rs` left `window_of_browser`; this is the one line
            // that joins them. Without it a link clicked in a background window opens its tab in
            // the foreground one, which is the multi-window shape of the bug popups.rs just fixed.
            crate::popups::install_opener(|state, opener, url, background| {
                let window = state
                    .lock()
                    .expect("state mutex poisoned")
                    .window_of_browser(opener);
                if std::env::var_os("BRU_DEBUG_POPUPS").is_some() {
                    eprintln!("bru[popups]: opener browser {opener} is in window {window:?}");
                }
                match window {
                    Some(window) => crate::tabs::new_tab_in(state, window, url, background),
                    // The opener is gone — its window closed while the popup was in flight. The
                    // current window is the only honest answer left, and losing the URL is worse.
                    None => crate::tabs::new_tab(state, url, background),
                }
            });

            // What Enter in the command line actually runs. Without this the round trip completes
            // and the command is dropped on the floor — `:open -t abv.bg` would print "no command
            // runner installed" and do nothing.
            crate::cmdline::set_runner(crate::exec::run_from_cmdline);

            // CEF asks for the client again through default_client when it creates popups, and
            // handing out a fresh one each time loses the handlers. It goes in the shared state
            // rather than in this handler because CEF builds a new handler object per callback.
            self.state
                .lock()
                .expect("state mutex poisoned")
                .set_client(BruClient::new(self.state.clone()));

            // M9, DECISIONS item 7: the start page comes from config.lua when it sets one. It can
            // only be asked for after the block above, which is what installs it.
            let url = CefString::from(&command_line.switch_value(Some(&CefString::from("url"))))
                .to_string();
            let start_page = crate::open::start_page();
            let url = CefString::from(if url.is_empty() { start_page.as_str() } else { url.as_str() });

            // The first window, made by the same function `:open -w` uses — see `window::create`.
            // `FirstTab::Startup` is the one thing about it that is special: `--restore` may fill it
            // instead of the start page, and only the first window asks.
            crate::window::create(
                &self.state,
                crate::window::FirstTab::Startup(&url.to_string()),
            );

            // Debug hook, off unless asked for. See state::schedule_close.
            let close_after =
                CefString::from(&command_line.switch_value(Some(&CefString::from("close-after-ms"))))
                    .to_string();
            if let Ok(delay_ms) = close_after.parse::<i64>() {
                schedule_close(delay_ms);
            }

            // Debug hook, off unless asked for. See state::schedule_open.
            let open_text =
                CefString::from(&command_line.switch_value(Some(&CefString::from("open"))))
                    .to_string();
            if !open_text.is_empty() {
                let after_ms = CefString::from(
                    &command_line.switch_value(Some(&CefString::from("open-after-ms"))),
                )
                .to_string()
                .parse::<i64>()
                .unwrap_or(2000);
                crate::state::schedule_open(
                    &open_text,
                    command_line.has_switch(Some(&CefString::from("open-tab"))) == 1,
                    command_line.has_switch(Some(&CefString::from("open-bg"))) == 1,
                    after_ms,
                );
            }

            // Debug hook, off unless asked for. See state::schedule_tab_script.
            let script =
                CefString::from(&command_line.switch_value(Some(&CefString::from("tab-script"))))
                    .to_string();
            if !script.is_empty() {
                let step_ms = CefString::from(
                    &command_line.switch_value(Some(&CefString::from("tab-step-ms"))),
                )
                .to_string()
                .parse::<i64>()
                .unwrap_or(3000);
                crate::state::schedule_tab_script(&script, step_ms);
            }

            // Debug hook, off unless asked for. See exec::schedule_cmd_script — the general form of
            // --tab-script, running real command strings through the real dispatcher.
            let cmds = CefString::from(&command_line.switch_value(Some(&CefString::from("cmd"))))
                .to_string();
            if !cmds.is_empty() {
                let step_ms = CefString::from(
                    &command_line.switch_value(Some(&CefString::from("cmd-step-ms"))),
                )
                .to_string()
                .parse::<i64>()
                .unwrap_or(1500);
                crate::exec::schedule_cmd_script(&cmds, step_ms);
            }

            // Debug hook, off unless asked for. See scroll::schedule_script.
            let script =
                CefString::from(&command_line.switch_value(Some(&CefString::from("scroll-script"))))
                    .to_string();
            if !script.is_empty() {
                let step_ms = CefString::from(
                    &command_line.switch_value(Some(&CefString::from("scroll-step-ms"))),
                )
                .to_string()
                .parse::<i64>()
                .unwrap_or(800);
                crate::scroll::schedule_script(&script, step_ms);
            }

// --- src/history.rs --------------------------------------------------------
            // Debug hook, off unless asked for. See history::schedule_script — it is the only way to
            // read what the completion contains, which no other harness here can show.
            let script =
                CefString::from(&command_line.switch_value(Some(&CefString::from("history-script"))))
                    .to_string();
            if !script.is_empty() {
                let step_ms = CefString::from(
                    &command_line.switch_value(Some(&CefString::from("history-step-ms"))),
                )
                .to_string()
                .parse::<i64>()
                .unwrap_or(1000);
                crate::history::schedule_script(&script, step_ms);
            }
// --- end src/history.rs ----------------------------------------------------

            // --- sessions (merge: this block belongs to src/session.rs's workstream) ------------
            // Debug hook, off unless asked for. See session::schedule_script.
            let script =
                CefString::from(&command_line.switch_value(Some(&CefString::from("session-script"))))
                    .to_string();
            if !script.is_empty() {
                let step_ms = CefString::from(
                    &command_line.switch_value(Some(&CefString::from("session-step-ms"))),
                )
                .to_string()
                .parse::<i64>()
                .unwrap_or(2000);
                crate::session::schedule_script(&script, step_ms);
            }
            // --- end sessions -------------------------------------------------------------------

            // --- src/settings.rs (merge: this block belongs to the settings workstream) ---------
            // Debug hook, off unless asked for. See settings::schedule_probe — it reads what
            // Chromium answers for a setting at a URL, which is the only evidence that a `:set`
            // reached the engine rather than only bru's own store.
            let probe =
                CefString::from(&command_line.switch_value(Some(&CefString::from("settings-probe"))))
                    .to_string();
            if !probe.is_empty() {
                let after_ms = CefString::from(
                    &command_line.switch_value(Some(&CefString::from("settings-probe-after-ms"))),
                )
                .to_string()
                .parse::<i64>()
                .unwrap_or(5000);
                crate::settings::schedule_probe(&probe, after_ms);
            }
            // --- end src/settings.rs ----------------------------------------------------------

            // --- M12 (merge: this block belongs to src/hints.rs's workstream) --------------------
            // Debug hook, off unless asked for. See hints::schedule_hint_script.
            let script =
                CefString::from(&command_line.switch_value(Some(&CefString::from("hint-script"))))
                    .to_string();
            if !script.is_empty() {
                let step_ms = CefString::from(
                    &command_line.switch_value(Some(&CefString::from("hint-step-ms"))),
                )
                .to_string()
                .parse::<i64>()
                .unwrap_or(2000);
                crate::hints::schedule_hint_script(&script, step_ms);
            }

            // --- src/editor.rs --------------------------------------------------------------
            // Debug hook, off unless asked for. See editor::schedule_ask_script — the only way to
            // read a form field back and so the only way `edit-text` can be checked twice.
            let script =
                CefString::from(&command_line.switch_value(Some(&CefString::from("ask-script"))))
                    .to_string();
            if !script.is_empty() {
                let step_ms = CefString::from(
                    &command_line.switch_value(Some(&CefString::from("ask-step-ms"))),
                )
                .to_string()
                .parse::<i64>()
                .unwrap_or(1000);
                crate::editor::schedule_ask_script(&script, step_ms);
            }
            // --- end src/editor.rs ----------------------------------------------------------

            // --- src/caret.rs (merge: this block belongs to caret mode's workstream) -------------
            // Debug hook, off unless asked for. See caret::schedule_caret_script.
            let script =
                CefString::from(&command_line.switch_value(Some(&CefString::from("caret-script"))))
                    .to_string();
            if !script.is_empty() {
                let step_ms = CefString::from(
                    &command_line.switch_value(Some(&CefString::from("caret-step-ms"))),
                )
                .to_string()
                .parse::<i64>()
                .unwrap_or(1200);
                crate::caret::schedule_caret_script(&script, step_ms);
            }
            // --- end src/caret.rs ---------------------------------------------------------------

            // --- src/macros.rs (merge: this block belongs to the macros workstream) -------------
            // Debug hook, off unless asked for. See macros::schedule_macro_script.
            let script =
                CefString::from(&command_line.switch_value(Some(&CefString::from("macro-script"))))
                    .to_string();
            if !script.is_empty() {
                let step_ms = CefString::from(
                    &command_line.switch_value(Some(&CefString::from("macro-step-ms"))),
                )
                .to_string()
                .parse::<i64>()
                .unwrap_or(900);
                crate::macros::schedule_macro_script(&script, step_ms);
            }
            // --- end src/macros.rs --------------------------------------------------------------

            // --- src/greasemonkey.rs ------------------------------------------------------------
            // Debug hook, off unless asked for. See greasemonkey::schedule_probe — it evaluates an
            // expression in every tab and prints the answer, which is the only way to read what a
            // userscript did to a page without a screenshot, and the only way to prove that the
            // same script's `cefQuery` was refused.
            let probe =
                CefString::from(&command_line.switch_value(Some(&CefString::from("gm-probe"))))
                    .to_string();
            if !probe.is_empty() {
                let delays = CefString::from(
                    &command_line.switch_value(Some(&CefString::from("gm-probe-after-ms"))),
                )
                .to_string();
                let delays = if delays.is_empty() { "3000".to_string() } else { delays };
                crate::greasemonkey::schedule_probe(&probe, &delays);
            }
            // --- end src/greasemonkey.rs --------------------------------------------------------
        }

        fn default_client(&self) -> Option<Client> {
            self.state.lock().expect("state mutex poisoned").client()
        }
    }
}
