import Foundation

/// Tier 4.3 — JavaScript snippets injected into a WKWebView for the
/// webview-interaction socket commands. Mirrors `nestty-linux/src/webview.rs::js`
/// snippet-for-snippet so the wire result shape is identical across platforms.
///
/// Each helper returns the JS source as a String. Caller passes that to
/// `WKWebView.evaluateJavaScript(...)` (via `WebViewController.executeJS`),
/// receives back a JSON-encoded result string, and decodes via
/// `JSONSerialization.jsonObject(with:)` before handing the parsed value
/// off to the socket completion. Returning JSON-as-string from the JS side
/// (rather than the `Any?` that WKWebView would otherwise produce) keeps
/// types stable across the bridge — no surprise `NSNumber` vs `NSString`
/// coercion the way `getElementById(...).className` would otherwise hit.
enum WebViewJS {
    /// Find one element. Returns null when not found, an object with
    /// tag/text/value/href/src/class/id/rect/visible otherwise.
    static func querySelector(_ selector: String) -> String {
        let sel = jsString(selector)
        return """
        (() => {
            const el = document.querySelector(\(sel));
            if (!el) return JSON.stringify(null);
            const r = el.getBoundingClientRect();
            return JSON.stringify({
                tag: el.tagName.toLowerCase(),
                text: el.innerText?.slice(0, 2000) || "",
                value: el.value || null,
                href: el.href || null,
                src: el.src || null,
                class: el.className || "",
                id: el.id || "",
                rect: { x: r.x, y: r.y, w: r.width, h: r.height },
                visible: r.width > 0 && r.height > 0,
            });
        })()
        """
    }

    /// Find all matching elements (capped at `limit`, default 50). Returns
    /// an array of element summaries with index/tag/text/value/href/class/id/rect.
    static func querySelectorAll(_ selector: String, limit: Int) -> String {
        let sel = jsString(selector)
        return """
        (() => {
            const els = [...document.querySelectorAll(\(sel))].slice(0, \(limit));
            return JSON.stringify(els.map((el, i) => {
                const r = el.getBoundingClientRect();
                return {
                    index: i,
                    tag: el.tagName.toLowerCase(),
                    text: el.innerText?.slice(0, 500) || "",
                    value: el.value || null,
                    href: el.href || null,
                    class: el.className || "",
                    id: el.id || "",
                    rect: { x: r.x, y: r.y, w: r.width, h: r.height },
                };
            }));
        })()
        """
    }

    /// Get computed CSS values for the listed properties on the first match.
    static func getStyles(_ selector: String, properties: [String]) -> String {
        let sel = jsString(selector)
        let propsJSON = jsArray(properties)
        return """
        (() => {
            const el = document.querySelector(\(sel));
            if (!el) return JSON.stringify(null);
            const cs = getComputedStyle(el);
            const props = \(propsJSON);
            const result = {};
            props.forEach(p => result[p] = cs.getPropertyValue(p));
            return JSON.stringify(result);
        })()
        """
    }

    /// Programmatically click() the first matching element.
    static func click(_ selector: String) -> String {
        let sel = jsString(selector)
        return """
        (() => {
            const el = document.querySelector(\(sel));
            if (!el) return JSON.stringify({ ok: false, error: "not found" });
            el.click();
            return JSON.stringify({ ok: true });
        })()
        """
    }

    /// Set `.value` on the first match + dispatch `input` and `change`
    /// events so React-style listeners pick up the change.
    static func fill(_ selector: String, value: String) -> String {
        let sel = jsString(selector)
        let val = jsString(value)
        return """
        (() => {
            const el = document.querySelector(\(sel));
            if (!el) return JSON.stringify({ ok: false, error: "not found" });
            el.focus();
            el.value = \(val);
            el.dispatchEvent(new Event('input', { bubbles: true }));
            el.dispatchEvent(new Event('change', { bubbles: true }));
            return JSON.stringify({ ok: true });
        })()
        """
    }

    /// Scroll into view by selector, OR scroll the viewport to (x,y) when
    /// selector is nil. Smooth + center on element-mode.
    static func scroll(selector: String?, x: Int, y: Int) -> String {
        if let sel = selector {
            let s = jsString(sel)
            return """
            (() => {
                const el = document.querySelector(\(s));
                if (!el) return JSON.stringify({ ok: false, error: "not found" });
                el.scrollIntoView({ behavior: "smooth", block: "center" });
                return JSON.stringify({ ok: true });
            })()
            """
        } else {
            return """
            (() => {
                window.scrollTo(\(x), \(y));
                return JSON.stringify({ ok: true, scrollX: window.scrollX, scrollY: window.scrollY });
            })()
            """
        }
    }

    /// Page metadata: title, url, dimensions, form/link/input/button counts.
    static func pageInfo() -> String {
        """
        (() => {
            return JSON.stringify({
                title: document.title,
                url: location.href,
                width: document.documentElement.scrollWidth,
                height: document.documentElement.scrollHeight,
                viewportWidth: window.innerWidth,
                viewportHeight: window.innerHeight,
                scrollX: window.scrollX,
                scrollY: window.scrollY,
                forms: document.forms.length,
                links: document.links.length,
                images: document.images.length,
                inputs: document.querySelectorAll('input, textarea, select').length,
                buttons: document.querySelectorAll('button, [role="button"], input[type="submit"]').length,
            });
        })()
        """
    }

    // MARK: - Helpers

    /// JSON-encode a string so it can be embedded literally in JS source.
    /// `JSONSerialization` with `.fragmentsAllowed` quotes it the same way
    /// `serde_json::to_string` does on Linux. Defensive: any failure falls
    /// back to an empty quoted string so a single bad selector doesn't
    /// crash the whole webview command.
    private static func jsString(_ s: String) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: s, options: [.fragmentsAllowed]),
              let str = String(data: data, encoding: .utf8)
        else {
            return "\"\""
        }
        return str
    }

    private static func jsArray(_ items: [String]) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: items),
              let str = String(data: data, encoding: .utf8)
        else {
            return "[]"
        }
        return str
    }
}
