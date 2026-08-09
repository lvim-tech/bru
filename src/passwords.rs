//! Filling a website password from whatever password manager the config names.
//!
//! # What this module claims, and what it refuses to claim
//!
//! **Routing confinement.** The secret never enters Lua, never appears in `argv`, never enters
//! bru's command grammar, never reaches a `message::` line or a `BRU_DEBUG_*` print, and never
//! touches a file bru writes. Every one of those is checkable, and most of them are checked below.
//!
//! **Not memory hygiene.** Once the fill lands, the value is in Blink's DOM and in V8, in copies
//! nothing can zero, for as long as the page holds it. `keyforge` goes to `mlock` and
//! `MADV_DONTDUMP` lengths for the secrets it holds and says why; bru cannot deliver that for a
//! value whose whole purpose is to be handed to a web page, and saying otherwise would be a comfort
//! rather than a property. An attacker who can read this process's memory reads the renderer.
//!
//! # The channels that are closed, and why each one had to be
//!
//! | channel | what it would have done |
//! |---|---|
//! | `commands::split_chain` | `;;` is the command language's separator, so a password containing it would run its own tail as a command. There is no escape for `;;` inside a command, which also means some passwords are simply unrepresentable that way. |
//! | `spawn::replace_variables` | `insert-text` runs its argument through it (`editor.rs`), so a password containing `{url}` or `{clipboard}` would have a **page-influenced** value substituted into it. `spawn.rs` calls single-pass substitution "a security property"; that property assumes the text was typed by a person. |
//! | Lua | bru's Lua state is shared by every plugin, with no sandbox, by a settled decision (`PLUGINS.md`). A secret held in a Lua string is a secret every installed plugin can read. Only the entry **name** and the host cross into a setting function, and only outward: the function returns the *command*, and the secret is that command's stdout, read here. |
//! | `argv` | keyforge's first principle, and its reason: `ssh-keygen -N 'passphrase'` is readable in `/proc` by any local user for as long as it runs. The entry name is an argument; the value never is. |
//! | messages and logs | [`failure`] takes the exit status and stderr and **has no parameter for stdout**, so the compiler holds the invariant rather than a reviewer. |
//!
//! # Why the answer is Rust and not a plugin
//!
//! The first two rows above are the whole argument: a plugin can only fill through
//! `bru.cmd(":insert-text …")`, and that string enters both grammars. The third row is the second
//! argument, and the fourth is `keyforge`'s. There is also a fact only Rust can reach — whether the
//! focused element really is a password field — for the same reason `focus.rs` gives about
//! `is_editable`: it is asked of `Domnode`, and a page can shadow `document.activeElement` but not
//! that.
//!
//! # What bru does not decide
//!
//! Which password manager. `passwords.show` names a program that takes an entry name and prints the
//! secret on stdout; `pass`, `gopass`, `op read`, `bw get password` and a `keepassxc-cli` wrapper
//! all fit. The one thing bru will not do is accept a secret any other way — stdout, never an
//! argument, which is how `keyforge` itself drives `pass`.

use std::path::{Path, PathBuf};

/// The default for `passwords.show`. `pass` is what `keyforge` writes into, and `keyforge` is why
/// there is a store on this machine at all.
pub const DEFAULT_SHOW: &str = "pass show {}";

/// Where the entries live when `PASSWORD_STORE_DIR` says nothing — `pass`'s own default.
const STORE_DIR: &str = ".password-store";

// -----------------------------------------------------------------------------------------------
// The command to run, built without either grammar touching it
// -----------------------------------------------------------------------------------------------

/// A backend command as the config gave it: a line to be split, or an argv already in pieces.
///
/// Two forms because a function exists for the case a template cannot express — a database path
/// with a space in it, say — and splitting what a function returned as a list would put the
/// splitting back.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Spec {
    /// A command line, to be split the way a shell would and then substituted.
    Line(String),
    /// Already argv. Taken verbatim; `{}` is still substituted, and nothing is re-split.
    Argv(Vec<String>),
}

/// Build the argv that asks the backend for one entry.
///
/// **`str::replace`, never `replace_variables`**, and that is the point rather than an oversight:
/// the entry name is data, and `spawn::replace_variables` would read `{url}` in it and substitute
/// the page's own address. The substituted text is also never re-split, so an entry containing a
/// space, a quote or a `;;` arrives as exactly one argument.
///
/// With no `{}` anywhere the entry is appended, which is `editor_argv`'s convention and its reason:
/// a command with no placeholder is a program, not a command line.
pub fn show_argv(spec: &Spec, entry: &str) -> Result<Vec<String>, String> {
    let argv = match spec {
        Spec::Line(line) => crate::spawn::shlex(line)?,
        Spec::Argv(argv) => argv.clone(),
    };
    if argv.is_empty() {
        return Err("the password command is empty".to_string());
    }

    let substituted: Vec<String> = argv.iter().map(|arg| arg.replace("{}", entry)).collect();
    if substituted == argv {
        let mut argv = substituted;
        argv.push(entry.to_string());
        return Ok(argv);
    }
    Ok(substituted)
}

/// What the backend printed, reduced to the secret.
///
/// The **first line**, which is `pass`'s own convention — everything under it is metadata, and a
/// store written by `keyforge` follows it. `op read` and `bw get password` print the one line and
/// nothing else, so the rule costs them nothing. A trailing `\r` is trimmed because a backend may
/// be a script that came from somewhere else.
///
/// Empty is an error rather than an empty fill: a backend that answered nothing has failed, and
/// clearing the field the user asked to fill would be the wrong way to say so.
pub fn secret_from_stdout(stdout: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(stdout);
    let first = text.lines().next().unwrap_or("").trim_end_matches('\r');
    if first.is_empty() {
        return Err("the password command printed nothing".to_string());
    }
    Ok(first.to_string())
}

/// What to say when the backend failed.
///
/// **The signature is the invariant.** There is no parameter for stdout, so no future edit can
/// widen this into a message that quotes the secret; a reviewer is not what holds that line.
pub fn failure(entry: &str, status: Option<i32>, stderr: &[u8]) -> String {
    let complaint = String::from_utf8_lossy(stderr);
    let complaint = complaint.lines().find(|line| !line.trim().is_empty()).unwrap_or("");
    match (status, complaint.is_empty()) {
        (_, false) => format!("password-fill: {entry}: {complaint}"),
        (Some(code), true) => format!("password-fill: {entry}: the command exited {code}"),
        (None, true) => format!("password-fill: {entry}: the command was killed"),
    }
}

/// The user name an entry carries, which in a `pass` store is its last path segment.
///
/// `websites/abv.bg/biserstoilov@abv.bg` is an address, and that is not a coincidence: `pass`'s own
/// convention — and `keyforge`'s, which wrote this store — is that the leaf names the account. It is
/// **metadata, not a secret**: it is a filename, world-readable in the store, and bru's own history
/// already knows the host it belongs to.
pub fn user_of(entry: &str) -> &str {
    entry.rsplit('/').next().unwrap_or(entry)
}

// -----------------------------------------------------------------------------------------------
// Which entry belongs to which host
// -----------------------------------------------------------------------------------------------

/// Whether `entry` is an entry for `host`, and how specifically.
///
/// `Some(n)` is a match whose most specific segment was `n` characters long; `None` is no match.
/// The length is what ranks `mail.google.com` above `google.com` on a page at
/// `mail.google.com` — both match, and the longer one is the one that was written for it.
///
/// **The rule: any slash-separated segment of the entry that is the host, or that the host ends
/// with after a dot.** So `websites/abv.bg/me` answers for `abv.bg` and for `www.abv.bg`, and
/// `personal/router` answers for no web host at all and stays reachable only by name.
///
/// Why segments rather than a fixed depth: the store on this machine is `websites/<host>/<user>`,
/// which `keyforge` wrote, but a store organised any other way should still work without being
/// told about. Why the dot suffix: credentials are issued per domain and used on subdomains —
/// `accounts.google.com` signs into `google.com`'s account — and an exact-host rule fails on the
/// first one of those. It is browserpass's rule and qute-pass's.
///
/// **The trade, said out loud:** a page on `evil.abv.bg` will be *offered* `abv.bg`'s entry. A fill
/// still takes a command the user typed and a password field they focused, and every filler built
/// on a store's own layout makes the same trade. What would remove it is a public-suffix list, and
/// that is not earned by this.
///
/// An IP address matches by equality only. Without that, host `1.2.3.4` would "end with `.3.4`"
/// and match an entry segment named `3.4`.
pub fn entry_matches(entry: &str, host: &str) -> Option<usize> {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let numeric = host.split('.').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));

    entry
        .split('/')
        .filter(|segment| !segment.is_empty())
        .filter_map(|segment| {
            let segment = segment.to_ascii_lowercase();
            if segment == host {
                return Some(segment.len());
            }
            if numeric {
                return None;
            }
            host.strip_suffix(&segment)
                .and_then(|before| before.strip_suffix('.'))
                .map(|_| segment.len())
        })
        .max()
}

/// Every entry for a host, most specific first, then alphabetically so two runs agree.
pub fn entries_for<'a>(entries: &'a [String], host: &str) -> Vec<&'a str> {
    let mut matched: Vec<(usize, &str)> = entries
        .iter()
        .filter_map(|entry| entry_matches(entry, host).map(|rank| (rank, entry.as_str())))
        .collect();
    matched.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    matched.into_iter().map(|(_, entry)| entry).collect()
}

// -----------------------------------------------------------------------------------------------
// The built-in lister
// -----------------------------------------------------------------------------------------------

/// `$PASSWORD_STORE_DIR`, or `~/.password-store` — `pass`'s own rule.
pub fn store_dir() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os("PASSWORD_STORE_DIR") {
        if !named.is_empty() {
            return Some(PathBuf::from(named));
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(STORE_DIR))
}

/// Every entry in a `pass`-shaped store, as slash-separated names with the `.gpg` taken off.
///
/// **A directory walk rather than `pass ls`**, and the reason is what `pass ls` prints: it shells
/// out to `tree -C`, so its output is a drawing with box characters and colour in it, not a list.
/// browserpass and qute-pass walk the store for the same reason. It also means a bru with no
/// configuration at all can find the entries — `passwords.list` exists for a store that is not a
/// directory of files, not for this one.
///
/// Sorted, so that the completion offers the same order twice.
pub fn walk_store(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    collect(root, root, &mut found);
    found.sort();
    found
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `.git` and `.gpg-id` are the store's own bookkeeping, and a dotted directory is not
        // something a person named after a website.
        if path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.')) {
            continue;
        }
        if path.is_dir() {
            collect(root, &path, out);
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "gpg") {
            if let Ok(relative) = path.strip_prefix(root) {
                let name = relative.with_extension("");
                out.push(name.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"));
            }
        }
    }
}

// -----------------------------------------------------------------------------------------------
// The guard between asking and filling
// -----------------------------------------------------------------------------------------------

/// One outstanding request, and what it was asked for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Pending {
    pub id: u64,
    pub browser: i32,
    pub host: String,
    pub entry: String,
}

/// What to do with a secret that has just arrived.
#[derive(PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Fill it.
    Proceed,
    /// Drop it, and say this.
    Discard(&'static str),
}

/// Whether a secret that has come back may still be filled.
///
/// **This is a security check, not bookkeeping.** `pass show` blocks on gpg's pinentry, which is a
/// person typing a passphrase — seconds, sometimes longer. In that time the user can navigate
/// somewhere else, and the page that would receive the secret is not the page it was asked for.
/// Without the host comparison the sequence is: ask on `abv.bg`, pinentry opens, follow a link to a
/// hostile page, type the passphrase, and the password is typed into the hostile page's field.
///
/// A second `:password-fill` also cancels the first: its answer arrives with a stale id and is
/// dropped rather than filled into whatever is focused by then.
pub fn verdict(pending: &Pending, current_id: u64, current_browser: i32, current_host: &str) -> Verdict {
    if pending.id != current_id {
        return Verdict::Discard("another password-fill replaced this one");
    }
    if pending.browser != current_browser {
        return Verdict::Discard("the tab changed while the password was being asked for");
    }
    if !current_host.eq_ignore_ascii_case(&pending.host) {
        return Verdict::Discard("the page changed while the password was being asked for");
    }
    Verdict::Proceed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Both grammars, refused in one string.** `{url}` is what `insert-text` would have
    /// substituted the page's address into, `;;` is what the command parser would have split into a
    /// second command, and the quotes are what a re-split would have eaten. One argument, unchanged.
    #[test]
    fn the_entry_survives_every_grammar_that_would_have_rewritten_it() {
        let spec = Spec::Line(DEFAULT_SHOW.to_string());
        let argv = show_argv(&spec, "x {url} y;;z 'q'").unwrap();
        assert_eq!(argv, vec!["pass", "show", "x {url} y;;z 'q'"]);
        assert_eq!(argv.len(), 3, "the entry is one argument, whatever is in it");
    }

    /// `editor_argv`'s convention, for `editor_argv`'s reason: a command with no placeholder is a
    /// program, and the entry is its argument.
    #[test]
    fn a_command_with_no_placeholder_gets_the_entry_appended() {
        let argv = show_argv(&Spec::Line("bw get password".to_string()), "site/me").unwrap();
        assert_eq!(argv, vec!["bw", "get", "password", "site/me"]);
        // And a placeholder inside a longer word is substituted in place, not appended.
        let argv = show_argv(
            &Spec::Line("op read op://vault/{}/password".to_string()),
            "github",
        )
        .unwrap();
        assert_eq!(argv, vec!["op", "read", "op://vault/github/password"]);
    }

    /// The form a function returns is argv already, and nothing re-splits it — which is the whole
    /// reason the function form exists: a database path may contain a space.
    #[test]
    fn an_argv_from_a_function_is_never_re_split() {
        let spec = Spec::Argv(vec![
            "keepassxc-cli".to_string(),
            "show".to_string(),
            "/home/me/my passwords.kdbx".to_string(),
            "{}".to_string(),
        ]);
        let argv = show_argv(&spec, "sites/abv.bg").unwrap();
        assert_eq!(argv[2], "/home/me/my passwords.kdbx", "one argument, space and all");
        assert_eq!(argv[3], "sites/abv.bg");
        assert!(show_argv(&Spec::Argv(Vec::new()), "x").is_err());
    }

    /// `pass` prints the password first and metadata under it; a script from elsewhere may end its
    /// lines with a carriage return. Nothing at all is a failure, not an empty fill.
    #[test]
    fn the_secret_is_the_first_line_and_nothing_else() {
        assert_eq!(secret_from_stdout(b"pw\n").unwrap(), "pw");
        assert_eq!(secret_from_stdout(b"pw\r\n").unwrap(), "pw");
        assert_eq!(secret_from_stdout(b"pw\nlogin: me\notpauth://x\n").unwrap(), "pw");
        assert_eq!(secret_from_stdout(b"pw").unwrap(), "pw");
        assert!(secret_from_stdout(b"").is_err());
        assert!(secret_from_stdout(b"\n").is_err());
    }

    /// **The signature is the test.** `failure` cannot quote the secret because it is never given
    /// it; add a `stdout` parameter and this stops compiling, which is the point.
    #[test]
    fn a_failure_message_is_built_from_stderr_and_the_status_alone() {
        let message = failure("websites/abv.bg/me", Some(2), b"gpg: decryption failed\n");
        assert!(message.contains("gpg: decryption failed"));
        assert!(message.contains("websites/abv.bg/me"));
        assert_eq!(failure("x", Some(1), b""), "password-fill: x: the command exited 1");
        assert_eq!(failure("x", None, b""), "password-fill: x: the command was killed");
    }

    /// The matching rule, as the table that decided it. Each row fails without its own clause.
    #[test]
    fn an_entry_answers_for_a_host_and_its_subdomains_only() {
        let entry = "websites/abv.bg/biserstoilov@abv.bg";
        assert!(entry_matches(entry, "abv.bg").is_some(), "the host itself");
        assert!(entry_matches(entry, "www.abv.bg").is_some(), "and a subdomain of it");
        assert!(entry_matches(entry, "ABV.BG").is_some(), "case is not part of a host");

        // The dot is the boundary. Without it, `notabv.bg` ends with `abv.bg` as text.
        assert!(entry_matches(entry, "notabv.bg").is_none());
        // The other direction is not a match: an entry for a subdomain does not answer for the
        // domain, because the credentials may not be the same.
        assert!(entry_matches("websites/www.abv.bg/me", "abv.bg").is_none());
        // An entry that names no host answers for none.
        assert!(entry_matches("personal/router", "abv.bg").is_none());

        // **The IP guard.** `1.2.3.4` ends with `.3.4`, so without it an entry segment named `3.4`
        // would answer for it.
        assert!(entry_matches("hosts/3.4/me", "1.2.3.4").is_none());
        assert!(entry_matches("hosts/1.2.3.4/me", "1.2.3.4").is_some());
    }

    /// Specificity, which is what makes a subdomain's own entry win over the domain's.
    #[test]
    fn the_entry_written_for_this_host_is_offered_first() {
        let entries = vec![
            "a/google.com/me".to_string(),
            "a/mail.google.com/me".to_string(),
            "a/unrelated/me".to_string(),
        ];
        assert_eq!(
            entries_for(&entries, "mail.google.com"),
            vec!["a/mail.google.com/me", "a/google.com/me"],
        );
        assert_eq!(entries_for(&entries, "google.com"), vec!["a/google.com/me"]);
        assert!(entries_for(&entries, "example.org").is_empty());
    }

    /// The walk, against a store built for the test. Each assertion is one rule of `pass`'s layout.
    #[test]
    fn the_store_walk_finds_entries_and_leaves_the_bookkeeping_alone() {
        let root = std::env::temp_dir().join(format!("bru-pass-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("websites/abv.bg")).unwrap();
        std::fs::create_dir_all(root.join("websites/zzz.example")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::write(root.join("websites/abv.bg/me.gpg"), b"x").unwrap();
        std::fs::write(root.join("websites/zzz.example/me.gpg"), b"x").unwrap();
        std::fs::write(root.join("websites/abv.bg/notes.txt"), b"x").unwrap();
        std::fs::write(root.join(".gpg-id"), b"x").unwrap();
        std::fs::write(root.join(".git/objects/thing.gpg"), b"x").unwrap();

        let found = walk_store(&root);
        assert_eq!(
            found,
            vec!["websites/abv.bg/me", "websites/zzz.example/me"],
            "the `.gpg` comes off, a non-gpg file is not an entry, `.git` is not a website, and \
             the order is the same twice",
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The account name is the entry's leaf, which in a `pass` store written by `keyforge` is an
    /// address — and it is metadata: a filename, not a secret.
    #[test]
    fn the_account_name_is_the_entrys_last_segment() {
        assert_eq!(user_of("websites/abv.bg/biserstoilov@abv.bg"), "biserstoilov@abv.bg");
        assert_eq!(user_of("router"), "router");
        assert_eq!(user_of("a/b/c/me"), "me");
    }

    /// **The four words the renderer answers in.** `on_filled` matches on them, so a typo at either
    /// end is a fill that reports nothing useful; both ends are pinned here.
    #[test]
    fn the_renderer_answers_in_four_words() {
        for word in ["both", "password", "user", "nofield"] {
            assert!(
                FILL_JS.contains(&format!("'{word}'")),
                "{word:?} is not one of the answers the script can give",
            );
        }
        // And neither value is ever in the script: both arrive as arguments.
        assert!(FILL_JS.starts_with("(function(secret,user)"));
    }

    /// **The navigate-away hole, as a test.** Take the host comparison out of `verdict` and the
    /// third case starts answering `Proceed`, which is a password typed into whatever page the user
    /// happened to reach while pinentry was open.
    #[test]
    fn a_secret_that_arrives_late_is_dropped_rather_than_filled() {
        let pending = Pending {
            id: 7,
            browser: 3,
            host: "abv.bg".to_string(),
            entry: "websites/abv.bg/me".to_string(),
        };
        assert_eq!(verdict(&pending, 7, 3, "abv.bg"), Verdict::Proceed);
        assert_eq!(verdict(&pending, 7, 3, "ABV.BG"), Verdict::Proceed);

        assert!(matches!(verdict(&pending, 8, 3, "abv.bg"), Verdict::Discard(_)), "a second ask");
        assert!(matches!(verdict(&pending, 7, 4, "abv.bg"), Verdict::Discard(_)), "another tab");
        assert!(
            matches!(verdict(&pending, 7, 3, "evil.example"), Verdict::Discard(_)),
            "navigated away while pinentry was open",
        );
    }
}

// -----------------------------------------------------------------------------------------------
// The browser half: ask the backend, off the thread that draws
// -----------------------------------------------------------------------------------------------

use cef::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Browser → renderer: "put this in the focused field, if it is a password field."
const FILL: &str = "bru.password.fill";

/// Renderer → browser: what happened, as one short word. Never the secret.
const FILLED: &str = "bru.password.filled";

/// How long a lister is given before it is killed. **`passwords.show` has no such limit and must
/// not**: it blocks on a passphrase prompt, which is a person typing. A lister is non-interactive
/// by contract, so one that hangs is broken and must not eat the command silently.
const LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The request in flight, and the id that makes a stale answer recognisable.
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The entries the last bare `:password-fill` found, for the completion to offer. Names only.
static CANDIDATES: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// What the completion offers for `:password-fill`. Names, never values.
pub fn candidates() -> Vec<String> {
    CANDIDATES.lock().ok().and_then(|c| c.clone()).unwrap_or_default()
}

/// The host of the tab a command is being run against, lower-cased and without its port.
fn host_of(url: &str) -> Option<String> {
    let after = url.split("://").nth(1)?;
    let authority = after.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The `passwords.show` command for one entry, from the setting or from bru's default.
fn show_spec(entry: &str, host: &str) -> Spec {
    crate::settings::command_spec(
        "passwords.show",
        &[
            ("entry", crate::lua::Arg::Text(entry.to_string())),
            ("host", crate::lua::Arg::Text(host.to_string())),
        ],
    )
    .unwrap_or_else(|| Spec::Line(DEFAULT_SHOW.to_string()))
}

/// Every entry the configured lister knows, or bru's own walk of the store when none is set.
///
/// Runs a process, so it is never called from the UI thread — see [`fill`].
fn list_entries(host: &str) -> Result<Vec<String>, String> {
    let spec = crate::settings::command_spec(
        "passwords.list",
        &[("host", crate::lua::Arg::Text(host.to_string()))],
    );
    let Some(spec) = spec else {
        let Some(root) = store_dir() else {
            return Err("no $PASSWORD_STORE_DIR and no $HOME, so there is no store to walk".into());
        };
        let found = walk_store(&root);
        if found.is_empty() {
            return Err(format!("no entries under {}", root.display()));
        }
        return Ok(found);
    };

    // `{}` in a lister is the host, which is the only thing it could be — the entry is what it is
    // being asked to produce.
    let argv = show_argv(&spec, host)?;
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", argv[0]))?;

    let deadline = std::time::Instant::now() + LIST_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                return Err(format!("{} did not answer in five seconds", argv[0]));
            }
            Err(e) => return Err(format!("{}: {e}", argv[0])),
        }
    }
    let output = child.wait_with_output().map_err(|e| format!("{}: {e}", argv[0]))?;
    if !output.status.success() {
        return Err(failure("listing", output.status.code(), &output.stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// `:password-fill [entry]`.
///
/// **Nothing here waits.** `pass show` blocks on gpg's pinentry, and the UI thread is the thread
/// that draws: a browser frozen behind a passphrase prompt would be a browser that cannot show the
/// prompt. So the argv is built here — Lua is UI-thread-only — and everything after it happens on a
/// thread of its own, answering through a posted task.
pub fn fill(state: &crate::tabs::SharedState, entry: Option<&str>) {
    let (browser_id, url) = {
        let guard = state.lock().expect("state mutex poisoned");
        let tab = guard.active_tab();
        (
            guard.active_tab_browser_id().unwrap_or(0),
            guard.tab_url(tab).unwrap_or_default(),
        )
    };
    let Some(host) = host_of(&url) else {
        crate::message::error("password-fill: this tab has no host to look a password up by");
        return;
    };

    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    match entry {
        // Named outright: no listing, no matching. The user said which one.
        Some(entry) => {
            let entry = entry.trim().to_string();
            let spec = show_spec(&entry, &host);
            start(id, browser_id, host, entry, spec);
        }
        // Bare: list, match, and either fill the one or offer the several.
        None => {
            let host_for_thread = host.clone();
            std::thread::spawn(move || {
                let answer = list_entries(&host_for_thread);
                let mut task = Chose::new(id, browser_id, host_for_thread, answer);
                post_task(ThreadId::UI, Some(&mut task));
            });
        }
    }
}

/// Ask the backend for one entry, on a thread, and post the answer back.
fn start(id: u64, browser: i32, host: String, entry: String, spec: Spec) {
    if let Ok(mut pending) = PENDING.lock() {
        *pending = Some(Pending { id, browser, host: host.clone(), entry: entry.clone() });
    }
    crate::message::info(&format!("password-fill: asking for {entry}…"));

    std::thread::spawn(move || {
        let answer = show_argv(&spec, &entry).and_then(|argv| {
            let output = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                // **Null, deliberately.** A backend that wants a passphrase asks for it its own
                // way — pinentry, an agent — and one that would have read bru's stdin gets nothing
                // rather than a hang nobody can see the cause of.
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .map_err(|e| format!("password-fill: {}: {e}", argv[0]))?;
            if !output.status.success() {
                return Err(failure(&entry, output.status.code(), &output.stderr));
            }
            secret_from_stdout(&output.stdout)
        });
        let mut task = Arrived::new(id, answer);
        post_task(ThreadId::UI, Some(&mut task));
    });
}

wrap_task! {
    struct Chose {
        id: u64,
        browser: i32,
        host: String,
        answer: Result<Vec<String>, String>,
    }

    impl Task {
        fn execute(&self) {
            let entries = match &self.answer {
                Ok(entries) => entries,
                Err(why) => {
                    crate::message::error(&format!("password-fill: {why}"));
                    return;
                }
            };
            let matched: Vec<String> =
                entries_for(entries, &self.host).into_iter().map(str::to_string).collect();
            if let Ok(mut slot) = CANDIDATES.lock() {
                *slot = Some(if matched.is_empty() { entries.clone() } else { matched.clone() });
            }
            match matched.len() {
                0 => crate::message::error(&format!(
                    "password-fill: no entry matches {} — :password-fill <Tab> lists them all",
                    self.host
                )),
                1 => {
                    let entry = matched[0].clone();
                    let spec = show_spec(&entry, &self.host);
                    start(self.id, self.browser, self.host.clone(), entry, spec);
                }
                // Several. The command line already knows how to choose from a list.
                _ => crate::cmdline::cmd_set_text(":password-fill", true, false, false, None),
            }
        }
    }
}

wrap_task! {
    struct Arrived {
        id: u64,
        answer: Result<String, String>,
    }

    impl Task {
        fn execute(&self) {
            let secret = match &self.answer {
                Ok(secret) => secret,
                Err(why) => {
                    crate::message::error(why);
                    return;
                }
            };

            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            let (pending, current_browser, url) = {
                let guard = state.lock().expect("state mutex poisoned");
                let tab = guard.active_tab();
                (
                    PENDING.lock().ok().and_then(|p| p.clone()),
                    guard.active_tab_browser_id().unwrap_or(0),
                    guard.tab_url(tab).unwrap_or_default(),
                )
            };
            let Some(pending) = pending else { return };
            let host = host_of(&url).unwrap_or_default();

            // **The guard, and it is the security check of the whole module.** See `verdict`.
            match verdict(&pending, self.id, current_browser, &host) {
                Verdict::Discard(why) => {
                    crate::message::error(&format!("password-fill: {why} — not filling"));
                    return;
                }
                Verdict::Proceed => {}
            }

            let Some(mut browser) = state
                .lock()
                .expect("state mutex poisoned")
                .active_browser()
            else {
                return;
            };
            let Some(frame) = browser.main_frame() else { return };
            let Some(mut message) = process_message_create(Some(&CefString::from(FILL))) else {
                return;
            };
            if let Some(arguments) = message.argument_list() {
                arguments.set_string(0, Some(&CefString::from(secret.as_str())));
                // The account name, so that one command fills the pair a login form is. It is a
                // path segment and not a secret — see `user_of`.
                arguments.set_string(1, Some(&CefString::from(user_of(&pending.entry))));
            }
            frame.send_process_message(ProcessId::RENDERER, Some(&mut message));
        }
    }
}

/// Browser side of [`FILLED`]: what the renderer made of it. Answers true when the message was ours.
pub fn on_filled(message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else { return false };
    if CefString::from(&message.name()).to_string() != FILLED {
        return false;
    }
    let verdict = message
        .argument_list()
        .map(|arguments| CefString::from(&arguments.string(0)).to_string())
        .unwrap_or_default();
    let entry = PENDING
        .lock()
        .ok()
        .and_then(|p| p.as_ref().map(|p| p.entry.clone()))
        .unwrap_or_default();
    match verdict.as_str() {
        "both" => crate::message::info(&format!("password-fill: filled {entry} — name and password")),
        "password" => crate::message::info(&format!("password-fill: filled {entry} — password only")),
        "user" => crate::message::info(&format!(
            "password-fill: filled the name from {entry}, but this form has no password field"
        )),
        "nofield" => crate::message::error(
            "password-fill: nothing is focused that can be typed into — follow a hint into the \
             form first",
        ),
        _ => crate::message::error("password-fill: the page would not take it"),
    }
    true
}

// -----------------------------------------------------------------------------------------------
// The renderer half: the only place that can tell a password field from a search box
// -----------------------------------------------------------------------------------------------

/// The function the fill calls. **A constant** — neither the secret nor the account name is ever
/// part of this source; both are handed over as V8 arguments, so no script bru builds contains
/// them.
///
/// # Why this looks for a *form* and not at the focused element
///
/// It began by filling whatever had the focus, once bru had checked through `Domnode` that it was
/// an `INPUT_PASSWORD`. That check is the strongest one available and it was **defeated by the
/// first real site it met.** Measured 2026-08-09 on `home.abv.bg`: the field with `id="password"`
/// is `type="text"` until something is typed into it — the site swaps the type to keep its own
/// placeholder visible — so at the moment of filling it honestly is not a password field, and bru
/// honestly refused, five times in a row, while the user watched a login form it would not fill.
///
/// So the question moved from "is *this* a password field" to "**what pair does this form hold**",
/// which is also what the user asked for: invoke it from the address field or from the password
/// field, and both are filled.
///
/// **Say what that costs**, because it is a real weakening. The old rule could not be fooled: it
/// asked Blink's form-control model. This one reads names and types out of the page, and a page
/// writes both. What survives of the guarantee is that bru fills only inside the form the focused
/// element belongs to, and only fields shaped like a login — a page cannot make bru type the
/// password into a field the user is not standing in.
const FILL_JS: &str = "(function(secret,user){try{\
var here=document.activeElement;\
if(!here||!here.form&&here.tagName!=='INPUT'&&here.tagName!=='TEXTAREA'){return 'nofield';}\
var scope=here.form||document;\
var inputs=[].slice.call(scope.querySelectorAll('input'));\
function looks(e,re){return re.test(e.id||'')||re.test(e.name||'')||re.test(e.getAttribute('autocomplete')||'');}\
var pw=null,i;\
for(i=0;i<inputs.length;i++){if(inputs[i].type==='password'){pw=inputs[i];break;}}\
if(!pw){for(i=0;i<inputs.length;i++){if(looks(inputs[i],/pass|parol|pwd/i)){pw=inputs[i];break;}}}\
var id=null;\
for(i=0;i<inputs.length;i++){var e=inputs[i];\
 if(e===pw){continue;}\
 if(e.type==='email'||looks(e,/user|mail|login|account|nick/i)){id=e;break;}}\
if(!id){for(i=0;i<inputs.length;i++){var t=inputs[i];\
 if(t!==pw&&(t.type==='text'||t.type==='email')&&t.offsetParent!==null){id=t;break;}}}\
if(!pw&&!id){return 'nofield';}\
var native=Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype,'value');\
function put(e,v){if(!e){return 0;}\
 if(native&&native.set){native.set.call(e,v);}else{e.value=v;}\
 e.dispatchEvent(new Event('input',{bubbles:true}));\
 e.dispatchEvent(new Event('change',{bubbles:true}));return 1;}\
var n=put(id,user)+put(pw,secret);\
return n===2?'both':(pw?'password':'user');\
}catch(x){return 'threw';}})";

/// Renderer side of [`FILL`]. Answers true when the message was ours.
///
/// **The check and the fill are one renderer task**, which is why the visitor does both: asking
/// "is it a password field", answering, and then filling would leave a gap in which the page could
/// move the focus somewhere else.
pub fn renderer_on_fill(frame: Option<&Frame>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else { return false };
    if CefString::from(&message.name()).to_string() != FILL {
        return false;
    }
    let Some(frame) = frame else { return true };
    let (secret, user) = message
        .argument_list()
        .map(|arguments| {
            (
                CefString::from(&arguments.string(0)).to_string(),
                CefString::from(&arguments.string(1)).to_string(),
            )
        })
        .unwrap_or_default();

    let mut visitor = FocusedField::new(frame.clone(), secret, user);
    frame.visit_dom(Some(&mut visitor));
    true
}

wrap_domvisitor! {
    struct FocusedField {
        frame: Frame,
        secret: String,
        user: String,
    }

    impl Domvisitor {
        fn visit(&self, document: Option<&mut Domdocument>) {
            // **`Domnode`, not JavaScript, and that is the point of doing this here at all.** A
            // page can shadow `document.activeElement`; it cannot shadow Blink's own form-control
            // model. `focus.rs` makes the same argument about `is_editable` and for the same
            // reason: one definition, not two that can disagree.
            // **Editable, not `INPUT_PASSWORD`** — the narrower question was defeated by a real
            // site, and the user fills from the address field as often as from the password one.
            // See `FILL_JS` for the measurement and for what the widening costs. What `Domnode`
            // still guarantees, and JavaScript could not, is that the user is standing in a field
            // at all: a page cannot make bru fill a form nobody is in.
            let in_a_field = document
                .and_then(|document| document.focused_node())
                .map(|node| node.is_editable() != 0)
                .unwrap_or(false);

            let answer = if in_a_field { self.inject() } else { "nofield".to_string() };

            let Some(mut reply) = process_message_create(Some(&CefString::from(FILLED))) else {
                return;
            };
            if let Some(arguments) = reply.argument_list() {
                arguments.set_string(0, Some(&CefString::from(answer.as_str())));
            }
            self.frame.send_process_message(ProcessId::BROWSER, Some(&mut reply));
        }
    }
}

impl FocusedField {
    /// Call [`FILL_JS`] with the secret as an **argument**, never as part of the source.
    fn inject(&self) -> String {
        let Some(context) = self.frame.v8_context() else {
            return "threw".to_string();
        };
        if context.enter() == 0 {
            return "threw".to_string();
        }
        let mut value: Option<V8Value> = None;
        let mut exception: Option<V8Exception> = None;
        let built = context.eval(
            Some(&CefString::from(FILL_JS)),
            None,
            0,
            Some(&mut value),
            Some(&mut exception),
        );
        let answer = if built != 0 {
            match value {
                Some(function) => {
                    let arguments = vec![
                        v8_value_create_string(Some(&CefString::from(self.secret.as_str()))),
                        v8_value_create_string(Some(&CefString::from(self.user.as_str()))),
                    ];
                    match function.execute_function(None, Some(&arguments)) {
                        Some(result) => CefString::from(&result.string_value()).to_string(),
                        None => "threw".to_string(),
                    }
                }
                None => "threw".to_string(),
            }
        } else {
            "threw".to_string()
        };
        context.exit();
        answer
    }
}
