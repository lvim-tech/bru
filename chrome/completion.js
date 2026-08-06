// The completion table.
//
// One entry point, called by bottom.js on every render:
//
//   window.bruCompletion.render(state.completion)
//
// and the shape it is given is the one src/ipc.rs pushes:
//
//   {categories: [{name: "History",
//                  widths: [40, 50, 10],
//                  items: [{cols: ["url", "title", "5 days ago"],
//                           match: [[0, 2], [11, 3]]}]}],
//    selected: [cat_index, item_index]}
//
// `match` is a list of [start, length] ranges into the FIRST column only, so
// the chrome highlights what matched without re-running the filter that Rust
// already ran. Category order is Rust's — Search engines, Quickmarks,
// Bookmarks, History — and is not sorted here.
//
// Two things this file must not do, and they are the whole contract:
//
//   - It never decides what is selected. Tab/Shift-Tab/Ctrl-N/Ctrl-P are
//     command-mode bindings handled in Rust, which pushes a new `selected`.
//     This renders that index and has no opinion about it.
//   - It empties #completion with textContent = "", never with markup that
//     happens to be blank. `#completion:empty { display: none }` is the only
//     thing that collapses the bar back to 24px, and :empty counts a
//     whitespace text node as content.
//
// Text goes in through textContent throughout. A page title is attacker-
// controlled and this document has cefQuery on it.

(function () {
  "use strict";

  // <span class="col">, with each matched range wrapped in <span class="match">.
  // Ranges are sorted and clipped here rather than trusted: they are indices
  // computed on the Rust side against a string that crossed a JSON boundary,
  // and one bad pair should cost a highlight, not the row.
  function column(text, ranges) {
    var el = document.createElement("span");
    el.className = "col";
    text = text === null || text === undefined ? "" : String(text);

    if (!ranges || !ranges.length) {
      el.textContent = text;
      return el;
    }

    var sorted = ranges.slice().sort(function (a, b) {
      return (a && a[0]) - (b && b[0]);
    });

    var at = 0;
    for (var i = 0; i < sorted.length; i++) {
      var range = sorted[i] || [];
      var start = Math.floor(range[0]);
      var len = Math.floor(range[1]);
      if (!isFinite(start) || !isFinite(len) || len <= 0) {
        continue;
      }
      // Overlapping or already-consumed ranges collapse into the run before
      // them instead of reordering the text.
      if (start < at) {
        len -= at - start;
        start = at;
        if (len <= 0) {
          continue;
        }
      }
      if (start >= text.length) {
        break;
      }
      if (start > at) {
        el.appendChild(document.createTextNode(text.slice(at, start)));
      }
      var hit = document.createElement("span");
      hit.className = "match";
      hit.textContent = text.slice(start, start + len);
      el.appendChild(hit);
      at = start + len;
    }

    if (at < text.length) {
      el.appendChild(document.createTextNode(text.slice(at)));
    }
    return el;
  }

  // "40fr 50fr 10fr" from [40, 50, 10] — qutebrowser's column_widths, which are
  // per model and not per row, so every category of one model lines up. A row
  // with fewer columns than the model has leaves the last track empty rather
  // than stretching into it. Bad numbers cost the widths, not the row.
  function tracks(widths) {
    if (!widths || !widths.length) {
      return "";
    }
    var out = [];
    for (var i = 0; i < widths.length; i++) {
      var w = Math.floor(widths[i]);
      if (!isFinite(w) || w <= 0) {
        return "";
      }
      out.push(w + "fr");
    }
    return out.join(" ");
  }

  function item(spec, selected, columns) {
    var row = document.createElement("div");
    row.className = selected ? "item selected" : "item";
    if (columns) {
      row.style.gridTemplateColumns = columns;
    }

    var cols = (spec && spec.cols) || [];
    for (var i = 0; i < cols.length; i++) {
      // Only the first column carries match ranges; the rest are plain.
      row.appendChild(column(cols[i], i === 0 ? spec.match : null));
    }
    return row;
  }

  // A category with no items is not drawn at all — a lone header is a row of
  // height that says nothing, and it would push the bar taller for nothing.
  // Skipping it does not disturb `selected`, which indexes the array as sent.
  function category(spec, selectedItem) {
    var items = (spec && spec.items) || [];
    if (!items.length) {
      return null;
    }

    var el = document.createElement("div");
    el.className = "category";

    var header = document.createElement("div");
    header.className = "cat-header";
    header.textContent = (spec && spec.name) || "";
    el.appendChild(header);

    var columns = tracks(spec && spec.widths);
    for (var i = 0; i < items.length; i++) {
      el.appendChild(item(items[i] || {}, i === selectedItem, columns));
    }
    return el;
  }

  function render(completion) {
    var host = document.getElementById("completion");
    if (!host) {
      return;
    }

    // Closed is an empty #completion and nothing else — no class, no style,
    // no height written from JavaScript. Doing it first also means a malformed
    // push leaves the bar closed rather than half-drawn.
    host.textContent = "";

    // An array is accepted as well as the object, so a build of bottom.js that
    // still passes the bare category list renders instead of going blank.
    var categories = null;
    var selected = null;
    if (completion) {
      if (completion.length !== undefined) {
        categories = completion;
      } else {
        categories = completion.categories;
        selected = completion.selected;
      }
    }
    if (!categories || !categories.length) {
      return;
    }

    var selCat = selected && selected.length === 2 ? selected[0] : -1;
    var selItem = selected && selected.length === 2 ? selected[1] : -1;

    var chosen = null;
    for (var c = 0; c < categories.length; c++) {
      var el = category(categories[c], c === selCat ? selItem : -1);
      if (!el) {
        continue;
      }
      host.appendChild(el);
      if (c === selCat) {
        chosen = el.querySelector(".item.selected");
      }
    }

    // The table has its own scrolling once it is past --completion-max-h, so
    // the selected row has to be brought to it; Rust moved the index and the
    // row it named must be on screen. "nearest" is what keeps a selection that
    // is already visible from jumping the list under it.
    if (chosen && chosen.scrollIntoView) {
      chosen.scrollIntoView({ block: "nearest" });
    } else {
      host.scrollTop = 0;
    }
  }

  window.bruCompletion = { render: render };
})();
