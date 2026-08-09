// bru's inspector divider — the strip that sets the docked inspector's height.
// src/devtools.rs.
//
// Two messages, and nothing in between:
//
//   divider -> Rust   {type:"divider", phase:"start"}
//                     -> {"top":<line, in this page's pixels>, "min":…, "max":…}
//                     {type:"divider", phase:"end", y:<line the drag ended on>}
//
// **The view this page is in grows to cover the page and the inspector for as
// long as the button is down, and that is the whole design.** Everything else
// was tried first and each failed the same way, measured 2026-08-09 and written
// up as CEF-NOTES trap 26: a page is told where the pointer is only while the
// pointer is over it, and a pointer that leaves a small strip *upward* is not
// reported at all. One drag, 410 events reaching this page:
//
//     movementY > 0 (downward)   271
//     movementY = 0              139
//     movementY < 0 (upward)       0
//
// `screenY`, `clientY` and summed `movementY` each produced a drag that could
// shrink the panel and never grow it, because a six-pixel strip can only chase a
// pointer it is told about, and it is told only when the pointer is below it.
// Pointer lock would have reported movement rather than position and been immune
// to all of it; this CEF has no implementation of it at all — `requestPointerLock`
// rejects with Chromium's own `UnknownError` catch-all, from the default
// `WebContentsDelegate` nobody overrode.
//
// So the strip stops chasing and surrounds instead. While it is expanded the
// pointer has nowhere to be that is not this document, both directions report at
// full rate, and the drag needs no query per frame either: this page draws the
// split itself and Rust hears one number, on release.
(function () {
    "use strict";

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
                console.error("bru: divider query failed (" + code + "): " + message);
            },
        });
    }

    // Where the line is while the drag runs, and what it may not pass. Null when
    // no drag is in progress, which is what makes a stray pointermove — one with
    // no button held — cost nothing.
    var line = null;
    var limits = null;
    var pointerId = null;
    // Where in the strip it was grabbed. The expansion moves this document's
    // origin up to the top of the page area, so the pointer's coordinate jumps by
    // however far the line was down the window; subtracting the grab keeps the
    // line under the same part of the strip it was taken by.
    var grab = 0;
    // How thick the strip really is, from Rust at the press. Not guessed from
    // how thick the line is drawn: the two were four and twelve, and the preview
    // promised eight pixels of inspector that the release did not give.
    var strip = 0;
    var frame = null;

    var split = document.getElementById("split");
    var above = document.getElementById("above");
    var below = document.getElementById("below");
    var abovePx = above ? above.querySelector(".px") : null;
    var belowPx = below ? below.querySelector(".px") : null;

    function draw() {
        frame = null;
        if (line === null || !split) {
            return;
        }
        split.style.top = line + "px";
        // The two fields meet at the line. Their edges are set here rather than
        // in CSS because only this file knows where the line is.
        if (above) {
            above.style.height = line + "px";
        }
        if (below) {
            below.style.top = line + strip + "px";
        }
        // The numbers the release will produce, from the total Rust measured
        // rather than from `innerHeight`. The two differ by a pixel on a scaled
        // display — `innerHeight` is fractional and rounds up — and a preview
        // that says 215 where the release gives 214 is a preview to distrust.
        if (abovePx) {
            abovePx.textContent = line + "px";
        }
        if (belowPx) {
            belowPx.textContent = limits.total - line - strip + "px";
        }
    }

    document.addEventListener("pointerdown", function (event) {
        if (event.button !== 0 || line !== null) {
            return;
        }
        pointerId = event.pointerId;
        grab = event.clientY;
        // Capture is still worth taking. It is not what makes the drag work any
        // more — the expansion is — but a release that happens over a scrollbar
        // or outside the window still has to arrive here to end the drag.
        if (document.documentElement.setPointerCapture) {
            try {
                document.documentElement.setPointerCapture(event.pointerId);
            } catch (e) {
                console.error("bru: divider could not capture the pointer: " + e);
            }
        }
        query({ type: "divider", phase: "start" }, function (response) {
            try {
                limits = JSON.parse(response);
            } catch (e) {
                console.error("bru: bad divider geometry: " + e);
                return;
            }
            line = limits.top;
            strip = limits.strip;
            split.style.height = strip + "px";
            document.body.classList.add("dragging");
            draw();
        });
        event.preventDefault();
    });

    document.addEventListener("pointermove", function (event) {
        if (line === null || event.pointerId !== pointerId) {
            return;
        }
        // Clamped here, against the numbers Rust gave at the press, so that the
        // preview never shows a split the release would refuse.
        // **Rounded here, not at the release.** `clientY` is fractional on a
        // scaled display, and a fraction is not a height: it would reach Rust as
        // a number the settings store has to refuse or truncate, and it would
        // draw the preview at a boundary the release could not reproduce.
        var wanted = Math.round(event.clientY - grab);
        if (wanted < limits.min) {
            wanted = limits.min;
        }
        if (wanted > limits.max) {
            wanted = limits.max;
        }
        line = wanted;
        if (frame === null) {
            frame = window.requestAnimationFrame(draw);
        }
        event.preventDefault();
    });

    function finish(event) {
        if (line === null || (event && event.pointerId !== pointerId)) {
            return;
        }
        var ended = line;
        line = null;
        limits = null;
        pointerId = null;
        if (frame !== null) {
            window.cancelAnimationFrame(frame);
            frame = null;
        }
        document.body.classList.remove("dragging");
        query({ type: "divider", phase: "end", y: ended });
    }

    document.addEventListener("pointerup", finish);
    // A capture broken by the compositor, by a window losing focus, by anything:
    // the drag ends where it stands. Without this the view would stay expanded
    // over the page for ever, which is a far worse failure than a drag that stops
    // early.
    document.addEventListener("pointercancel", finish);
    document.addEventListener("lostpointercapture", finish);
    // The pointer leaving the window entirely — off the top of the screen, say —
    // is the same thing: nothing more is coming, so settle where it is.
    window.addEventListener("blur", function () {
        finish(null);
    });
})();
