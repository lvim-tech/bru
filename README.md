# bru

A keyboard-driven browser, written in Rust on CEF 151.

It exists for one reason: **scrolling.** qutebrowser's never felt like Brave's, and the cause turned
out not to be the engine — both run Blink — but Qt feeding QtWebEngine discrete wheel steps. bru
puts nothing between the keyboard and Chromium: `j` and `k` are `send_mouse_wheel_event`, the same
call a real wheel makes, and the difference is immediate.

Everything else follows from keeping that true.

## What it is

- **qutebrowser's vocabulary.** The default bindings are transcribed from `configdata.yml`, so `f`
  hints, `o` opens, `d` closes a tab, `gg` and `G` jump, `:` is the command line. 288 default
  bindings, 166 commands, 67 settings.
- **One binary.** No embedded runtime to install, no Python, no Qt. CEF is a prebuilt Chromium
  distribution — nothing here compiles a browser engine.
- **Its own data.** `~/.local/share/bru/` holds history, quickmarks and bookmarks. bru neither reads
  nor writes any other browser's files.
- **Configured in Lua**, and only where you say so. Every setting has a default compiled in, so a bru
  with no configuration at all is fully configured; `~/.config/bru/config.lua` holds overrides. Lua
  rather than a data format because a setting is allowed to be a *function* — a tab title can be
  computed from the tab.

## Building

CEF is a 1.5 GB prebuilt distribution and is not vendored. Export it once from a
[cef-rs](https://github.com/tauri-apps/cef-rs) checkout:

```sh
cargo run -p export-cef-dir -- --force ~/.local/share/cef
```

Then:

```sh
export CEF_PATH="$HOME/.local/share/cef"
export LD_LIBRARY_PATH="$LD_LIBRARY_PATH:$CEF_PATH"
cargo build && ./target/debug/bru
```

## Using it

`:help` — or `bru://chrome/help` — is generated from the running binary rather than written
alongside it, so it lists the keys and commands this build actually has. The same is true of
`bru://chrome/settings`, which shows every setting beside what Chromium is really enforcing.

A few worth knowing:

| | |
|---|---|
| `f` / `F` | hint a link, in this tab or a new one |
| `o` / `O` | open, in this tab or a new one |
| `d` / `u` | close a tab, undo that |
| `wi` | the web inspector — `wIj` below the page, `wIl` beside it, `wIw` in a window |
| `gd` | download the page |
| `:` | the command line, with completion over commands, settings, history and marks |

The docked inspector has a divider you can drag, and the download prompt is a file picker: type to
search, `<Tab>` to complete to the real folder and step into it, `<Shift-Tab>` to step back out.

## Scripting

bru listens on a unix socket, so a running browser can be driven from a shell:

```sh
bru --remote ':open -t https://example.com'
bru --remote 'js 0 document.title'
```

A second browser needs a socket of its own — `bru --socket=/tmp/b.sock --url=… &`, then
`bru --socket=/tmp/b.sock --remote '…'`. Without it, `--remote` reaches whichever browser bound the
default address first, which is the one you are using.

Plugins are Lua, loaded from `~/.local/share/bru/plugins`, and can register commands, answer events
and read settings. They are never on the key path: a binding names a command bru implements in Rust,
so pressing `j` does not enter an interpreter.

## Where it stands

64 000 lines across 53 modules, 614 tests. Ad blocking uses Brave's own engine, linked directly
rather than through a binding. Sessions, cookies, downloads, hints, caret mode, marks, macros,
per-site stylesheets and userscripts are implemented; the notable gaps are printing beyond
Chromium's own dialog, and an inspector docked to the left or the top.

Linux and Wayland are what it is developed and measured on. Nothing in it is Linux-only by design,
and nothing else has been tried.
