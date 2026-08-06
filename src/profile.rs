//! Which directory Chromium keeps its own state in — CEF-NOTES trap 10.
//!
//! `Settings.root_cache_path` left empty means `~/.config/cef_user_data`, shared with every other
//! CEF application on the machine, and Chromium says so on every start:
//!
//! ```text
//! WARNING:cef/libcef/common/resource_util.cc:83] Please customize CefSettings.root_cache_path
//! for your application. Use of the default value may lead to unintended process singleton
//! behavior.
//! ```
//!
//! "Unintended process singleton behavior" is not cosmetic. CEF takes a process-singleton lock on
//! that directory, so the *second* process to start dies: measured 2026-08-06, `initialize`
//! returned 0 with "Opening in existing browser session." on stderr and bru aborted on the assert
//! in `main.rs`, exit 101.
//!
//! So bru names its own directory, `~/.local/share/bru/cef`, beside the history and the
//! quickmarks — DESIGN.md's "bru owns its data" covers Chromium's half of it too.
//!
//! That alone would only move the collision: two brus would then fight over *that* directory
//! instead. The lock below is what stops them. It is bru's own, not Chromium's:
//!
//! - The first bru claims `~/.local/share/bru/cef` by creating a symlink `bru.lock` inside it
//!   pointing at its pid. `symlink(2)` fails with `EEXIST` rather than overwriting, so the claim is
//!   atomic and two brus starting in the same millisecond cannot both win it.
//! - A second bru finds the lock held by a live pid and takes `~/.local/share/bru/cef.<its pid>`
//!   instead, which nothing else can be using. It says so on stderr, because a browser quietly
//!   running on a different profile than the one you expect is worth a line.
//! - A lock left behind by a bru that crashed names a pid that is gone; it is replaced rather than
//!   obeyed. Chromium's own `SingletonLock` works the same way and for the same reason.
//!
//! A scratch directory is removed when that bru exits, and any belonging to a pid that no longer
//! exists are swept at startup, so a crash costs 5.9 MB once rather than 5.9 MB per run (measured
//! 2026-08-06 on a fresh profile — the shared 114 MB `cef_user_data` is years of accumulated
//! component downloads, not what one run writes).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether this run is a `--private` one.
///
/// A process-wide flag rather than an argument threaded through every caller, because the answer is
/// decided once, before `initialize`, and cannot change afterwards — and because the modules that
/// have to obey it ([`crate::data`] and [`crate::cmdline`]) are reached from CEF callbacks that own
/// no argument list of their own. It is set inside [`Profile::private`] rather than by `main`, so
/// that the switch and the behaviour cannot drift apart: taking a throwaway Chromium profile and
/// recording nothing of bru's own are one decision, not two.
static PRIVATE: AtomicBool = AtomicBool::new(false);

/// Whether nothing this run does may be written down.
///
/// **What it turns off is history, and only history:** bru's visit log, the completion table built
/// from it, and the command line's own `cmd-history`. A quickmark, a bookmark or a session is a
/// thing the user saved *by name*, and a browser that silently dropped a `:bookmark-add` because the
/// run happened to be private would be surprising in the other direction — the user asked for that
/// file to change. Chrome and Brave draw the line in the same place: an incognito window keeps no
/// history and still keeps a bookmark made in it.
///
/// It is deliberately **not** consulted on the read side. A private run still completes from the
/// history that is already there, still loads `cmd-history` into `<Ctrl-P>`, and still opens a
/// quickmark by name. Reading writes nothing. Forgetting the user's own marks as well would be
/// qutebrowser's `--temp-basedir`, which is a *debug* switch (`qutebrowser.py:115` registers `-T` in
/// the debug argument group) and works by pointing the whole basedir at `mkdtemp` and `rmtree`ing it
/// at exit (`app.py:68`, `quitter.py:247-250`) — so it throws the config away too and starts with no
/// bookmarks at all. That is a different tool with a different purpose.
pub fn is_private() -> bool {
    PRIVATE.load(Ordering::Relaxed)
}

/// The symlink bru claims a profile directory with. Chromium's own singleton file in the same
/// directory is `SingletonLock`; this one is deliberately a different name, because it has to be
/// held from before `initialize` — which is exactly when Chromium's does not exist yet.
const LOCK: &str = "bru.lock";

/// The directory handed to `Settings.root_cache_path`, and whether bru made it up for this run.
pub struct Profile {
    path: PathBuf,
    /// A directory that exists only because the shared one was taken. Removed by [`Profile::release`].
    scratch: bool,
}

impl Profile {
    /// Decide where this process keeps Chromium's state.
    ///
    /// `explicit` is `--user-data-dir`, which is honoured verbatim and neither locked nor swept: a
    /// caller naming a directory has taken responsibility for it. Everything else gets the shared
    /// directory, or a scratch one if the shared directory is in use.
    ///
    /// `None` means neither `$XDG_DATA_HOME` nor `$HOME` is set, and bru cannot name a directory at
    /// all. The caller leaves `root_cache_path` empty in that case, which is where this started.
    pub fn choose(explicit: Option<&str>) -> Option<Profile> {
        if let Some(dir) = explicit.filter(|dir| !dir.is_empty()) {
            return Some(Profile { path: PathBuf::from(dir), scratch: false });
        }

        let data_dir = crate::data::data_dir()?;
        sweep(&data_dir);

        let shared = data_dir.join("cef");
        if claim(&shared) {
            return Some(Profile { path: shared, scratch: false });
        }

        // `cef` → `cef.<pid>`. Nothing else on this machine can be using it, so no claim is needed.
        let scratch = shared.with_extension(std::process::id().to_string());
        std::fs::create_dir_all(&scratch).ok()?;
        eprintln!(
            "bru: another bru holds {}; this one is using {} and will remove it on exit",
            shared.display(),
            scratch.display()
        );
        Some(Profile { path: scratch, scratch: true })
    }

    /// `--private`: a profile directory that is deleted when this process exits.
    ///
    /// **What it covers, and what it does not.** Chromium writes cookies, `localStorage`, its own
    /// history and its own `Login Data` into `<root_cache_path>/Default` on every run — measured
    /// 2026-08-06, and *not* switched off by leaving `Settings.cache_path` empty the way
    /// `cef_types.h` promises (see `main.rs`). This makes that directory a throwaway one, so
    /// nothing Chromium wrote survives the run.
    ///
    /// It also raises [`PRIVATE`], which is what stops **bru's own** history: `data.rs` records no
    /// visit and `cmdline.rs` writes no `cmd-history`. See [`is_private`] for why quickmarks,
    /// bookmarks and sessions are still written.
    ///
    /// One honest limit remains: Chromium's bytes still reach the disk while bru is running, and a
    /// bru that is killed rather than closed leaves them until the next start sweeps them. This is a
    /// directory removed on exit, not an in-memory profile. A real off-the-record profile is a
    /// `RequestContext` per browser with an empty `cache_path`, which belongs to whoever owns
    /// browser creation.
    ///
    /// The directory is the same `cef.<pid>` shape [`Profile::choose`] falls back to, so [`sweep`]
    /// already collects one left by a crash, and [`Profile::release`] already deletes it. It is
    /// emptied first: a pid is reused eventually, and a private run must not open a dead run's
    /// cookies.
    pub fn private() -> Option<Profile> {
        // Before the directory, and unconditionally: if `data_dir` answers `None` there is nowhere
        // to write anyway, but a later `is_private()` that answered `false` because this line was
        // skipped would be the one failure mode worth ruling out by ordering.
        PRIVATE.store(true, Ordering::Relaxed);
        throwaway(&crate::data::data_dir()?)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Let the directory go: drop the lock this process holds, and delete a scratch directory
    /// entirely. Called after `shutdown()`, when nothing is writing to it any more.
    pub fn release(self) {
        if holder(&self.path) == Some(std::process::id()) {
            let _ = std::fs::remove_file(self.path.join(LOCK));
        }
        if self.scratch {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// The pure half of [`Profile::private`], so a test can drive it against a directory of its own
/// rather than against the `~/.local/share/bru` this machine is actually using.
fn throwaway(data_dir: &Path) -> Option<Profile> {
    sweep(data_dir);

    let dir = data_dir.join("cef").with_extension(std::process::id().to_string());
    // A pid is reused eventually. Whatever a dead run of the same pid left behind is not this
    // run's, and a private run opening it would be the one thing this switch exists to prevent.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    Some(Profile { path: dir, scratch: true })
}

/// Try to become the one process using `dir`.
///
/// The symlink is the whole mechanism: `symlink(2)` is atomic and refuses to overwrite, so the
/// success of that one call *is* the claim. Its target is the pid, so a lock nobody is behind can
/// be told from one that is.
fn claim(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let lock = dir.join(LOCK);
    let me = std::process::id().to_string();
    if std::os::unix::fs::symlink(&me, &lock).is_ok() {
        return true;
    }
    match holder(dir) {
        // Held, and by something that is still running.
        Some(pid) if is_alive(pid) => false,
        // A crash left it behind, or it is not a lock this code wrote. Either way nobody is behind
        // it: take it. Losing the race to replace it just means taking a scratch directory.
        _ => {
            let _ = std::fs::remove_file(&lock);
            std::os::unix::fs::symlink(&me, &lock).is_ok()
        }
    }
}

/// The pid named by `dir`'s lock, if it has one that reads as a pid.
fn holder(dir: &Path) -> Option<u32> {
    std::fs::read_link(dir.join(LOCK))
        .ok()?
        .to_str()?
        .parse()
        .ok()
}

/// Whether a pid is still running. `/proc/<pid>` rather than `kill(pid, 0)` so that no signal is
/// sent and no `libc` call is needed; bru is Linux-only and `/proc` is not optional here.
fn is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).is_dir()
}

/// Remove scratch profiles left behind by brus that are gone.
///
/// Deliberately narrow: only entries of `data_dir` itself, only real directories (never symlinks),
/// only names of the exact form `cef.<digits>`, and only when no process holds that pid. The shared
/// `cef` is named `cef` and can never match.
fn sweep(data_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.strip_prefix("cef.")) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if is_alive(pid) {
            continue;
        }
        // `file_type` does not follow symlinks, so a symlink named `cef.1` is not a directory here
        // and is left alone rather than followed and emptied.
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch directory of this test run's own, never the user's `~/.local/share/bru`.
    struct TempDir {
        dir: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> TempDir {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("bru-profile-test-{}-{n}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("could not make the test directory");
            TempDir { dir }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A pid that is certainly not running: one above the machine's maximum.
    fn dead_pid() -> u32 {
        std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .ok()
            .and_then(|max| max.trim().parse::<u32>().ok())
            .unwrap_or(4_194_304)
            + 1
    }

    #[test]
    fn the_first_claim_wins_and_the_second_does_not() {
        let t = TempDir::new("claim");
        let dir = t.dir.join("cef");
        assert!(claim(&dir), "an unclaimed directory must be claimable");
        assert_eq!(holder(&dir), Some(std::process::id()));
        // The second call is this same process, but the lock does not know that: what it refuses is
        // a *live* holder, which is exactly what a second bru would find.
        assert!(
            !claim(&dir),
            "a directory locked by a live pid must not be claimable again"
        );
    }

    #[test]
    fn a_lock_left_by_a_dead_process_is_taken_over() {
        let t = TempDir::new("stale");
        let dir = t.dir.join("cef");
        std::fs::create_dir_all(&dir).unwrap();
        let dead = dead_pid();
        std::os::unix::fs::symlink(dead.to_string(), dir.join(LOCK)).unwrap();
        assert!(!is_alive(dead));

        assert!(claim(&dir), "a lock nobody is behind must be replaceable");
        assert_eq!(holder(&dir), Some(std::process::id()));
    }

    #[test]
    fn releasing_drops_the_lock_and_deletes_only_a_scratch_directory() {
        let t = TempDir::new("release");

        let shared = t.dir.join("cef");
        assert!(claim(&shared));
        Profile { path: shared.clone(), scratch: false }.release();
        assert!(shared.exists(), "the shared profile survives its process");
        assert_eq!(holder(&shared), None, "and its lock is gone");

        let scratch = t.dir.join("cef.999999");
        std::fs::create_dir_all(&scratch).unwrap();
        Profile { path: scratch.clone(), scratch: true }.release();
        assert!(!scratch.exists(), "a scratch profile does not survive its process");
    }

    #[test]
    fn sweeping_removes_only_dead_scratch_profiles() {
        let t = TempDir::new("sweep");

        let shared = t.dir.join("cef");
        let dead = t.dir.join(format!("cef.{}", dead_pid()));
        let live = t.dir.join(format!("cef.{}", std::process::id()));
        let history = t.dir.join("history.sqlite");
        let odd = t.dir.join("cef.notanumber");
        for dir in [&shared, &dead, &live, &odd] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(&history, b"not a directory").unwrap();

        sweep(&t.dir);

        assert!(!dead.exists(), "a scratch profile of a dead pid should be swept");
        assert!(shared.exists(), "the shared profile is not a scratch profile");
        assert!(live.exists(), "a scratch profile in use must survive");
        assert!(odd.exists(), "only cef.<digits> is a scratch profile");
        assert!(history.exists(), "nothing else in the data directory is touched");
    }

    /// `--private` promises that nothing Chromium wrote outlives the run, and the only thing
    /// standing behind that promise is that the directory is scratch and starts empty.
    #[test]
    fn a_private_profile_starts_empty_and_does_not_survive_its_process() {
        let t = TempDir::new("private");

        // A dead run of this very pid left a cookie database behind. It must not be inherited.
        let stale = t.dir.join(format!("cef.{}", std::process::id()));
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("Cookies"), b"a previous run's cookies").unwrap();

        let profile = throwaway(&t.dir).expect("a private profile");
        assert_eq!(profile.path(), stale, "the throwaway directory is cef.<pid>");
        assert!(profile.path().exists());
        assert!(
            !profile.path().join("Cookies").exists(),
            "a private profile must not open what a dead run of the same pid left"
        );

        // And the shared profile is untouched: --private must never delete the real one.
        let shared = t.dir.join("cef");
        std::fs::create_dir_all(&shared).unwrap();

        profile.release();
        assert!(!stale.exists(), "a private profile does not survive its process");
        assert!(shared.exists(), "--private leaves the persistent profile alone");
    }

    #[test]
    fn an_explicit_directory_is_taken_verbatim_and_never_swept() {
        let t = TempDir::new("explicit");
        let named = t.dir.join("somewhere/else");
        let profile = Profile::choose(Some(named.to_str().unwrap())).expect("an explicit path");
        assert_eq!(profile.path(), named);
        // Not created, not locked, not scratch: --user-data-dir belongs to whoever passed it, and
        // release must not delete it.
        assert!(!named.exists());
        profile.release();
        assert!(!named.exists());
    }
}
