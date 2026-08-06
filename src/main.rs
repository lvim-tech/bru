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
mod data;
mod downloads;
mod editor;
mod devtools;
mod exec;
mod favicon;
mod find;
mod help;
mod hints;
mod history;
mod ipc;
mod keys;
mod load;
mod macros;
mod message;
mod modes;
mod navigate;
mod open;
mod popups;
mod profile;
mod scroll;
mod spawn;
mod session;
mod settings;
mod state;
mod tabs;
mod window;

use cef::*;

fn main() -> Result<(), &'static str> {
    // Has to run before any other CEF call.
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

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

    // Said out loud rather than left to the name. A user who types `--private` and then finds the
    // site in the completion has been misled by one word; one line at startup is what keeps the
    // switch's promise the size it actually is.
    if private {
        if let Some(profile) = profile.as_ref() {
            eprintln!(
                "bru: --private: Chromium's profile is {} and is deleted when bru exits",
                profile.path().display()
            );
        }
        eprintln!("bru: --private: bru's own history, quickmarks and bookmarks are NOT affected");
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

    // After shutdown, so nothing is still writing to the directory being let go of.
    if let Some(profile) = profile {
        profile.release();
    }
    Ok(())
}