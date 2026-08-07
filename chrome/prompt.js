// The prompt block: the question bru is waiting on an answer to.
//
// Its own file, like completion.js, for the same reason: bottom.js owns the
// status line and the command line, and a third renderer inside it is a third
// thing to read before changing either. bottom.js calls exactly two functions
// here — render() on every push, and value() when Rust asks what the input
// holds.
//
// **Every string in the object handed to render() may have come from a web
// page**, and this file is the last place that matters. Three rules, all of
// them enforced here and none of them by the caller:
//
//   1. Nothing is ever assigned to innerHTML. textContent everywhere, so a
//      page's `alert("<b>bru</b> asks")` is those characters and not markup.
//   2. The title, the origin and the key hints are Rust's — src/prompt.rs
//      builds all three — and each goes in an element of its own with a class
//      of its own. A page's text can say anything it likes; what it cannot do
//      is appear where bru's own words appear.
//   3. The page's text is already one line and already capped when it arrives
//      (src/prompt.rs::one_line), and .prompt-text scrolls inside a fixed
//      height, so it cannot make the block taller than Rust told the window it
//      would be.
//
// The input is the same arrangement #cmdline has: Rust's pushes carry a
// revision, the value is only overwritten when the revision is new, and every
// edit is reported back with {type:"prompt-text"}. That report is a mirror
// Rust reads for the file listing and nothing else; what an answer is built
// from is value(), asked for at the moment <Return> is pressed.

(function () {
  "use strict";

  var input = null;
  var appliedRev = -1;
  var sentText = null;
  var sentCursor = -1;

  function query(request) {
    if (typeof window.cefQuery !== "function") {
      return;
    }
    window.cefQuery({
      request: JSON.stringify(request),
      onSuccess: function () {},
      onFailure: function (code, message) {
        console.error("bru: prompt query failed (" + code + "): " + message);
      },
    });
  }

  function report() {
    if (!input) {
      return;
    }
    var text = input.value;
    var cursor = input.selectionStart;
    if (text === sentText && cursor === sentCursor) {
      return;
    }
    sentText = text;
    sentCursor = cursor;
    query({ type: "prompt-text", text: text, cursor: cursor });
  }

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) {
      node.className = className;
    }
    if (text !== undefined && text !== null) {
      // textContent, never innerHTML — see the header. This is the single line
      // that keeps a page's own string out of the markup.
      node.textContent = text;
    }
    return node;
  }

  // spec = {kind, title, origin, text, fields, items, selected, keys, queued,
  //         rev} or null.
  function render(spec) {
    var root = document.getElementById("prompt");
    if (!root) {
      return;
    }
    if (!spec) {
      // textContent = "", never blank markup: `#prompt:empty { display: none }`
      // is the only thing that takes the block's height back, and :empty counts
      // whitespace text nodes.
      root.textContent = "";
      input = null;
      appliedRev = -1;
      sentText = null;
      sentCursor = -1;
      return;
    }

    var focused = null;
    root.textContent = "";
    root.setAttribute("data-kind", spec.kind || "");

    var head = el("div", "prompt-title", spec.title || "");
    if (spec.queued) {
      // How many more are waiting. Rust's number, so a page cannot claim to be
      // the last of a queue it is not in.
      head.appendChild(el("span", "prompt-queued", "+" + spec.queued));
    }
    root.appendChild(head);
    root.appendChild(el("div", "prompt-origin", spec.origin || ""));
    if (spec.text) {
      root.appendChild(el("div", "prompt-text", spec.text));
    }

    (spec.fields || []).forEach(function (field, i) {
      var row = el("div", "prompt-field");
      if (field.label) {
        row.appendChild(el("label", null, field.label));
      }
      var box = document.createElement("input");
      box.type = field.secret ? "password" : "text";
      box.spellcheck = false;
      box.autocomplete = "off";
      box.id = "prompt-input-" + i;
      box.value = field.value || "";
      box.addEventListener("input", report);
      // Left, Right, Home and End fire no input event, and Rust's readline
      // commands are wrong by however far the caret moved without one.
      box.addEventListener("keyup", report);
      row.appendChild(box);
      root.appendChild(row);
      if (field.focused) {
        focused = box;
      }
    });

    (spec.items || []).forEach(function (item, i) {
      var row = el("div", "prompt-item", item);
      if (i === spec.selected) {
        row.className = "prompt-item selected";
      }
      root.appendChild(row);
    });

    (spec.keys || []).forEach(function (pair) {
      var row = el("div", "prompt-key");
      row.appendChild(el("b", null, pair[0]));
      row.appendChild(el("span", null, pair[1]));
      root.appendChild(row);
    });

    input = focused;
    if (!input) {
      appliedRev = -1;
      sentText = null;
      return;
    }
    // Only an edit Rust made — a readline command, a file selected with <Tab>,
    // the picker's answer — carries a revision this side has not applied yet.
    // Without this a status-bar update arriving mid-word would rewrite what is
    // being typed, exactly as it would in #cmdline.
    if (typeof spec.rev === "number" && spec.rev !== appliedRev) {
      appliedRev = spec.rev;
      sentText = input.value;
      sentCursor = -1;
    }
    if (document.activeElement !== input) {
      input.focus();
    }
    var field = (spec.fields || []).filter(function (f) {
      return f.focused;
    })[0];
    var at = field && typeof field.cursor === "number" ? field.cursor : input.value.length;
    at = Math.max(0, Math.min(at, input.value.length));
    if (input.selectionStart !== at || input.selectionEnd !== at) {
      input.setSelectionRange(at, at);
    }
    sentCursor = at;
  }

  // Rust calls this through execute_java_script when <Return> runs
  // prompt-accept on a question with a line in it. The DOM answers, so the last
  // character typed cannot be lost to a race with the Return that followed it.
  function promptAccept() {
    query({ type: "prompt-accept", text: input ? input.value : "" });
  }

  window.bruPrompt = { render: render, promptAccept: promptAccept };
})();
