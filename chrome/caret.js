// The page half of caret mode and of marks. Injected into a tab's main frame by src/caret.rs; never
// served over bru://, because it has to run in the page's own world to see the page's document.
//
// A port of qutebrowser 3.7.0's javascript/caret.js, cut to the part that is not Chrome's
// caret-browsing extension carrying its own Cursor/TraverseUtil tree around. The engine underneath
// all 1,446 of those lines is one DOM call — `Selection.modify(alter, direction, granularity)` —
// and Chromium 151 is exactly the engine it was written against, so that call is what bru uses.
// What is kept beyond it: `setInitialCursor` (put the caret at the first visible text when there is
// no selection), the caret element (a collapsed selection is invisible otherwise), `selectLine`'s
// re-anchoring dance, and `find_selected_focused_link` from webelem.js.
//
// **This file decides nothing.** Rust sends a list of primitive operations and the page applies
// them in order; whether a move extends a selection, how many times it repeats, and what a line
// selection has to re-anchor are all answered in src/caret.rs before the list is built. The page
// reports what happened — the selection's text, whether it is collapsed, and where the caret is —
// and that report is the only thing bru believes about the page.

"use strict";

window.__bru_caret = (function () {
    const CARET_ID = "__bru_caret_cursor";

    // qutebrowser's caret is a 1px bar with `mix-blend-mode: difference`, so it is visible on a page
    // of any colour without knowing the page's colours. `all: initial` keeps the page's own
    // stylesheet out, and `pointer-events: none` keeps it out of the way of a click.
    const CARET_CSS =
        "all: initial;" +
        "position: fixed;" +
        "z-index: 2147483647;" +
        "pointer-events: none;" +
        "width: 2px;" +
        "background-color: #f0f0f0;" +
        "mix-blend-mode: difference;";

    const state = {
        token: null,
        // The element under the caret at the last report, kept only so `follow` has something to
        // look at without walking the document again.
        node: null,
    };

    function report(token, kind, data, text) {
        if (typeof window.cefQuery !== "function") {
            return;
        }
        window.cefQuery({
            "request": JSON.stringify({
                "type": "caret",
                "token": token,
                "kind": kind,
                // Percent-encoded, like chrome/hints.js's payload and for the same reason: neither
                // value can then carry a quote or a backslash, so the Rust side reads them out with
                // two string searches instead of a JSON parser it would have to audit. `text` is a
                // field of its own because a selection may contain any character at all, including
                // whatever separator `data` uses.
                "data": encodeURIComponent(data),
                "text": encodeURIComponent(text || ""),
            }),
            "onSuccess": function () {},
            "onFailure": function () {},
        });
    }

    // ---------------------------------------------------------------------------------------------
    // The caret element
    // ---------------------------------------------------------------------------------------------

    // Where the caret is: a zero-width rect at the selection's *focus*, which is the end that moves.
    // `getBoundingClientRect` on a collapsed range is empty in some positions, so fall back to the
    // focus node's own box.
    function caretRect() {
        const sel = window.getSelection();
        if (!sel || sel.rangeCount === 0 || !sel.focusNode) {
            return null;
        }
        let rect = null;
        try {
            const range = document.createRange();
            range.setStart(sel.focusNode, sel.focusOffset);
            range.collapse(true);
            rect = range.getBoundingClientRect();
        } catch (exc) {
            rect = null;
        }
        if (!rect || (rect.height === 0 && rect.width === 0 && rect.top === 0)) {
            const node = sel.focusNode.nodeType === Node.TEXT_NODE
                ? sel.focusNode.parentElement
                : sel.focusNode;
            rect = node && node.getBoundingClientRect ? node.getBoundingClientRect() : null;
        }
        if (!rect || rect.height === 0) {
            return null;
        }
        return rect;
    }

    function drawCaret() {
        const rect = caretRect();
        let node = document.getElementById(CARET_ID);
        if (!rect) {
            if (node && node.parentNode) {
                node.parentNode.removeChild(node);
            }
            return null;
        }
        if (!node) {
            node = document.createElement("div");
            node.id = CARET_ID;
            document.documentElement.appendChild(node);
        }
        node.style.cssText = CARET_CSS +
            `left: ${Math.round(rect.left)}px;` +
            `top: ${Math.round(rect.top)}px;` +
            `height: ${Math.round(rect.height)}px;`;
        return rect;
    }

    function removeCaret() {
        const node = document.getElementById(CARET_ID);
        if (node && node.parentNode) {
            node.parentNode.removeChild(node);
        }
    }

    // ---------------------------------------------------------------------------------------------
    // setInitialCursor — javascript/caret.js:857, without position_caret.js's Cursor walk
    // ---------------------------------------------------------------------------------------------

    // The first text node with a non-empty box at or below the top of the viewport. qutebrowser
    // reaches the same place through TraverseUtil; a TreeWalker filtered on visible boxes is the
    // same answer in twenty lines, and it is the one Chromium can compute directly.
    function firstVisibleTextNode() {
        const walker = document.createTreeWalker(
            document.body || document.documentElement,
            NodeFilter.SHOW_TEXT,
            {
                "acceptNode": function (node) {
                    if (!node.nodeValue || !node.nodeValue.trim()) {
                        return NodeFilter.FILTER_REJECT;
                    }
                    const parent = node.parentElement;
                    if (!parent) {
                        return NodeFilter.FILTER_REJECT;
                    }
                    const style = window.getComputedStyle(parent);
                    if (style.visibility !== "visible" || style.display === "none") {
                        return NodeFilter.FILTER_REJECT;
                    }
                    const range = document.createRange();
                    range.selectNodeContents(node);
                    const rect = range.getBoundingClientRect();
                    if (rect.height <= 0 || rect.width <= 0) {
                        return NodeFilter.FILTER_REJECT;
                    }
                    // At or below the top of the viewport: entering caret mode should start where
                    // the page is being read, not where it was scrolled away from.
                    if (rect.bottom < 0 || rect.top > window.innerHeight) {
                        return NodeFilter.FILTER_REJECT;
                    }
                    return NodeFilter.FILTER_ACCEPT;
                },
            }
        );
        return walker.nextNode();
    }

    function setInitialCursor() {
        const sel = window.getSelection();
        if (sel && sel.toString().length > 0) {
            // There was already a selection — a search result, a drag. qutebrowser keeps it and
            // enters with selectionState NORMAL; the Rust side reads that off `collapsed` below.
            return;
        }
        const node = firstVisibleTextNode();
        if (!node) {
            return;
        }
        // Skip the leading whitespace of the text node, so the caret lands on the first character
        // rather than in the indentation of the source.
        const text = node.nodeValue || "";
        let offset = 0;
        while (offset < text.length && /\s/u.test(text[offset])) {
            offset += 1;
        }
        try {
            sel.setBaseAndExtent(node, offset, node, offset);
        } catch (exc) {
            // A node that has gone away between the walk and here.
        }
    }

    // ---------------------------------------------------------------------------------------------
    // The primitives Rust composes
    // ---------------------------------------------------------------------------------------------

    function apply(op) {
        const sel = window.getSelection();
        if (!sel) {
            return;
        }
        switch (op[0]) {
        case "modify":
            // [ "modify", alter, direction, granularity ]
            sel.modify(op[1], op[2], op[3]);
            break;
        case "reverse":
            // javascript/caret.js:1128. Swap anchor and focus so the *other* end of the selection
            // becomes the one that moves. `extentNode`/`baseNode` there are the deprecated aliases
            // of focus/anchor.
            if (sel.rangeCount > 0 && sel.anchorNode && sel.focusNode) {
                sel.setBaseAndExtent(
                    sel.focusNode, sel.focusOffset, sel.anchorNode, sel.anchorOffset
                );
            }
            break;
        case "drop":
            // qutebrowser calls removeAllRanges here (javascript/caret.js:1421). bru collapses to
            // the caret instead: with no range at all there is nothing left for `modify` to act on,
            // so `<Ctrl-Space>` would strand caret mode with dead movement keys until it was left
            // and re-entered. Collapsing drops the selection — which is what `selection-drop`
            // means — and leaves the caret where it was.
            if (sel.rangeCount > 0) {
                sel.collapseToFocus();
            }
            break;
        default:
            break;
        }
    }

    // Run the operations Rust built, then say what the page looks like now.
    function run(token, ops) {
        if (token !== state.token) {
            return;
        }
        for (const op of ops) {
            try {
                apply(op);
            } catch (exc) {
                // A `modify` that cannot go any further throws on some pages; the rest of the list
                // is still worth running, and the report below tells Rust where it ended up.
            }
        }
        publish(token);
    }

    // The one report. Everything Rust knows about the page's caret comes through here.
    function publish(token) {
        const rect = drawCaret();
        const sel = window.getSelection();
        const text = sel ? sel.toString() : "";
        const collapsed = !sel || sel.isCollapsed ? 1 : 0;
        state.node = sel && sel.focusNode
            ? (sel.focusNode.nodeType === Node.TEXT_NODE ? sel.focusNode.parentElement : sel.focusNode)
            : null;
        const box = rect
            ? `${Math.round(rect.left)},${Math.round(rect.top)},` +
              `${Math.round(rect.width)},${Math.round(rect.height)}`
            : "";
        report(
            token,
            "state",
            `${box}|${collapsed}|${window.innerWidth},${window.innerHeight}`,
            text
        );
    }

    // ---------------------------------------------------------------------------------------------
    // Entry, exit, marks and selection-follow
    // ---------------------------------------------------------------------------------------------

    function enter(token) {
        state.token = token;
        setInitialCursor();
        publish(token);
    }

    function leave() {
        state.token = null;
        state.node = null;
        removeCaret();
        const sel = window.getSelection();
        if (sel) {
            // `_on_mode_left` drops the selection when caret mode ends (webenginetab.py:328).
            sel.removeAllRanges();
        }
    }

    // Where the page is scrolled to, for `` ` `` and `'`. The same expression src/scroll.rs's probe
    // uses, so a mark and the status bar's percentage can never disagree about which element scrolls.
    function mark(token, what, key) {
        const elem = document.scrollingElement || document.documentElement;
        const x = elem ? Math.round(elem.scrollLeft) : 0;
        const y = elem ? Math.round(elem.scrollTop) : 0;
        const max_y = elem ? Math.max(0, elem.scrollHeight - elem.clientHeight) : 0;
        report(token, "mark", `${what}|${key}|${x},${y},${max_y}`, "");
    }

    // webelem.js `find_selected_focused_link`, and webenginetab.py's `_follow_selected_cb` around
    // it: the anchor inside the selection, or — with nothing selected — the focused element. The
    // page reports where it would be clicked and what it points at; Rust decides which of the two
    // to use, and does the clicking itself on Chromium's real input path.
    function follow(token, tab) {
        const sel = window.getSelection();
        let elem = null;
        if (sel && sel.rangeCount > 0 && sel.focusNode) {
            const start = sel.focusNode.nodeType === Node.TEXT_NODE
                ? sel.focusNode.parentElement
                : sel.focusNode;
            if (start && start.closest) {
                elem = start.closest("a[href], area[href]");
            }
            if (!elem && sel.anchorNode) {
                const other = sel.anchorNode.nodeType === Node.TEXT_NODE
                    ? sel.anchorNode.parentElement
                    : sel.anchorNode;
                if (other && other.closest) {
                    elem = other.closest("a[href], area[href]");
                }
            }
        }
        if (!elem && document.activeElement && document.activeElement.closest) {
            elem = document.activeElement.closest("a[href], area[href]");
        }

        let url = "";
        let point = "";
        if (elem) {
            if (typeof elem.href === "string") {
                url = elem.href;
            }
            // The same first-usable-client-rect rule as chrome/hints.js: an <a> containing a
            // display:block child has a bounding box that spans the line, and its centre misses.
            const rects = elem.getClientRects();
            for (let i = 0; i < rects.length; ++i) {
                const rect = rects[i];
                if (rect.width > 1 && rect.height > 1) {
                    const side = Math.min(rect.width, rect.height);
                    const x = Math.round(rect.left + (side / 2));
                    const y = Math.round(rect.top + (side / 2));
                    if (x >= 0 && y >= 0) {
                        point = `${x},${y}`;
                    }
                    break;
                }
            }
        }
        report(token, "follow", `${tab}|${point}`, url);
    }

    return {
        "enter": enter,
        "leave": leave,
        "run": run,
        "mark": mark,
        "follow": follow,
    };
})();
