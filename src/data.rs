//! Everything under `~/.local/share/bru/` — history, quickmarks, bookmarks.
//!
//! This is the only module that touches persistent state, and it is bru's own state entirely.
//! **Nothing here opens a file belonging to qutebrowser — not its config directory, not its data
//! directory, not read-only, not ever** (DESIGN.md; DECISIONS.md item 3, decided 2026-08-06).
//! `nothing_here_names_qutebrowser` at the bottom of this file enforces it against the source
//! text. A shared history database would mean two writers on a schema neither owns, and the
//! lock contention would land on whichever browser happened to be open. qutebrowser's file formats
//! are copied as a *shape* — they have been lived with for years — and that is the whole of the
//! relationship. If importing an existing profile is ever wanted it is a separate one-shot command,
//! never a runtime dependency.
//!
//! Three files, all rewritten by bru and never hand-edited:
//!
//! | | |
//! |---|---|
//! | `history.sqlite` | `History` + `CompletionHistory`, qutebrowser's split |
//! | `quickmarks` | `name url` per line |
//! | `bookmarks` | `url title` per line |
//!
//! ## `--private`
//!
//! Under `--private` this module **reads** all three and **writes** only two of them. No visit
//! reaches `history.sqlite`, in either table, and no title is corrected in it; a quickmark or a
//! bookmark saved by name is written exactly as it would be otherwise. The argument for the split is
//! in `profile::is_private`, and the flag arrives through [`Data::open`] once rather than being
//! asked for per visit. `cmd-history`, which is the fourth file bru owns and lives in `cmdline.rs`,
//! follows history rather than the marks.
//!
//! The two-table split is the reason completion stays fast. `History` is the raw visit log and
//! grows without bound — one row per page load, forever. `CompletionHistory` holds the newest
//! entry per URL, so the query behind every keystroke of `:open …` scans one row per *site* rather
//! than one per *visit*, and its `last_atime` index means the `ORDER BY` costs nothing. Measured on
//! this machine at 9,000 rows: see `completion_at_nine_thousand_rows` at the bottom of this file.

// The module-level `#![allow(dead_code)]` that stood here is gone: `src/history.rs` records the
// visits, `src/exec.rs` has the arms for `quickmark-save`, `quickmark-load`, `quickmark-del`,
// `bookmark-add`, `bookmark-load`, `bookmark-del`, `bookmark-list` and `history`, and
// `completion.rs` reads the three sources. What is still unreached is marked one item at a time,
// with the milestone it is waiting for, so that a warning here is now a real one.

use rusqlite::Connection;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// A quickmark: a short name the user typed, and where it goes. `b <name>` loads it.
///
/// The *name* is the primary key, so two quickmarks may point at the same URL. Stored as
/// `name url` per line, which is qutebrowser's format and is parsed the same way — the name may
/// contain spaces, so the split is from the right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quickmark {
    pub name: String,
    pub url: String,
}

/// A bookmark: a URL and the page title it had when `M` was pressed.
///
/// The *URL* is the primary key, mirroring qutebrowser. Stored as `url title` per line; the title
/// may be empty and may contain spaces, so the split is from the left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub url: String,
    pub title: String,
}

/// One row of `History`, the visit log — what `bru://chrome/history` lists.
///
/// The completion reads `CompletionHistory` instead and gets one row per *site*; this is one row per
/// *visit*, which is the whole point of the page: it answers "when was I on that" rather than "what
/// have I been on".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visit {
    pub url: String,
    pub title: String,
    /// `YYYY-MM-DD`, local time — what the page groups by.
    pub date: String,
    /// `HH:MM`, local time.
    pub time: String,
}

/// One row of `CompletionHistory`, which is what the History completion category is built from.
///
/// `last_atime` is a Unix timestamp in seconds, and the rows come back newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMatch {
    pub url: String,
    pub title: String,
    pub last_atime: i64,
    /// The same instant as `last_atime`, formatted the way qutebrowser's completion shows it —
    /// `histcategory.py:86-88`, format default `configdata.yml:1406`. Done in SQL because SQLite
    /// knows the local timezone and bru has no date crate.
    pub atime: String,
}

/// What can go wrong in this module. Every variant is something a running browser must survive:
/// none of these may take the window down.
#[derive(Debug)]
pub enum DataError {
    /// The database refused the statement, or the file is not a database.
    Sql(rusqlite::Error),
    /// The directory or one of the two text files could not be read or written.
    Io(std::io::Error),
    /// Neither `$XDG_DATA_HOME` nor `$HOME` is set, so there is no `~/.local/share/bru` to find.
    NoDataDir,
    /// `quickmark-load` for a name that was never saved.
    NoSuchQuickmark(String),
    /// A quickmark or bookmark with an empty name or URL — qutebrowser rejects both.
    Empty(&'static str),
}

impl fmt::Display for DataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataError::Sql(e) => write!(f, "history database: {e}"),
            DataError::Io(e) => write!(f, "{e}"),
            DataError::NoDataDir => write!(f, "neither XDG_DATA_HOME nor HOME is set"),
            DataError::NoSuchQuickmark(name) => write!(f, "quickmark '{name}' does not exist"),
            DataError::Empty(what) => write!(f, "cannot save a mark with an empty {what}"),
        }
    }
}

impl std::error::Error for DataError {}

impl From<rusqlite::Error> for DataError {
    fn from(e: rusqlite::Error) -> DataError {
        DataError::Sql(e)
    }
}

impl From<std::io::Error> for DataError {
    fn from(e: std::io::Error) -> DataError {
        DataError::Io(e)
    }
}

type Result<T> = std::result::Result<T, DataError>;

/// `$XDG_DATA_HOME/bru`, or `~/.local/share/bru`.
///
/// The same precedence `config_path` in `config.rs` uses, and a relative `$XDG_DATA_HOME` is
/// ignored the same way — the spec says it must be absolute, and a relative one would put the
/// history database wherever bru happened to be launched from.
pub fn data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))?;
    Some(base.join("bru"))
}

/// The one open `Data` for the process, or `None` if it could not be opened.
///
/// Lazily built on first use so that no other module has to be edited to install it, and so a
/// broken or unwritable `~/.local/share/bru` costs the completion its history and nothing else —
/// the browser still starts, still browses, and prints why once rather than on every keystroke.
///
/// `Data` holds a `rusqlite::Connection`, which is `Send` but not `Sync`, so it lives behind a
/// mutex like the rest of bru's shared state.
pub fn instance() -> Option<&'static Mutex<Data>> {
    static INSTANCE: OnceLock<Option<Mutex<Data>>> = OnceLock::new();
    INSTANCE
        .get_or_init(|| match Data::open_default() {
            Ok(data) => Some(Mutex::new(data)),
            Err(e) => {
                eprintln!("bru: no history, quickmarks or bookmarks this session: {e}");
                None
            }
        })
        .as_ref()
}

/// Everything bru persists, and the only handle to it.
pub struct Data {
    dir: PathBuf,
    conn: Connection,
    /// Read once at open, kept in memory, and the whole file rewritten on every change. Twenty-odd
    /// entries on this machine — re-reading the file to answer a completion keystroke would be
    /// three orders of magnitude more work than reading a `Vec`.
    quickmarks: Vec<Quickmark>,
    bookmarks: Vec<Bookmark>,
    /// The last URL recorded, so a page that reports itself twice — a title arriving after the
    /// address, a same-document navigation — does not become two visits. qutebrowser's
    /// `WebHistory._last_url`, `browser/history.py`.
    last_url: Option<String>,
    /// `--private`: read the history, never add to it. Read once at open from
    /// [`crate::profile::is_private`] and carried here rather than asked for again per visit, so
    /// that the answer cannot change halfway through a run and so that the tests below can build a
    /// private `Data` without touching a process-wide flag the other tests share.
    private: bool,
}

impl Data {
    /// Open `~/.local/share/bru`, creating the directory and the schema if this is the first run.
    pub fn open_default() -> Result<Data> {
        Data::open(&data_dir().ok_or(DataError::NoDataDir)?)
    }

    /// Open an explicit directory. Tests use this; nothing else should, because there is exactly
    /// one such directory and `instance()` already knows where it is.
    pub fn open(dir: &Path) -> Result<Data> {
        Data::open_with(dir, crate::profile::is_private())
    }

    /// [`Data::open`] with the private decision passed in rather than read from the process.
    ///
    /// The database is opened either way, because a private run still *reads* history — the
    /// completion is the reason `--private` is usable at all. Only the writes are refused, and they
    /// are refused in [`Data::record_visit_at`] and [`Data::update_title`], which is every path into
    /// the two history tables.
    fn open_with(dir: &Path, private: bool) -> Result<Data> {
        std::fs::create_dir_all(dir)?;
        let conn = Connection::open(dir.join("history.sqlite"))?;
        // Chromium is not gentle about how a browser exits, and the default rollback journal loses
        // the tail of the log to a hard kill. WAL survives it, and lets the completion read while
        // a visit is being written.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        create_schema(&conn)?;

        let quickmarks = read_quickmarks(&dir.join("quickmarks"))?;
        let bookmarks = read_bookmarks(&dir.join("bookmarks"))?;
        Ok(Data { dir: dir.to_path_buf(), conn, quickmarks, bookmarks, last_url: None, private })
    }

    /// The directory this `Data` owns. Only tests want it today; `:history-clear` and an import
    /// command will both need to name it in a message.
    #[allow(dead_code)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    // -- history -------------------------------------------------------------------------------

    /// Record a page visit. Called when a tab finishes navigating.
    ///
    /// `redirect` marks an intermediate URL that the user never saw the content of: it goes in the
    /// visit log, because the log is a log, and stays out of the completion, because completing to
    /// a URL that immediately bounces elsewhere is noise. qutebrowser does exactly this in
    /// `WebHistory.add_url`.
    ///
    /// Returns `Ok(false)` when the visit was deliberately not recorded — an excluded scheme, an
    /// empty URL, the same URL as the visit before it, or a `--private` run.
    pub fn record_visit(&mut self, url: &str, title: &str, redirect: bool) -> Result<bool> {
        self.record_visit_at(url, title, redirect, now())
    }

    /// [`Data::record_visit`] with the timestamp supplied rather than read from the clock. Tests
    /// need a history whose ordering is known, and an import command would need this too.
    pub fn record_visit_at(&mut self, url: &str, title: &str, redirect: bool, atime: i64) -> Result<bool> {
        // `--private`. First, before anything else in the function, because everything after this
        // line ends in an `INSERT`: a visit is a record of where the user went, and a run that
        // announced it would keep no record and then wrote one would be worse than no switch. The
        // completion still *reads* the table — see `profile::is_private`.
        if self.private {
            return Ok(false);
        }
        if url.is_empty() || is_excluded(url) {
            return Ok(false);
        }
        // A redirect is never the "last URL": the page the user lands on follows it immediately and
        // must not be swallowed as a duplicate.
        if !redirect && self.last_url.as_deref() == Some(url) {
            return Ok(false);
        }

        self.conn.execute(
            "INSERT INTO History (url, title, atime, redirect) VALUES (?1, ?2, ?3, ?4)",
            (url, title, atime, redirect as i32),
        )?;

        if !redirect {
            // The upsert is the point of the PRIMARY KEY: revisiting a page moves its one
            // completion row to the top instead of adding a second one. Without it the completion
            // table would grow like the visit log and the query would slow down with every reload.
            self.conn.execute(
                "INSERT INTO CompletionHistory (url, title, last_atime) VALUES (?1, ?2, ?3)
                 ON CONFLICT(url) DO UPDATE SET title = excluded.title, last_atime = excluded.last_atime",
                (url, title, atime),
            )?;
            self.last_url = Some(url.to_string());
        }
        Ok(true)
    }

    /// Correct the title of a URL already recorded, without recording a second visit.
    ///
    /// CEF reports the address before the title — `on_address_change` fires at commit,
    /// `on_title_change` when the document has one — so the row written at navigation frequently
    /// holds the old page's title or none at all. This fixes it in place.
    pub fn update_title(&mut self, url: &str, title: &str) -> Result<()> {
        // A private run wrote no row to correct, and an `UPDATE` that matched a row from an earlier
        // ordinary run would move that row's title to the one this run saw — a write, and a
        // detectable trace of the private visit, from the one history call that is not an insert.
        if self.private || url.is_empty() || title.is_empty() {
            return Ok(());
        }
        self.conn.execute("UPDATE CompletionHistory SET title = ?2 WHERE url = ?1", (url, title))?;
        self.conn.execute(
            "UPDATE History SET title = ?2 WHERE rowid = (
                 SELECT rowid FROM History WHERE url = ?1 ORDER BY atime DESC LIMIT 1)",
            (url, title),
        )?;
        Ok(())
    }

    /// The History completion category: rows whose URL or title matches every space-separated word
    /// of `pattern`, newest first, at most `limit` of them.
    ///
    /// This is qutebrowser's filter, from `completion/models/histcategory.py`: the pattern is split
    /// on spaces and each word must appear in the URL *or* the title, in any order. `%` and `_` the
    /// user typed are escaped, so `:open 100%` searches for the characters rather than matching
    /// everything. An empty pattern matches everything and gives the most recent `limit` pages,
    /// which is what `:open ` with nothing after it should show.
    pub fn history_completion(&self, pattern: &str, limit: usize) -> Result<Vec<HistoryMatch>> {
        let words: Vec<String> = pattern.split(' ').map(|w| format!("%{}%", escape_like(w))).collect();
        let where_clause = (1..=words.len())
            .map(|i| format!("(url LIKE ?{i} ESCAPE '\\' OR title LIKE ?{i} ESCAPE '\\')"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let sql = format!(
            "SELECT url, title, last_atime,
                    strftime('%Y-%m-%d %H:%M', last_atime, 'unixepoch', 'localtime')
             FROM CompletionHistory
             WHERE {where_clause} ORDER BY last_atime DESC LIMIT ?{}",
            words.len() + 1
        );

        let mut stmt = self.conn.prepare_cached(&sql)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = words.iter().map(|w| w as &dyn rusqlite::ToSql).collect();
        let limit = limit as i64;
        params.push(&limit);

        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok(HistoryMatch {
                url: row.get(0)?,
                title: row.get(1)?,
                last_atime: row.get(2)?,
                atime: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many rows the visit log and the completion table hold. For `:history` and for tests;
    /// the difference between the two is the whole argument for the split.
    pub fn history_counts(&self) -> Result<(usize, usize)> {
        let visits: i64 = self.conn.query_row("SELECT count(*) FROM History", [], |r| r.get(0))?;
        let completion: i64 = self.conn.query_row("SELECT count(*) FROM CompletionHistory", [], |r| r.get(0))?;
        Ok((visits as usize, completion as usize))
    }

    /// The newest `limit` visits, for `bru://chrome/history`.
    ///
    /// Redirects are left out: the log keeps them because it is a log, but a page that listed the
    /// URL that bounced *and* the one it bounced to would show every visit twice. That is
    /// qutebrowser's `qute://history` too — `qutescheme.py:201-225` reads the same table and drops
    /// them (`history.py:421`, where a redirect never reaches the completion either).
    ///
    /// The two timestamp strings are formatted in SQL because SQLite knows the local timezone and
    /// bru has no date crate — the same reason `history_completion` formats `atime` there.
    pub fn recent_visits(&self, limit: usize) -> Result<Vec<Visit>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT url, title,
                    strftime('%Y-%m-%d', atime, 'unixepoch', 'localtime'),
                    strftime('%H:%M', atime, 'unixepoch', 'localtime')
             FROM History WHERE redirect = 0 ORDER BY atime DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(Visit {
                url: row.get(0)?,
                title: row.get(1)?,
                date: row.get(2)?,
                time: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

// --- src/utilcmds.rs -------------------------------------------------------
    /// `history-clear` — forget every visit and every completion row (`history.py:437-452`).
    ///
    /// Returns how many visits were in the log, because "cleared" with no number reads the same
    /// whether there were nine thousand rows or none.
    ///
    /// `DELETE FROM` rather than `DROP`/`CREATE`: the schema and its index are what
    /// `create_schema` promises, and rebuilding them here would be a second place they are
    /// spelled. The freed pages are handed back to the filesystem with a `VACUUM`, since a history
    /// that was cleared and left a 40 MB file behind has not really been cleared.
    pub fn clear_history(&mut self) -> Result<usize> {
        let (visits, _) = self.history_counts()?;
        self.conn.execute("DELETE FROM History", ())?;
        self.conn.execute("DELETE FROM CompletionHistory", ())?;
        self.last_url = None;
        // Outside a transaction by definition; `execute` is fine with that and a failure here is
        // not a failure to clear, so it is deliberately not propagated.
        let _ = self.conn.execute("VACUUM", ());
        Ok(visits)
    }

    /// `quickmarks-reload` / `bookmarks-reload` — read the file again (`commands.py:1190`, `:1361`).
    ///
    /// The two lists are read once at open and kept in memory (see the field docs), which is what
    /// makes a completion keystroke free and what makes an edit in `$EDITOR` invisible until this
    /// is called. Returns how many entries the file now holds.
    pub fn reload_marks(&mut self, which: crate::utilcmds::Marks) -> Result<usize> {
        match which {
            crate::utilcmds::Marks::Quickmarks => {
                self.quickmarks = read_quickmarks(&self.dir.join("quickmarks"))?;
                Ok(self.quickmarks.len())
            }
            crate::utilcmds::Marks::Bookmarks => {
                self.bookmarks = read_bookmarks(&self.dir.join("bookmarks"))?;
                Ok(self.bookmarks.len())
            }
        }
    }
// --- end src/utilcmds.rs ---------------------------------------------------

    /// Forget one URL, from both tables. Behind the completion's delete key —
    /// `completers.rs`'s `History` category calls it, as `urlmodel.py:14-19` does.
    pub fn forget_url(&mut self, url: &str) -> Result<()> {
        self.conn.execute("DELETE FROM History WHERE url = ?1", (url,))?;
        self.conn.execute("DELETE FROM CompletionHistory WHERE url = ?1", (url,))?;
        if self.last_url.as_deref() == Some(url) {
            self.last_url = None;
        }
        Ok(())
    }

    // -- quickmarks ----------------------------------------------------------------------------

    /// Every quickmark, in the order the file holds them. The completion reads this directly.
    pub fn quickmarks(&self) -> &[Quickmark] {
        &self.quickmarks
    }

    /// `quickmark-save` — the command behind `m`. Overwrites a quickmark of the same name, which is
    /// what qutebrowser does once its confirmation prompt is answered; bru has no prompt mode yet,
    /// so the overwrite is silent and the caller is expected to have the name already.
    ///
    /// Returns whether an existing quickmark was replaced.
    pub fn quickmark_save(&mut self, name: &str, url: &str) -> Result<bool> {
        if name.trim().is_empty() {
            return Err(DataError::Empty("name"));
        }
        if url.is_empty() {
            return Err(DataError::Empty("URL"));
        }
        let replaced = match self.quickmarks.iter_mut().find(|q| q.name == name) {
            Some(existing) => {
                existing.url = url.to_string();
                true
            }
            None => {
                self.quickmarks.push(Quickmark { name: name.to_string(), url: url.to_string() });
                false
            }
        };
        self.write_quickmarks()?;
        Ok(replaced)
    }

    /// `quickmark-load` — the command behind `b`. Returns the URL to open, owned, because the
    /// caller is holding a mutex over the whole of `Data` and should not be holding a borrow into
    /// it while it navigates.
    pub fn quickmark_load(&self, name: &str) -> Result<String> {
        self.quickmarks
            .iter()
            .find(|q| q.name == name)
            .map(|q| q.url.clone())
            .ok_or_else(|| DataError::NoSuchQuickmark(name.to_string()))
    }

    /// `quickmark-del`. Returns whether there was one to delete.
    pub fn quickmark_del(&mut self, name: &str) -> Result<bool> {
        let before = self.quickmarks.len();
        self.quickmarks.retain(|q| q.name != name);
        if self.quickmarks.len() == before {
            return Ok(false);
        }
        self.write_quickmarks()?;
        Ok(true)
    }

    // -- bookmarks -----------------------------------------------------------------------------

    /// Every bookmark, in file order. The completion reads this directly.
    pub fn bookmarks(&self) -> &[Bookmark] {
        &self.bookmarks
    }

    /// `bookmark-add` — the command behind `M`, which qutebrowser binds with `--toggle`, so
    /// pressing it on an already-bookmarked page removes the bookmark.
    ///
    /// Returns `true` if the bookmark was added, `false` if it was removed. With `toggle` false an
    /// existing URL has its title refreshed and `false` comes back, rather than an error: `M` on a
    /// page whose title has changed should not fail.
    pub fn bookmark_add(&mut self, url: &str, title: &str, toggle: bool) -> Result<bool> {
        if url.is_empty() {
            return Err(DataError::Empty("URL"));
        }
        let existing = self.bookmarks.iter().position(|b| b.url == url);
        let added = match (existing, toggle) {
            (Some(i), true) => {
                self.bookmarks.remove(i);
                false
            }
            (Some(i), false) => {
                self.bookmarks[i].title = title.to_string();
                false
            }
            (None, _) => {
                self.bookmarks.push(Bookmark { url: url.to_string(), title: title.to_string() });
                true
            }
        };
        self.write_bookmarks()?;
        Ok(added)
    }

    /// `bookmark-del`. Returns whether there was one to delete.
    pub fn bookmark_del(&mut self, url: &str) -> Result<bool> {
        let before = self.bookmarks.len();
        self.bookmarks.retain(|b| b.url != url);
        if self.bookmarks.len() == before {
            return Ok(false);
        }
        self.write_bookmarks()?;
        Ok(true)
    }

    fn write_quickmarks(&self) -> Result<()> {
        let body: String = self.quickmarks.iter().map(|q| format!("{} {}\n", q.name, q.url)).collect();
        write_atomically(&self.dir.join("quickmarks"), &body)
    }

    fn write_bookmarks(&self) -> Result<()> {
        let body: String = self
            .bookmarks
            .iter()
            .map(|b| if b.title.is_empty() { format!("{}\n", b.url) } else { format!("{} {}\n", b.url, b.title) })
            .collect();
        write_atomically(&self.dir.join("bookmarks"), &body)
    }
}

/// The schema, created on first run and idempotent afterwards.
///
/// It is qutebrowser's, from `browser/history.py` (`History` :164, `CompletionHistory` :138), and
/// it is copied because the split earns its keep, not because anything is shared: this database is
/// bru's own file and no other program opens it.
///
/// The `CompletionHistory` index on `last_atime` is what the completion query's `ORDER BY` walks.
/// Without it SQLite sorts the whole matching set in a temporary B-tree on every keystroke — see
/// `completion_query_walks_the_last_atime_index`, which reads the query plan and fails if it does.
fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS History (
             url TEXT NOT NULL,
             title TEXT NOT NULL,
             atime INTEGER NOT NULL,
             redirect INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS HistoryIndex ON History (url);
         CREATE INDEX IF NOT EXISTS HistoryAtimeIndex ON History (atime);

         CREATE TABLE IF NOT EXISTS CompletionHistory (
             url TEXT PRIMARY KEY,
             title TEXT NOT NULL,
             last_atime INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS CompletionHistoryAtimeIndex ON CompletionHistory (last_atime);",
    )
}

/// Schemes that are never worth completing to, and never worth a row in the log.
///
/// The rule is qutebrowser's, from `WebHistory._is_excluded_entirely` (`history.py:245-257`): a URL
/// "which can't be visited at a later point; or which is usually excessively long". The list is
/// longer than qutebrowser's because bru learns its addresses from `DisplayHandler::on_address_change`
/// and therefore sees things Qt never showed qutebrowser:
///
/// | | |
/// |---|---|
/// | `data:` | qutebrowser's. A `data:` URL can be a megabyte of base64, and would be a megabyte of history |
/// | `view-source:` | qutebrowser's |
/// | `bru://` | bru's own chrome — `bru://chrome/top.html` is not a page the user navigated to. qutebrowser excludes `qute://back` and `qute://pdfjs` for the same reason |
/// | `about:` | **CEF.** A `BrowserView` with no URL commits `about:blank`, so every tab bru opens would otherwise log one |
/// | `chrome-error://` | **CEF.** A failed navigation commits `chrome-error://chromewebdata/`; completing to it would reopen the error, not the site |
/// | `chrome://`, `chrome-untrusted://`, `devtools://` | **CEF.** Chromium's own UI, which bru does not offer and cannot be asked to reopen |
/// | `javascript:`, `blob:` | neither survives the page it belonged to |
///
/// Measured 2026-08-06 before the list grew: a run that visited two pages logged four rows, the two
/// extras being `about:blank` from the first tab and one `chrome-error://chromewebdata/`.
fn is_excluded(url: &str) -> bool {
    const EXCLUDED: [&str; 9] = [
        "data:",
        "view-source:",
        "bru://",
        "about:",
        "chrome-error://",
        "chrome://",
        "chrome-untrusted://",
        "devtools://",
        "javascript:",
    ];
    let lower = url.to_ascii_lowercase();
    // `blob:` is spelled out rather than listed because a blob URL carries its origin after the
    // scheme (`blob:https://example.com/<uuid>`), and a prefix test reads oddly beside the rest.
    lower.starts_with("blob:") || EXCLUDED.iter().any(|scheme| lower.starts_with(scheme))
}

/// Escape the two SQL `LIKE` wildcards so a user's `%` or `_` is a literal character.
///
/// The backslash itself has to go first, or escaping `%` would then have its own backslash escaped
/// and the pattern would search for a backslash.
fn escape_like(word: &str) -> String {
    word.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Write via a temporary file in the same directory and rename over the target.
///
/// `rename(2)` within a directory is atomic, so a crash — and Chromium arranges plenty — leaves
/// either the old quickmarks file or the new one, never a half-written one. Truncating in place
/// would put a user's twenty-two quickmarks one power cut away from being gone.
fn write_atomically(path: &Path, body: &str) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `name url` per line, split from the right so a name may contain spaces. Blank lines and `#`
/// comments are skipped, as qutebrowser's `UrlMarkManager.reload` does; a line with no space at all
/// is not a quickmark and is dropped.
fn read_quickmarks(path: &Path) -> Result<Vec<Quickmark>> {
    let Some(text) = read_if_present(path)? else {
        return Ok(Vec::new());
    };
    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let (name, url) = line.trim_end().rsplit_once(' ')?;
            Some(Quickmark { name: name.trim().to_string(), url: url.to_string() })
        })
        .filter(|q| !q.name.is_empty() && !q.url.is_empty())
        .collect())
}

/// `url title` per line, split from the left so the title may contain spaces. A line with only a
/// URL is a bookmark with an empty title, which qutebrowser also allows.
fn read_bookmarks(path: &Path) -> Result<Vec<Bookmark>> {
    let Some(text) = read_if_present(path)? else {
        return Ok(Vec::new());
    };
    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| match line.trim_end().split_once(' ') {
            Some((url, title)) => Bookmark { url: url.to_string(), title: title.trim_start().to_string() },
            None => Bookmark { url: line.trim().to_string(), title: String::new() },
        })
        .filter(|b| !b.url.is_empty())
        .collect())
}

/// `None` for a file that is not there — the first run, which is not an error. Anything else is.
fn read_if_present(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(DataError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh `~/.local/share/bru`-shaped directory under the scratch space of this test run, and
    /// never the user's real one. Same shape as `TempConfig` in `config.rs`.
    struct TempData {
        dir: PathBuf,
    }

    impl TempData {
        fn new(name: &str) -> TempData {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("bru-data-test-{}-{n}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            TempData { dir }
        }

        fn open(&self) -> Data {
            Data::open_with(&self.dir, false).expect("could not open the test data directory")
        }

        /// The same directory opened as a `--private` run would open it. Not through the process
        /// flag: `cargo test` runs these in threads that share it, and one test switching it on
        /// would silently stop every other test's history from being written.
        fn open_private(&self) -> Data {
            Data::open_with(&self.dir, true).expect("could not open the test data directory")
        }
    }

    impl Drop for TempData {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn first_run_creates_the_directory_and_the_schema() {
        let t = TempData::new("first-run");
        assert!(!t.dir.exists(), "the test directory must not exist yet");
        let data = t.open();
        assert!(t.dir.join("history.sqlite").exists());

        let mut tables: Vec<String> = data
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        tables.sort();
        assert_eq!(tables, vec!["CompletionHistory".to_string(), "History".to_string()]);

        // The columns are STAGE2-CONTRACTS' and qutebrowser's, in order.
        let columns = |table: &str| -> Vec<String> {
            data.conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(columns("History"), ["url", "title", "atime", "redirect"]);
        assert_eq!(columns("CompletionHistory"), ["url", "title", "last_atime"]);
    }

    #[test]
    fn opening_twice_keeps_what_was_there() {
        let t = TempData::new("reopen");
        {
            let mut data = t.open();
            data.record_visit_at("https://example.com/", "Example", false, 1000).unwrap();
            data.quickmark_save("ex", "https://example.com/").unwrap();
            data.bookmark_add("https://example.com/", "Example", false).unwrap();
        }
        let data = t.open();
        assert_eq!(data.history_counts().unwrap(), (1, 1));
        assert_eq!(data.quickmarks(), [Quickmark { name: "ex".into(), url: "https://example.com/".into() }]);
        assert_eq!(data.bookmarks(), [Bookmark { url: "https://example.com/".into(), title: "Example".into() }]);
    }

    #[test]
    fn revisiting_a_url_replaces_its_completion_row_but_appends_a_visit() {
        // The PRIMARY KEY upsert. Break `ON CONFLICT(url) DO UPDATE` in `record_visit_at` and this
        // fails: either the second insert errors on the unique constraint, or — with the constraint
        // gone too — the completion table grows a duplicate and the title stays stale.
        let t = TempData::new("upsert");
        let mut data = t.open();
        data.record_visit_at("https://example.com/", "Old title", false, 1000).unwrap();
        data.record_visit_at("https://other.com/", "Other", false, 1500).unwrap();
        data.record_visit_at("https://example.com/", "New title", false, 2000).unwrap();

        // Three visits logged, two sites completed.
        assert_eq!(data.history_counts().unwrap(), (3, 2));

        let hits = data.history_completion("example", 10).unwrap();
        assert_eq!(hits.len(), 1, "a revisited URL must have exactly one completion row");
        assert_eq!(hits[0].title, "New title", "the upsert must carry the new title over");
        assert_eq!(hits[0].last_atime, 2000);

        // And it is now the newest, so it sorts first.
        let all = data.history_completion("", 10).unwrap();
        assert_eq!(all.iter().map(|h| h.url.as_str()).collect::<Vec<_>>(), ["https://example.com/", "https://other.com/"]);
    }

    #[test]
    fn completion_query_walks_the_last_atime_index() {
        // The index from the contract, checked by reading SQLite's own plan rather than by
        // asserting it is there. Drop `CompletionHistoryAtimeIndex` and the plan says
        // "USE TEMP B-TREE FOR ORDER BY" instead — a full sort of every match, on every keystroke.
        let t = TempData::new("index-plan");
        let mut data = t.open();
        for i in 0..500 {
            data.record_visit_at(&format!("https://example.com/{i}"), "Example", false, 1000 + i).unwrap();
        }

        let plan: Vec<String> = data
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT url, title, last_atime FROM CompletionHistory
                 WHERE (url LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\')
                 ORDER BY last_atime DESC LIMIT 25",
            )
            .unwrap()
            .query_map(["%example%"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let plan = plan.join(" | ");

        assert!(
            plan.contains("CompletionHistoryAtimeIndex"),
            "the completion query must walk the last_atime index; plan was: {plan}"
        );
        assert!(
            !plan.contains("TEMP B-TREE"),
            "the completion query must not sort into a temporary B-tree; plan was: {plan}"
        );
    }

    #[test]
    fn every_word_must_match_url_or_title_in_any_order() {
        // qutebrowser's filter, histcategory.py: split on spaces, each word in url OR title.
        let t = TempData::new("filter");
        let mut data = t.open();
        data.record_visit_at("https://github.com/rust-lang/rust", "The Rust Programming Language", false, 3000).unwrap();
        data.record_visit_at("https://news.ycombinator.com/", "Hacker News", false, 2000).unwrap();
        data.record_visit_at("https://doc.rust-lang.org/std/", "std - Rust", false, 1000).unwrap();

        let urls = |p: &str| -> Vec<String> {
            data.history_completion(p, 10).unwrap().into_iter().map(|h| h.url).collect()
        };

        // One word in the URL.
        assert_eq!(urls("ycombinator"), ["https://news.ycombinator.com/"]);
        // One word in the title only.
        assert_eq!(urls("hacker"), ["https://news.ycombinator.com/"]);
        // Two words, one from the URL and one from the title, in an order neither appears in.
        assert_eq!(urls("programming github"), ["https://github.com/rust-lang/rust"]);
        // Both Rust pages, newest first.
        assert_eq!(urls("rust"), ["https://github.com/rust-lang/rust", "https://doc.rust-lang.org/std/"]);
        // A word that matches nothing kills the whole match.
        assert!(urls("rust nothinghere").is_empty());
        // Case-insensitive, like SQLite's LIKE on ASCII.
        assert_eq!(urls("HACKER"), ["https://news.ycombinator.com/"]);
    }

    #[test]
    fn a_typed_percent_is_a_character_and_not_a_wildcard() {
        // Escaping the LIKE wildcards. Remove `escape_like` and this returns both rows, because
        // '%x%y%' matches anything containing x then y.
        let t = TempData::new("escape");
        let mut data = t.open();
        data.record_visit_at("https://example.com/100%25-done", "100% done", false, 2000).unwrap();
        data.record_visit_at("https://example.com/1000-donuts", "1000 donuts", false, 1000).unwrap();

        let hits = data.history_completion("100%", 10).unwrap();
        assert_eq!(hits.len(), 1, "a typed % must be a literal");
        assert_eq!(hits[0].title, "100% done");

        // The other wildcard, and the escape character itself.
        assert_eq!(data.history_completion("100_", 10).unwrap().len(), 0, "a typed _ must be a literal");
        assert_eq!(data.history_completion("\\", 10).unwrap().len(), 0);
    }

    #[test]
    fn an_empty_pattern_gives_the_most_recent_pages() {
        let t = TempData::new("empty-pattern");
        let mut data = t.open();
        for i in 0..10 {
            data.record_visit_at(&format!("https://example.com/{i}"), "Example", false, 1000 + i).unwrap();
        }
        let hits = data.history_completion("", 3).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.url.as_str()).collect::<Vec<_>>(),
            ["https://example.com/9", "https://example.com/8", "https://example.com/7"]
        );
    }

    #[test]
    fn redirects_are_logged_and_never_completed() {
        let t = TempData::new("redirect");
        let mut data = t.open();
        data.record_visit_at("http://example.com/", "", true, 1000).unwrap();
        data.record_visit_at("https://example.com/", "Example", false, 1000).unwrap();

        assert_eq!(data.history_counts().unwrap(), (2, 1));
        let hits = data.history_completion("example", 10).unwrap();
        assert_eq!(hits.iter().map(|h| h.url.as_str()).collect::<Vec<_>>(), ["https://example.com/"]);
    }

    #[test]
    fn the_same_url_twice_running_is_one_visit_but_a_round_trip_is_two() {
        let t = TempData::new("dedup");
        let mut data = t.open();
        assert!(data.record_visit_at("https://a.com/", "A", false, 1000).unwrap());
        assert!(!data.record_visit_at("https://a.com/", "A", false, 1001).unwrap(), "an immediate repeat is not a visit");
        assert!(data.record_visit_at("https://b.com/", "B", false, 1002).unwrap());
        assert!(data.record_visit_at("https://a.com/", "A", false, 1003).unwrap(), "going back is a visit");
        assert_eq!(data.history_counts().unwrap(), (3, 2));
    }

    #[test]
    fn excluded_schemes_never_reach_the_database() {
        let t = TempData::new("excluded");
        let mut data = t.open();
        assert!(!data.record_visit_at("data:text/html,<h1>hi</h1>", "hi", false, 1000).unwrap());
        assert!(!data.record_visit_at("view-source:https://example.com/", "", false, 1000).unwrap());
        assert!(!data.record_visit_at("bru://bottom", "", false, 1000).unwrap());
        assert!(!data.record_visit_at("", "", false, 1000).unwrap());
        assert_eq!(data.history_counts().unwrap(), (0, 0));
    }

    /// The five schemes the list grew by once a real `DisplayHandler` was feeding it. Every one of
    /// these was observed coming out of `on_address_change` on this machine, or is the same kind of
    /// address as one that was; none of them is a page a user can be sent back to.
    #[test]
    fn the_addresses_cef_reports_that_are_not_pages() {
        let t = TempData::new("cef-addresses");
        let mut data = t.open();
        for url in [
            "about:blank",
            "chrome-error://chromewebdata/",
            "chrome://newtab/",
            "chrome-untrusted://new-tab-page/",
            "devtools://devtools/bundled/devtools_app.html",
            "javascript:void(0)",
            "blob:https://example.com/8f2c-4d1e",
            // Case is not the browser's to normalise: Chromium lowercases a scheme, but a row that
            // slipped in because of capitals would be a row nobody could explain.
            "About:Blank",
            "DATA:text/html,x",
        ] {
            assert!(
                !data.record_visit_at(url, "", false, 1000).unwrap(),
                "{url} reached the history"
            );
        }
        assert_eq!(data.history_counts().unwrap(), (0, 0));

        // And the near misses that must still be recorded — a prefix test is easy to make too eager.
        for url in [
            "https://about.gitlab.com/",
            "https://example.com/chrome://x",
            "https://blob.example.com/",
        ] {
            assert!(data.record_visit_at(url, "", false, 1000).unwrap(), "{url} was dropped");
        }
        assert_eq!(data.history_counts().unwrap(), (3, 3));
    }

    /// The visit log, newest first, with redirects left out — what `bru://chrome/history` lists.
    #[test]
    fn the_history_page_reads_visits_not_sites() {
        let t = TempData::new("recent-visits");
        let mut data = t.open();
        data.record_visit_at("https://a.com/", "A", false, 1_760_000_000).unwrap();
        data.record_visit_at("https://b.com/", "B", false, 1_760_000_060).unwrap();
        // The same page again: one row in the completion table, two in the log.
        data.record_visit_at("https://a.com/", "A", false, 1_760_000_120).unwrap();
        data.record_visit_at("http://redirect.example/", "", true, 1_760_000_180).unwrap();

        assert_eq!(data.history_counts().unwrap(), (4, 2), "the log keeps the redirect");

        let visits = data.recent_visits(10).unwrap();
        let urls: Vec<&str> = visits.iter().map(|v| v.url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://a.com/", "https://b.com/", "https://a.com/"],
            "newest first, and no redirect"
        );
        // Both timestamp strings come out of SQLite in local time, so only the shape is asserted.
        assert_eq!(visits[0].date.len(), 10, "YYYY-MM-DD, got {:?}", visits[0].date);
        assert_eq!(visits[0].time.len(), 5, "HH:MM, got {:?}", visits[0].time);
        assert_eq!(visits[0].title, "A");

        assert_eq!(data.recent_visits(2).unwrap().len(), 2, "the limit is honoured");
    }

    #[test]
    fn a_title_arriving_after_the_address_corrects_the_row() {
        let t = TempData::new("late-title");
        let mut data = t.open();
        data.record_visit_at("https://example.com/", "", false, 1000).unwrap();
        data.update_title("https://example.com/", "Example Domain").unwrap();

        assert_eq!(data.history_counts().unwrap(), (1, 1), "a late title must not add a visit");
        assert_eq!(data.history_completion("domain", 10).unwrap()[0].title, "Example Domain");
    }

    #[test]
    fn forgetting_a_url_clears_both_tables() {
        let t = TempData::new("forget");
        let mut data = t.open();
        data.record_visit_at("https://a.com/", "A", false, 1000).unwrap();
        data.record_visit_at("https://b.com/", "B", false, 1001).unwrap();
        data.record_visit_at("https://a.com/", "A", false, 1002).unwrap();
        data.forget_url("https://a.com/").unwrap();
        assert_eq!(data.history_counts().unwrap(), (1, 1));
        assert!(data.history_completion("a.com", 10).unwrap().is_empty());
    }

    #[test]
    fn quickmarks_round_trip_through_the_file_in_qutebrowsers_format() {
        let t = TempData::new("quickmarks");
        {
            let mut data = t.open();
            data.quickmark_save("go", "https://www.google.com/").unwrap();
            data.quickmark_save("two words", "https://example.com/").unwrap();
        }
        // The file is `name url` per line, which is what qutebrowser's format is — copied as a
        // shape, in bru's own directory, and read back only from there.
        let text = std::fs::read_to_string(t.dir.join("quickmarks")).unwrap();
        assert_eq!(text, "go https://www.google.com/\ntwo words https://example.com/\n");

        let data = t.open();
        assert_eq!(data.quickmark_load("go").unwrap(), "https://www.google.com/");
        assert_eq!(data.quickmark_load("two words").unwrap(), "https://example.com/");
        assert!(matches!(data.quickmark_load("nope"), Err(DataError::NoSuchQuickmark(_))));
    }

    #[test]
    fn saving_a_quickmark_over_an_existing_name_replaces_it() {
        let t = TempData::new("quickmark-replace");
        let mut data = t.open();
        assert!(!data.quickmark_save("go", "https://www.google.com/").unwrap());
        assert!(data.quickmark_save("go", "https://duckduckgo.com/").unwrap(), "an existing name is replaced");
        assert_eq!(data.quickmarks().len(), 1);
        assert_eq!(data.quickmark_load("go").unwrap(), "https://duckduckgo.com/");

        assert!(data.quickmark_del("go").unwrap());
        assert!(!data.quickmark_del("go").unwrap());
        assert_eq!(std::fs::read_to_string(t.dir.join("quickmarks")).unwrap(), "");
    }

    #[test]
    fn an_empty_quickmark_name_or_url_is_refused() {
        let t = TempData::new("quickmark-empty");
        let mut data = t.open();
        assert!(matches!(data.quickmark_save("", "https://example.com/"), Err(DataError::Empty("name"))));
        assert!(matches!(data.quickmark_save("  ", "https://example.com/"), Err(DataError::Empty("name"))));
        assert!(matches!(data.quickmark_save("x", ""), Err(DataError::Empty("URL"))));
        assert!(data.quickmarks().is_empty());
    }

    #[test]
    fn bookmarks_round_trip_and_the_m_binding_toggles() {
        let t = TempData::new("bookmarks");
        let mut data = t.open();
        assert!(data.bookmark_add("https://example.com/", "Example Domain", true).unwrap());
        assert!(data.bookmark_add("https://other.com/", "", true).unwrap());
        assert_eq!(
            std::fs::read_to_string(t.dir.join("bookmarks")).unwrap(),
            "https://example.com/ Example Domain\nhttps://other.com/\n"
        );

        // `M` on a page that is already bookmarked removes it — qutebrowser binds bookmark-add
        // with --toggle.
        assert!(!data.bookmark_add("https://example.com/", "Example Domain", true).unwrap());
        assert_eq!(data.bookmarks(), [Bookmark { url: "https://other.com/".into(), title: String::new() }]);

        // Without toggle, a repeat refreshes the title instead of erroring.
        assert!(!data.bookmark_add("https://other.com/", "Other", false).unwrap());
        assert_eq!(data.bookmarks()[0].title, "Other");

        let data = t.open();
        assert_eq!(data.bookmarks(), [Bookmark { url: "https://other.com/".into(), title: "Other".into() }]);
    }

    #[test]
    fn malformed_and_commented_lines_are_skipped_rather_than_fatal() {
        let t = TempData::new("malformed");
        std::fs::create_dir_all(&t.dir).unwrap();
        std::fs::write(t.dir.join("quickmarks"), "# a comment\n\n   \ngo https://google.com/\nnourlhere\n").unwrap();
        std::fs::write(t.dir.join("bookmarks"), "# c\n\nhttps://a.com/ A title\nhttps://b.com/\n").unwrap();

        let data = t.open();
        assert_eq!(data.quickmarks(), [Quickmark { name: "go".into(), url: "https://google.com/".into() }]);
        assert_eq!(
            data.bookmarks(),
            [
                Bookmark { url: "https://a.com/".into(), title: "A title".into() },
                Bookmark { url: "https://b.com/".into(), title: String::new() },
            ]
        );
    }

    /// `--private` covers bru's own history, which is the whole point of this workstream: the switch
    /// used to print a line saying it did not.
    ///
    /// Both halves matter. Take the `if self.private` out of `record_visit_at` and the first
    /// assertion fails; make `--private` discard the marks as well — qutebrowser's `--temp-basedir`
    /// behaviour — and the last two fail.
    #[test]
    fn a_private_run_records_no_history_and_still_saves_what_was_saved_by_name() {
        let t = TempData::new("private");

        // An ordinary run first, so there is something to read and something to leave alone.
        {
            let mut data = t.open();
            data.record_visit_at("https://before.example/", "Before", false, 1000).unwrap();
        }

        let mut data = t.open_private();

        assert!(
            !data.record_visit_at("https://secret.example/", "Secret", false, 2000).unwrap(),
            "a private run must record no visit"
        );
        assert!(
            !data.record_visit_at("https://secret.example/", "", true, 2001).unwrap(),
            "not even a redirect, which an ordinary run does log"
        );
        // The title correction is the other write into the two tables, and it is an UPDATE rather
        // than an INSERT — so it would leave a trace on a row that already existed.
        data.update_title("https://before.example/", "Retitled by the private run").unwrap();

        assert_eq!(data.history_counts().unwrap(), (1, 1), "only the earlier run's visit is there");
        assert_eq!(
            data.history_completion("before", 10).unwrap()[0].title,
            "Before",
            "a private run must not rewrite a title it did not record"
        );
        // Reading is untouched: the completion is why --private is usable at all.
        assert_eq!(data.history_completion("before", 10).unwrap().len(), 1);
        assert!(data.history_completion("secret", 10).unwrap().is_empty());

        // And the two things the user saved by name are saved, on disk, exactly as always.
        data.quickmark_save("q", "https://kept.example/").unwrap();
        data.bookmark_add("https://kept.example/", "Kept", true).unwrap();
        assert_eq!(std::fs::read_to_string(t.dir.join("quickmarks")).unwrap(), "q https://kept.example/\n");
        assert_eq!(std::fs::read_to_string(t.dir.join("bookmarks")).unwrap(), "https://kept.example/ Kept\n");

        // They survive the private run, because they are not the private run's to throw away.
        let after = t.open();
        assert_eq!(after.quickmark_load("q").unwrap(), "https://kept.example/");
        assert_eq!(after.bookmarks().len(), 1);
        assert_eq!(after.history_counts().unwrap(), (1, 1));
    }

    #[test]
    fn nothing_here_names_qutebrowser() {
        // DESIGN.md, and DECISIONS.md item 3 decided 2026-08-06: bru is fully independent. This
        // catches a path to qutebrowser's own files being pasted in later, in this module, by
        // anyone. Comments say "qutebrowser" all over the file — a *path* is the thing that must
        // never appear.
        let source = include_str!("data.rs");
        // Assembled at runtime rather than written out, or this test's own needles would be the
        // thing it finds.
        let other = format!("{}browser", "qute");
        for needle in [format!(".config/{other}"), format!(".local/share/{other}"), format!("{other}/quickmarks")] {
            assert!(!source.contains(&needle), "src/data.rs must never name {needle}");
        }
    }

    #[test]
    fn the_data_dir_is_bru_own_under_xdg_data_home() {
        // Not `std::env::set_var` — it is unsafe in edition 2024 and races the other tests' threads.
        // The precedence is asserted against whatever the environment holds, which on this machine
        // is $HOME with no $XDG_DATA_HOME.
        let dir = data_dir().expect("HOME is set in any environment this runs in");
        assert!(dir.ends_with("bru"), "{dir:?}");
        assert!(dir.is_absolute(), "{dir:?}");
        assert!(!dir.to_string_lossy().contains("qutebrowser"), "{dir:?}");
        if std::env::var_os("XDG_DATA_HOME").is_none() {
            assert!(dir.ends_with(".local/share/bru"), "{dir:?}");
        }
    }

    /// The number that decides whether this milestone worked.
    ///
    /// The user's qutebrowser profile holds ~9,100 History rows, so that is the size bru's own
    /// history will reach. The completion query runs on every keystroke of `:open …`, between the
    /// key going down and the frame that shows the result — anything a person can feel here is a
    /// failure, and "it feels fast" is not a measurement.
    ///
    /// Measured on this machine, printed by `cargo test -- --nocapture`.
    #[test]
    fn completion_at_nine_thousand_rows() {
        let t = TempData::new("nine-thousand");
        let mut data = t.open();

        // 9,000 distinct pages across 300 hosts, plus a second visit to every tenth one so the
        // visit log is bigger than the completion table, as a real profile's is.
        let hosts = 300;
        let build = std::time::Instant::now();
        // One transaction around the lot: this is generating a fixture, not measuring writes, and
        // 9,900 separate WAL commits would dominate the test's runtime.
        data.conn.execute_batch("BEGIN").unwrap();
        for i in 0..9000i64 {
            let url = format!("https://host{}.example.com/page/{}/some-path-segment", i % hosts, i);
            let title = format!("Page {i} on host {} — a title of about the usual length", i % hosts);
            data.record_visit_at(&url, &title, false, 1_700_000_000 + i).unwrap();
            if i % 10 == 0 {
                data.last_url = None;
                data.record_visit_at(&url, &title, false, 1_760_000_000 + i).unwrap();
            }
        }
        data.conn.execute_batch("COMMIT").unwrap();
        let build = build.elapsed();
        let (visits, completion) = data.history_counts().unwrap();
        assert_eq!((visits, completion), (9900, 9000));

        // The patterns a person actually types on the way to a URL: one character, then more.
        let patterns = ["h", "ho", "hos", "host1", "host12", "host12 page", "usual length", "zzzznotfound"];
        let mut worst = std::time::Duration::ZERO;
        println!("\n  {completion} completion rows ({visits} visits), built in {build:?}");
        for pattern in patterns {
            // Ten runs: the first pays for preparing the statement, the rest are what a keystroke
            // after the first one costs.
            let mut hits = 0;
            let start = std::time::Instant::now();
            for _ in 0..10 {
                hits = data.history_completion(pattern, 25).unwrap().len();
            }
            let each = start.elapsed() / 10;
            worst = worst.max(each);
            println!("  {each:>12?}  {hits:>2} hits  {pattern:?}");
        }
        println!();

        // Generous by two orders of magnitude against what this machine measures, because the
        // point of the assert is to catch the index being dropped or the query being rewritten
        // into a scan, not to fail on a loaded build machine.
        assert!(worst < std::time::Duration::from_millis(50), "the completion query took {worst:?} at 9,000 rows");
    }
}

// -----------------------------------------------------------------------------------------------
// The completion module's view of this one
// -----------------------------------------------------------------------------------------------

/// Binds `src/data.rs` to `src/completion.rs`.
///
/// The two were written in parallel against a contract, and they met in the middle differently:
/// completion expected to walk every row and filter in Rust, this module filters in SQL. SQL won on
/// a measurement — 0.74 ms against 9,000 rows on the worst realistic keystroke — and it is also
/// where qutebrowser filters history (`histcategory.py:75-83`). So the walk is fed rows that are
/// already filtered, ordered and limited; completion's own filter then passes all of them, which
/// costs nothing and keeps its unit tests meaningful against in-memory fixtures.
pub struct DataSources;

impl crate::completion::Sources for DataSources {
    fn search_engines(&self) -> Vec<(String, String)> {
        crate::open::engines().for_completion()
    }

    fn quickmarks(&self) -> Vec<(String, String)> {
        with_data(|data| {
            data.quickmarks()
                .iter()
                .map(|mark| (mark.url.clone(), mark.name.clone()))
                .collect()
        })
        .unwrap_or_default()
    }

    fn bookmarks(&self) -> Vec<(String, String)> {
        with_data(|data| {
            data.bookmarks()
                .iter()
                .map(|mark| (mark.url.clone(), mark.title.clone()))
                .collect()
        })
        .unwrap_or_default()
    }

    fn history(
        &self,
        pattern: &str,
        visit: &mut dyn FnMut(crate::completion::HistoryRow<'_>) -> crate::completion::Flow,
    ) {
        let rows = with_data(|data| {
            data.history_completion(pattern, crate::completion::MAX_ITEMS)
                .unwrap_or_default()
        })
        .unwrap_or_default();

        for row in &rows {
            let flow = visit(crate::completion::HistoryRow {
                url: &row.url,
                title: &row.title,
                atime: &row.atime,
            });
            if flow == crate::completion::Flow::Stop {
                break;
            }
        }
    }
}

/// Runs `f` against the one open `Data`, or answers `None` when there is no data directory to open.
/// A completion that cannot reach the database shows the categories it can and no history — never
/// an error in the user's face while they are typing.
fn with_data<T>(f: impl FnOnce(&Data) -> T) -> Option<T> {
    let data = instance()?;
    let guard = data.lock().ok()?;
    Some(f(&guard))
}
