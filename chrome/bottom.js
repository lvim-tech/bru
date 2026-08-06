// The status line, the completion table and the command line.
//
// Two directions, and only two. Rust pushes state by calling bru.render(<json>)
// through execute_java_script; the page tells Rust it exists by sending one
// cefQuery on load. Nothing else in the chrome talks to Rust: keys never reach
// here, because a key that entered JavaScript would already have cost more than
// the scrolling this browser was built for.

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

  window.bru = {
    // state = {url, title, mode, keystring, scroll, tabindex}
    render: function (state) {
      state = state || {};
      put("url", state.url);
      put("keystring", state.keystring);
      put("scroll", state.scroll);
      put("tabindex", state.tabindex);
      document.body.setAttribute("data-mode", state.mode || "normal");
      // The title is not a status-line field of its own — the window wears it —
      // but it is pushed with the rest and is what proves the display handler
      // arrived, so keep it addressable.
      document.body.setAttribute("data-title", state.title || "");
    },
  };

  function ready() {
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
