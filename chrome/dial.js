// bru://chrome/dial — the mutations. This file draws NOTHING.
//
// Every tile, every heading and every icon is already in the document that
// arrived: src/dial.rs renders the page from `Data`, the way /help and
// /history are rendered, and the module comment there says why. What is left
// for JavaScript is the four things a document cannot do to itself — remove a
// tile, add one, rename or regroup one, and move one — plus Undo.
//
// So the rule this file keeps is: **ask Rust, then reload.** There is no
// client-side re-render, because a second renderer for the same tiles is a
// second description of what a tile is, and the two drift. Reloading is cheap:
// the document is served from memory by the scheme handler, and a mutation is
// something a person does once, not something that happens per frame.
//
// The one exception is a drag, where the tile follows the pointer locally so
// there is something to look at; the arrangement is sent once, on drop, and
// the reload follows that.
//
// The keyboard is bru's own. Tiles are <a> and the controls are <button>, so
// `f` hints all of them (chrome/hints.js's "all" group lists both), and the
// three inputs put bru into insert mode on the first key, as src/keys.rs does
// for any editable field. Nothing here binds a key of its own: a page that
// stole `j` from normal mode would be a page you cannot scroll.

(function () {
    "use strict";

    // ------------------------------------------------------------------ query

    function query(request, onSuccess) {
        if (typeof window.cefQuery !== "function") {
            // Not inside bru. Every control on this page is inert then, and
            // saying so beats a button that silently does nothing.
            note("This page only works inside bru.");
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
                // Rust has already said this in the status bar — see
                // dial::on_page_query, which calls message::error before it
                // fails the callback. This is the console copy, for the case
                // where the bar has scrolled on.
                console.error("bru: dial query failed (" + code + "): " + message);
            },
        });
    }

    // Ask, then reload. Everything below goes through here.
    function change(request) {
        query(request, function () {
            window.location.reload();
        });
    }

    function note(text) {
        var summary = document.querySelector("#controls .summary");
        if (summary) {
            summary.textContent = text;
        }
    }

    // The tile a control belongs to, and the URL that names it to Rust. The
    // URL is the primary key on both sides — see the `-- the dial --` section
    // in src/data.rs.
    function tileOf(node) {
        return node ? node.closest(".tile") : null;
    }

    function urlOf(tile) {
        return tile ? tile.getAttribute("data-url") : null;
    }

    // ------------------------------------------------- delete, edit, add
    //
    // Adding and editing are the same three fields at the bottom of the page.
    // `✎` fills them from the tile and turns `Add` into `Edit`; `Cancel`, or
    // Escape in any of the fields, puts the form back to adding.
    //
    // `editing` is the URL the form is currently pointed at, or null. It is the
    // whole of the mode: everything else — the button label, the read-only URL,
    // the marked tile — is derived from it in `setMode`, so the form cannot end
    // up saying "Edit" while sending an `add`.

    var form = document.getElementById("add");
    var urlBox = document.getElementById("add-url");
    var titleBox = document.getElementById("add-title");
    var groupBox = document.getElementById("add-group");
    var goButton = document.getElementById("add-go");
    var cancelButton = document.getElementById("add-cancel");

    var editing = null;

    function setMode(url) {
        editing = url;
        var tile = url
            ? document.querySelector('.tile[data-url="' + cssEscape(url) + '"]')
            : null;

        var marked = document.querySelector(".tile.editing");
        if (marked) {
            marked.classList.remove("editing");
        }
        if (tile) {
            tile.classList.add("editing");
        }

        // The URL is editable in both modes. It is still the *key* the change is
        // addressed by, which is why `editing` holds the address the tile had
        // when ✎ was pressed and the form sends both: `url` names the tile,
        // `new_url` is what it should point at from now on. Data::dial_retarget
        // replaces the entry in place, so correcting an address does not send
        // the tile to the end of the dial.
        goButton.textContent = url ? "Edit" : "Add";
        cancelButton.hidden = url === null;
    }

    function clearForm() {
        urlBox.value = "";
        titleBox.value = "";
        groupBox.value = "";
        setMode(null);
    }

    // The tile's own name and the heading it sits under. Read out of the
    // document rather than kept in a parallel table: the document is what Rust
    // rendered, so it cannot be stale, and there is nowhere else for the two to
    // disagree.
    function startEditing(tile) {
        if (!tile) {
            return;
        }
        var name = tile.querySelector(".name");
        var section = tile.closest("section.group");
        var heading = section ? section.querySelector("h2") : null;

        urlBox.value = urlOf(tile);
        titleBox.value = name ? name.textContent : "";
        groupBox.value = heading ? heading.textContent : "";
        setMode(urlOf(tile));

        titleBox.focus();
        titleBox.select();
        form.scrollIntoView({ block: "nearest" });
    }

    // A URL goes into a selector, and a URL can hold a quote. CSS.escape is in
    // every Chromium this runs on; the fallback is here because a selector that
    // throws would take the whole handler with it.
    function cssEscape(value) {
        if (window.CSS && typeof window.CSS.escape === "function") {
            return window.CSS.escape(value);
        }
        return value.replace(/["\\]/g, "\\$&");
    }

    // `closest` and not `event.target.classList`, and that distinction is a bug
    // this file already had: the two marks became inline <svg> (see PENCIL and
    // CROSS in src/dial.rs), so a click lands on the <svg> or on the <path>
    // inside it and never on the <button> that carries the class. Asking the
    // target what it is inside answers the same for the button, for its svg and
    // for any element either of them grows later.
    document.addEventListener("click", function (event) {
        var target = event.target;
        var within = function (selector) {
            return target && target.closest ? target.closest(selector) : null;
        };

        if (within(".del")) {
            event.preventDefault();
            var doomed = urlOf(tileOf(target));
            if (doomed) {
                // No confirmation, on purpose, and it is the same decision
                // cookies.rs writes down at length: Undo is the honest half.
                // A dialog only asks whether you meant it; being able to put
                // the tile back is what makes a misclick cost nothing.
                change({ type: "dial", action: "delete", url: doomed });
            }
            return;
        }

        if (within(".edit")) {
            event.preventDefault();
            startEditing(tileOf(target));
            return;
        }

        if (within("#add-cancel")) {
            event.preventDefault();
            clearForm();
            return;
        }

        if (within("#undo")) {
            event.preventDefault();
            change({ type: "dial", action: "undo" });
        }
    });

    if (form) {
        form.addEventListener("submit", function (event) {
            event.preventDefault();
            var url = urlBox.value.trim();
            if (!url) {
                note("An address is the one field a tile cannot do without.");
                return;
            }
            if (editing !== null) {
                change({
                    type: "dial",
                    action: "edit",
                    url: editing,
                    new_url: url,
                    title: titleBox.value,
                    group: groupBox.value,
                });
                return;
            }
            change({
                type: "dial",
                action: "add",
                url: url,
                title: titleBox.value,
                group: groupBox.value,
            });
        });

        // Escape leaves editing and empties the form. It does **not** also leave
        // insert mode: src/keys.rs answers the next Escape, so one press gets
        // out of the form and the second out of the mode, which is the order
        // the same two presses have everywhere else in bru.
        form.addEventListener("keydown", function (event) {
            if (event.key === "Escape") {
                event.stopPropagation();
                clearForm();
            }
        });
    }

    // ---------------------------------------------------------------- reorder
    //
    // The tile follows the pointer locally, and the whole arrangement is sent
    // once on drop — never one "move to index N" per step. Data::dial_reorder
    // takes the full order for that reason: it is idempotent, and a tile the
    // page never knew about (one a `:dial-add` put there while this document
    // was open) is kept rather than dropped.

    var dragged = null;

    document.addEventListener("dragstart", function (event) {
        dragged = tileOf(event.target);
        if (!dragged) {
            return;
        }
        dragged.classList.add("dragging");
        // Firefox and Chromium both need the payload set for a drag to start
        // at all. Nothing reads it; the element is held in `dragged`.
        if (event.dataTransfer) {
            event.dataTransfer.effectAllowed = "move";
            event.dataTransfer.setData("text/plain", urlOf(dragged) || "");
        }
    });

    document.addEventListener("dragend", function () {
        if (dragged) {
            dragged.classList.remove("dragging");
            dragged = null;
        }
    });

    document.addEventListener("dragover", function (event) {
        if (!dragged) {
            return;
        }
        // Without this the drop is refused by the default handler.
        event.preventDefault();

        var over = tileOf(event.target);
        if (!over || over === dragged) {
            // Dropping onto the empty part of a group's row appends to it,
            // which is how a tile is moved into a group that has none of its
            // own tiles near the pointer.
            var tiles = event.target.closest
                ? event.target.closest(".tiles")
                : null;
            if (tiles && tiles !== dragged.parentNode) {
                tiles.appendChild(dragged);
            }
            return;
        }
        // Insert before or after, by which half of the tile the pointer is on.
        var box = over.getBoundingClientRect();
        var after = event.clientX > box.left + box.width / 2;
        over.parentNode.insertBefore(dragged, after ? over.nextSibling : over);
    });

    document.addEventListener("drop", function (event) {
        if (!dragged) {
            return;
        }
        event.preventDefault();

        // Document order, which after the moves above is what the screen
        // shows. Rust rewrites the file to match, and the group a tile ends up
        // under is the heading it was dropped beneath — sent as an edit,
        // because moving between groups is a change of the tile and not of the
        // order.
        var moved = dragged;
        var section = moved.closest("section.group");
        var heading = section ? section.querySelector("h2") : null;
        var group = heading ? heading.textContent : "";

        var order = [];
        var all = document.querySelectorAll(".tile");
        for (var i = 0; i < all.length; i++) {
            var url = urlOf(all[i]);
            if (url) {
                order.push(url);
            }
        }

        // Two requests, in order: the group first, so that the reorder — which
        // is what reloads the page — sees a file the edit has already touched.
        query(
            {
                type: "dial",
                action: "edit",
                url: urlOf(moved),
                title: "",
                group: group,
            },
            function () {
                change({ type: "dial", action: "reorder", urls: order });
            },
        );
    });
})();
