// bru's panel — the completion table and the prompt, in the one chrome view that
// changes size. src/window.rs, src/completers.rs, src/prompt.rs.
//
// Two directions, and only two, exactly as bottom.js has them:
//
//   Rust  -> panel   `bru.render(<the same state object the bar is given>)`
//   panel -> Rust    {type:"ready", view:"panel"}   once, on load
//                    {type:"height", px}            after every draw that moved it
//
// It is handed the *whole* state object rather than a slice of it. One JSON
// string is built per push and executed in both chrome frames; this file reads
// the two keys it draws and ignores the rest, and bottom.js does the opposite.
// A second serialisation would be a second thing to keep in step for no gain —
// the payload is already built, and neither document is confused by a key it has
// no element for.
(function () {
    "use strict";

    var VIEW = "panel";

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

    // What the view's height was last asked to be. A push happens for a scroll
    // report and for a title change; asking Rust to relayout on every one of
    // them would be a round trip per keystroke for an answer that almost never
    // moves.
    var reportedHeight = null;

    function reportHeight() {
        // **The two blocks, added — not `documentElement.scrollHeight`.**
        //
        // That was the first attempt and it is self-referential: `scrollHeight`
        // on the document element is never less than the viewport, and the
        // viewport is the view whose height this number decides. The panel
        // asked for 760px — the height the box layout happened to give it
        // before the first report — and kept it.
        //
        // Both children are `display: none` when empty, so `offsetHeight` is 0
        // for a block that is closed, and with neither open this is 0 — which is
        // what makes the box layout give this view no room at all rather than a
        // blank band above the bar.
        var prompt = document.getElementById("prompt");
        var completion = document.getElementById("completion");
        var px =
            (prompt ? prompt.offsetHeight : 0) +
            (completion ? completion.offsetHeight : 0);
        if (px === reportedHeight) {
            return;
        }
        reportedHeight = px;
        query({ type: "height", px: px });
    }

    window.bru = {
        render: function (state) {
            state = state || {};
            if (window.bruCompletion) {
                window.bruCompletion.render(state.completion);
            }
            // prompt.js owns every string in it, because every string in it may
            // have come from a web page.
            if (window.bruPrompt) {
                window.bruPrompt.render(state.prompt);
            }
            // Last, and after the DOM is written: this is what sizes the view,
            // and it has to measure what was just drawn rather than what is
            // about to be.
            reportHeight();
        },
    };

    query({ type: "ready", view: VIEW }, function (response) {
        // Rust answers the ready query with the current state, so a panel that
        // has just loaded is never a frame behind the bar beside it.
        try {
            window.bru.render(JSON.parse(response));
        } catch (e) {
            console.error("bru: bad ready response: " + e);
        }
    });
})();
