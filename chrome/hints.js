// The page half of hint mode. Injected into a tab's main frame by src/hints.rs; never served over
// bru://, because it has to run in the page's own world to see the page's elements.
//
// A port of qutebrowser 3.7.0's javascript/webelem.js — `is_hidden_css`, `is_visible`,
// `iframe_same_domain`, `find_shadow_roots`, `get_frame_offset`/`add_offset_rect` — plus
// `webengineelem.rect_on_view` and `webelem._mouse_pos`. Those four tests are years of accumulated
// edge cases (ACE editors that set opacity 0, bootstrap's custom-control-input, <a> elements whose
// bounding box spans a block child, cross-origin frames that throw on `.document`), and they are
// exactly what is worth taking. The Qt plumbing around them is not.
//
// **This file decides nothing.** It collects elements, reports their click points, and draws the
// labels Rust hands it. Which label a keystroke matches is answered in Rust, in src/hints.rs — a
// keypress must never cross into the page to be matched.

"use strict";

window.__bru_hints = (function () {
    // hints.selectors, configdata.yml:1803 — every group, in qutebrowser's order and spelling.
    // These lists are the accumulated answer to "what is clickable", and the groups are the whole
    // point of `;i`, `;t` and `;o`: `links` is not "the `all` list filtered afterwards", it is a
    // different query. Rust names the group; this file never chooses one.
    const SELECTORS = {
        "all": [
            "a",
            "area",
            "textarea",
            "select",
            'input:not([type="hidden"])',
            "button",
            "frame",
            "iframe",
            "img",
            "link",
            "summary",
            '[contenteditable]:not([contenteditable="false"])',
            "[onclick]",
            "[onmousedown]",
            '[role="link"]',
            '[role="option"]',
            '[role="button"]',
            '[role="tab"]',
            '[role="checkbox"]',
            '[role="switch"]',
            '[role="menuitem"]',
            '[role="menuitemcheckbox"]',
            '[role="menuitemradio"]',
            '[role="treeitem"]',
            "[aria-haspopup]",
            "[ng-click]",
            "[ngClick]",
            "[data-ng-click]",
            "[x-ng-click]",
            '[tabindex]:not([tabindex="-1"])',
        ],
        "links": [
            "a[href]",
            "area[href]",
            "link[href]",
            '[role="link"][href]',
        ],
        "images": [
            "img",
        ],
        "media": [
            "audio",
            "img",
            "video",
        ],
        "url": [
            "[src]",
            "[href]",
        ],
        "inputs": [
            'input[type="text"]',
            'input[type="date"]',
            'input[type="datetime-local"]',
            'input[type="email"]',
            'input[type="month"]',
            'input[type="number"]',
            'input[type="password"]',
            'input[type="search"]',
            'input[type="tel"]',
            'input[type="time"]',
            'input[type="url"]',
            'input[type="week"]',
            "input:not([type])",
            '[contenteditable]:not([contenteditable="false"])',
            "textarea",
        ],
    };

    // A ceiling on how many hints one collection reports. The message router puts a request over
    // 16 KB through shared memory, where it arrives as binary and bru's string handler never sees
    // it; at four numbers per element the payload reaches that at roughly 1,600 elements. Nobody
    // types a four-character hint, so the cap costs nothing real and turns a silent failure into a
    // number printed on stderr.
    const MAX_HINTS = 1000;

    const ROOT_ID = "__bru_hints_root";

    // The style is qutebrowser's default hint appearance: colors.hints.fg = black,
    // colors.hints.bg = the yellow gradient, hints.border = 1px solid #E3BE23, hints.radius = 3,
    // hints.padding = 0/3/0/3, colors.hints.match.fg = green. `all: initial` first, so a page's own
    // stylesheet cannot reach in; `pointer-events: none` so a label never eats the synthetic click
    // that Rust aims at the element underneath it.
    const LABEL_CSS =
        "all: initial;" +
        "position: fixed;" +
        "z-index: 2147483647;" +
        "pointer-events: none;" +
        "font: bold 11px/1.2 monospace;" +
        "color: #000;" +
        "background: linear-gradient(rgba(255,247,133,0.9), rgba(255,197,66,0.9));" +
        "border: 1px solid #E3BE23;" +
        "border-radius: 3px;" +
        "padding: 0 3px;" +
        "white-space: nowrap;" +
        "text-transform: uppercase;";

    const state = {
        token: null,
        elems: [],
        rects: [],
        labels: [],
        nodes: [],
        root: null,
    };

    function get_frame_offset(frame) {
        if (frame === null) {
            return {"top": 0, "right": 0, "bottom": 0, "left": 0, "height": 0, "width": 0};
        }
        return frame.frameElement.getBoundingClientRect();
    }

    function add_offset_rect(base, offset) {
        return {
            "top": base.top + offset.top,
            "left": base.left + offset.left,
            "bottom": base.bottom + offset.top,
            "right": base.right + offset.left,
            "height": base.height,
            "width": base.width,
        };
    }

    function is_hidden_css(elem) {
        const win = elem.ownerDocument.defaultView;
        const style = win.getComputedStyle(elem, null);

        const invisible = style.getPropertyValue("visibility") !== "visible";
        const none_display = style.getPropertyValue("display") === "none";
        const zero_opacity = style.getPropertyValue("opacity") === "0";

        const is_framework = (
            // ACE editor
            elem.classList.contains("ace_text-input") ||
            // bootstrap CSS
            elem.classList.contains("custom-control-input")
        );

        return (invisible || none_display || (zero_opacity && !is_framework));
    }

    function is_visible(elem, frame) {
        if (is_hidden_css(elem)) {
            return false;
        }

        const offset_rect = get_frame_offset(frame);
        let rect = add_offset_rect(elem.getBoundingClientRect(), offset_rect);

        if (!rect ||
                rect.top > window.innerHeight ||
                rect.bottom < 0 ||
                rect.left > window.innerWidth ||
                rect.right < 0) {
            return false;
        }

        rect = elem.getClientRects()[0];
        return Boolean(rect);
    }

    function iframe_same_domain(frame) {
        try {
            frame.document; // eslint-disable-line no-unused-expressions
            return true;
        } catch (exc) {
            if (exc instanceof DOMException && exc.name === "SecurityError") {
                return false;
            }
            throw exc;
        }
    }

    function find_shadow_roots(container) {
        const roots = [];
        for (const elem of container.querySelectorAll("*")) {
            if (elem.shadowRoot) {
                roots.push(elem.shadowRoot, ...find_shadow_roots(elem.shadowRoot));
            }
        }
        return roots;
    }

    // webengineelem.rect_on_view: the first client rect wider and taller than one pixel. An <a>
    // containing a display:block child has a bounding box that spans the whole line, and clicking
    // its centre misses the link — qutebrowser issue 1298.
    function rect_on_view(elem, frame) {
        const offset = get_frame_offset(frame);
        const rects = elem.getClientRects();
        for (let i = 0; i < rects.length; ++i) {
            const rect = add_offset_rect(rects[i], offset);
            if (rect.width > 1 && rect.height > 1) {
                return rect;
            }
        }
        return null;
    }

    // webelem._mouse_pos: the centre of the largest square fitting into the rectangle's top/left
    // corner, so that a link partly covered by something else is still hit — qutebrowser issue 1005.
    function mouse_pos(rect) {
        const side = Math.min(rect.width, rect.height);
        const x = Math.round(rect.left + (side / 2));
        const y = Math.round(rect.top + (side / 2));
        if (x < 0 || y < 0) {
            return null;
        }
        return [x, y];
    }

    function clear() {
        if (state.root && state.root.parentNode) {
            state.root.parentNode.removeChild(state.root);
        }
        const stale = document.getElementById(ROOT_ID);
        if (stale && stale.parentNode) {
            stale.parentNode.removeChild(stale);
        }
        state.root = null;
        state.nodes = [];
        state.elems = [];
        state.rects = [];
        state.labels = [];
        state.token = null;
    }

    function report(token, kind, data) {
        if (typeof window.cefQuery !== "function") {
            return;
        }
        window.cefQuery({
            "request": JSON.stringify({
                "type": "hints",
                "token": token,
                "kind": kind,
                // Percent-encoded so the payload can never carry a quote or a backslash, which is
                // what lets the Rust side read it out with two string searches instead of a JSON
                // parser it would then have to audit.
                "data": encodeURIComponent(data),
            }),
            "onSuccess": function () {},
            "onFailure": function () {},
        });
    }

    // Find every hintable element in `group` and tell Rust where each one would be clicked.
    //
    // An unknown group is refused rather than silently answered with `all`: hinting every button on
    // the page when `;i` asked for images looks exactly like a bug in the visibility test, and
    // costs an afternoon to find.
    function collect(token, group) {
        clear();
        state.token = token;

        const selector = (SELECTORS[group] || []).join(", ");
        if (!selector) {
            report(token, "elems", "");
            return;
        }

        // Everywhere an element can hide: the document, same-domain iframes, open shadow roots.
        const containers = [[document, null]];
        for (const frame of Array.from(window.frames)) {
            try {
                if (iframe_same_domain(frame)) {
                    containers.push([frame.document, frame]);
                }
            } catch (exc) {
                // A frame that has gone away between window.frames and the check.
            }
        }
        for (const root of find_shadow_roots(document)) {
            containers.push([root, null]);
        }

        const found = [];
        for (const [container, frame] of containers) {
            let matched = [];
            try {
                matched = container.querySelectorAll(selector);
            } catch (exc) {
                continue;
            }
            for (const elem of matched) {
                if (!is_visible(elem, frame)) {
                    continue;
                }
                const rect = rect_on_view(elem, frame);
                if (!rect) {
                    continue;
                }
                const pos = mouse_pos(rect);
                if (!pos) {
                    continue;
                }
                found.push([elem, rect, pos]);
                if (found.length >= MAX_HINTS) {
                    break;
                }
            }
            if (found.length >= MAX_HINTS) {
                break;
            }
        }

        const parts = [];
        for (const [elem, rect, pos] of found) {
            state.elems.push(elem);
            state.rects.push(rect);
            parts.push(`${pos[0]},${pos[1]},${Math.round(Math.max(rect.left, 0))},` +
                       `${Math.round(Math.max(rect.top, 0))}`);
        }

        report(token, "elems", parts.join("|"));
    }

    // Draw the labels Rust generated. One per collected element, in the same order.
    function show(labels) {
        state.labels = labels;
        state.nodes = [];

        const root = document.createElement("div");
        root.id = ROOT_ID;
        root.style.cssText = "all: initial; position: fixed; top: 0; left: 0; width: 0; " +
            "height: 0; z-index: 2147483647; pointer-events: none;";

        for (let i = 0; i < labels.length && i < state.rects.length; ++i) {
            const rect = state.rects[i];
            const node = document.createElement("span");
            node.style.cssText = LABEL_CSS +
                `left: ${Math.round(Math.max(rect.left, 0))}px;` +
                `top: ${Math.round(Math.max(rect.top, 0))}px;`;
            node.appendChild(document.createElement("span"));  // matched
            node.appendChild(document.createElement("span"));  // rest
            node.childNodes[0].style.cssText = "all: initial; color: #0f0; font: inherit;";
            node.childNodes[1].style.cssText = "all: initial; color: inherit; font: inherit;";
            node.childNodes[1].textContent = labels[i];
            root.appendChild(node);
            state.nodes.push(node);
        }

        state.root = root;
        document.documentElement.appendChild(root);
    }

    // Rust has matched `matched_len` characters and says these element indices are still in play.
    // No comparison happens here: the list is the answer, not the question.
    function filter(visible, matched_len) {
        const keep = new Set(visible);
        for (let i = 0; i < state.nodes.length; ++i) {
            const node = state.nodes[i];
            if (!keep.has(i)) {
                node.style.setProperty("display", "none", "important");
                continue;
            }
            node.style.setProperty("display", "inline", "important");
            const label = state.labels[i] || "";
            node.childNodes[0].textContent = label.slice(0, matched_len);
            node.childNodes[1].textContent = label.slice(matched_len);
        }
    }

    // webelem.resolve_url: the `href` attribute, or failing that `src`, resolved against the
    // document's base URL. `src` is what makes `;I` (hint images tab) open the image rather than
    // nothing, and it is why this is not simply `elem.href`.
    function resolve_url(elem) {
        for (const attr of ["href", "src"]) {
            const text = (elem.getAttribute && elem.getAttribute(attr) || "").trim();
            if (!text) {
                continue;
            }
            try {
                return new URL(text, elem.ownerDocument.baseURI).href;
            } catch (exc) {
                return "";
            }
        }
        // Not qutebrowser's, and needed because bru hints the element the selector matched: an
        // `[onclick]` <span> or a <button> sitting inside an <a> has no URL of its own, and the
        // link it lives in is the only sensible answer.
        const anchor = elem.closest && elem.closest("a[href], area[href], link[href]");
        if (anchor) {
            return resolve_url(anchor);
        }
        return "";
    }

    // The URL behind one element, for every target that acts on a URL rather than on a click.
    // Asked for only when a hint has been followed, so this is one element and not a second copy of
    // the whole page.
    function href(token, index) {
        const elem = state.elems[index];
        report(token, "href", elem ? resolve_url(elem) : "");
    }

    return {
        "collect": collect,
        "show": show,
        "filter": filter,
        "clear": clear,
        "href": href,
    };
})();
