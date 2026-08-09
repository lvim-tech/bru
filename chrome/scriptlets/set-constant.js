function bruSetConstant(path, raw) {
    // `+js(set, <path>, <value>)` — uBlock's `set-constant`, and what removes YouTube's
    // advertisement list from the response its player reads.
    //
    // **The prose is inside the function because the file has to begin with `function`.** The
    // engine decides how to call a scriptlet by reading the first line: a file starting with
    // `function <name>(` is invoked as `name(arg, arg)` with the arguments quoted properly, and
    // anything else is treated as a template whose markers it substitutes as text. The textual
    // form cannot carry YouTube's own arguments — one of them is `'"adPlacements"'`, and pasting
    // that inside a double-quoted string is a syntax error that kills the whole injected script,
    // every scriptlet in it, silently. Measured 2026-08-09.
    //
    // **Why a property and not an assignment.** Assigning once loses the race: the page's script
    // runs after this one and would overwrite it. A property whose setter ignores what it is given
    // wins whatever the order, which is the whole trick.
    //
    // **Why the whole chain is trapped and not only the last name.** Measured the same day on
    // `youtube.com/watch`: trapping `adPlacements` on a placeholder `ytInitialPlayerResponse` left
    // the field a live object, because the page does not *fill* that object — it assigns a whole
    // new one over it, and the trap goes with the object it was defined on. So every name before
    // the last is trapped too, and re-applies the rest of the path to whatever is assigned there.
    if (!path) { return; }

    var value;
    if (raw === "undefined") { value = undefined; }
    else if (raw === "null") { value = null; }
    else if (raw === "true") { value = true; }
    else if (raw === "false") { value = false; }
    else if (raw === "emptyObj") { value = {}; }
    else if (raw === "emptyArr") { value = []; }
    else if (raw === "noopFunc") { value = function () {}; }
    else if (raw === "''") { value = ""; }
    else if (/^-?\d+$/.test(raw)) { value = parseInt(raw, 10); }
    else { value = raw; }

    function isHolder(thing) {
        return thing !== null && (typeof thing === "object" || typeof thing === "function");
    }

    // The last name: read back the value, and swallow every write.
    function pin(owner, name) {
        try {
            var existing = Object.getOwnPropertyDescriptor(owner, name);
            if (existing && !existing.configurable) { return; }
            Object.defineProperty(owner, name, {
                configurable: true,
                enumerable: true,
                get: function () { return value; },
                // Swallowing the write is the point. Throwing would surface in the page's console
                // as an error in *its* script, which is a bug report about the wrong program.
                set: function () {},
            });
        } catch (e) {}
    }

    // Every name before the last: keep whatever the page puts here, and follow it.
    function follow(owner, chain) {
        var name = chain[0];
        if (chain.length === 1) { pin(owner, name); return; }
        var rest = chain.slice(1);

        var here = owner[name];
        // Follow what is already there — and then guard the name anyway, rather than returning.
        // **Returning here was the last bug but one.** By the time this runs YouTube has sometimes
        // already built `ytInitialPlayerResponse`, so this branch was taken, the last name was
        // pinned on that object, and nothing watched the name itself; the page then assigned a
        // *new* response over it and took the pin away with the old one.
        if (isHolder(here)) { follow(here, rest); }

        try {
            var existing = Object.getOwnPropertyDescriptor(owner, name);
            if (existing && !existing.configurable) { return; }

            // **A trap already here is joined, never replaced — replacing was the last bug.**
            // Three rules share the prefix `ytInitialPlayerResponse.`, and each call used to
            // `defineProperty` its own trap over the previous call's, so only the *last* rule's
            // field was re-pinned when the page assigned the real response — and the "last" rule
            // varied run to run, because the engine hands the rules over in hash order. Measured
            // 2026-08-09 on `youtube.com/watch`: `adPlacements` pinned and `undefined`, `adSlots`
            // and `playerAds` live objects; an earlier run of the same build had `adPlacements`
            // live instead. The getter carries the list so that joining survives any number of
            // injections; the marker is visible to a page that goes looking, and that is the
            // price of composing.
            if (existing && existing.get && existing.get.bruChains) {
                var chains = existing.get.bruChains;
                for (var i = 0; i < chains.length; i++) {
                    if (chains[i].join(".") === rest.join(".")) { return; }
                }
                chains.push(rest);
                return;
            }

            var held = here;
            var mine = [rest];
            var read = function () { return held; };
            read.bruChains = mine;
            Object.defineProperty(owner, name, {
                configurable: true,
                enumerable: true,
                get: read,
                set: function (assigned) {
                    held = assigned;
                    // The page has just built the object these paths run through. Trap the rest
                    // of every registered path on it now, before the page's next line reads it.
                    if (isHolder(assigned)) {
                        for (var i = 0; i < mine.length; i++) { follow(assigned, mine[i]); }
                    }
                },
            });
        } catch (e) {}
    }

    follow(window, String(path).split("."));
}
