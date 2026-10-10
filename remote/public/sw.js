// Service worker for the Remote Control web app. It caches nothing on
// purpose: the page is only useful live, and a cached copy would keep serving
// an old build after a relay deploy. It exists so the app can be installed to
// the Home Screen, and to show notifications.
//
// A push comes from the Mac itself (`src-tauri/src/remote/push.rs`), encrypted
// to this browser: { summary: true, title, body, alert } — "2 need you · 1
// done" — or, from an older Mac, { title, body, handle, id } per claude.
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
      if (n.summary) {
        // One notification, edited in place: a summary of who needs you and
        // who finished (`remote/mod.rs::summary_for`). It buzzes only when
        // something new arrived; a smaller number replaces it silently.
        for (const old of await self.registration.getNotifications()) {
          if (old.tag !== "mulpex-summary") old.close();
        }
        await self.registration.showNotification(n.title || "Mulpex", {
          body: n.body || "",
          icon: "/icons/icon-192.png",
          badge: "/icons/icon-192.png",
          tag: "mulpex-summary",
          renotify: !!n.alert,
          silent: !n.alert,
          data: {},
        });
        return;
      }
      // A Mac from before the summary: one notification per claude.
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

// Tapping it opens that claude (an older Mac's per-claude notification), or the
// main screen (the summary): in the app if it's already open, else fresh.
self.addEventListener("notificationclick", (e) => {
  e.notification.close();
  const { handle, id } = e.notification.data || {};
  const open = handle != null ? `${handle}.${id}` : "";
  e.waitUntil(
    (async () => {
      const wins = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      if (wins.length) {
        await wins[0].focus();
        wins[0].postMessage(open ? { open } : { home: true });
        return;
      }
      await self.clients.openWindow(open ? `/?open=${open}` : "/");
    })(),
  );
});
