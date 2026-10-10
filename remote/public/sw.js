// Service worker for the Remote Control web app. It caches nothing on
// purpose: the page is only useful live, and a cached copy would keep serving
// an old build after a relay deploy. It exists so the app can be installed to
// the Home Screen, and to show notifications.
//
// A push comes from the Mac itself (`src-tauri/src/remote/push.rs`), encrypted
// to this browser: { title, body, handle, id } — "claude#3 needs you".
self.addEventListener("install", () => self.skipWaiting());
// A phone that cached the page before the relay sent `Cache-Control` keeps
// showing the old app for hours. A changed sw.js is the one thing it fetches
// past that cache, so this worker reloads open windows once when it takes over.
self.addEventListener("activate", (e) =>
  e.waitUntil(
    (async () => {
      await self.clients.claim();
      for (const c of await self.clients.matchAll({ type: "window" })) c.navigate(c.url).catch(() => {});
    })(),
  ),
);

// The page itself always comes from the network, never the HTTP cache — so a
// deploy shows up on the next open. Assets are content-hashed and stay cached.
self.addEventListener("fetch", (e) => {
  if (e.request.mode !== "navigate") return;
  e.respondWith(fetch(e.request.url, { cache: "no-store", credentials: "same-origin" }).catch(() => fetch(e.request)));
});

self.addEventListener("push", (e) => {
  let n = {};
  try {
    n = e.data ? e.data.json() : {};
  } catch {}
  e.waitUntil(
    (async () => {
      // The app is open in front: you're already looking at it, so nothing
      // goes to the bar (the app clears what's there when it comes up).
      const wins = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      if (wins.some((w) => w.visibilityState === "visible")) return;
      await self.registration.showNotification(n.title || "Mulpex", {
        body: n.body || "A claude needs you",
        icon: "/icons/icon-192.png",
        badge: "/icons/icon-192.png",
        // One notification per claude: a second question replaces the first.
        tag: n.handle != null ? `claude-${n.handle}-${n.id}` : "mulpex",
        renotify: true,
        data: { handle: n.handle, id: n.id },
      });
    })(),
  );
});

// Tapping it opens that claude: in the app if it's already open, else fresh.
self.addEventListener("notificationclick", (e) => {
  e.notification.close();
  const { handle, id } = e.notification.data || {};
  const open = handle != null ? `${handle}.${id}` : "";
  e.waitUntil(
    (async () => {
      const wins = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      if (wins.length) {
        await wins[0].focus();
        if (open) wins[0].postMessage({ open });
        return;
      }
      await self.clients.openWindow(open ? `/?open=${open}` : "/");
    })(),
  );
});
