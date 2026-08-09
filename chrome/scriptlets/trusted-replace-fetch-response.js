function bruTrustedReplaceFetchResponse(pattern, replacement, needle) {
    // `+js(trusted-replace-fetch-response, <pattern>, <replacement>, <url filter>)` — rewrite what
    // a `fetch()` came back with, before the page sees it.
    //
    // **This is the half that covers clicking a video inside YouTube.** Opening one directly is
    // handled by `set-constant`, because the advertisement list is embedded in the HTML; clicking
    // one from the feed loads no page at all — YouTube fetches a fresh player response over the
    // network, and the only place to reach that one is here. Measured 2026-08-09 by wrapping
    // `fetch` and `XMLHttpRequest.prototype.open` on the live page and clicking a video: the
    // response comes from `/youtubei/v1/get_watch`, via `fetch`, **not** from `/youtubei/v1/player`
    // — which is why a filter whose URL needle is `player` watches every request and rewrites
    // none of the ones that matter.
    //
    // **It is `trusted-` for a reason worth reading.** A scriptlet that can rewrite one response
    // can rewrite any of them, which is why bru grants the permission only to filters written by
    // hand in `~/.config/bru/` and never to a downloaded list. See `adblock::TRUSTED`.
    //
    // A function rather than a text template, and here the difference is not stylistic: this
    // scriptlet's own arguments contain quotes, and a template pastes them into a string literal.
    // See `set-constant.js` for the syntax error that produced and how it was found.
    //
    // A pattern wrapped in `/…/flags` is a regular expression and anything else is a literal
    // string, both uBlock's spellings. An empty replacement deletes what matched, which is what
    // one of YouTube's own rules asks for.
    pattern = String(pattern || "");
    replacement = String(replacement === undefined ? "" : replacement);
    needle = String(needle || "");
    if (!pattern) { return; }

    function unquote(text) {
        if (text.length > 1 && text[0] === "'" && text[text.length - 1] === "'") {
            return text.slice(1, -1);
        }
        return text;
    }

    var matcher;
    var m = /^\/(.+)\/([a-z]*)$/.exec(pattern);
    if (m) {
        try {
            // `g` always, or `String.replace` would change the first occurrence only and a player
            // response carries several.
            matcher = new RegExp(m[1], m[2].indexOf("g") === -1 ? m[2] + "g" : m[2]);
        } catch (e) { return; }
    } else {
        // A plain pattern becomes an escaped global regex too — uBlock's semantics. Handing the
        // string itself to `String.replace` looks equivalent and is not: a string matcher
        // replaces the first occurrence only, silently, and which occurrence is first depends on
        // whatever else the response happens to mention.
        try {
            matcher = new RegExp(
                unquote(pattern).replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "g");
        } catch (e) { return; }
    }
    replacement = unquote(replacement);
    needle = unquote(needle).replace(/\?$/, "");

    var original = window.fetch;
    if (typeof original !== "function") { return; }

    window.fetch = function (input, init) {
        var url = "";
        try { url = typeof input === "string" ? input : (input && input.url) || ""; } catch (e) {}
        var wanted = !needle || url.indexOf(needle) !== -1;
        var answer = original.apply(this, arguments);
        if (!wanted) { return answer; }
        return answer.then(function (response) {
            // Only a body that is text can be searched, and only a readable one can be replaced —
            // a response already consumed by something else must be handed back untouched.
            try {
                if (!response || response.bodyUsed) { return response; }
            } catch (e) { return response; }
            return response.text().then(function (text) {
                var edited;
                try { edited = text.replace(matcher, replacement); } catch (e) { return response; }
                if (edited === text) {
                    // Nothing matched. Rebuild anyway: the body has been read, so the original
                    // response can no longer be given to the page.
                    edited = text;
                }
                var rebuilt = new Response(edited, {
                    status: response.status,
                    statusText: response.statusText,
                    headers: response.headers,
                });
                try {
                    Object.defineProperty(rebuilt, "url", { value: response.url });
                    Object.defineProperty(rebuilt, "type", { value: response.type });
                } catch (e) {}
                return rebuilt;
            }).catch(function () { return response; });
        });
    };
}
