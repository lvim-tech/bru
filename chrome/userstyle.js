// The keeper for a stylesheet bru puts into a page. src/userstyles.rs, src/scrollbar.rs
//
// Not "insert a <style>" — a thing that OWNS one and keeps it where it belongs.
// Modelled on qutebrowser's javascript/stylesheet.js, which was read before this
// was written because three separate failures here had the same shape and it had
// already solved all three:
//
//   1. THE DOCUMENT MAY NOT EXIST YET. This runs at context creation, which is
//      before the parser has produced a root element. `document.documentElement`
//      is null, and appending to it throws — measured 2026-08-07 with a userscript
//      that did nothing else.
//   2. THE DOCUMENT MAY BE REPLACED. A single-page application re-renders and the
//      root's children go with it; a style put in at load start was gone by the
//      time DuckDuckGo had drawn its results. The observer puts it back.
//   3. DOCUMENT ORDER DECIDES IT. At equal specificity the later stylesheet wins,
//      so a keeper holds its element at one END of the root's children and puts it
//      back when the page moves it — which is what `watchRoot` below is for. Which
//      end is the caller's to say.
//
// **A factory, not one object**, because two things need this and they need it at
// opposite ends of the cascade. `bruKeep("bru-scrollbar", "first")` is bru's own
// scrollbar, which goes in FIRST so a page's own scrollbar rules win;
// `bruKeep("bru-userstyle", "last")` is the user's CSS for the site, which goes
// LAST because there the user is meant to win.
//
// Written as one thing after the scrollbar was moved into the renderer and lost
// its element to the same document rewrite this already survived — two copies of
// an observer would have been two places to fix it the next time.
(function () {
    "use strict";

    if (window.bruKeep) {
        return;
    }

    var XHTML = "http://www.w3.org/1999/xhtml";
    var SVG = "http://www.w3.org/2000/svg";

    function make(id, where) {
        var rootElem = null;
        var styleElem = null;
        var css = "";
        var observer = null;

        // Keep the element at its end of the current root, and follow the root if
        // it is swapped for another. Observing `document` catches the root being
        // replaced; observing the root catches its children changing under it.
        function watchRoot() {
            if (!document.documentElement) {
                observer.observe(document, { childList: true });
                return;
            }
            if (rootElem !== document.documentElement) {
                rootElem = document.documentElement;
                observer.disconnect();
                observer.observe(document, { childList: true });
                observer.observe(rootElem, { childList: true });
            }
            if (!styleElem) {
                return;
            }
            if (where === "first") {
                if (styleElem !== rootElem.firstChild) {
                    rootElem.insertBefore(styleElem, rootElem.firstChild);
                }
            } else if (styleElem !== rootElem.lastChild) {
                rootElem.appendChild(styleElem);
            }
        }

        function create() {
            // The namespace matters: a `<style>` created in the XHTML namespace inside
            // an SVG document is an element the renderer ignores.
            var ns = XHTML;
            if (document.documentElement &&
                document.documentElement.namespaceURI === SVG) {
                ns = SVG;
            }
            styleElem = document.createElementNS(ns, "style");
            styleElem.id = id;
            styleElem.textContent = css;
            observer = new MutationObserver(watchRoot);
            watchRoot();
        }

        return {
            set: function (text) {
                css = text || "";
                if (!styleElem) {
                    create();
                } else {
                    styleElem.textContent = css;
                    watchRoot();
                }
            },
            // The toggle's off switch: the element goes and the observer with it, so a
            // page with styles turned off costs nothing at all.
            off: function () {
                if (observer) {
                    observer.disconnect();
                    observer = null;
                }
                if (styleElem && styleElem.parentNode) {
                    styleElem.parentNode.removeChild(styleElem);
                }
                styleElem = null;
                rootElem = null;
            },
        };
    }

    var kept = {};
    window.bruKeep = function (id, where) {
        if (!kept[id]) {
            kept[id] = make(id, where);
        }
        return kept[id];
    };
})();
