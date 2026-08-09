//! bru — a keyboard-driven browser on CEF.
//!
//! One process image serves every Chromium role. CEF re-executes this same binary for its renderer,
//! GPU and zygote processes, distinguished by a `--type=` switch, so `execute_process` has to come
//! before anything else and the non-browser cases must return without initialising CEF.

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
mod popups;
// --- lua runtime -------------------------------------------------------------------------------
mod plugins;
// --- end lua runtime ---------------------------------------------------------------------------
mod profile;
mod prompt;
mod scroll;
mod scrollbar;
mod spawn;
mod session;
mod settings;
mod settingspage;
mod state;
mod tabs;
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

    // --- src/remote.rs --------------------------------------------------------------------------
    // **Read off the raw argv, before CEF sees the command line, and taking everything after it.**
    //
    // `bru --remote :open -t https://x` has to work: that is the shape `lvim-preview`'s `browser`
    // option produces — an argv list with the URL appended — and CEF's parser would take `-t` for a
    // switch of its own and the URL for a positional. So the rest of the line is the message,
    // joined with spaces, and nothing else on it is looked at.
    let raw: Vec<String> = std::env::args().collect();
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

    let settings = Settings {
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
        // either way. That rule describes the Alloy runtime; these BrowserViews are Chrome style,
        // where the profile is `<root_cache_path>/Default` on disk whatever this field says. So
        // there is nothing to switch on here, and a `--cache-path` switch would have been a name
        // with no behaviour behind it. What survives a restart is decided by `--private` above.
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
}
