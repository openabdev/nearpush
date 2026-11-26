// nearpush service worker: shows Web Push notifications. It never caches or
// intercepts the page (no fetch handler).
'use strict';

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));

self.addEventListener('push', (event) => {
  let p = {};
  try { p = event.data ? event.data.json() : {}; } catch { p = {}; }
  if (!p || typeof p !== 'object') p = {};
  // The title comes from the daemon's fixed table and is shown as is.
  const title = typeof p.title === 'string' && p.title ? p.title : 'nearpush';
  const options = {
    body: typeof p.body === 'string' ? p.body : '',
    icon: '/icon-192.png',
    data: { url: '/' },
  };
  if (typeof p.tag === 'string' && p.tag) options.tag = p.tag;
  // Always show something: iOS revokes a subscription whose pushes show nothing.
  event.waitUntil(self.registration.showNotification(title, options));
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  event.waitUntil((async () => {
    const all = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    for (const c of all) {
      if ('focus' in c) return c.focus();
    }
    return self.clients.openWindow('/');
  })());
});
