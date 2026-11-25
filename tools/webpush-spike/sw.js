// Spike service worker: show every push as a notification (iOS requires a
// visible notification for each push, or it revokes the subscription).
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));

self.addEventListener("push", (event) => {
  let d = {};
  try { d = event.data ? event.data.json() : {}; } catch (_) { d = { body: event.data && event.data.text() }; }
  const title = d.title || "nearpush";
  const opts = { body: (d.body || "") + (d.sent ? " (" + d.sent + ")" : ""), tag: "nearpush-" + (d.state || "x") };
  event.waitUntil(self.registration.showNotification(title, opts));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  event.waitUntil(self.clients.openWindow("/"));
});
