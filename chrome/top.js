// The tab strip.
//
// Two directions, and only two. Rust pushes state by calling bru.render(<json>)
// through execute_java_script; the page tells Rust it exists by sending one
// cefQuery on load. Nothing else in the chrome talks to Rust: keys never reach
// here, because a key that entered JavaScript would already have cost more than
// the scrolling this browser was built for.
//
// The markup below is what chrome.css is written against:
//
//   <div class="tab active loaded"><span class="favicon"></span
//     ><span class="title">…</span></div>
//
// Odd/even striping is a :nth-child rule, so nothing here computes a position.

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

  // "loading" while the page is fetching, "error" if it failed, "loaded"
  // otherwise. The tab objects carry `loading`/`error` once M5 fills them in;
  // until then every tab is loaded, which is the honest default.
  function loadClass(tab) {
    if (tab.error) {
      return "error";
    }
    return tab.loading ? "loading" : "loaded";
  }

  window.bru = {
    // state = {tabs: [{title, url, active}, ...]}
    render: function (state) {
      var host = document.getElementById("tabs");
      if (!host) {
        return;
      }
      var tabs = (state && state.tabs) || [];

      // textContent, not innerHTML: #tabs:empty is a real selector and a
      // whitespace text node would defeat it.
      host.textContent = "";

      for (var i = 0; i < tabs.length; i++) {
        var tab = tabs[i] || {};

        var el = document.createElement("div");
        el.className = "tab " + (tab.active ? "active " : "") + loadClass(tab);
        el.title = tab.url || "";
        // The one place a pointer is worth having in a keyboard-driven browser:
        // the strip is the only chrome a mouse naturally goes for. The index is
        // the strip's own order, which is what BruState calls a tab index.
        el.dataset.index = String(i);

        var favicon = document.createElement("span");
        favicon.className = "favicon";
        el.appendChild(favicon);

        var title = document.createElement("span");
        title.className = "title";
        title.textContent = tab.title || tab.url || "";
        el.appendChild(title);

        host.appendChild(el);
      }
    },
  };

  function ready() {
    // An attribute, not a class: className belongs to the mode, and this
    // document must not fight the stylesheet for it.
    document.body.setAttribute("data-view", VIEW);

    // Delegated, so it survives every re-render of the strip.
    document.addEventListener("click", function (event) {
      var tab = event.target && event.target.closest && event.target.closest(".tab");
      if (!tab || !tab.dataset || tab.dataset.index === undefined) {
        return;
      }
      var index = parseInt(tab.dataset.index, 10);
      if (index >= 0) {
        query({ type: "tab-select", index: index });
      }
    });

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
