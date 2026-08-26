//! `bru-term <url>` — `bru --term <url>`, as one word.
//!
//! A launcher and nothing else: it finds the `bru` binary sitting beside itself and replaces this
//! process with it, `--term` prepended and every argument passed through. `exec` rather than a
//! child process, so there is exactly one bru in the process table, signals and the exit status
//! belong to the real browser, and the terminal session logic never learns a wrapper existed.
//!
//! Deliberately **not** baking in `--remote-debugging-port`: the port is a trust boundary the user
//! chooses at startup (`main.rs` writes down what it opens), and a wrapper that opened it silently
//! would be making that choice for them. `bru-term --remote-debugging-port=9222 <url>` says it out
//! loud and gets a dockable `:devtools` for it.

use std::os::unix::process::CommandExt;

fn main() {
    let me = std::env::current_exe().unwrap_or_else(|error| {
        eprintln!("bru-term: cannot find own path: {error}");
        std::process::exit(1);
    });
    // Beside this binary first — a build tree or an install prefix keeps the pair together — and
    // $PATH as the fallback for the odd layout that separates them.
    let sibling = me.parent().map(|dir| dir.join("bru"));
    let bru = match sibling.filter(|path| path.is_file()) {
        Some(path) => path,
        None => std::path::PathBuf::from("bru"),
    };
    let error = std::process::Command::new(&bru)
        .arg("--term")
        .args(std::env::args_os().skip(1))
        .exec();
    // `exec` only returns on failure.
    eprintln!("bru-term: could not run {}: {error}", bru.display());
    std::process::exit(1);
}
