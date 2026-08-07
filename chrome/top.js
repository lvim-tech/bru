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

  // Favicons are keyed by origin, the way a browser's favicon cache is keyed:
  // one site, one icon, and a second tab on the same site draws it with no
  // download. src/favicon.rs spells the key exactly as this does — it is
  // compared as a string, and a mismatch is an icon that never appears.
  function origin(url) {
    try {
      return new URL(url).origin;
    } catch (e) {
      return "";
    }
  }

  // The icons this strip has been handed, and the last state it was pushed.
  // Both are kept because the two arrive independently: an icon is a kilobyte
  // of base64 that is sent once and then never again, so it cannot live in the
  // state object that Rust pushes on every keystring and scroll change.
  var icons = {};
  var state = { tabs: [] };

  // --- tabs and statusbar -------------------------------------------------
  // src/settings.rs `tabs.favicons.show`. `pinned` is the value that pays for
  // the setting: a pinned tab shrinks to its contents, so an icon and no words
  // is what a row of them is for.
  function showFavicon(pushed, tab) {
    if (pushed.favicons === "never") {
      return false;
    }
    if (pushed.favicons === "pinned") {
      return !!tab.pinned;
    }
    return true;
  }
  // --- end tabs and statusbar -----------------------------------------------

  function draw() {
    var host = document.getElementById("tabs");
    if (!host) {
      return;
    }
    var tabs = state.tabs || [];

    // --- tabs and statusbar -------------------------------------------------
    // src/settings.rs `tabs.title.alignment`. An attribute, not a class:
    // top.js already owns className on the tab elements and data-view on the
    // body, and chrome.css holds the three rules — nothing here writes CSS.
    document.body.setAttribute("data-align", state.align || "left");
    // --- end tabs and statusbar ---------------------------------------------

    // textContent, not innerHTML: #tabs:empty is a real selector and a
    // whitespace text node would defeat it.
    host.textContent = "";

    for (var i = 0; i < tabs.length; i++) {
      var tab = tabs[i] || {};

      var el = document.createElement("div");
      // `pinned` is src/session.rs's addition; chrome.css already had colours
      // for it (--tabs-pinned-*) and drew nothing, because nothing set the
      // class.
      el.className =
        "tab " +
        (tab.active ? "active " : "") +
        (tab.pinned ? "pinned " : "") +
        loadClass(tab);
      // --- tabs and statusbar ---------------------------------------------
      // src/settings.rs `tabs.tooltips`. The attribute is the whole of what a
      // tooltip is here, so leaving it off is the whole of switching them off.
      // `!== false` rather than a truth test: a strip pushed by an older Rust
      // sends no `tooltips` key at all, and undefined must mean "as before".
      if (state.tooltips !== false) {
        el.title = tab.url || "";
      }
      // --- end tabs and statusbar -----------------------------------------
      // The one place a pointer is worth having in a keyboard-driven browser:
      // the strip is the only chrome a mouse naturally goes for. The index is
      // the strip's own order, which is what BruState calls a tab index.
      el.dataset.index = String(i);

      // --- tabs and statusbar ---------------------------------------------
      // src/settings.rs `tabs.favicons.show`: always, never, or only on
      // pinned tabs. The span is not created at all when it is not wanted —
      // it is `flex: 0 0 16px` in chrome.css, so an empty one is still 16px
      // of nothing, and `never` has to give that width back.
      if (showFavicon(state, tab)) {
        var favicon = document.createElement("span");
        favicon.className = "favicon";
        // A data: URL, because this page is served from bru:// and cannot fetch
        // an image from a site. Absent until the download finishes, which is
        // what keeps the box empty rather than broken.
        var icon = icons[origin(tab.url || "")];
        if (icon) {
          favicon.style.backgroundImage = 'url("' + icon + '")';
        }
        el.appendChild(favicon);
      }
      // --- end tabs and statusbar -----------------------------------------

      var title = document.createElement("span");
      title.className = "title";
      // --- tabs and statusbar ---------------------------------------------
      // src/settings.rs `tabs.title.format` / `tabs.title.format_pinned`. The
      // string arrives already assembled: `tabs::format_title` fills the
      // placeholders in Rust, where the index, the browser id and the tab's own
      // address all are, and this side draws what it is handed.
      //
      // The mute marker used to be written here, as `"[M] "` prepended to the
      // title — qutebrowser's `{audio}` (tabwidget.py:40) hard-coded. It is now
      // one of the placeholders the format fills, so a user who takes `{audio}`
      // out of their format gets a strip with no markers, which is what taking
      // it out means.
      //
      // The fallback is the old expression, for a strip whose Rust predates
      // `label`.
      title.textContent =
        tab.label !== undefined
          ? tab.label
          : (tab.muted ? "[M] " : "") + (tab.title || tab.url || "");
      // --- end tabs and statusbar -----------------------------------------
      el.appendChild(title);

      host.appendChild(el);
    }
  }

  window.bru = {
    // state = {tabs: [{title, url, active, pinned, muted}, ...]}
    render: function (pushed) {
      state = pushed || { tabs: [] };
      draw();
    },

    // One favicon, handed over as it arrives. Redraws, because the tab it
    // belongs to has almost certainly been drawn already — a download finishes
    // long after the tab appears.
    favicon: function (key, dataUrl) {
      if (!key || !dataUrl) {
        return;
      }
      icons[key] = dataUrl;
      draw();
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
