(() => {
  "use strict";
  // Pockets preview client, served by the preview proxy at
  // /__pockets-live/client.js (Pockets-owned, dependency-free ES2020).
  // Applies the tiered live updates from docs/live-preview.md:
  //   style  -> swap the affected stylesheet link, no reload
  //   morph  -> re-fetch this page and DOM-morph, preserving form state
  //   reload -> hard reload (changed static JS; spike 4: client-script
  //             state is never preserved)
  //   state  -> forwarded to window.__pocketsPreview for the frontend
  // The proxy is same-origin by construction: this script never fetches,
  // navigates, or opens anything but same-origin URLs.

  const PREFIX = "/__pockets-live";

  // morphdom 2.7.8 is loaded lazily (the injection is a single script tag);
  // the asset is the proxy's vendored UMD copy at /__pockets-live/morphdom.js.
  let morphdomPromise = null;
  function loadMorphdom() {
    if (!morphdomPromise) {
      morphdomPromise = new Promise((resolve, reject) => {
        const script = document.createElement("script");
        script.src = PREFIX + "/morphdom.js";
        script.onload = () => {
          if (typeof window.morphdom === "function") resolve(window.morphdom);
          else reject(new Error("morphdom did not define a global"));
        };
        script.onerror = () => {
          morphdomPromise = null;
          reject(new Error("morphdom failed to load"));
        };
        document.head.appendChild(script);
      });
    }
    return morphdomPromise;
  }

  function sameOrigin(href) {
    try {
      return new URL(href, location.href).origin === location.origin;
    } catch {
      return false;
    }
  }

  // Style tier: for each changed path, bump a cache-busting query param on
  // every same-origin stylesheet whose href carries the path stem (e.g.
  // "style.css"). No reload, no restart.
  function applyStyle(paths) {
    if (!Array.isArray(paths)) return;
    for (const path of paths) {
      const stem = String(path).split("/").pop();
      if (!stem) continue;
      for (const link of document.querySelectorAll('link[rel~="stylesheet"]')) {
        const href = link.getAttribute("href");
        if (!href || !href.includes(stem) || !sameOrigin(href)) continue;
        try {
          const url = new URL(href, location.href);
          if (url.origin !== location.origin) continue; // never leave the proxy origin
          url.searchParams.set("pockets-reload", String(Date.now()));
          link.setAttribute("href", url.pathname + url.search + url.hash);
        } catch {
          /* leave this link alone */
        }
      }
    }
  }

  // Morph tier (spikes/morph/RESULTS.md): re-fetch and morphdom the live
  // document. The form-preservation guard is mandatory — morphdom's default
  // silently clobbers user-entered input — and a morph whose new DOM lost
  // the focused element's id falls back to a plain reload, the spike's
  // documented fallback.
  let morphing = false;
  async function applyMorph() {
    if (morphing) return;
    morphing = true;
    try {
      const morphdom = await loadMorphdom();
      const response = await fetch(location.href, { cache: "no-store" });
      if (!response.ok) {
        location.reload();
        return;
      }
      const parsed = new DOMParser().parseFromString(
        await response.text(),
        "text/html",
      );
      const focusedId =
        document.activeElement && document.activeElement.id
          ? document.activeElement.id
          : null;
      morphdom(document.documentElement, parsed.documentElement, {
        onBeforeElUpdated: (from, to) => {
          if (to.tagName === "INPUT") {
            to.value = from.value;
            to.checked = from.checked;
          } else if (to.tagName === "TEXTAREA" || to.tagName === "SELECT") {
            to.value = from.value;
          }
          return true;
        },
      });
      if (focusedId && !document.getElementById(focusedId)) location.reload();
    } catch {
      location.reload();
    } finally {
      morphing = false;
    }
  }

  function handle(message) {
    if (!message || typeof message !== "object") return;
    switch (message.type) {
      case "style":
        applyStyle(message.paths);
        break;
      case "morph":
        void applyMorph();
        break;
      case "reload":
        location.reload();
        break;
      case "state":
        // The pane frontend consumes status through polling; exposing the
        // SSE state here is the documented relay for later use. Unknown
        // types are ignored.
        try {
          window.dispatchEvent(
            new CustomEvent("pockets-preview-state", { detail: message.state }),
          );
        } catch {
          /* nothing to relay to */
        }
        break;
      default:
        break;
    }
  }

  function connect() {
    const source = new EventSource(PREFIX + "/stream");
    source.onmessage = (event) => {
      let message;
      try {
        message = JSON.parse(event.data);
      } catch {
        return; // never act on malformed frames
      }
      handle(message);
    };
    // On transport failure EventSource reconnects by itself (the proxy sets
    // `retry:`); if it gives up entirely, retry slowly.
    source.addEventListener("error", () => {
      if (source.readyState === EventSource.CLOSED) setTimeout(connect, 3000);
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", connect, { once: true });
  } else {
    connect();
  }
})();
