// bru://chrome/cookies — the page src/cookies.rs serves a shell for.
//
// It is the first bru:// page in a *tab* that talks back. The two strips have
// done it since M4, but they are chrome and never hold anything a page put
// there; this one holds cookie names and values from the open web, so every
// string that arrives is written with textContent and never with innerHTML.
//
// Why the rows are not in the HTML: cookies arrive from an asynchronous
// visitor, and the scheme handler that produces the document runs on the IO
// thread and has to answer immediately. See the module comment in
// src/cookies.rs. So the shell asks, once, and renders what comes back.
//
// The keyboard, which is the only way this will be used:
//
//   The filter box has autofocus. bru enters insert mode on the first key that
//   arrives while an editable field has focus (src/keys.rs), so typing a domain
//   is the first thing that happens and nothing has to be pressed to start.
//
//   type        narrow by domain, in this file, with no round trip
//   Down / Up   move the picked row
//   Enter       delete the picked row
//   Tab         reach the two buttons
//   Escape      leave insert mode; then j/k scroll and f hints every ×
//
// Deleting is undoable — Rust keeps the records — and the bulk button arms
// before it fires. Neither is decoration: clearing the box and pressing the
// bulk button deletes every cookie in the browser.

(function () {
  "use strict";

  function query(request, onSuccess) {
    if (typeof window.cefQuery !== "function") {
      // Not running inside bru. Nothing else on this page works either, and
      // saying so beats a silent blank list.
      show("This page only works inside bru.");
      return;
    }
    window.cefQuery({
      request: JSON.stringify(request),
      onSuccess: function (response) {
        if (onSuccess) {
          onSuccess(JSON.parse(response));
        }
      },
      onFailure: function (code, message) {
        console.error("bru: cookies query failed (" + code + "): " + message);
        show("The cookie jar could not be read (" + code + "): " + message);
      },
    });
  }

  var filterBox = document.getElementById("filter");
  var summary = document.getElementById("summary");
  var keysLine = document.getElementById("keys");
  var rowsBox = document.getElementById("rows");
  var wipe = document.getElementById("wipe");
  var undo = document.getElementById("undo");

  // Everything Rust last handed over, and the subset the filter keeps. `shown`
  // is the array Down/Up walk and the bulk button acts on, so "delete the ones
  // shown" is exactly what is on the screen and not what happens to be in the
  // jar a moment later.
  var all = [];
  var shown = [];
  var picked = 0;
  var undoCount = 0;
  var armed = false;
  var armedTimer = null;

  function show(text) {
    summary.textContent = text;
  }

  // The filter rule, and it is deliberately the same sentence as
  // cookies::matches in Rust: a case-insensitive substring of the domain
  // exactly as Chromium spells it, leading dot and all. Domain only — the user
  // asked to search by domain, and matching names would make "session" list
  // the web. The dot stays because the page shows `.github.com` and that is
  // what gets typed; nothing is lost, since every substring of the stripped
  // form is a substring of the dotted one.
  function matches(row, filter) {
    if (!filter) {
      return true;
    }
    return row.d.toLowerCase().indexOf(filter) !== -1;
  }

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) {
      node.className = className;
    }
    if (text !== undefined) {
      // textContent, never innerHTML. A cookie's name and value are whatever a
      // server chose.
      node.textContent = text;
    }
    return node;
  }

  function expiryText(row) {
    if (!row.e) {
      return "session";
    }
    var d = new Date(row.e * 1000);
    if (isNaN(d.getTime())) {
      return "?";
    }
    // The user's own timezone, which is the only one that means anything when
    // you are looking at whether a login is about to lapse.
    return (
      d.getFullYear() +
      "-" +
      String(d.getMonth() + 1).padStart(2, "0") +
      "-" +
      String(d.getDate()).padStart(2, "0")
    );
  }

  function draw() {
    var filter = filterBox.value.trim().toLowerCase();
    shown = all.filter(function (row) {
      return matches(row, filter);
    });
    if (picked >= shown.length) {
      picked = shown.length - 1;
    }
    if (picked < 0) {
      picked = 0;
    }

    var domains = {};
    shown.forEach(function (row) {
      domains[row.d] = true;
    });
    var domainCount = Object.keys(domains).length;

    show(
      shown.length +
        " cookie" +
        (shown.length === 1 ? "" : "s") +
        " on " +
        domainCount +
        " domain" +
        (domainCount === 1 ? "" : "s") +
        (filter ? " matching " + filter : "") +
        (shown.length === all.length ? "" : " of " + all.length + " in all")
    );

    wipe.disabled = shown.length === 0;
    wipe.textContent = armed
      ? "Press again to delete " + shown.length
      : "Delete the " + shown.length + " shown";
    wipe.className = armed ? "armed" : "";

    undo.hidden = undoCount === 0;
    undo.textContent = "Undo (" + undoCount + ")";

    rowsBox.textContent = "";
    var lastDomain = null;
    var table = null;
    shown.forEach(function (row, i) {
      if (row.d !== lastDomain) {
        var count = shown.filter(function (other) {
          return other.d === row.d;
        }).length;
        var heading = el("h2", null, row.d);
        heading.appendChild(el("span", "count", String(count)));
        rowsBox.appendChild(heading);
        table = el("div", "group");
        rowsBox.appendChild(table);
        lastDomain = row.d;
      }
      var line = el("div", "row" + (i === picked ? " picked" : ""));
      var x = el("a", "x", "×");
      // A real href, because f hints reach what a browser considers clickable
      // and that is what makes this page usable without the mouse at all.
      x.href = "#";
      x.title = "delete this cookie";
      x.addEventListener("click", function (event) {
        event.preventDefault();
        remove([row.k]);
      });
      line.appendChild(x);
      line.appendChild(el("span", "name", row.n));
      line.appendChild(el("span", "value", row.v));
      line.appendChild(el("span", "path", row.p));
      line.appendChild(
        el("span", "flags", (row.s ? "Secure " : "") + (row.h ? "HttpOnly" : ""))
      );
      line.appendChild(el("span", "expiry", expiryText(row)));
      line.addEventListener("click", function () {
        picked = i;
        draw();
      });
      table.appendChild(line);
    });

    var current = rowsBox.querySelector(".picked");
    if (current && current.scrollIntoView) {
      current.scrollIntoView({ block: "nearest" });
    }
  }

  function load() {
    query({ type: "cookies", action: "list" }, function (answer) {
      all = answer.rows || [];
      undoCount = answer.undo || 0;
      if (answer.filter) {
        filterBox.value = answer.filter;
      }
      // One line the harness reads. It is how the count this file's own filter
      // arrives at is compared with the count cookies::matches arrives at over
      // the same jar — two implementations of one rule, checked against each
      // other rather than against a promise that they agree.
      draw();
      console.log(
        "bru-cookies: " +
          all.length +
          " total, " +
          shown.length +
          " shown for " +
          JSON.stringify(filterBox.value.trim().toLowerCase())
      );
    });
  }

  function remove(keys) {
    if (!keys.length) {
      return;
    }
    query({ type: "cookies", action: "delete", keys: keys }, function (answer) {
      undoCount = answer.undo || 0;
      console.log("bru-cookies: deleted " + answer.deleted);
      load();
    });
  }

  function disarm() {
    armed = false;
    if (armedTimer) {
      clearTimeout(armedTimer);
      armedTimer = null;
    }
  }

  // The bulk delete, and the reason it takes two presses. bru has no prompt
  // mode and no dialogs (DESIGN.md), so the confirmation is the button itself:
  // the first press arms it and it says what it is about to do, the second
  // does it, and it forgets after five seconds. Undo is the other half.
  wipe.addEventListener("click", function () {
    if (!shown.length) {
      return;
    }
    if (!armed) {
      armed = true;
      armedTimer = setTimeout(function () {
        disarm();
        draw();
      }, 5000);
      draw();
      return;
    }
    disarm();
    remove(
      shown.map(function (row) {
        return row.k;
      })
    );
  });

  undo.addEventListener("click", function () {
    query({ type: "cookies", action: "restore" }, function (answer) {
      undoCount = 0;
      console.log("bru-cookies: restored " + answer.restored);
      load();
    });
  });

  filterBox.addEventListener("input", function () {
    picked = 0;
    disarm();
    draw();
  });

  filterBox.addEventListener("keydown", function (event) {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      picked += event.key === "ArrowDown" ? 1 : -1;
      if (picked < 0) {
        picked = shown.length ? shown.length - 1 : 0;
      }
      if (picked >= shown.length) {
        picked = 0;
      }
      draw();
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      // One cookie, and it is the row the highlight is on. No arming: this is
      // a single cookie and Undo puts it back.
      if (shown[picked]) {
        remove([shown[picked].k]);
      }
    }
  });

  // Escape leaves insert mode in bru, and bru swallows it — the page never
  // sees it, which is correct: after Escape this is an ordinary page and j/k
  // scroll it. Nothing here binds Escape, and that is deliberate rather than
  // an omission.

  keysLine.hidden = false;
  load();
})();
