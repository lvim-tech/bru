// The status line, the completion table and the command line.
//
// Two directions, and only two. Rust pushes state by calling bru.render(<json>)
// through execute_java_script; the page tells Rust it exists by sending one
// cefQuery on load. Nothing else in the chrome talks to Rust: keys never reach
// here, because a key that entered JavaScript would already have cost more than
// the scrolling this browser was built for.
//
// One exception, and it is the command line's. In command mode the letters do
// reach #cmdline — CEF-NOTES.md trap 11 stays in force for everything else, and
// Rust decides key by key in cmdline::types_into_cmdline. This file reports what
// the input holds with three messages and nothing more:
//
//   {type:"text-changed", text, cursor}  every edit and every caret move
//   {type:"accept", text}                Enter, or Rust asking via bru.accept()
//   {type:"cancel"}                      Escape
//
// text-changed is a mirror Rust reads for the completion; it is one IPC hop
// behind and must never be what a command runs. That is why accept carries the
// text again: Rust asks, the DOM answers, and the last character typed cannot be
// lost to a race with the Enter that followed it.
//
// Rust's pushes carry cmdline.rev, and the input's value is only overwritten
// when the revision is new. Without that a status-bar update arriving mid-word —
// a URL change, a keystring — would rewrite what is being typed.
//
// Three things this file owns, because chrome.css depends on them:
//
//   - `document.body.className` is `mode-<mode>` and nothing else. That class is
//     what makes #cmdline visible in command mode; the view name lives in the
//     data-view attribute instead so the two never collide.
//   - #url carries one of https|http|warn|error|hover.
//   - #completion is emptied with textContent = "", never by writing back blank
//     markup. `#completion:empty { display: none }` is the only thing that
//     collapses the bar to 24px, and :empty counts whitespace text nodes.

(function () {
  "use strict";

  var VIEW = "bottom";

  function query(request, onSuccess) {
    if (typeof window.cefQuery !== "function") {
      return;
    }
    window.cefQuery({
      request: JSON.stringify(request),
      onSuccess: function (response) {
        if (onSuccess) {
          onSuccess(response);
        }
      },
      onFailure: function (code, message) {
        console.error("bru: query failed (" + code + "): " + message);
      },
    });
  }

  function put(id, text) {
    var el = document.getElementById(id);
    if (el) {
      el.textContent = text || "";
    }
  }

  // What the URL is coloured by. Rust may say so outright — a load error and a
  // link hover both come from handlers that know more than the string does — and
  // otherwise the scheme is the answer. bru:// is a SECURE scheme, so it reads
  // as https rather than as a warning.
  function urlClass(state) {
    if (state.urlstate) {
      return state.urlstate;
    }
    var url = state.url || "";
    if (!url) {
      return "";
    }
    if (url.indexOf("https://") === 0 || url.indexOf("bru://") === 0) {
      return "https";
    }
    if (url.indexOf("http://") === 0) {
      return "http";
    }
    return "warn";
  }

  // The completion table is completion.js's, not this file's. Stage 1 had a
  // renderer here that read {text, match} substrings; the contract sends
  // {cols: [...], match: [[start, len], …]}, and two renderers against two
  // shapes is one silently drawing the wrong thing. This file no longer knows
  // what a category looks like.
  //
  // Guarded because bottom.html gains its <script src="completion.js"> at merge:
  // until then the bar still renders, with no completion.
  function renderCompletion(categories) {
    if (window.bruCompletion) {
      window.bruCompletion.render(categories);
    }
  }

  // {level: "info"|"warning"|"error", text} or null. The class is what picks the
  // colours out of theme.css, and textContent is what makes `#message:empty`
  // true again — the stylesheet, not this file, decides that an empty message
  // is a hidden one.
  function renderMessage(message) {
    var el = document.getElementById("message");
    if (!el) {
      return;
    }
    if (!message || !message.text) {
      el.className = "";
      el.textContent = "";
      return;
    }
    el.className = message.level || "info";
    el.textContent = message.text;
  }

  // --- the command line ---------------------------------------------------

  var cmdline = null;
  // The last revision Rust said was its own edit, and the last thing reported
  // back. Both exist to keep the two sides from talking over each other.
  var appliedRev = -1;
  var sentText = null;
  var sentCursor = -1;

  function report() {
    if (!cmdline) {
      return;
    }
    var text = cmdline.value;
    var cursor = cmdline.selectionStart;
    // Arrow keys, Home and End move the caret without changing the text, and
    // Rust's readline commands need to know where it is. Nothing is sent when
    // neither moved: this runs on keyup, which is every keystroke.
    if (text === sentText && cursor === sentCursor) {
      return;
    }
    sentText = text;
    sentCursor = cursor;
    query({ type: "text-changed", text: text, cursor: cursor });
  }

  // Rust calls this through execute_java_script when <Return> runs
  // command-accept, and the keydown below calls it if Enter ever reaches the
  // input directly. Two callers, one message; Rust ignores an accept that
  // arrives when command mode is already over, so a double one is harmless.
  function accept() {
    if (!cmdline) {
      return;
    }
    query({ type: "accept", text: cmdline.value });
  }

  function cancel() {
    query({ type: "cancel" });
  }

  function renderCmdline(spec) {
    if (!cmdline) {
      return;
    }
    spec = spec || {};
    var text = spec.text || "";

    // Only an edit Rust made — cmd-set-text, a readline command, a history
    // recall — carries a revision this side has not applied yet.
    if (typeof spec.rev === "number" && spec.rev !== appliedRev) {
      appliedRev = spec.rev;
      if (cmdline.value !== text) {
        cmdline.value = text;
      }
      sentText = cmdline.value;
      sentCursor = -1;
    }

    if (!spec.focus) {
      if (document.activeElement === cmdline) {
        cmdline.blur();
      }
      return;
    }

    if (document.activeElement !== cmdline) {
      cmdline.focus();
    }
    var at = typeof spec.cursor === "number" ? spec.cursor : cmdline.value.length;
    at = Math.max(0, Math.min(at, cmdline.value.length));
    // Guarded: setSelectionRange on every push would drag the caret back to
    // wherever Rust last thought it was, one keystroke ago.
    if (cmdline.selectionStart !== at || cmdline.selectionEnd !== at) {
      cmdline.setSelectionRange(at, at);
    }
    sentCursor = at;
  }

  window.bru = {
    accept: accept,
    cancel: cancel,

    // state = {url, title, mode, keystring, scroll, tabindex, search, download,
    //          cmdline, completion, message}
    //
    // Every key ipc::bar_json emits is drawn by something below. `title` is the
    // one that is not a status field — the window wears it — and it goes onto
    // data-title at the end.
    render: function (state) {
      state = state || {};

      // In qutebrowser's `statusbar.widgets` order, which is the order the
      // elements sit in inside #statusline; see bottom.html.
      put("keystring", state.keystring);
      // src/find.rs: `Match [3/17]`, or empty for no search. Empty is what
      // `#statusline > span:empty { display: none }` reads, so a bar with no
      // search running is the same width it was before this field existed.
      put("search", state.search);
      put("url", state.url);
      put("scroll", state.scroll);
      put("tabindex", state.tabindex);
      // src/downloads.rs::summary: `[dl 45%]`, `[dl 3 45%]`, or empty. Same
      // rule — nothing running is an empty span, which is a hidden one.
      put("download", state.download);

      var url = document.getElementById("url");
      if (url) {
        url.className = urlClass(state);
      }

      // The stylesheet reads the mode off className and nothing else off it.
      document.body.className = "mode-" + (state.mode || "normal");

      renderMessage(state.message);
      renderCompletion(state.completion);
      renderCmdline(state.cmdline);

      // The title is not a status-line field — the window wears it — but it is
      // pushed with the rest and is what proves the display handler arrived, so
      // keep it addressable.
      document.body.setAttribute("data-title", state.title || "");
    },
  };

  function ready() {
    // An attribute, not a class: className belongs to the mode.
    document.body.setAttribute("data-view", VIEW);

    cmdline = document.getElementById("cmdline");
    if (cmdline) {
      cmdline.addEventListener("input", report);
      // Left, Right, Home and End fire no input event, and Rust's readline
      // commands are wrong by however far the caret moved without it.
      cmdline.addEventListener("keyup", report);
      cmdline.addEventListener("keydown", function (e) {
        if (e.key === "Enter") {
          e.preventDefault();
          accept();
        } else if (e.key === "Escape") {
          e.preventDefault();
          cancel();
        }
      });
    }

    query({ type: "ready", view: VIEW }, function (response) {
      // Rust answers the ready query with the current state, so the bar is
      // never blank between load and the first push.
      try {
        window.bru.render(JSON.parse(response));
      } catch (e) {
        console.error("bru: bad ready response: " + e);
      }
    });

    // Round-trip probe, for proving the router answers as well as receives.
    // Reached with ?probe=1 on the URL, which needs no input at all — this is a
    // keyboard-driven browser and there is nothing here to click.
    //
    // The answer goes to the console, beside the onFailure above, and NOT into
    // an element: it used to be written to #echo-result, which bottom.html has
    // never had, so the one thing the probe exists to show landed nowhere and
    // `?probe=1` looked exactly like a router that had not answered. There was
    // an #echo button listener beside it for the same non-existent markup.
    if (window.location.search.indexOf("probe=1") !== -1) {
      query({ type: "echo", text: "hello from " + VIEW }, function (response) {
        console.log("bru: echo answered " + JSON.stringify(response));
      });
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", ready);
  } else {
    ready();
  }
})();
