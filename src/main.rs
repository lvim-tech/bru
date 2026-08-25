//! bru — a keyboard-driven browser on CEF.
//!
//! One process image serves every Chromium role. CEF re-executes this same binary for its renderer,
//! GPU and zygote processes, distinguished by a `--type=` switch, so `execute_process` has to come
//! before anything else and the non-browser cases must return without initialising CEF.

// **The nested `if let` is the style here, and it is a choice rather than an omission.** Clippy would
// have each of them collapsed into an edition-2024 let-chain; what that buys is one line fewer and
// what it costs is a longer one — two conditions and a `&&` on a line that already carries a pattern
// — plus a reflow of the block under it. With formatting authored rather than generated (see
// `rustfmt.toml`, which turns rustfmt off for the same reason) nothing tidies up after that collapse:
// it is a hand reindent of every block, across the adblock, cookies, ipc and password paths among
// others, for no correctness gain at all.
//
// Said once, at the crate root, so that `cargo clippy --all-targets -- -D warnings` is clean and can
// be run in CI — which is the whole point of not leaving 37 warnings standing. It is the only lint
// bru allows away, and a new one has to earn its line here the same way.
//
// One attribute covers the tree: bru is a single binary crate — `Cargo.toml` declares `[[bin]]` and
// no `[lib]` — so there is no `lib.rs` needing the same.
#![allow(clippy::collapsible_if)]

mod adblock;
mod app;
mod bindings;
mod caret;
mod chrome;
mod clip;
mod cmdline;
mod commands;
mod completers;
mod completion;
mod config;
mod cookies;
mod csp;
mod data;
mod downloads;
mod editor;
mod devtools;
// --- plugin events ------------------------------------------------------------------------------
mod events;
// --- end plugin events --------------------------------------------------------------------------
mod exec;
mod favicon;
mod find;
mod focus;
mod greasemonkey;
mod help;
mod hints;
mod history;
mod ipc;
mod keys;
// The two moments that are not about any page: the browser started, and it is going away.
mod lifetime;
mod load;
// The second file allowed to mention `mlua`: the shared state, the plugin registry over it, the
// handles a function-valued setting holds, and the `bru.on` that `events.rs` registers through.
mod lua;
mod macros;
mod message;
mod modes;
mod navigate;
mod open;
// Filling a website password from whatever manager the config names. The secret never enters
// Lua, argv, the command grammar or a file bru writes — see the module head.
mod passwords;
mod popups;
// --- lua runtime -------------------------------------------------------------------------------
mod plugins;
// --- end lua runtime ---------------------------------------------------------------------------
mod profile;
mod prompt;
mod scroll;
mod scrollbar;
mod spawn;
// `--ssh=<destination>`: an `ssh -D` SOCKS tunnel, and Chromium pointed at it.
mod ssh;
// How a window is drawn, and everything only that answer owns. The terminal frontend's seam.
mod shell;
mod session;
mod settings;
mod settingspage;
mod state;
mod tabs;
// A spike: the kitty graphics protocol and what this pane costs. `--term-probe` only.
mod term;
// The terminal frontend, phase by phase. Each is a module of its own so the phases do not collide.
mod term_compose;
mod term_keys;
mod term_paint;
mod term_session;
// A spike: one windowless browser painted into the terminal. `--term-spike=<url>` only.
mod term_spike;
// Which terminal bru was launched from, and how to ask it for a pane. Used by `:spawn --split`.
mod terminal;
// How bru learns that ~/.config/bru/theme.css has been rewritten under it.
mod theme_watch;
// `bru --remote <line>` — the one door into a running browser from outside it.
mod remote;
// Per-site CSS from ~/.config/bru/styles/<domain>/.
mod userstyles;
mod utilcmds;
mod window;

use cef::*;

fn main() -> Result<(), &'static str> {
    // Has to run before any other CEF call.
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let raw: Vec<String> = std::env::args().collect();

    // --- src/term.rs ------------------------------------------------------------------------------
    // **First, because this run is not a browser and must not be mistaken for one.**
    //
    // Measured 2026-08-25, by getting it wrong: with the check further down, `bru --term-probe`
    // reached `handover()` a few lines below, matched none of its refusals, and handed `open -w` to
    // the browser that was already running — so the probe printed nothing, exited 0, and opened a
    // window in somebody's browser instead. That is the same failure `--remote-debugging-port` had
    // and it is the shape of the trap: **every switch that means "this process does something other
    // than be a browser" has to be answered before the handover, not after it.** `--remote` is
    // above for the same reason.
    if raw.iter().any(|arg| arg == "--term-probe") {
        return match term::probe() {
            Ok(()) => Ok(()),
            Err(why) => {
                eprintln!("bru: --term-probe: {why}");
                Err("the terminal probe failed")
            }
        };
    }
    // --- end src/term.rs --------------------------------------------------------------------------

    // --- src/remote.rs --------------------------------------------------------------------------
    // **Read off the raw argv, before CEF sees the command line, and taking everything after it.**
    //
    // `bru --remote :open -t https://x` has to work: that is the shape `lvim-preview`'s `browser`
    // option produces — an argv list with the URL appended — and CEF's parser would take `-t` for a
    // switch of its own and the URL for a positional. So the rest of the line is the message,
    // joined with spaces, and nothing else on it is looked at.
    if let Some(at) = raw.iter().position(|arg| arg == "--remote") {
        let line = raw[at + 1..].join(" ");
        return match remote::send(&line) {
            Ok(()) => Ok(()),
            Err(why) => {
                eprintln!("bru: {why}");
                Err("the remote call failed")
            }
        };
    }
    // **A second bru hands its page to the first and exits**, which is what every other browser
    // does and what `xdg-open`, a `.desktop` entry and `BROWSER=bru` all assume. Before this, a
    // second start ran a whole second browser — `remote.rs` called that "the honest behaviour",
    // and it is honest for a browser somebody starts on purpose and wrong for a link somebody
    // clicked. The escape hatch is spelled out rather than implied: `--new-instance`, or a
    // `--socket=` of one's own, which is what the tests and a scratch browser already use.
    //
    // It runs here, before CEF is initialised, so a handed-over link costs a socket write and an
    // exit rather than a browser's worth of startup.
    if let Some(line) = handover(&raw) {
        if remote::send(&line).is_ok() {
            return Ok(());
        }
        // Nothing was listening, or it would not take it. Be the browser.
    }
    // --- end src/remote.rs ----------------------------------------------------------------------


    let args = args::Args::new();
    let Some(cmd_line) = args.as_cmd_line() else {
        return Err("could not parse the command line");
    };

    let is_browser_process = cmd_line.has_switch(Some(&CefString::from("type"))) != 1;


    // The same App object goes to both execute_process and initialize. execute_process is what
    // gives the child processes an App at all, and two callbacks are only reachable that way:
    // on_register_custom_schemes, which has to run in every process for bru:// to be a real origin
    // in the renderer, and render_process_handler, which only exists there. The state the App
    // carries is browser-process state; in a child it is constructed and never filled in.
    let mut app = app::BruApp::new(state::BruState::new());
    let ret = execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );

    if !is_browser_process {
        // A renderer, GPU or zygote process. execute_process ran its loop and is done; initialising
        // CEF here would start a second browser.
        assert!(ret >= 0, "child process could not execute");
        return Ok(());
    }
    assert_eq!(ret, -1, "browser process could not execute");

    // Where Chromium keeps its own state. Left empty this is `~/.config/cef_user_data`, shared with
    // every other CEF application on the machine and singleton-locked, so the second bru to start
    // died on the assert below with "Opening in existing browser session." — CEF-NOTES trap 10.
    // `profile.rs` names bru's own directory and, when another bru already holds it, one that
    // nothing else can be using.
    //
    // `--private` asks for a directory that is deleted when this process exits, so a run's cookies
    // and logins do not outlive it. See `profile::Profile::private` for what that does and does not
    // cover.
    let private = cmd_line.has_switch(Some(&CefString::from("private"))) == 1;
    let user_data_dir =
        CefString::from(&cmd_line.switch_value(Some(&CefString::from("user-data-dir")))).to_string();
    let profile = if private {
        profile::Profile::private()
    } else {
        profile::Profile::choose(Some(user_data_dir.as_str()))
    };

    // Said out loud rather than left to the name, because the switch covers two different stores and
    // a user is owed the boundary between them. The second line used to read "bru's own history,
    // quickmarks and bookmarks are NOT affected", which was true and was the reason to finish the
    // job: a switch that needs a disclaimer to be honest is not finished. `data.rs` now records no
    // visit and `cmdline.rs` writes no `cmd-history` under `--private`, so the line describes what
    // is kept rather than apologising for it — a quickmark, bookmark or session is a thing the user
    // saved by name, and dropping one silently would be the opposite surprise
    // (`profile::is_private`).
    if private {
        if let Some(profile) = profile.as_ref() {
            eprintln!(
                "bru: --private: Chromium's profile is {} and is deleted when bru exits",
                profile.path().display()
            );
        }
        eprintln!(
            "bru: --private: no page reaches bru's history and no command line is saved; \
             a quickmark, bookmark or session you save by name still is"
        );
    }

    // --- src/remote.rs: the other door ---------------------------------------------------------
    // **`--remote-debugging-port` costs bru no code, and that is exactly why it is owed this.** CEF
    // reads the switch off the argv `args::Args` hands it — the field behind it is
    // `remote_debugging_port` in `cef_types.h` — so the port opens with nothing here asking for it.
    // Measured 2026-08-25 against `Chrome/151.0.7922.72`: `Runtime.evaluate`, `Page.navigate`,
    // `Page.captureScreenshot`, `Accessibility.getFullAXTree` and `Input.dispatchKeyEvent` each
    // drive a live bru. A door that wide is owed the paragraph `remote.rs` writes for its socket
    // ("What it is not"), and three of its facts are not ones a reader should have to infer:
    //
    // - **It is a wider boundary than `--remote`'s, not the same one.** That socket sits in
    //   `$XDG_RUNTIME_DIR`, which the kernel gives this user alone at mode 0700, and is itself
    //   0600. This is a loopback TCP port, and *every local account* can connect to 127.0.0.1.
    //   Measured on this machine 2026-08-25 with the port open: `/proc/net/tcp` holds one listener
    //   for it and its local address is `0100007F`, i.e. 127.0.0.1, and a connection to the same
    //   port on this host's LAN address is refused. Loopback-only is Chromium's default, not
    //   something bru sets — `--remote-debugging-address` is honoured by headless Chromium, which
    //   CEF is not, so there is no switch here to get it wrong with.
    // - **A CDP client is bru, not a page inside it.** `ipc.rs` refuses `cefQuery` from any frame
    //   that is not a `bru://` page, and `Runtime.evaluate` aimed at a chrome target does not walk
    //   around that check — it *satisfies* it, because the injected code genuinely runs in a
    //   `bru://` frame. There is no second boundary inside the port.
    // - **The chrome pages are on the list and cannot be taken off it.** `/json/list` answers with
    //   four targets: the page, plus `bru://chrome/top.html`, `bottom.html` and `panel.html`. CEF
    //   151 exposes no target-filtering hook — the whole remote-debugging surface it offers is that
    //   one settings field — so a client has to pick its target by URL or it will drive the tab
    //   strip by accident. Said in the README beside the example, because that is where somebody
    //   writing the client is looking.
    //
    // Said out loud at startup for the same reason `--private` is: a browser behaving differently
    // from the one a person thinks they started is worth a line.
    let debugging_port =
        CefString::from(&cmd_line.switch_value(Some(&CefString::from("remote-debugging-port"))))
            .to_string();
    if cmd_line.has_switch(Some(&CefString::from("remote-debugging-port"))) == 1 {
        // `0` is Chromium's "pick one", and it writes the number it picked to `DevToolsActivePort`
        // in the profile rather than to this line, so the line must not pretend to know it.
        let where_to_look = if debugging_port.trim() == "0" {
            " (0: Chromium picks the port and writes it to DevToolsActivePort in the profile)"
        } else {
            ""
        };
        eprintln!(
            "bru: --remote-debugging-port={debugging_port}: the DevTools protocol is \
             listening on 127.0.0.1{where_to_look}"
        );
        eprintln!(
            "bru: --remote-debugging-port: anything that can reach that port drives this \
             browser, bru's own bru:// chrome pages included; every local account can reach \
             127.0.0.1"
        );
    }
    // --- end src/remote.rs: the other door -----------------------------------------------------

    // --- src/ssh.rs -----------------------------------------------------------------------------
    // **Before `initialize`, because the switch it produces has to be on the command line CEF is
    // about to read**, and after the subprocess check above, because a renderer that started its
    // own ssh would be one tunnel per process and one password prompt per tab.
    //
    // A failure here **ends the run**. A person who asked to browse through a tunnel and got a
    // browser that quietly went out of this machine's own interface has been given the one thing
    // they were trying to avoid — so the rule is the same one `--private` follows: the switch
    // either means what it says or bru does not start.
    if let Some(destination) = ssh::destination_from(&raw) {
        if let Err(why) = ssh::start(destination) {
            eprintln!("bru: --ssh={destination}: {why}");
            // **The scratch profile is let go of here or it is not let go of at all.** It was made
            // a few lines up and the release at the end of `main` is past this return, so a
            // `--private` run whose tunnel failed used to leave its directory behind — measured
            // 2026-08-25 against `--ssh=nowhere.invalid`, which left 66 MB of `cef.<pid>` for the
            // next start's sweep to find. CEF has not been initialised, so nothing else is holding
            // it.
            if let Some(profile) = profile {
                profile.release();
            }
            return Err("the ssh tunnel could not be started");
        }
    }
    // --- end src/ssh.rs -------------------------------------------------------------------------

    // --- src/term_spike.rs --------------------------------------------------------------------
    // **Only under the switch, because the header says not to enable it otherwise**
    // (`cef_types.h`: "Do not enable this value if the application does not use windowless
    // rendering"). It has to be decided here: `initialize` consumes it, and a browser cannot be
    // made windowless later by asking nicely.
    let windowless = i32::from(term_spike::url_from(&raw).is_some());
    // --- end src/term_spike.rs ----------------------------------------------------------------

    let settings = Settings {
        windowless_rendering_enabled: windowless,
        // The sandbox needs a setuid helper installed by root. Off until bru is packaged; the
        // Chromium sandbox is worth having back before this is used for anything real.
        no_sandbox: 1,
        // `cache_path` is empty, and **that does not mean what `cef_types.h` says it means.** The
        // header promises "browsers will be created in incognito mode where in-memory caches are
        // used for storage and no profile-specific data is persisted to disk". Measured 2026-08-06
        // on CEF 151, against httpbin.org, with a scratch `--user-data-dir`: a cookie set with
        // `max-age=86400` and a `localStorage` key both came back after a full restart, and
        // `<root>/Default/Cookies` is a real SQLite file holding the row (`is_persistent = 1`).
        // Setting `cache_path` to the root as well was measured too and changed nothing at all —
        // the two profile trees differed only in a blob UUID and one cache entry's name, 4.9 MB
        // either way. The header attributes that rule to the Alloy runtime, and **these
        // BrowserViews are Alloy style** — `window.rs:1128` sets `RuntimeStyle::ALLOY` and says
        // why — so the promise does not hold even where the header claims it does. The profile is
        // `<root_cache_path>/Default` on disk whatever this field says. So there is nothing to
        // switch on here, and a `--cache-path` switch would have been a name with no behaviour
        // behind it. What survives a restart is decided by `--private` above.
        root_cache_path: profile
            .as_ref()
            .map(|profile| CefString::from(profile.path().to_string_lossy().as_ref()))
            .unwrap_or_default(),
        ..Default::default()
    };

    assert_eq!(
        initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut(),
        ),
        1,
        "CEF could not initialise"
    );


    run_message_loop();
    shutdown();
    crate::remote::cleanup();
    // bru started this process, so bru ends it — see `ssh::stop`. After `shutdown`, so nothing is
    // still trying to load a page through it.
    crate::ssh::stop();

    // After shutdown, so nothing is still writing to the directory being let go of.
    if let Some(profile) = profile {
        profile.release();
    }
    Ok(())
}
// --- src/remote.rs ------------------------------------------------------------------------------
/// The command a second bru sends to the first, or `None` when this bru must be a browser itself.
///
/// **The subprocess check is first and is not optional.** CEF starts renderers, GPU and zygote
/// processes by re-executing this binary with `--type=`, and their command lines are full of
/// Chromium's own arguments. A zygote that handed a "page" to the running browser and exited would
/// take the browser it belongs to down with it.
///
/// `--new-instance` is the way to say no, and `--socket=` is the other: a browser on a socket of its
/// own is asking for a browser of its own, and nothing is listening there to hand anything to.
///
/// With no page named, the handover is `open -w` — a new window in the browser that is running,
/// which is what clicking a browser's icon does everywhere else. Doing nothing would leave a person
/// clicking an icon and watching nothing happen.
fn handover(args: &[String]) -> Option<String> {
    for arg in args {
        if arg == "--type" || arg.starts_with("--type=") {
            return None;
        }
        if arg == "--new-instance" || arg == "--remote" {
            return None;
        }
        // **The two switches that ask for a browser of their own, and used to be eaten here.**
        // `--socket=` needs no arm because nothing is listening on a socket of one's own, so the
        // send below fails and this bru becomes the browser (see the test). These two have no such
        // luck: measured 2026-08-25, `bru --remote-debugging-port=9222 https://x` with a bru
        // already running handed the page over and exited, so the port was never opened and
        // nothing said so — the person asked for a debuggable browser and got a tab in an
        // undebuggable one. `--ssh` is the same shape and worse: the page would load in a browser
        // that is not going through the tunnel that was asked for.
        if arg == "--remote-debugging-port" || arg.starts_with("--remote-debugging-port=") {
            return None;
        }
        if arg == "--ssh" || arg.starts_with("--ssh=") {
            return None;
        }
        // The terminal spike is a browser of its own by construction: it paints into *this* pane.
        if arg.starts_with("--term-spike") {
            return None;
        }
    }

    // `--url=` means the same thing here as it does to `app.rs`, and wins for the same reason.
    if let Some(url) = args.iter().find_map(|arg| arg.strip_prefix("--url=")) {
        let url = url.trim();
        if !url.is_empty() {
            return Some(format!("open -t {url}"));
        }
    }

    // The first bare argument, skipping this binary's own name.
    match args
        .iter()
        .skip(1)
        .find(|arg| !arg.starts_with('-') && !arg.trim().is_empty())
    {
        Some(url) => Some(format!("open -t {}", url.trim())),
        None => Some("open -w".to_string()),
    }
}
// --- end src/remote.rs --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::handover;

    fn argv(rest: &[&str]) -> Vec<String> {
        std::iter::once("bru".to_string())
            .chain(rest.iter().map(|arg| arg.to_string()))
            .collect()
    }

    /// What a clicked link, a menu entry and `xdg-open` each produce.
    #[test]
    fn a_second_bru_hands_its_page_over() {
        assert_eq!(
            handover(&argv(&["https://example.com/"])).as_deref(),
            Some("open -t https://example.com/"),
        );
        assert_eq!(
            handover(&argv(&["--url=https://example.com/"])).as_deref(),
            Some("open -t https://example.com/"),
        );
        // Clicking the icon with a browser already running opens a window in it, which is what
        // every other browser does. Nothing at all would look like a broken launcher.
        assert_eq!(handover(&argv(&[])).as_deref(), Some("open -w"));
    }

    /// **The check that keeps this from killing the browser it belongs to.** CEF re-executes this
    /// binary for every renderer, GPU and zygote process, with `--type=` and a command line of
    /// Chromium's own. One of those handing a page over and exiting would take the tab with it.
    #[test]
    fn a_subprocess_never_hands_anything_over() {
        assert_eq!(handover(&argv(&["--type=renderer"])), None);
        assert_eq!(handover(&argv(&["--type=zygote", "https://example.com/"])), None);
        assert_eq!(handover(&argv(&["--type"])), None);
    }

    /// Two ways to ask for a browser of your own, and both are spelled out rather than implied.
    #[test]
    fn asking_for_a_browser_of_your_own_is_honoured() {
        assert_eq!(handover(&argv(&["--new-instance"])), None);
        assert_eq!(handover(&argv(&["--new-instance", "https://example.com/"])), None);
        // `--remote` is the other client entirely, and `main` has already answered it by here.
        assert_eq!(handover(&argv(&["--remote", "tabs"])), None);
        // `--socket=` needs no arm: nothing is listening on a socket of one's own, so the send
        // fails and this bru becomes the browser. That is asserted by the shape of `main`, not
        // here, and it is why a scratch browser still works.
        assert!(handover(&argv(&["--socket=/run/user/1000/x.sock"])).is_some());
    }

    /// **A switch whose whole point is this process must not be answered by another one.** Both of
    /// these used to fall through to the handover and die without a word — see the comment beside
    /// their arms. A URL alongside them changes nothing: the URL is why a person reaches for the
    /// switch in the first place.
    #[test]
    fn a_debugging_port_or_a_tunnel_is_a_browser_of_your_own() {
        assert_eq!(handover(&argv(&["--remote-debugging-port=9222"])), None);
        assert_eq!(
            handover(&argv(&["--remote-debugging-port=9222", "https://example.com/"])),
            None
        );
        // Chromium spells its switches with `=`; the bare form is what a typo looks like, and it
        // has to refuse too rather than hand the page over on the way to failing.
        assert_eq!(handover(&argv(&["--remote-debugging-port"])), None);
        assert_eq!(handover(&argv(&["--ssh=user@host"])), None);
        assert_eq!(handover(&argv(&["--ssh=user@host", "https://example.com/"])), None);
        assert_eq!(handover(&argv(&["--ssh"])), None);
    }

    /// The prefix match is a match on *these* switches and not on everything that starts like them.
    /// `--remote` is already refused by its own arm; the point here is that a longer name bru does
    /// not know stays a page hand-over rather than silently becoming a second browser.
    #[test]
    fn a_switch_that_merely_starts_the_same_is_not_one_of_them() {
        assert!(handover(&argv(&["--remote-debugging-pipe", "https://example.com/"])).is_some());
        assert!(handover(&argv(&["--sshfs=/mnt/x", "https://example.com/"])).is_some());
    }
}
