// The page half of `:navigate prev` / `:navigate next` — `[[`, `]]`, `{{`, `}}`.
//
// Evaluated in the tab's main frame by src/navigate.rs, in the page's own world, because that is
// the only place the page's links exist. Never served over bru://.
//
// **This file decides nothing.** It reports every link and what a decision could be made from —
// tag, rel, class, text, resolved href — and which of them is the "next" one is answered in Rust,
// against qutebrowser's `hints.prev_regexes` / `hints.next_regexes` (configdata.yml:1766-1793) in
// `browser/navigate.py::_find_prevnext`. Putting the patterns here would hand a page the ability to
// see which of its links bru is about to follow, and would make the heuristic untestable without a
// browser.
//
// The expression evaluates to one string, because that is what a V8 eval hands back through
// `frame.v8_context()`: one record per line, five tab-separated fields, in document order.

(function () {
    "use strict";

    // `hints.selectors["links"]`, configdata.yml:1838. qutebrowser's `:navigate prev/next` asks for
    // exactly this group (`webelem.css_selector('links', baseurl)`), so an <a> with no href — a
    // named anchor — is not a candidate, and neither is a button.
    var SELECTOR = 'a[href], area[href], link[href], [role="link"][href]';

    // A ceiling on the payload. A prev/next link is near the top or the bottom of a document and
    // both ends are inside 500 links on any page worth paging through; a comment thread with
    // thousands of profile links would otherwise send a megabyte through the process message for an
    // answer that was decided in its first few records.
    var MAX_LINKS = 500;

    // Enough text for `\bprevious\b` or `»` and no more. qutebrowser matches its regexes against
    // the whole of an element's text; a card wrapped in one <a> can carry paragraphs of it, and
    // every character of that is payload. Truncating can only make bru match *less* eagerly.
    var MAX_TEXT = 100;

    function clean(text) {
        return String(text || "")
            .replace(/\s+/g, " ")
            .trim()
            .slice(0, MAX_TEXT);
    }

    // `href` is resolved here rather than in Rust: `new URL(value, base)` is the browser's own
    // resolver, and re-deriving RFC 3986 reference resolution in Rust to answer `../page2.html`
    // would be a second implementation to keep correct. Which schemes may then be followed is a
    // decision, and stays in Rust.
    function resolve(elem) {
        var raw = elem.getAttribute("href");
        if (!raw) {
            return "";
        }
        try {
            return new URL(raw, document.baseURI).href;
        } catch (e) {
            return "";
        }
    }

    var out = [];
    var elems = document.querySelectorAll(SELECTOR);
    for (var i = 0; i < elems.length && out.length < MAX_LINKS; i++) {
        var elem = elems[i];
        var href = resolve(elem);
        if (!href) {
            continue;
        }
        out.push(
            [
                elem.tagName.toLowerCase(),
                clean(elem.getAttribute("rel")),
                clean(elem.getAttribute("class")),
                // `textContent`, not `innerText`: qutebrowser's webelem.js reports the same, and
                // innerText is layout-dependent — it is empty for a link that is scrolled out of
                // view in some engines, and paging links often are.
                clean(elem.textContent),
                href,
            ].join("\t"),
        );
    }
    return out.join("\n");
})();
