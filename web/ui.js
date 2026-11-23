"use strict";

// Браузерные API изолированы: отсутствие возможности не блокирует плеер.
window.PumpUX = (() => {
  let wasm = null, database = null;
  const frequencies = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
  const wasmReady = import("/assets/ui-liquid-glass/pkg/ui_liquid_glass.js")
    .then(async module => { await module.default(); wasm = module; return true; })
    .catch(() => false);
  const matches = (query, text) => wasm ? wasm.settings_match(query, text) :
    query.toLocaleLowerCase().split(/\s+/).every(word => text.toLocaleLowerCase().includes(word));
  function palette(image) {
    if (!image.complete || !image.naturalWidth || new URL(image.src, location.href).origin !== location.origin) return null;
    const canvas = document.createElement("canvas");
    canvas.width = canvas.height = 24;
    const ctx = canvas.getContext("2d", {willReadFrequently:true});
    ctx.drawImage(image, 0, 0, 24, 24);
    const pixels = ctx.getImageData(0, 0, 24, 24).data;
    let rgb;
    if (wasm) rgb = wasm.palette_rgb(pixels);
    else {
      const sum = [0, 0, 0]; let count = 0;
      for (let i = 0; i < pixels.length; i += 4) if (pixels[i+3] > 127) {
        sum.forEach((_, channel) => sum[channel] += pixels[i+channel]); count++;
      }
      rgb = count ? (Math.floor(sum[0]/count)<<16) | (Math.floor(sum[1]/count)<<8) | Math.floor(sum[2]/count) : 0x555566;
    }
    return [(rgb>>16)&255, (rgb>>8)&255, rgb&255].join(", ");
  }
  function autoEq(raw) {
    // AutoEq GraphicEQ интерполируется на десять полос в логарифмической шкале.
    const line = raw.split(/\r?\n/).find(line => /^\s*GraphicEQ:/i.test(line));
    let points;
    if (line) points = line.replace(/^\s*GraphicEQ:/i, "").split(";").filter(x => x.trim()).map(pair => pair.trim().split(/\s+/).map(Number));
    else {
      let data;
      try { data = JSON.parse(raw); } catch { throw new Error("Нужен AutoEq GraphicEQ.txt или JSON {gains:[10 значений], preamp:число}."); }
      if (!Array.isArray(data.gains) || data.gains.length !== 10 || !data.gains.every(x => typeof x === "number" && Number.isFinite(x) && Math.abs(x) <= 12)) throw new Error("JSON должен содержать 10 усилений от −12 до +12 dB.");
      if (data.preamp != null && (!Number.isFinite(data.preamp) || data.preamp < -24 || data.preamp > 0)) throw new Error("Preamp должен быть от −24 до 0 dB.");
      return {gains:data.gains, preamp:data.preamp ?? -Math.max(0, ...data.gains)};
    }
    if (!points.length || points.some(p => p.length !== 2 || !p.every(Number.isFinite) || p[0] <= 0 || Math.abs(p[1]) > 60)) throw new Error("Некорректные частоты или усиления AutoEq.");
    points.sort((a,b) => a[0]-b[0]);
    if (points.some((p,i) => i && p[0] === points[i-1][0])) throw new Error("Частоты AutoEq не должны повторяться.");
    const gains = frequencies.map(f => {
      const i = points.findIndex(p => p[0] >= f);
      if (i === -1) return points.at(-1)[1];
      if (i === 0) return points[0][1];
      const [a,b] = [points[i-1], points[i]];
      return a[1]+(b[1]-a[1])*Math.log(f/a[0])/Math.log(b[0]/a[0]);
    });
    if (gains.some(g => Math.abs(g) > 12)) throw new Error("Профиль выходит за диапазон десятиполосного EQ: ±12 dB.");
    return {gains:gains.map(g => Math.round(g*10)/10), preamp:-Math.max(0,...gains)};
  }
  async function db() {
    if (!window.indexedDB) throw new Error("IndexedDB недоступна в этом браузере.");
    if (!database) database = new Promise((resolve,reject) => {
      const request = indexedDB.open("pump-client", 1);
      request.onupgradeneeded = () => {
        request.result.createObjectStore("preferences");
        request.result.createObjectStore("downloads", {keyPath:"id"});
      };
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
      request.onblocked = () => reject(new Error("Закрой другие вкладки PUMP для обновления хранилища."));
    }).catch(error => { database = null; throw error; });
    return database;
  }
  async function transaction(store, method, ...args) {
    const connection = await db();
    return new Promise((resolve,reject) => {
      const tx = connection.transaction(store, method === "get" || method === "getAll" ? "readonly" : "readwrite");
      const request = tx.objectStore(store)[method](...args);
      tx.oncomplete = () => resolve(request.result);
      tx.onabort = tx.onerror = () => reject(tx.error || request.error || new Error("Ошибка локального хранилища."));
    });
  }
  async function download(track, source) {
    const url = new URL(source, location.href);
    if (url.origin !== location.origin || !/^https?:$/.test(url.protocol)) throw new Error("Скачивание доступно только для локального аудио или серверного proxy.");
    const response = await fetch(url);
    if (!response.ok) throw new Error("Не удалось скачать аудио: HTTP " + response.status);
    const type = response.headers.get("content-type") || "";
    if (!/^(audio|video)\//.test(type) && !type.startsWith("application/octet-stream")) throw new Error("Сервер вернул не аудиофайл.");
    const limit = 150 * 1024 * 1024;
    if (Number(response.headers.get("content-length")) > limit) throw new Error("Файл больше 150 МБ.");
    const reader = response.body.getReader(), chunks = []; let size = 0;
    while (true) {
      const {done,value} = await reader.read(); if (done) break;
      size += value.length;
      if (size > limit) { await reader.cancel(); throw new Error("Файл больше 150 МБ."); }
      chunks.push(value);
    }
    let lyrics=null,artwork=null;
    try{const response=await fetch("/api/lyrics/"+encodeURIComponent(track.id));if(response.ok)lyrics=await response.json();}catch{}
    if(track.artwork&&new URL(track.artwork,location.href).origin===location.origin){try{const response=await fetch(track.artwork);if(response.ok&&Number(response.headers.get("content-length"))<=8*1024*1024)artwork=await response.blob();}catch{}}
    const record = {id:track.id, track:{...track,offlineLyrics:lyrics?.plain||lyrics?.synced}, lyrics, artwork, blob:new Blob(chunks,{type}), size, at:Date.now()};
    await transaction("downloads", "put", record);
    return record;
  }
  return {frequencies, wasmReady, matches, palette, autoEq, transaction, download};
})();
