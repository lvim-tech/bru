// The keeper for a per-site stylesheet. src/userstyles.rs
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
//   3. THE PAGE MAY OUTRANK IT. A stylesheet the page adds later wins on document
//      order at equal specificity, so this keeps itself the LAST child of the
//      root — which is what `watch_root` below is for.
//
// One object per document, hung off `window.bruStyle`, so a second call replaces
// the CSS rather than adding a second element.
(function () {
    "use strict";

    if (window.bruStyle) {
        return;
    }

    var XHTML = "http://www.w3.org/1999/xhtml";
    var SVG = "http://www.w3.org/2000/svg";

    var rootElem = null;
    var styleElem = null;
    var css = "";
    var observer = null;

    // Keep the element last under the current root, and follow the root if it is
    // swapped for another. Observing `document` catches the root being replaced;
    // observing the root catches its children changing under it.
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
        if (styleElem && styleElem.parentNode !== rootElem) {
            rootElem.appendChild(styleElem);
        } else if (styleElem && styleElem !== rootElem.lastChild) {
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
        styleElem.id = "bru-userstyle";
        styleElem.textContent = css;
        observer = new MutationObserver(watchRoot);
        watchRoot();
    }

    window.bruStyle = {
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
})();
