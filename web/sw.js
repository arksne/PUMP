"use strict";
// Кэшируются только ресурсы интерфейса; API и серверные загрузки не подменяются.
const CACHE = "pump-shell-v4";
const SHELL = ["/assets/dsp-client.js", "/assets/dsp-worklet.js", "/assets/ui-liquid-glass/pkg/pump_dsp.wasm", "/", "/assets/app.js", "/assets/ui.js", "/assets/ui.css", "/assets/ui-liquid-glass/pkg/ui_liquid_glass.js", "/assets/ui-liquid-glass/pkg/ui_liquid_glass_bg.wasm"];
self.addEventListener("install", event => event.waitUntil(caches.open(CACHE).then(cache => cache.addAll(SHELL))));
self.addEventListener("activate", event => event.waitUntil((async () => {
  for (const key of await caches.keys()) if (key.startsWith("pump-shell-") && key !== CACHE) await caches.delete(key);
  await self.clients.claim();
})()));
self.addEventListener("fetch", event => {
  const url = new URL(event.request.url);
  if (event.request.method !== "GET" || url.origin !== self.location.origin || !SHELL.includes(url.pathname) || url.search) return;
  event.respondWith((async () => {
    const cache = await caches.open(CACHE);
    try {
      const response = await fetch(event.request);
      if (response.ok) await cache.put(event.request, response.clone());
      return response;
    } catch (error) {
      const cached = await cache.match(event.request);
      if (cached) return cached;
      throw error;
    }
  })());
});
