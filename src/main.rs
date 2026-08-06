//! bru — a keyboard-driven browser on CEF.
//!
//! One process image serves every Chromium role. CEF re-executes this same binary for its renderer,
//! GPU and zygote processes, distinguished by a `--type=` switch, so `execute_process` has to come
//! before anything else and the non-browser cases must return without initialising CEF.

mod app;
mod bindings;
mod commands;
mod config;
mod keys;
mod modes;
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
    let ret = execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());

    if !is_browser_process {
        // A renderer, GPU or zygote process. execute_process ran its loop and is done; initialising
        // CEF here would start a second browser.
        assert!(ret >= 0, "child process could not execute");
        return Ok(());
    }
    assert_eq!(ret, -1, "browser process could not execute");

    let mut app = app::BruApp::new();
    let settings = Settings {
        // The sandbox needs a setuid helper installed by root. Off until bru is packaged; the
        // Chromium sandbox is worth having back before this is used for anything real.
        no_sandbox: 1,
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
    Ok(())
}
