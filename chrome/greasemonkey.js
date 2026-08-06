// bru's Greasemonkey wrapper — a port of qutebrowser 3.7.0's
// javascript/greasemonkey_wrapper.js, which is the file that decides what a userscript is allowed
// to do. Its answer is the reason greasemonkey is not a privilege escalation, and bru copies it
// exactly:
//
//   **Everything below runs in the page's own world with the page's own powers.**
//
// GM_xmlhttpRequest is an ordinary XMLHttpRequest — same origin rules, same CORS, no bypass, none
// of Tampermonkey's privileged fetch. GM_setValue is localStorage. GM_setClipboard is the page's
// own `copy` event. A userscript can do nothing a `<script>` in the page could not already do, so a
// script with `@match *://*/*` on a bank page has exactly the bank page's authority and no more.
//
// Two names are stubs that say so out loud rather than pretending: GM_getResourceText and
// GM_registerMenuCommand. Both would need something bru does not have (a fetched @resource; a
// browser menu), and both are stubbed in qutebrowser for the same reason.
//
// ---------------------------------------------------------------------------------------------
// Rust substitutes four sentinels into a copy of this text, once per script, in
// `Script::wrapped`. Each is named `BRU_` + its role and appears **exactly once below**, and
// nowhere in this header — a sentinel mentioned twice would be substituted twice, and the second
// copy landing in the middle of a comment is a syntax error that only exists in the generated file.
// `greasemonkey::tests::every_sentinel_appears_exactly_once_in_the_template` is what keeps that
// from happening again:
//
//   SCRIPT_NAME   the namespace/name pair, already escaped for a JS string literal
//   SCRIPT_INFO   GM_info as JSON, escaped for a JS string literal and JSON.parse'd
//   RUN_AT        document-start | document-end | document-idle, already normalised
//   SOURCE        the user's own script, verbatim, dropped into an empty block comment
//
// The sentinels are written so that this file parses as-is — `node --check chrome/greasemonkey.js`
// is part of the check, and a template that only became valid after substitution could not have
// one. There is no "use strict" anywhere on purpose: the `with` block at the bottom is what gives a
// script the global scope it expects, and strict mode forbids it.
(function () {
    const bru_gm_name = "@@BRU_SCRIPT_NAME@@";
    const bru_gm_id = "__gm_" + bru_gm_name;
    const bru_gm_run_at = "@@BRU_RUN_AT@@";

    const GM_info = JSON.parse("@@BRU_SCRIPT_INFO@@");

    function GM_log(text) {
        console.log(text);
    }

    function checkKey(key, funcName) {
        if (typeof key !== "string") {
            throw new Error(`${funcName} requires the first parameter to be of type string, not '${typeof key}'`);
        }
    }

    // Storage is the page's own localStorage, under a per-script prefix. That means a script's
    // values are readable by the page it runs on — which is exactly the level of privilege
    // everything here is held to, and the same thing qutebrowser does.
    function GM_setValue(key, value) {
        checkKey(key, "GM_setValue");
        if (typeof value !== "string" &&
            typeof value !== "number" &&
            typeof value !== "boolean") {
            throw new Error(`GM_setValue requires the second parameter to be of type string, number or boolean, not '${typeof value}'`);
        }
        localStorage.setItem(bru_gm_id + key, value);
    }

    function GM_getValue(key, default_) {
        checkKey(key, "GM_getValue");
        return localStorage.getItem(bru_gm_id + key) || default_;
    }

    function GM_deleteValue(key) {
        checkKey(key, "GM_deleteValue");
        localStorage.removeItem(bru_gm_id + key);
    }

    function GM_listValues() {
        const keys = [];
        for (let i = 0; i < localStorage.length; i++) {
            if (localStorage.key(i).startsWith(bru_gm_id)) {
                keys.push(localStorage.key(i).slice(bru_gm_id.length));
            }
        }
        return keys;
    }

    function GM_openInTab(url) {
        window.open(url);
    }

    // The whole security argument in one function. It is `new XMLHttpRequest()`, in the page's
    // world, subject to the page's CORS: a cross-origin GET fails here exactly as it fails for the
    // page's own scripts. The Tampermonkey *name* is kept so scripts written against it find what
    // they expect; the Tampermonkey *powers* are not, and never were in qutebrowser either.
    function GM_xmlhttpRequest(/* object */ details) {
        details.method = details.method ? details.method.toUpperCase() : "GET";

        if (!details.url) {
            throw new Error("GM_xmlhttpRequest requires a URL.");
        }

        const oXhr = new XMLHttpRequest();
        if ("onreadystatechange" in details) {
            oXhr.onreadystatechange = function () {
                details.onreadystatechange(oXhr);
            };
        }
        if ("onload" in details) {
            oXhr.onload = function () { details.onload(oXhr); };
        }
        if ("onerror" in details) {
            oXhr.onerror = function () { details.onerror(oXhr); };
        }
        if ("overrideMimeType" in details) {
            oXhr.overrideMimeType(details.overrideMimeType);
        }

        oXhr.open(details.method, details.url, true);

        if ("headers" in details) {
            for (const header in details.headers) {
                oXhr.setRequestHeader(header, details.headers[header]);
            }
        }

        if ("data" in details) {
            oXhr.send(details.data);
        } else {
            oXhr.send();
        }
    }

    function GM_addStyle(/* String */ styles) {
        const oStyle = document.createElement("style");
        oStyle.setAttribute("type", "text/css");
        oStyle.appendChild(document.createTextNode(styles));

        const head = document.getElementsByTagName("head")[0];
        if (head === undefined) {
            // No head yet — a document-start script is the usual reason. Stick it wherever.
            document.documentElement.appendChild(oStyle);
        } else {
            head.appendChild(oStyle);
        }
    }

    // Based on GreaseMonkey:
    // https://github.com/greasemonkey/greasemonkey/blob/4.11/src/bg/api-provider-source.js#L232-L249
    function GM_setClipboard(text) {
        function onCopy(event) {
            document.removeEventListener("copy", onCopy, true);

            event.stopImmediatePropagation();
            event.preventDefault();

            event.clipboardData.setData("text/plain", text);
        }

        document.addEventListener("copy", onCopy, true);
        document.execCommand("copy");
    }

    // Stubbed, and it says so on the console rather than returning a plausible lie. @resource would
    // have to be fetched, and bru never fetches a script or its resources by itself — see the head
    // of src/greasemonkey.rs. qutebrowser stubs this one identically.
    function GM_getResourceText(name) {
        console.info(`${GM_info.script.name} called unimplemented GM_getResourceText(${name})`);
    }

    // Stubbed for a different reason: bru has no menu to register a command into. Defined so the
    // greasemonkey 4 polyfill does not build a broken one on window, which is why qutebrowser has
    // it too.
    function GM_registerMenuCommand(caption) {
        console.info(`${GM_info.script.name} called unimplemented GM_registerMenuCommand(${caption})`);
    }

    // The greasemonkey 4.0 async API, over the same functions. Nothing new is reachable through it.
    const GM = {};
    GM.info = GM_info;
    const bru_gm_entries = {
        "log": GM_log,
        "addStyle": GM_addStyle,
        "setClipboard": GM_setClipboard,
        "deleteValue": GM_deleteValue,
        "getValue": GM_getValue,
        "listValues": GM_listValues,
        "openInTab": GM_openInTab,
        "setValue": GM_setValue,
        "xmlHttpRequest": GM_xmlhttpRequest,
        "getResourceText": GM_getResourceText,
        "registerMenuCommand": GM_registerMenuCommand,
    };
    for (const newKey in bru_gm_entries) {
        const old = bru_gm_entries[newKey];
        if (old && (typeof GM[newKey] === "undefined")) {
            GM[newKey] = function (...args) {
                return new Promise((resolve, reject) => {
                    try {
                        resolve(old(...args));
                    } catch (e) {
                        reject(e);
                    }
                });
            };
        }
    }

    const unsafeWindow = window;

    /*
     * Try to give userscripts an environment that they expect. Which seems to be that the global
     * window object should look the same as the page's one and that if a script writes to an
     * attribute of window all other scripts should be able to access that variable in the global
     * scope. Use a Proxy to stop scripts from actually changing the global window (that's what
     * unsafeWindow is for). Use the "with" statement to make the proxy provide what looks like
     * global scope.
     *
     * Note what this is and is not: it keeps userscripts from stamping on the *page's* globals. It
     * is not a boundary in the other direction — the page can read this proxy off its own window,
     * exactly as it can in qutebrowser.
     */
    if (!window.__bru_gm_window_proxy) {
        const bru_gm_window_shadow = {}; // stores local changes to window
        const bru_gm_windowProxyHandler = {
            get: function (target, prop) {
                if (prop in bru_gm_window_shadow) {
                    return bru_gm_window_shadow[prop];
                }
                if (prop in target) {
                    if (typeof target[prop] === "function" && typeof target[prop].prototype === "undefined") {
                        // Getting TypeError: Illegal Execution when callers try to execute eg
                        // addEventListener from here because they were returned unbound
                        return target[prop].bind(target);
                    }
                    return target[prop];
                }
                return undefined;
            },
            set: function (target, prop, val) {
                bru_gm_window_shadow[prop] = val;
                return true;
            },
            has: function (target, key) {
                return key in bru_gm_window_shadow || key in target;
            },
        };
        window.__bru_gm_window_proxy = new Proxy(unsafeWindow, bru_gm_windowProxyHandler);
    }
    const bru_gm_window_proxy = window.__bru_gm_window_proxy;

    function bru_gm_body() {
        with (bru_gm_window_proxy) {
            // We can't return `this` or `bru_gm_window_proxy` from the proxy's get('window')
            // because the Proxy implementation does typechecking on read-only things. So `window`
            // is shadowed more conventionally here.
            const window = bru_gm_window_proxy;
            // ====== The actual user script source ====== //
/*@@BRU_SOURCE@@*/
            // ====== End User Script ====== //
        }
    }

    function bru_gm_run() {
        try {
            bru_gm_body();
        } catch (e) {
            console.error(`bru: userscript ${bru_gm_name} threw: ${e}`);
        }
    }

    // @run-at, honoured in the page rather than by three different injection points in Rust.
    //
    // The wrapper is handed to the frame once, from the renderer's on_context_created — which is
    // before any of the page's own scripts have run, and is therefore document-start. The other two
    // moments are reached from there by waiting for the event that defines them, which is what
    // makes them right in subframes and on documents that were already complete when we arrived.
    if (bru_gm_run_at === "document-start") {
        bru_gm_run();
    } else if (bru_gm_run_at === "document-idle") {
        if (document.readyState === "complete") {
            setTimeout(bru_gm_run, 0);
        } else {
            window.addEventListener("load", function () { setTimeout(bru_gm_run, 0); }, { once: true });
        }
    } else {
        // document-end, and the default for anything else — the same fallback qutebrowser uses.
        if (document.readyState === "loading") {
            document.addEventListener("DOMContentLoaded", bru_gm_run, { once: true });
        } else {
            bru_gm_run();
        }
    }
})();
