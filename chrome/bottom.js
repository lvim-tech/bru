// The status line, the completion table and the command line.
//
// Two directions, and only two. Rust pushes state by calling bru.render(<json>)
// through execute_java_script; the page tells Rust it exists by sending one
// cefQuery on load. Nothing else in the chrome talks to Rust: keys never reach
// here, because a key that entered JavaScript would already have cost more than
// the scrolling this browser was built for.
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

  // <span class="col">…</span>, with the matched substring in <span class="match">.
  // A column is either a plain string or {text, match}.
  function column(spec) {
    var el = document.createElement("span");
    el.className = "col";

    var text = typeof spec === "string" ? spec : (spec && spec.text) || "";
    var match = typeof spec === "string" ? "" : (spec && spec.match) || "";
    var at = match ? text.indexOf(match) : -1;

    if (at === -1) {
      el.textContent = text;
      return el;
    }

    el.appendChild(document.createTextNode(text.slice(0, at)));
    var hit = document.createElement("span");
    hit.className = "match";
    hit.textContent = text.slice(at, at + match.length);
    el.appendChild(hit);
    el.appendChild(document.createTextNode(text.slice(at + match.length)));
    return el;
  }

  // state.completion = [{name, items: [{cols: [...], selected}]}, ...], in
  // qutebrowser's order: Search engines, Quickmarks, Bookmarks, History.
  // Nothing pushes this yet — the completion model is a later milestone — but
  // the clear below runs on every render and is what keeps the bar at 24px.
  function renderCompletion(categories) {
    var host = document.getElementById("completion");
    if (!host) {
      return;
    }

    host.textContent = "";
    if (!categories || !categories.length) {
      return;
    }

    for (var c = 0; c < categories.length; c++) {
      var category = categories[c] || {};
      var items = category.items || [];

      var el = document.createElement("div");
      el.className = "category";

      var header = document.createElement("div");
      header.className = "cat-header";
      header.textContent = category.name || "";
      el.appendChild(header);

      for (var i = 0; i < items.length; i++) {
        var item = items[i] || {};
        var row = document.createElement("div");
        row.className = item.selected ? "item selected" : "item";

        var cols = item.cols || [];
        for (var j = 0; j < cols.length; j++) {
          row.appendChild(column(cols[j]));
        }
        el.appendChild(row);
      }

      host.appendChild(el);
    }
  }

  window.bru = {
    // state = {url, title, mode, keystring, scroll, tabindex}
    render: function (state) {
      state = state || {};

      put("keystring", state.keystring);
      put("url", state.url);
      put("scroll", state.scroll);
      put("tabindex", state.tabindex);

      var url = document.getElementById("url");
      if (url) {
        url.className = urlClass(state);
      }

      // The stylesheet reads the mode off className and nothing else off it.
      document.body.className = "mode-" + (state.mode || "normal");

      renderCompletion(state.completion);

      // The title is not a status-line field — the window wears it — but it is
      // pushed with the rest and is what proves the display handler arrived, so
      // keep it addressable.
      document.body.setAttribute("data-title", state.title || "");
    },
  };

  function ready() {
    // An attribute, not a class: className belongs to the mode.
    document.body.setAttribute("data-view", VIEW);

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
    // Reachable two ways because a keyboard-driven browser has no pointer to
    // click with while it is being built: the button when there is one, and
    // ?probe=1 on the URL, which needs no input at all.
    function echo() {
      query({ type: "echo", text: "hello from " + VIEW }, function (response) {
        put("echo-result", response);
      });
    }

    var button = document.getElementById("echo");
    if (button) {
      button.addEventListener("click", echo);
    }
    if (window.location.search.indexOf("probe=1") !== -1) {
      echo();
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", ready);
  } else {
    ready();
  }
})();
