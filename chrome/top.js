// The tab strip.
//
// Two directions, and only two. Rust pushes state by calling bru.render(<json>)
// through execute_java_script; the page tells Rust it exists by sending one
// cefQuery on load. Nothing else in the chrome talks to Rust: keys never reach
// here, because a key that entered JavaScript would already have cost more than
// the scrolling this browser was built for.

(function () {
  "use strict";

  var VIEW = "top";

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

  window.bru = {
    // state = {tabs: [{title, url, active}, ...]}
    render: function (state) {
      var host = document.getElementById("tabs");
      if (!host) {
        return;
      }
      var tabs = (state && state.tabs) || [];
      host.textContent = "";
      for (var i = 0; i < tabs.length; i++) {
        var tab = tabs[i] || {};
        var el = document.createElement("span");
        el.className = tab.active ? "tab active" : "tab";
        el.title = tab.url || "";
        el.textContent = tab.title || tab.url || "";
        host.appendChild(el);
      }
    },
  };

  function ready() {
    query({ type: "ready", view: VIEW }, function (response) {
      // Rust answers the ready query with the current state, so the strip is
      // never blank between load and the first push.
      try {
        window.bru.render(JSON.parse(response));
      } catch (e) {
        console.error("bru: bad ready response: " + e);
      }
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", ready);
  } else {
    ready();
  }
})();
