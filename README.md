# bru

A keyboard-driven browser, written in Rust on CEF 151.

It exists for one reason: **scrolling.** qutebrowser's never felt like Brave's, and the cause turned
out not to be the engine — both run Blink — but Qt feeding QtWebEngine discrete wheel steps. bru
puts nothing between the keyboard and Chromium: `j` and `k` are `send_mouse_wheel_event`, the same
call a real wheel makes, and the difference is immediate.

Everything else follows from keeping that true.

## What it is

- **qutebrowser's vocabulary.** The default bindings are transcribed from `configdata.yml`, so `f`
  hints, `o` opens, `d` closes a tab, `gg` and `G` jump, `:` is the command line.
  **288 default bindings, 173 commands, 68 settings.**
- **One binary.** No embedded runtime to install, no Python, no Qt. CEF is a prebuilt Chromium
  distribution — nothing here compiles a browser engine.
- **Its own data.** `~/.local/share/bru/` holds history, quickmarks, bookmarks, sessions and the
  filter lists. bru neither reads nor writes any other browser's files.
- **Configured in Lua**, and only where you say so. Every setting has a default compiled in, so a
  bru with no configuration at all is fully configured; `~/.config/bru/config.lua` holds overrides.
  Lua rather than a data format because a setting is allowed to be a *function* — a tab title can be
  computed from the tab.
- **Lua is never on the key path.** A binding names a command bru implements in Rust. Pressing `j`
  does not enter an interpreter, which is the whole point of the paragraph above this list.

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

Linux and Wayland are what it is developed and measured on. Nothing in it is Linux-only by design,
and nothing else has been tried.

## Modes

bru is modal, in qutebrowser's sense: which mode a window is in decides what a key means. The mode
is drawn in the status bar, and `<Escape>` leaves any mode that can be left.

| mode | how you get there | bindings |
|---|---|---|
| `normal` | the default | 177 |
| `insert` | following a hint into a text field, or `i` | 4 |
| `command` | `:`, `/`, `?` | 32 |
| `hint` | `f`, `F`, `;` and friends | 5 |
| `caret` | `v` | 29 |
| `passthrough` | `<Ctrl-v>` — every key goes to the page | 1 |
| `prompt` | a question with a line to type into | 28 |
| `yesno` | a question that takes `y` or `n` | 8 |
| `set_mark` / `jump_mark` | `m` and `` ` `` — the next key names a register | 1 / 1 |
| `record_macro` / `run_macro` | `q` and `@` | 1 / 1 |

**Insert mode is only ever entered by a click bru itself sent** — a hint, or `:click-element`. Not
by a page focusing its own field, not by Tab, and not by a real mouse click, because bru cannot tell
those apart from a page taking the focus back and would then be unable to leave insert mode with
`<Escape>`. `input.insert_mode.auto_load` turns the first of those back on if you want it.

## Keys

The full, live list is `:help` — or `bru://chrome/help` — which is generated from the running binary
rather than written beside it, so it can never drift from the build you have.

A few worth knowing first:

| | |
|---|---|
| `f` / `F` | hint a link, in this tab or a new one |
| `o` / `O` | open, in this tab or a new one |
| `d` / `u` | close a tab, undo that |
| `wi` | the web inspector — `wIj` below the page, `wIl` beside it, `wIw` in a window |
| `gd` | download the page |
| `:` | the command line, with completion over commands, settings, history and marks |

<details>
<summary>Every normal-mode binding (177)</summary>

| key | command |
|---|---|
| `'` | `mode-enter jump_mark` |
| `+` | `zoom-in` |
| `-` | `zoom-out` |
| `.` | `cmd-repeat-last` |
| `/` | `cmd-set-text /` |
| `:` | `cmd-set-text :` |
| `;I` | `hint images tab` |
| `;O` | `hint links fill :open -t -r {hint-url}` |
| `;R` | `hint --rapid links window` |
| `;Y` | `hint links yank-primary` |
| `;b` | `hint all tab-bg` |
| `;d` | `hint links download` |
| `;f` | `hint all tab-fg` |
| `;h` | `hint all hover` |
| `;i` | `hint images` |
| `;o` | `hint links fill :open {hint-url}` |
| `;r` | `hint --rapid links tab-bg` |
| `;t` | `hint inputs` |
| `;y` | `hint links yank` |
| `<Alt+1>` | `tab-focus 1` |
| `<Alt+2>` | `tab-focus 2` |
| `<Alt+3>` | `tab-focus 3` |
| `<Alt+4>` | `tab-focus 4` |
| `<Alt+5>` | `tab-focus 5` |
| `<Alt+6>` | `tab-focus 6` |
| `<Alt+7>` | `tab-focus 7` |
| `<Alt+8>` | `tab-focus 8` |
| `<Alt+9>` | `tab-focus -1` |
| `<Alt+m>` | `tab-mute` |
| `<Ctrl+Alt+p>` | `print` |
| `<Ctrl+F5>` | `reload -f` |
| `<Ctrl+PgDown>` | `tab-next` |
| `<Ctrl+PgUp>` | `tab-prev` |
| `<Ctrl+Return>` | `selection-follow -t` |
| `<Ctrl+Shift+Tab>` | `nop` |
| `<Ctrl+Shift+n>` | `open -p` |
| `<Ctrl+Shift+t>` | `undo` |
| `<Ctrl+Shift+w>` | `close` |
| `<Ctrl+Tab>` | `tab-focus last` |
| `<Ctrl+^>` | `tab-focus last` |
| `<Ctrl+a>` | `navigate increment` |
| `<Ctrl+b>` | `scroll-page 0 -1` |
| `<Ctrl+d>` | `scroll-page 0 0.5` |
| `<Ctrl+f>` | `scroll-page 0 1` |
| `<Ctrl+h>` | `home` |
| `<Ctrl+n>` | `open -w` |
| `<Ctrl+p>` | `tab-pin` |
| `<Ctrl+q>` | `quit` |
| `<Ctrl+s>` | `stop` |
| `<Ctrl+t>` | `open -t` |
| `<Ctrl+u>` | `scroll-page 0 -0.5` |
| `<Ctrl+v>` | `mode-enter passthrough` |
| `<Ctrl+w>` | `tab-close` |
| `<Ctrl+x>` | `navigate decrement` |
| `<Escape>` | `clear-keychain ;; search ;; fullscreen --leave` |
| `<F11>` | `fullscreen` |
| `<F5>` | `reload` |
| `<Return>` | `selection-follow` |
| `<back>` | `back` |
| `<forward>` | `forward` |
| `=` | `zoom` |
| `?` | `cmd-set-text ?` |
| `@` | `macro-run` |
| `B` | `cmd-set-text -s :quickmark-load -t` |
| `D` | `tab-close -o` |
| `F` | `hint all tab` |
| `G` | `scroll-to-perc` |
| `H` | `back` |
| `J` | `tab-next` |
| `K` | `tab-prev` |
| `L` | `forward` |
| `M` | `bookmark-add` |
| `N` | `search-prev` |
| `O` | `cmd-set-text -s :open -t` |
| `PP` | `open -t -- {primary}` |
| `Pp` | `open -t -- {clipboard}` |
| `R` | `reload -f` |
| `Sb` | `bookmark-list --jump` |
| `Sh` | `history` |
| `Sq` | `bookmark-list` |
| `Ss` | `set` |
| `T` | `cmd-set-text -sr :tab-focus` |
| `U` | `undo -w` |
| `V` | `mode-enter caret ;; selection-toggle --line` |
| `ZQ` | `quit` |
| `ZZ` | `quit --save` |
| `[[` | `navigate prev` |
| `]]` | `navigate next` |
| <code>&#96;</code> | `mode-enter set_mark` |
| `ad` | `download-cancel` |
| `b` | `cmd-set-text -s :quickmark-load` |
| `cd` | `download-clear` |
| `co` | `tab-only` |
| `d` | `tab-close` |
| `f` | `hint` |
| `g$` | `tab-focus -1` |
| `g0` | `tab-focus 1` |
| `gB` | `cmd-set-text -s :bookmark-load -t` |
| `gC` | `tab-clone` |
| `gD` | `tab-give` |
| `gJ` | `tab-move +` |
| `gK` | `tab-move -` |
| `gO` | `cmd-set-text :open -t -r {url:pretty}` |
| `gU` | `navigate up -t` |
| `g^` | `tab-focus 1` |
| `ga` | `open -t` |
| `gb` | `cmd-set-text -s :bookmark-load` |
| `gd` | `download` |
| `gf` | `view-source` |
| `gg` | `scroll-to-perc 0` |
| `gi` | `hint inputs --first` |
| `gm` | `tab-move` |
| `go` | `cmd-set-text :open {url:pretty}` |
| `gt` | `cmd-set-text -s :tab-select` |
| `gu` | `navigate up` |
| `h` | `scroll left` |
| `i` | `mode-enter insert` |
| `j` | `scroll down` |
| `k` | `scroll up` |
| `l` | `scroll right` |
| `m` | `quickmark-save` |
| `n` | `search-next` |
| `o` | `cmd-set-text -s :open` |
| `pP` | `open -- {primary}` |
| `pp` | `open -- {clipboard}` |
| `q` | `macro-record` |
| `r` | `reload` |
| `sf` | `save` |
| `sk` | `cmd-set-text -s :bind` |
| `sl` | `cmd-set-text -s :set -t` |
| `ss` | `cmd-set-text -s :set` |
| `tIH` | `config-cycle -p -u *://*.{url:host}/* content.images ;; reload` |
| `tIh` | `config-cycle -p -u *://{url:host}/* content.images ;; reload` |
| `tIu` | `config-cycle -p -u {url} content.images ;; reload` |
| `tSH` | `config-cycle -p -u *://*.{url:host}/* content.javascript.enabled ;; reload` |
| `tSh` | `config-cycle -p -u *://{url:host}/* content.javascript.enabled ;; reload` |
| `tSu` | `config-cycle -p -u {url} content.javascript.enabled ;; reload` |
| `th` | `back -t` |
| `tiH` | `config-cycle -p -t -u *://*.{url:host}/* content.images ;; reload` |
| `tih` | `config-cycle -p -t -u *://{url:host}/* content.images ;; reload` |
| `tiu` | `config-cycle -p -t -u {url} content.images ;; reload` |
| `tl` | `forward -t` |
| `ts` | `styles-toggle` |
| `tsH` | `config-cycle -p -t -u *://*.{url:host}/* content.javascript.enabled ;; reload` |
| `tsh` | `config-cycle -p -t -u *://{url:host}/* content.javascript.enabled ;; reload` |
| `tsu` | `config-cycle -p -t -u {url} content.javascript.enabled ;; reload` |
| `u` | `undo` |
| `v` | `mode-enter caret` |
| `wB` | `cmd-set-text -s :bookmark-load -w` |
| `wIc` | `devtools-close` |
| `wIf` | `devtools-focus` |
| `wIj` | `devtools bottom` |
| `wIl` | `devtools right` |
| `wIw` | `devtools window` |
| `wO` | `cmd-set-text :open -w {url:pretty}` |
| `wP` | `open -w -- {primary}` |
| `wb` | `cmd-set-text -s :quickmark-load -w` |
| `wf` | `hint all window` |
| `wh` | `back -w` |
| `wi` | `devtools` |
| `wl` | `forward -w` |
| `wo` | `cmd-set-text -s :open -w` |
| `wp` | `open -w -- {clipboard}` |
| `xO` | `cmd-set-text :open -b -r {url:pretty}` |
| `xo` | `cmd-set-text -s :open -b` |
| `yD` | `yank domain -s` |
| `yM` | `yank inline [{title}]({url:yank}) -s` |
| `yP` | `yank pretty-url -s` |
| `yT` | `yank title -s` |
| `yY` | `yank -s` |
| `yd` | `yank domain` |
| `ym` | `yank inline [{title}]({url:yank})` |
| `yp` | `yank pretty-url` |
| `yt` | `yank title` |
| `yy` | `yank` |
| `{{` | `navigate prev -t` |
| `}}` | `navigate next -t` |

</details>

<details>
<summary>command mode (32)</summary>

| key | command |
|---|---|
| `<Alt+Backspace>` | `rl-backward-kill-word` |
| `<Alt+b>` | `rl-backward-word` |
| `<Alt+d>` | `rl-kill-word` |
| `<Alt+f>` | `rl-forward-word` |
| `<Ctrl+?>` | `rl-delete-char` |
| `<Ctrl+Return>` | `command-accept --rapid` |
| `<Ctrl+Shift+Tab>` | `completion-item-focus prev-category` |
| `<Ctrl+Shift+c>` | `completion-item-yank --sel` |
| `<Ctrl+Shift+w>` | `rl-filename-rubout` |
| `<Ctrl+Tab>` | `completion-item-focus next-category` |
| `<Ctrl+a>` | `rl-beginning-of-line` |
| `<Ctrl+b>` | `rl-backward-char` |
| `<Ctrl+c>` | `completion-item-yank` |
| `<Ctrl+d>` | `completion-item-del` |
| `<Ctrl+e>` | `rl-end-of-line` |
| `<Ctrl+f>` | `rl-forward-char` |
| `<Ctrl+h>` | `rl-backward-delete-char` |
| `<Ctrl+k>` | `rl-kill-line` |
| `<Ctrl+n>` | `command-history-next` |
| `<Ctrl+p>` | `command-history-prev` |
| `<Ctrl+u>` | `rl-unix-line-discard` |
| `<Ctrl+w>` | `rl-rubout \" \"` |
| `<Ctrl+y>` | `rl-yank` |
| `<Down>` | `completion-item-focus --history next` |
| `<Escape>` | `mode-leave` |
| `<PgDown>` | `completion-item-focus next-page` |
| `<PgUp>` | `completion-item-focus prev-page` |
| `<Return>` | `command-accept` |
| `<Shift+Del>` | `completion-item-del` |
| `<Shift+Tab>` | `completion-item-focus prev` |
| `<Tab>` | `completion-item-focus next` |
| `<Up>` | `completion-item-focus --history prev` |

</details>

<details>
<summary>caret mode (29)</summary>

| key | command |
|---|---|
| `$` | `move-to-end-of-line` |
| `0` | `move-to-start-of-line` |
| `<Ctrl+Space>` | `selection-drop` |
| `<Escape>` | `mode-leave` |
| `<Return>` | `yank selection` |
| `<Space>` | `selection-toggle` |
| `G` | `move-to-end-of-document` |
| `H` | `scroll left` |
| `J` | `scroll down` |
| `K` | `scroll up` |
| `L` | `scroll right` |
| `V` | `selection-toggle --line` |
| `Y` | `yank selection -s` |
| `[` | `move-to-start-of-prev-block` |
| `]` | `move-to-start-of-next-block` |
| `b` | `move-to-prev-word` |
| `c` | `mode-enter normal` |
| `e` | `move-to-end-of-word` |
| `gg` | `move-to-start-of-document` |
| `h` | `move-to-prev-char` |
| `j` | `move-to-next-line` |
| `k` | `move-to-prev-line` |
| `l` | `move-to-next-char` |
| `o` | `selection-reverse` |
| `v` | `selection-toggle` |
| `w` | `move-to-next-word` |
| `y` | `yank selection` |
| `{` | `move-to-end-of-prev-block` |
| `}` | `move-to-end-of-next-block` |

</details>

<details>
<summary>prompt and yesno modes (28 + 8)</summary>

| key | command |
|---|---|
| `<Alt+Backspace>` | `rl-backward-kill-word` |
| `<Alt+Shift+y>` | `prompt-yank --sel` |
| `<Alt+b>` | `rl-backward-word` |
| `<Alt+d>` | `rl-kill-word` |
| `<Alt+e>` | `prompt-fileselect-external` |
| `<Alt+f>` | `rl-forward-word` |
| `<Alt+y>` | `prompt-yank` |
| `<Ctrl+?>` | `rl-delete-char` |
| `<Ctrl+Shift+w>` | `rl-filename-rubout` |
| `<Ctrl+a>` | `rl-beginning-of-line` |
| `<Ctrl+b>` | `rl-backward-char` |
| `<Ctrl+e>` | `rl-end-of-line` |
| `<Ctrl+f>` | `rl-forward-char` |
| `<Ctrl+h>` | `rl-backward-delete-char` |
| `<Ctrl+k>` | `rl-kill-line` |
| `<Ctrl+p>` | `prompt-open-download --pdfjs` |
| `<Ctrl+u>` | `rl-unix-line-discard` |
| `<Ctrl+w>` | `rl-rubout \" \"` |
| `<Ctrl+x>` | `prompt-open-download` |
| `<Ctrl+y>` | `rl-yank` |
| `<Down>` | `prompt-item-focus next` |
| `<Escape>` | `mode-leave` |
| `<Left>` | `rl-backward-char` |
| `<Return>` | `prompt-accept` |
| `<Right>` | `rl-forward-char` |
| `<Shift+Tab>` | `prompt-dir out` |
| `<Tab>` | `prompt-dir in` |
| `<Up>` | `prompt-item-focus prev` |

| key | command |
|---|---|
| `<Alt+Shift+y>` | `prompt-yank --sel` |
| `<Alt+y>` | `prompt-yank` |
| `<Escape>` | `mode-leave` |
| `<Return>` | `prompt-accept` |
| `N` | `prompt-accept --save no` |
| `Y` | `prompt-accept --save yes` |
| `n` | `prompt-accept no` |
| `y` | `prompt-accept yes` |

</details>

<details>
<summary>hint, insert, passthrough, mark and macro modes</summary>

| key | command |
|---|---|
| `<Ctrl+b>` | `hint all tab-bg` |
| `<Ctrl+f>` | `hint links` |
| `<Ctrl+r>` | `hint --rapid links tab-bg` |
| `<Escape>` | `mode-leave` |
| `<Return>` | `hint-follow` |
| key | command |
|---|---|
| `<Ctrl+e>` | `edit-text` |
| `<Escape>` | `mode-leave` |
| `<Shift+Escape>` | `fake-key <Escape>` |
| `<Shift+Ins>` | `insert-text -- {primary}` |
| key | command |
|---|---|
| `<Shift+Escape>` | `mode-leave` |
| key | command |
|---|---|
| `<Escape>` | `mode-leave` |
| key | command |
|---|---|
| `<Escape>` | `mode-leave` |
| key | command |
|---|---|
| `<Escape>` | `mode-leave` |
| key | command |
|---|---|
| `<Escape>` | `mode-leave` |

</details>

Rebind with `:bind`, or in `config.lua`:

```lua
bru.bind("normal", "<Ctrl+,>", "set")
bru.unbind("normal", "d")
```

## Commands

Every command below is what a binding names, what `:` accepts, and what `bru.cmd()` runs from Lua —
one vocabulary, three doors. Arguments in `<>` are required and `[]` optional.

### Navigating

| command | arguments | flags | what it does |
|---|---|---|---|
| `open` | `[url]` | `-t/--tab`, `-b/--bg`, `-w/--window`, `-p/--private`, `-r/--related` | Open a URL, a file or a search. What a bare word means is decided by src/open.rs. |
| `back` | — | `-t/--tab`, `-b/--bg`, `-w/--window` | Go back in this tab's history. A count goes back that many entries. |
| `forward` | — | `-t/--tab`, `-b/--bg`, `-w/--window` | Go forward in this tab's history. |
| `reload` | — | `-f/--force` | Reload the page; with -f, ignoring the cache. |
| `stop` | — | — | Stop loading the page. |
| `home` | — | — | Open the start page in this tab. |
| `navigate` | `<prev\|next\|up\|increment\|decrement\|strip>` | `-t/--tab`, `-b/--bg`, `-w/--window` | Follow a next/previous link, walk up the path, or step the last number in the URL. |
| `scroll-to-anchor` | `<name>` | — | Go to a fragment on the page. A navigation, not a wheel event. |
| `view-source` | — | `-e/--edit`, `--pygments` |  |
| `print` | — | — | Hand the page to Chromium's print dialog. |
| `save` | `[what…]` | — | Write bru's own files — history, quickmarks, bookmarks — to disk now. Not the page. |
| `screenshot` | `<filename>` | `--rect`, `-f/--force` | Write the showing page to a PNG; --rect WxH+X+Y takes part of it. |
| `edit-url` | `[url]` | `-t/--tab`, `-b/--bg`, `-w/--window` | Edit the page's URL in $EDITOR and open it if it changed. |
| `yank` | `[url\|pretty-url\|title\|domain\|selection\|inline <text>]` | `-s/--sel` | Copy something about the page to the clipboard, or with -s to the primary selection. |
| `click-element` | `<id\|css\|position\|focused> [value]` | `--target`, `--force-event`, `--select-first` | Click an element the page holds, chosen without a hint label. |

### Tabs and windows

| command | arguments | flags | what it does |
|---|---|---|---|
| `tab-clone` | — | `-b/--bg`, `-w/--window`, `-p/--private` | Open the showing page a second time. |
| `tab-close` | — | `-o/--opposite`, `-f/--force` | Close the showing tab, or with -o every tab on the other side of it. |
| `tab-focus` | `[index]` | — |  |
| `tab-give` | `[window-id]` | — | Move the showing tab to another window, or with no id to a new one. |
| `tab-move` | `[+\|-\|start\|end\|index]` | — | Move the showing tab along the strip. |
| `tab-mute` | — | — | Mute the showing tab's audio, or unmute it. |
| `tab-next` | — | — | Show the next tab. |
| `tab-only` | — | `-f/--force` | Close every tab in this window except the one showing. |
| `tab-pin` | — | — | Pin the showing tab, or unpin it: a pinned tab keeps its place and asks before it closes. |
| `tab-prev` | — | — | Show the previous tab. |
| `tab-select` | `[[window-id/]index or text]` | — | Show a tab by address, or by a word in its title or URL. |
| `tab-take` | `<[window-id/]index>` | `-k/--keep` | Take a tab from another window into this one. |
| `close` | — | — | Close this window. The last one closing exits. |
| `undo` | — | `-w/--window` | Reopen the last closed tab, or with -w the last closed window and its tabs. |
| `window-only` | — | — | Close every window except this one. |
| `quit` | — | `--save` | Close every window and exit. |
| `restart` | — | — | Save the open tabs, start bru again and reopen them. |

### Scrolling and zoom

| command | arguments | flags | what it does |
|---|---|---|---|
| `scroll` | `<up\|down\|left\|right\|top\|bottom\|page-up\|page-down>` | — | Scroll the page. A count repeats it. This is the wheel event bru was built for. |
| `scroll-page` | `<x> <y>` | — | Scroll by a fraction of a page. A count multiplies both. |
| `scroll-px` | `<dx> <dy>` | — | Scroll by a number of pixels. |
| `scroll-to-perc` | `[percentage]` | `-x/--horizontal` | Jump to a percentage of the page; with no percentage, to its end. |
| `zoom` | `[percentage]` | — | Set the page's zoom; with no value, back to 100%. |
| `zoom-in` | — | — | Zoom in one step. |
| `zoom-out` | — | — | Zoom out one step. |
| `fullscreen` | — | `--enter`, `--leave` | Toggle the window's fullscreen, or force it either way. |

### Hints

| command | arguments | flags | what it does |
|---|---|---|---|
| `hint` | `[group] [target] [text]` | `--mode`, `--add-history`, `-r/--rapid`, `-f/--first` |  |
| `hint-follow` | — | — |  |

### Searching

| command | arguments | flags | what it does |
|---|---|---|---|
| `search` | `[text]` | `-r/--reverse` | Find text on the page. With no text the search is cleared. |
| `search-next` | — | — | Go to the next match, in the direction the search was started in. |
| `search-prev` | — | — | Go to the previous match. |

### Caret mode and selection

| command | arguments | flags | what it does |
|---|---|---|---|
| `move-to-end-of-document` | — | — | Caret: to the bottom of the document. |
| `move-to-end-of-line` | — | — | Caret: to the end of the line. |
| `move-to-end-of-next-block` | — | — | Caret: to the end of the next block. |
| `move-to-end-of-prev-block` | — | — | Caret: to the end of the previous block. |
| `move-to-end-of-word` | — | — | Caret: to the end of this word. |
| `move-to-next-char` | — | — | Caret: one character right. |
| `move-to-next-line` | — | — | Caret: one line down. |
| `move-to-next-word` | — | — | Caret: to the start of the next word. |
| `move-to-prev-char` | — | — | Caret: one character left. |
| `move-to-prev-line` | — | — | Caret: one line up. |
| `move-to-prev-word` | — | — | Caret: to the start of the previous word. |
| `move-to-start-of-document` | — | — | Caret: to the top of the document. |
| `move-to-start-of-line` | — | — | Caret: to the start of the line. |
| `move-to-start-of-next-block` | — | — | Caret: to the start of the next block. |
| `move-to-start-of-prev-block` | — | — | Caret: to the start of the previous block. |
| `selection-drop` | — | — | Drop the selection and keep the caret. |
| `selection-follow` | — | `-t/--tab` | Follow the link the selection is on. |
| `selection-reverse` | — | — | Swap which end of the selection the caret is on. |
| `selection-toggle` | — | `--line` | Start or stop selecting from the caret. |

### The command line

| command | arguments | flags | what it does |
|---|---|---|---|
| `cmd-set-text` | `<text>` | `-s/--space`, `-a/--append`, `-r/--run-on-count` |  |
| `cmd-edit` | — | `--run` | Edit the command line in $EDITOR. |
| `cmd-run-with-count` | `<count> <command>` | — | Run a command as though a count had been typed before it. |
| `cmd-repeat` | `<times> <command>` | — | Run a command several times. A count multiplies the number. |
| `cmd-repeat-last` | — | — | Run the last command again. |
| `cmd-later` | `<duration> <command>` | — |  |
| `command-accept` | — | `--rapid` | Run what the command line holds; with --rapid, and stay in command mode. |
| `command-history-next` | — | — | Recall the next line from the command history. |
| `command-history-prev` | — | — | Recall the previous line from the command history. |
| `completion-item-focus` | `<next\|prev\|next-category\|prev-category\|next-page\|prev-page>` | `-H/--history` |  |
| `completion-item-del` | — | — | Delete the completion entry that is highlighted. |
| `completion-item-yank` | — | `--sel` | Copy the highlighted completion entry. |
| `edit-command` | — | `--run` | Edit the command line in $EDITOR. |
| `repeat` | `<times> <command>` | — | Run a command several times. A count multiplies the number. |
| `repeat-command` | — | — | Run the last command again. |
| `run-with-count` | `<count> <command>` | — | Run a command as though a count had been typed before it. |
| `later` | `<duration> <command>` | — |  |

### Readline editing (in the command line and prompts)

| command | arguments | flags | what it does |
|---|---|---|---|
| `rl-backward-char` | — | — | Command line: one character left. |
| `rl-backward-delete-char` | — | — | Command line: delete the character before the cursor. |
| `rl-backward-kill-word` | — | — | Command line: delete the word before the cursor. |
| `rl-backward-word` | — | — | Command line: one word left. |
| `rl-beginning-of-line` | — | — | Command line: to the start. |
| `rl-delete-char` | — | — | Command line: delete the character under the cursor. |
| `rl-end-of-line` | — | — | Command line: to the end. |
| `rl-filename-rubout` | — | — | Command line: delete back one path segment. |
| `rl-forward-char` | — | — | Command line: one character right. |
| `rl-forward-word` | — | — | Command line: one word right. |
| `rl-kill-line` | — | — | Command line: delete forward to the end. |
| `rl-kill-word` | — | — | Command line: delete the word after the cursor. |
| `rl-rubout` | `<delimiters>` | — | Command line: delete back to one of the characters given. |
| `rl-unix-filename-rubout` | — | — | Command line: delete back one path or word. |
| `rl-unix-line-discard` | — | — | Command line: delete back to the start. |
| `rl-unix-word-rubout` | — | — | Command line: delete back one whitespace-separated word. |
| `rl-yank` | — | — | Command line: put back what was last deleted. |

### Prompts and downloads

| command | arguments | flags | what it does |
|---|---|---|---|
| `download` | `[url]` | `--dest`, `-m/--mhtml` |  |
| `download-cancel` | — | `-a/--all` | Cancel a download the count names, or the last one. |
| `download-clear` | — | — | Forget the finished downloads. No file is touched. |
| `download-delete` | — | — | Delete a finished download's file and its row. |
| `download-open` | `[command]` | `-d/--dir` | Open a finished download, or with -d the directory it landed in. |
| `download-remove` | — | `-a/--all` | Take a finished download off the list, keeping its file. |
| `download-retry` | — | — | Start a failed download again. |
| `prompt-accept` | `[value]` | `--save` | Answer the question that is open; --save remembers the answer for this site. |
| `prompt-dir` | `<in\|out>` | — |  |
| `prompt-fileselect-external` | — | — | Hand a file question to a real file browser. |
| `prompt-item-focus` | `<next\|prev>` | — | Move through a question's file list, or between a login's two fields. |
| `prompt-open-download` | `[command]` | `--pdfjs` |  |
| `prompt-yank` | — | `--sel` | Copy the URL the open question is about. |

### History, marks, sessions

| command | arguments | flags | what it does |
|---|---|---|---|
| `history` | — | `-b/--bg` | Open the page that lists what bru has visited. |
| `history-clear` | — | `-f/--force` | Empty bru's visit log and the completion built from it. |
| `quickmark-add` | `<url> <name>` | — | Save a URL as a quickmark under a name, naming both. |
| `quickmark-del` | `[name]` | — | Delete a quickmark; with no name, the one on the showing page. |
| `quickmark-load` | `<name>` | `-t/--tab`, `-b/--bg`, `-w/--window` | Open a quickmark. |
| `quickmark-save` | `[name]` | — |  |
| `quickmarks-reload` | — | — | Re-read the quickmarks file from disk. |
| `bookmark-add` | `[url] [title]` | `--toggle` | Bookmark a URL, or the showing page. |
| `bookmark-del` | `[url]` | — | Delete a bookmark; with no URL, the showing page's. |
| `bookmark-list` | — | `--jump`, `-b/--bg` | Open the page that lists the bookmarks and quickmarks. |
| `bookmark-load` | `<url>` | `-t/--tab`, `-b/--bg`, `-w/--window`, `-d/--delete` | Open a bookmark, and with -d delete it as it opens. |
| `bookmarks-reload` | — | — | Re-read the bookmarks file from disk. |
| `session-save` | `[name]` | `-f/--force` | Write the open windows and tabs to a session file. |
| `session-load` | `<name>` | `-c/--clear`, `--history` |  |
| `session-delete` | `<name>` | — | Delete a session file. |

### Modes, macros, text

| command | arguments | flags | what it does |
|---|---|---|---|
| `mode-enter` | `<mode>` | — | Enter a mode by name. The modes a question puts you in cannot be entered this way. |
| `mode-leave` | — | — | Leave the mode you are in for normal mode. |
| `clear-keychain` | — | — | Forget a half-typed key sequence. |
| `nop` | — | — | Do nothing. Bound where a key must reach neither the page nor a browser default. |
| `fake-key` | `<keystring>` | `-g/--global` | Send a keypress to the page as if it had been typed. |
| `insert-text` | `<text>` | — | Type text into the focused field. |
| `edit-text` | — | — | Edit the focused text field in $EDITOR. |
| `open-editor` | — | — | Edit the focused text field in $EDITOR. |
| `macro-record` | `[register]` | — | Start recording keys into a register, or stop the recording that is running. |
| `macro-run` | `[register]` | — | Replay a register. A count replays it that many times; @ means the last one run. |

### Configuration

| command | arguments | flags | what it does |
|---|---|---|---|
| `set` | `[option] [value]` | `-p/--print`, `-t/--temp`, `-u/--pattern/--url` |  |
| `bind` | `[key] [command]` | `-m/--mode`, `-d/--default` |  |
| `unbind` | `<key>` | `-m/--mode` | Take a binding out of the running browser. |
| `config-cycle` | `<option> [values…]` | `-p/--print`, `-t/--temp`, `-u/--pattern/--url` | Step a setting through a list of values, or through true and false. |
| `config-dict-add` | `<option> <key> <value>` | `-p/--print`, `-t/--temp`, `-u/--pattern/--url`, `--replace` | Put one pair into a dictionary setting. |
| `config-dict-remove` | `<option> <key>` | `-p/--print`, `-t/--temp`, `-u/--pattern/--url` |  |
| `config-list-add` | `<option> <value>` | `-p/--print`, `-t/--temp`, `-u/--pattern/--url` | Append one entry to a list setting. |
| `config-list-remove` | `<option> <value>` | `-p/--print`, `-t/--temp`, `-u/--pattern/--url` | Take one entry out of a list setting. |
| `config-unset` | `<option>` | `-p/--print`, `-t/--temp`, `-u/--pattern/--url` | Put one setting back to bru's own value. |
| `config-clear` | — | `--save` |  |
| `config-diff` | — | — |  |
| `config-write` | `<file>` | `--defaults`, `--force` |  |
| `config-source` | `[filename]` | `--clear` | Re-read config.lua over the running browser. |
| `config-edit` | — | `--no-source/--no_source` |  |

### Blocking

| command | arguments | flags | what it does |
|---|---|---|---|
| `adblock-info` | — | — | What is loaded, what it has blocked, and what it costs per request. |
| `adblock-toggle` | — | — | Turn blocking on or off for this session. |
| `adblock-update` | — | — | Fetch the filter lists and recompile them. |

### Plugins, styles, themes

| command | arguments | flags | what it does |
|---|---|---|---|
| `plugin-list` | — | — | What plugins are loaded, what each one registered, and why the rest are not. |
| `plugin-reload` | `[name]` | — |  |
| `plugin-disable` | `<name>` | — |  |
| `greasemonkey-reload` | — | `-f/--force`, `-q/--quiet` |  |
| `styles-toggle` | — | — | Turn the per-site stylesheets in ~/.config/bru/styles/<domain>/ on or off, in the tabs that are open as well as the next one. |
| `colorscheme` | `[<name>]` | `-r/--reload` | Paint the chrome with a theme from ~/.config/bru/themes/; with no name, list them; --reload re-reads ~/.config/bru/theme.css, which is what themer writes. |

### Tools and diagnostics

| command | arguments | flags | what it does |
|---|---|---|---|
| `devtools` | `[position]` | — |  |
| `devtools-close` | — | — | Close the inspector, wherever it is. |
| `devtools-focus` | — | — | Bring the inspector forward. |
| `jseval` | `<javascript>` | `-f/--file`, `-u/--url`, `--world`, `-q/--quiet` |  |
| `messages` | `[level]` | `-f/--logfilter`, `--plain`, `-t/--tab`, `-b/--bg`, `-w/--window` | Open the page holding everything the status bar has said. |
| `message-info` | `<text>` | — | Say something in the status bar. |
| `message-warning` | `<text>` | — | Say something in the status bar, as a warning. |
| `message-error` | `<text>` | — | Say something in the status bar, as an error. |
| `clear-messages` | — | — | Take whatever the status bar is saying away now. |
| `process` | `[pid] [show\|terminate\|kill]` | — | Look at what :spawn started, or stop it. |
| `version` | — | `-p/--paste` | Open the page naming this build and the Chromium under it. |
| `help` | — | `-t/--tab` | Open this page. |
| `cookies` | `[domain]` | `-b/--bg` |  |
| `spawn` | `<command> [arguments…]` | `-u/--userscript`, `-d/--detach`, `-o/--output`, `-m/--output-messages`, `-v/--verbose` |  |


## Blocking

Brave's own ad-blocking engine, linked directly rather than through a binding — the same engine
qutebrowser reaches through `python-adblock`, with no FFI and no interpreter between a request and
the decision about it. It works in three layers.

**The network layer** decides before a request is initiated: nothing goes on the wire, no
connection, no DNS lookup, no cookie. It answers in `get_resource_request_handler` rather than in
`on_before_resource_load` so that one CEF object is allocated per *blocked* request instead of one
per request.

**The cosmetic layer** hides what the network layer cannot stop — the empty box the advert would
have filled, the reserved column that shifts the text sideways, the in-page overlay. Those are the
page's own markup, not requests, and only hiding an element removes them. The rules come from the
same lists; the browser process answers each document with a stylesheet at document-start.

**Lists are data, not configuration.** They live in `~/.local/share/bru/adblock/`, and the compiled
engine is cached beside them. **bru ships none and downloads none by itself** — `:adblock-update` is
the one thing in the whole browser that reaches the network on its own account, and it does so
because somebody typed it. The two defaults are qutebrowser's:

```
https://easylist.to/easylist/easylist.txt
https://easylist.to/easylist/easyprivacy.txt
```

Add your own and fetch them:

```
:config-list-add content.blocking.adblock.lists https://example.com/mine.txt
:adblock-update
```

| command | what it does |
|---|---|
| `:adblock-update` | fetch every configured list and recompile |
| `:adblock-toggle` | turn blocking off and on, both layers |
| `:adblock-info` | what was blocked on this page, and what the matcher costs per request |

| setting | what it governs |
|---|---|
| `content.blocking.adblock.lists` | which lists `:adblock-update` fetches |
| `content.autofill` | Chromium's own form-fill dropdowns. **Off**, because they are native windows that take the keyboard — the first `<Escape>` in a login field goes to closing one instead of leaving insert mode |

**The scriptlet layer** is the third, and it exists for the one advertisement the other two cannot
touch: the one *inside* a video. That one arrives over the same connection as the video, from the
same hosts, so there is no request to cancel; and it is not an element, so there is nothing to hide.
It is announced as a field in the player's own response, and the only way to remove it is to edit
that response before the player reads it. A `##+js(name, args…)` rule names one of the scriptlets
bru ships — they are compiled into the binary from `chrome/scriptlets/`, not loaded from disk — and
the engine hands back the code to run at document-start.

### Your own rules, and the only list bru trusts

```
~/.config/bru/filters.txt
```

uBlock Origin's syntax, read at startup, and rebuilt when it changes. **Trust comes from where the
file is, not from a setting.** It sits in the directory you hand-write, so a `trusted-` rule in it —
one that may rewrite *any* network response the page receives, not merely an advertisement — is
allowed to run. Nothing bru downloads is ever trusted, and there is deliberately no switch to make
it so, because that switch's only purpose would be to let somebody be talked into ticking it. uBlock
draws the line in the same place: "My filters" may, and no subscribed list may, including its own.

Removing YouTube's in-video advertisement takes both kinds, because the response arrives two ways:

```
! opening a video directly — the response is embedded in the page
www.youtube.com##+js(set, ytInitialPlayerResponse.playerAds, undefined)
www.youtube.com##+js(set, ytInitialPlayerResponse.adPlacements, undefined)

! clicking one from the feed — no page loads; YouTube fetches a new response
www.youtube.com##+js(trusted-replace-fetch-response, /"(adPlacements|adSlots|playerAds)"/, '"no_ads"', get_watch)
```

**What is not implemented, said plainly.** Cosmetic rules keyed by class or id (`##.ad-banner`) need
the page to report the classes it contains and ask again, which needs a `MutationObserver`; today
only the rules a list writes against the hostname, and the generic ones that are not class- or
id-keyed, are applied. Of uBlock's roughly two hundred scriptlets bru ships **two** — `set-constant`
and `trusted-replace-fetch-response` — because each one is code bru runs in every page a rule names,
so each is here only when something is actually asking for it. Only `fetch` is wrapped, not
`XMLHttpRequest`: measured 2026-08-09, YouTube uses XHR for logging alone.

## Lua

One Lua state for the process, on the CEF UI thread. It exists because a setting may hold a
function; it is not, and must never become, part of handling a keystroke.

### `~/.config/bru/config.lua`

Read once at startup if it exists. bru never writes it, and never creates the directory.

```lua
bru.set("fonts.default_family", "Roboto, sans-serif")
bru.set("content.blocking.adblock.lists", { "https://easylist.to/easylist/easylist.txt" })
bru.set("url.searchengines", { ["gh"] = "https://github.com/search?q={}" })
bru.search("aur", "https://aur.archlinux.org/packages?K={}")   -- one engine, without the table

bru.bind("normal", "<Ctrl+,>", "set")
bru.unbind("normal", "d")

-- A setting may be a function, which is the reason the config is Lua at all.
bru.set("tabs.title.format", function(tab)
  return string.format("%d: %s", tab.index, tab.title)
end)
```

| function | when | what it does |
|---|---|---|
| `bru.set(name, value)` | config only | one setting; refuses a name bru does not have |
| `bru.bind(mode, keys, command)` | config only | one binding |
| `bru.unbind(mode, keys)` | config only | remove one |
| `bru.search(name, template)` | config only | add one search engine |
| `bru.get(name)` | any time | a setting's value in force, as text |
| `bru.cmd(line)` | any time | run any command — the same vocabulary `:` takes |
| `bru.command(name, fn)` | plugins | register a new command |
| `bru.on(event, fn)` | plugins | answer an event |

The four marked *config only* raise outside `config.lua`: at runtime `:set` and `:bind` are the
commands that do it, and a plugin reaching into a `Config` nobody is holding is a bug rather than a
feature.

### Plugins

**A plugin is a directory with an `init.lua` in it.** That is the whole format.

```
~/.local/share/bru/plugins/
    hello/
        init.lua        ← run once, at startup
    readlater/
        init.lua
        store.lua       ← your own modules, however you like
```

The file runs once, registers what it wants through the `bru` table, and returns nothing. **Loading
order is by directory name**, so two plugins that both want the same command name resolve the same
way on every start rather than by whatever order the filesystem felt like.

`--plugin-dir=<path>` replaces the search directory outright, which is how an example is run without
installing it.

Note that `~/.local/share/bru/plugins` is bru's **data** directory, not `~/.config/bru`. Plugins are
code, and configer owns the config directory; bru never writes to it.

#### What a plugin may call

The same `bru` table the config uses, minus the four that only mean something while `config.lua` is
being read (`set`, `bind`, `unbind`, `search`). What is left is:

| | |
|---|---|
| `bru.command(name, fn)` | register `:name` |
| `bru.on(event, fn)` | answer an event |
| `bru.cmd(line)` | run any of the 173 commands |
| `bru.get(name)` | a setting's value in force, as text |

#### A command

The handler is given **the rest of the command line as one string** — not a table, not split on
spaces, because a command's arguments are its own business. A string it returns is shown in the
status bar; returning nothing is fine.

```lua
bru.command("hello", function(args)
  return "hello " .. (args ~= "" and args or "world")
end)
```

`:hello there` then answers `hello there`.

#### An event

Handlers are given one table. Every event carries `window`; the rest depend on the event.

| event | fields |
|---|---|
| `page-loaded` | `url` |
| `url-changed` | `url` |
| `tab-opened` | `index`, `url`, `title` |
| `tab-switched` | `index`, `url`, `title` |
| `tab-closed` | `index`, `url`, `title` |
| `mode-changed` | `from`, `to` |
| `download-finished` | `filename`, `url`, `succeeded`, `state` |
| `config-sourced` | `path`, `cleared` |

```lua
bru.on("download-finished", function(ev)
  if ev.succeeded then
    bru.cmd(":spawn -- notify-send 'downloaded' " .. ev.filename)
  end
end)
```

An unknown event name is refused **at registration**, with the list of real ones, so a typo is an
error where it is written rather than a handler that is simply never called.

#### When a plugin is wrong

This is the argument for Lua over a native plugin, and it was measured rather than assumed: the same
bug — index 99 of a three-element list — printed `attempt to index a nil value` in Lua and left the
process running, and in a `.so` it aborted the whole browser, because a panic cannot cross
`extern "C"`. So:

- **A plugin that throws while loading is named, with its file, and skipped.** The others still load.
- **A handler that throws prints once**, and the browser goes on.
- **Three throws in one session disables that plugin**, with a line saying so. `:plugin-reload` is
  how you say you have fixed it.

| | |
|---|---|
| `:plugin-list` | what is loaded, and what is off and why |
| `:plugin-reload` | re-read the directory |
| `:plugin-disable <name>` | turn one off for this session |

#### There is no sandbox, and that is deliberate

A plugin has the powers of code you put in your own data directory. `debug` is available, because a
pure-Lua module written for neovim uses `debug.getinfo` to find its own directory. `bru.cmd(":spawn
…")` starts processes. Nothing here pretends otherwise: **a half sandbox is more dangerous than
none, because it gets trusted.** A plugin that must not be trusted belongs in another process, and
`:spawn` is the door.

## Configuration

**Every default is compiled into the binary** — 68 settings and 288
bindings. A bru with no `~/.config/bru/` at all is fully configured, and bru writes nothing there.
What you keep in that directory is only what you have changed:

```
~/.config/bru/
    config.lua            your overrides
    filters.txt           your own ad-block rules — the only ones bru trusts
    theme.css             the generated theme
    styles/<host>/*.css   per-site CSS
```

Precedence is: bru's compiled-in default, then `config.lua`, then anything `:set` does at runtime.

### Seeing what is set

| | |
|---|---|
| `:set <name>?` | one setting's value |
| `bru://chrome/settings` | all of them, beside what **Chromium** is really enforcing |
| `:config-diff` → `bru://chrome/config` | only what you have changed, as the Lua that reproduces it |
| `bru://chrome/config/defaults` | everything bru ships, commented out |
| `:config-write <file>` | your changes, to a file |
| `:config-write --defaults <file>` | all the defaults, to a file, as a reference |

Flags come before the file: `:config-write --defaults ~/bru-defaults.lua`.

### Every setting bru ships

| setting | default |
|---|---|
| `statusbar.mode.style` | `"full"` |
| `statusbar.mode.labels` | `{ ["normal"] = "normal", ["insert"] = "insert", ["caret"] = "caret", ["command"] = "com…` |
| `url.searchengines` | `{ ["DEFAULT"] = "https://duckduckgo.com/?q={}", ["am"] = "https://www.amazon.com/s?k={}…` |
| `url.open_base_url` | `true` |
| `content.javascript.enabled` | `true` |
| `content.images` | `true` |
| `input.insert_mode.auto_load` | `false` |
| `input.insert_mode.auto_enter` | `true` |
| `input.insert_mode.auto_leave` | `true` |
| `input.insert_mode.leave_on_load` | `true` |
| `content.autoplay` | `true` |
| `content.mute` | `false` |
| `content.javascript.can_open_tabs_automatically` | `false` |
| `content.notifications.enabled` | `"ask"` |
| `content.geolocation` | `"ask"` |
| `content.media.audio_capture` | `"ask"` |
| `content.media.video_capture` | `"ask"` |
| `content.mouse_lock` | `"ask"` |
| `content.register_protocol_handler` | `"ask"` |
| `content.persistent_storage` | `"ask"` |
| `content.javascript.clipboard` | `"ask"` |
| `content.headers.do_not_track` | `true` |
| `content.hyperlink_auditing` | `false` |
| `content.headers.accept_language` | `"en-US,en"` |
| `content.autofill` | `false` |
| `content.blocking.adblock.lists` | `{ "https://easylist.to/easylist/easylist.txt", "https://easylist.to/easylist/easyprivac…` |
| `devtools.height` | `400` |
| `devtools.width` | `500` |
| `content.csp.bypass` | `{}` |
| `tabs.background` | `true` |
| `tabs.show` | `"always"` |
| `tabs.position` | `"top"` |
| `tabs.title.format` | `"{audio}{index}: {current_title}"` |
| `tabs.title.format_pinned` | `"{index}"` |
| `tabs.favicons.show` | `"always"` |
| `tabs.title.alignment` | `"left"` |
| `tabs.wrap` | `true` |
| `tabs.tooltips` | `true` |
| `statusbar.show` | `"always"` |
| `statusbar.position` | `"bottom"` |
| `statusbar.widgets` | `{ "keypress", "search_match", "url", "scroll", "tabs", "download" }` |
| `hints.chars` | `"asdfghjkl"` |
| `hints.min_chars` | `1` |
| `hints.auto_follow` | `"unique-match"` |
| `hints.uppercase` | `false` |
| `hints.scatter` | `true` |
| `scrollbar.width` | `12` |
| `scrollbar.style` | `true` |
| `scrollbar.page_overrides` | `true` |
| `downloads.location.prompt` | `true` |
| `downloads.remove_finished` | `-1` |
| `messages.timeout` | `3000` |
| `messages.limit` | `100` |
| `zoom.default` | `100` |
| `zoom.levels` | `{ "25%", "33%", "50%", "67%", "75%", "90%", "100%", "110%", "125%", "150%", "175%", "20…` |
| `scroll.step_px` | `120` |
| `plugins.enabled` | `true` |
| `content.user_styles` | `true` |
| `fonts.default_family` | `"monospace"` |
| `fonts.default_size` | `13` |
| `fonts.default_weight` | `"normal"` |
| `completion.height` | `300` |
| `start_page` | *(none — leaving it unset is what it means)* |
| `scrollbar.thumb` | *(none — leaving it unset is what it means)* |
| `scrollbar.track` | *(none — leaving it unset is what it means)* |
| `editor.command` | *(none — leaving it unset is what it means)* |
| `downloads.location.directory` | *(none — leaving it unset is what it means)* |
| `colors.scheme` | *(none — leaving it unset is what it means)* |

## Scripting

bru listens on a unix socket, so a running browser can be driven from a shell:

```sh
bru --remote ':open -t https://example.com'
bru --remote 'js 0 document.title'
bru --remote tabs
```

A second browser needs a socket of its own — `bru --socket=/tmp/b.sock &`, then
`bru --socket=/tmp/b.sock --remote '…'`. Without it, `--remote` reaches whichever browser bound the
default address first, which is the one you are using. The socket is in `$XDG_RUNTIME_DIR` at mode
0600: **it is not a security boundary**, and anything that can write it can drive the browser,
`:spawn` included.

## Where it stands

Around 65 000 lines across 55 modules, 173 commands, 625 tests. Sessions, cookies,
downloads, hints, caret mode, marks, macros, per-site stylesheets, userscripts, the docked
inspector and both blocking layers are implemented. The notable gaps are printing beyond Chromium's
own dialog, an inspector docked to the left or the top, and the two halves of cosmetic filtering
named above.
