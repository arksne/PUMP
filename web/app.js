"use strict";

const node = id => document.getElementById(id);
const audio = node("audio"), seek = node("seekBar"), cover = node("cover"), frame = node("videoFrame");
const tabs = {player:["scrPlayer","tabPlayer"], search:["scrSearch","tabSearch"], lyrics:["scrLyrics","tabLyrics"], rooms:["scrRooms","tabRooms"], stats:["scrStats","tabStats"]};
const styles = ["card", "vinyl", "clover"];
const labels = {full:"Полная запись", preview:"Превью", youtube:"Видео"};
const known = new Map();
let results = [], queue = [], selected = null, index = -1, currentTab = "player";
let style = 0, shuffle = false, repeat = false, rate = 1, sleep = null;
let mountedVideo = null, lyricLines = [], lyricIndex = -1, lyricRequest = null;
let searchRequest = null, searchTimer = null, statsSeq = 0;
let token = stored("pump.session", ""), pending = stored("pump.listens", []);
let gettingSession = null, activeSession = null, sending = null, listening = null, meter = null;
let room = null;

function stored(key, fallback) {
  try { return JSON.parse(localStorage.getItem(key)) ?? fallback; } catch { return fallback; }
}
function persist(key, value) {
  try { localStorage.setItem(key, JSON.stringify(value)); } catch {}
}
const favorites = new Set(stored("pump.favorites", []));
for (const track of stored("pump.favorite-tracks", [])) if (track?.id) known.set(track.id, track);
if (!Array.isArray(pending)) pending = [];

const text = (id, value) => { node(id).textContent = value; };
function fmt(seconds) {
  const value = Math.max(0, Math.floor(seconds || 0));
  return Math.floor(value / 60) + ":" + String(value % 60).padStart(2, "0");
}
function element(tag, css, content) {
  const item = document.createElement(tag);
  if (css) item.className = css;
  if (content) item.textContent = content;
  return item;
}
function asset(value) {
  if (typeof value !== "string") return null;
  try {
    const url = new URL(value, location.href);
    return url.origin === location.origin || url.protocol === "https:" ? url.href : null;
  } catch { return null; }
}
async function api(path, {method="GET", body, signal, auth=false, keepalive=false} = {}) {
  const headers = {};
  if (body !== undefined) headers["Content-Type"] = "application/json";
  if (auth && token) headers.Authorization = "Bearer " + token;
  const res = await fetch(path, {method, headers, signal, keepalive, body:body === undefined ? undefined : JSON.stringify(body)});
  if (!res.ok) {
    let message = "Ошибка сервера (" + res.status + ").";
    try { message = (await res.json()).error || message; } catch {}
    throw Object.assign(new Error(message), {status:res.status});
  }
  return res.status === 204 ? null : res.json();
}
function telegramReady() {
  if (window.Telegram?.WebApp || (!location.hash.includes("tgWebApp") && !new URLSearchParams(location.search).has("telegram"))) {
    return Promise.resolve(window.Telegram?.WebApp);
  }
  return new Promise(resolve => {
    const tag = document.createElement("script");
    const timeout = setTimeout(() => resolve(undefined), 3800);
    tag.onload = tag.onerror = () => { clearTimeout(timeout); resolve(window.Telegram?.WebApp); };
    tag.src = "https://telegram.org/js/telegram-web-app.js";
    document.head.append(tag);
  });
}
const sdk = telegramReady();
sdk.then(app => {
  app?.ready?.();
  app?.expand?.();
  app?.BackButton?.onClick?.(back);
  syncBack();
});
function syncBack() {
  const back = window.Telegram?.WebApp?.BackButton;
  if (currentTab !== "player" || !node("sheet").hidden || node("queueSheet").open) back?.show?.();
  else back?.hide?.();
}
function back() {
  if (node("queueSheet").open) node("queueSheet").close();
  else if (!node("sheet").hidden) settings(false);
  else goTab("player");
}
async function session() {
  if (activeSession) return activeSession;
  if (!gettingSession) {
    gettingSession = (async () => {
      const app = await sdk;
      const result = await api("/api/session", {method:"POST", auth:true, body:{init_data:app?.initData || null}});
      if (token && token !== result.token) { pending = []; persist("pump.listens", pending); }
      token = result.token;
      persist("pump.session", token);
      activeSession = result;
      return result;
    })().finally(() => { gettingSession = null; });
  }
  return gettingSession;
}

async function search() {
  const query = node("qInput").value.trim(), source = node("source").value;
  searchRequest?.abort();
  if (source === "favorites") {
    const words = query.toLocaleLowerCase().split(/\s+/).filter(Boolean);
    results = [...favorites].map(id => known.get(id)).filter(track => track &&
      words.every(word => (track.title + " " + track.artist).toLocaleLowerCase().includes(word)));
    renderResults(results.length ? "Избранное" : "Подходящих записей в избранном нет.");
    return;
  }
  const request = new AbortController();
  searchRequest = request;
  text("sStatus", "Ищем…");
  try {
    const data = await api("/api/search?" + new URLSearchParams({q:query, source}), {signal:request.signal});
    if (request.signal.aborted) return;
    results = data.tracks;
    results.forEach(track => known.set(track.id, track));
    const count = results.length ? "Найдено: " + results.length : "Треки не найдены.";
    renderResults(data.notice ? count + " " + data.notice : count);
  } catch (error) {
    if (!request.signal.aborted) { results = []; renderResults(error.message); }
  }
}
function renderResults(status) {
  text("sStatus",status);
  node("list").replaceChildren(...results.map((track,i) => trackButton(track,i,"track")));
}
function trackButton(track, i, action) {
  const row = element("button","item glass");
  row.type = "button";
  row.dataset.action = action;
  row.dataset.value = String(i);
  const artwork = element("span","item-art","♫");
  const image = asset(track.artwork);
  if (image) {
    const img = element("img");
    img.alt = "";
    img.loading = "lazy";
    img.addEventListener("error", () => artwork.replaceChildren("♫"), {once:true});
    artwork.replaceChildren(img);
    img.src = image;
  }
  const info = element("span","item-info");
  info.append(element("span","item-t",track.title),
    element("span","item-s",[track.artist,labels[track.kind],track.source].filter(Boolean).join(" · ")));
  row.append(artwork,info,element("span","","›"));
  return row;
}
function renderQueue() {
  const entries = isGuest() ? [selected].filter(Boolean) : queue;
  const rows = entries.map((track,i) => {
    const row = trackButton(track,i,"queue-track");
    row.disabled = isGuest() || Boolean(room?.host && track.kind === "youtube");
    if (track.id === selected?.id) row.setAttribute("aria-current","true");
    return row;
  });
  node("queueList").replaceChildren(...(rows.length ? rows : [element("p","status","Выбери трек из поиска.")]));
}

function clearVideo() {
  frame.hidden = true;
  frame.removeAttribute("src");
  mountedVideo = null;
  node("artBox").classList.remove("video");
}
function selectTrack(track, remote=false) {
  if (!track || (!remote && isGuest())) return;
  finalizeListen();
  audio.pause();
  audio.removeAttribute("src");
  clearVideo();
  selected = track;
  known.set(track.id,track);
  meter = null;
  text("badge",labels[track.kind] || "PUMP");
  text("tTitle",track.title);
  text("tArtist",track.artist || track.source);
  text("playerStatus","");
  text("tCur","0:00");
  text("tDur","0:00");
  seek.value = "0";
  const image = asset(track.artwork);
  cover.hidden = !image;
  node("coverFallback").hidden = Boolean(image);
  if (image) cover.src = image; else cover.removeAttribute("src");
  node("bg").style.backgroundImage = image ? "url(" + JSON.stringify(image) + ")" : "none";
  node("attribution").hidden = !track.attribution;
  const url = asset(track.audio_url);
  if (track.kind !== "youtube" && url) {
    audio.src = url;
    audio.playbackRate = rate;
  }
  audio.load();
  updateButtons();
  if (node("queueSheet").open) renderQueue();
  loadLyrics(track);
}
cover.addEventListener("error", () => { cover.hidden = true; node("coverFallback").hidden = false; });

function requestPlay() {
  if (selected?.kind === "youtube") {
    if (!/^[A-Za-z0-9_-]{11}$/.test(selected.video_id || "")) return;
    frame.src = "https://www.youtube-nocookie.com/embed/" + selected.video_id + "?autoplay=1";
    frame.hidden = false;
    mountedVideo = selected.id;
    node("artBox").classList.add("video");
    text("playerStatus","Видео управляется во встроенном плеере YouTube.");
    updateButtons();
    return;
  }
  if (!audio.getAttribute("src")) return;
  audio.play().catch(error => {
    if (error.name === "AbortError") return;
    if (isGuest()) { room.sound = false; renderRoom(); }
    else text("playerStatus","Не удалось начать. Попробуй нажать Play ещё раз.");
  });
}
function pick(i) {
  if (!results[i] || isGuest()) return;
  if (room?.host && results[i].kind === "youtube") {
    text("sStatus","Для комнаты выбери трек с прямым аудиопотоком.");
    return;
  }
  queue = results.slice();
  playQueue(i);
  goTab("player");
}
function playQueue(i) {
  const track = queue[i];
  if (!track || isGuest() || (room?.host && track.kind === "youtube")) return;
  index = i;
  selectTrack(track);
  requestPlay();
  publishSoon();
}
function step(delta) {
  if (isGuest() || !queue.length) return;
  if (delta < 0 && selected?.kind !== "youtube" && audio.currentTime > 3) {
    audio.currentTime = 0;
    return;
  }
  const eligible = queue.map((track,i) => [track,i]).filter(([track]) => !room?.host || track.kind !== "youtube");
  if (!eligible.length) return;
  const current = Math.max(0,eligible.findIndex(([,i]) => i === index));
  const next = shuffle && eligible.length > 1
    ? (current + 1 + Math.floor(Math.random()*(eligible.length-1)))%eligible.length
    : (current + delta + eligible.length)%eligible.length;
  playQueue(eligible[next][1]);
}
function playPause() {
  if (isGuest()) return;
  if (!selected) { if (results.length) pick(0); else goTab("search"); return; }
  if (selected.kind === "youtube") { if (!mountedVideo) requestPlay(); return; }
  if (audio.paused) requestPlay(); else audio.pause();
}
function pressed(id,value,css) {
  node(id).classList.toggle(css,value);
  node(id).setAttribute("aria-pressed",String(value));
}
function updateButtons() {
  const guest = isGuest(), playing = !audio.paused && !audio.ended;
  node("playBtn").classList.toggle("is-playing",playing);
  node("playBtn").setAttribute("aria-label",playing ? "Пауза" : "Играть");
  node("playBtn").disabled = guest || Boolean(mountedVideo);
  node("artBox").classList.toggle("playing",playing);
  seek.disabled = guest || !selected || selected.kind === "youtube" || !Number.isFinite(audio.duration) || audio.duration <= 0;
  for (const control of document.querySelectorAll("[data-action=previous],[data-action=next],[data-action=shuffle],[data-action=repeat],[data-action=rate],[data-action=sleep]")) {
    control.disabled = guest || !selected;
  }
  node("likeBtn").disabled = !selected;
  pressed("likeBtn",Boolean(selected && favorites.has(selected.id)),"liked");
}
function like() {
  if (!selected) return;
  if (!favorites.delete(selected.id)) favorites.add(selected.id);
  persist("pump.favorites",[...favorites]);
  persist("pump.favorite-tracks",[...favorites].map(id => known.get(id)).filter(Boolean));
  updateButtons();
}
function setRate(value, button) {
  rate = Number(value);
  audio.playbackRate = rate;
  document.querySelectorAll("[data-action=rate]").forEach(item => item.classList.toggle("active",button === item));
  publishSoon();
}
function toggleSleep() {
  if (sleep) { clearTimeout(sleep); sleep = null; }
  else sleep = setTimeout(() => { audio.pause(); clearVideo(); toggleSleep(); updateButtons(); },900000);
  text("sleepBtn",sleep ? "Таймер: 15 мин" : "Таймер: выкл.");
  node("sleepBtn").classList.toggle("active",Boolean(sleep));
}
function settings(open) {
  node("sheet").hidden = !open;
  node("sheet").classList.toggle("open",open);
  node("backdrop").classList.toggle("open",open);
  syncBack();
}
function goTab(tab) {
  if (!tabs[tab]) return;
  currentTab = tab;
  for (const [name,[screen,button]] of Object.entries(tabs)) {
    node(screen).classList.toggle("active",tab === name);
    node(button).classList.toggle("active",tab === name);
    node(button).setAttribute("aria-current",tab === name ? "page" : "false");
  }
  if (tab === "stats") showStats();
  if (tab === "lyrics") syncLyrics(true);
  syncBack();
}

async function loadLyrics(track) {
  lyricRequest?.abort();
  const request = new AbortController();
  lyricRequest = request;
  lyricLines = [];
  lyricIndex = -1;
  const box = node("lyricsBox");
  box.replaceChildren(element("p","status","Загружаем текст…"));
  try {
    const data = await api("/api/lyrics/"+encodeURIComponent(track.id),{signal:request.signal});
    if (request.signal.aborted) return;
    for (const row of (data.synced || "").split(/\r?\n/)) {
      const match = /^\[(\d{1,3}):(\d{2})(?:\.(\d{1,3}))?\]\s*(.+)$/u.exec(row);
      if (match) lyricLines.push({time:Number(match[1])*60+Number(match[2])+Number("0."+(match[3]||"0")),text:match[4]});
    }
    lyricLines.sort((a,b) => a.time-b.time);
    box.replaceChildren();
    if (!lyricLines.length) box.append(element("p","plain-lyrics",data.plain || "Текст не найден."));
    else lyricLines.forEach((line,i) => {
      const button = element("button","l-line",line.text);
      button.type = "button";
      button.dataset.action = "lyric";
      button.dataset.value = String(i);
      box.append(button);
    });
    syncLyrics();
  } catch (error) {
    if (!request.signal.aborted) box.replaceChildren(element("p","status",error.message));
  }
}
function syncLyrics(force=false) {
  let next = -1;
  while (next+1 < lyricLines.length && lyricLines[next+1].time <= audio.currentTime) next++;
  if (next === lyricIndex && !force) return;
  const children = node("lyricsBox").children;
  children.item(lyricIndex)?.classList.remove("active");
  children.item(next)?.classList.add("active");
  if (currentTab === "lyrics") children.item(next)?.scrollIntoView({block:"nearest"});
  lyricIndex = next;
}

function sampleListening() {
  const now = performance.now(), position = audio.currentTime;
  if (meter && meter.id === selected?.id && !audio.paused && !audio.seeking && !audio.ended) {
    const elapsed = Math.min(2500, Math.max(0, now - meter.at));
    const delta = (position - meter.position) * 1000 / audio.playbackRate;
    if (delta > 0 && delta < elapsed + 400) {
      if (!listening) listening = {track_id:meter.id,milliseconds:0};
      listening.milliseconds += Math.round(Math.min(elapsed,delta));
      if (listening.milliseconds >= 5000) finalizeListen();
    }
  }
  meter = selected ? {id:selected.id, at:now, position} : null;
}
function finalizeListen() {
  const event = listening;
  listening = null;
  if (!event || event.milliseconds < 100 || typeof crypto.randomUUID !== "function") return;
  pending.push({...event,event_id:crypto.randomUUID()});
  if (pending.length > 256) pending.shift();
  persist("pump.listens", pending);
  flushListens().catch(() => {});
}
async function flushListens() {
  if (sending) return sending;
  sending = (async () => {
    if (!pending.length) return;
    await session();
    while (pending.length) {
      const event = pending[0];
      try {
        await api("/api/listens",{method:"POST",auth:true,body:event,keepalive:true});
      } catch (error) {
        if (![400,404].includes(error.status)) throw error;
      }
      pending = pending.filter(item => item.event_id !== event.event_id);
      persist("pump.listens", pending);
    }
  })().finally(() => { sending = null; });
  return sending;
}
async function showStats() {
  const seq = ++statsSeq;
  text("statsStatus","Считаем прослушивания…");
  try {
    await session();
    finalizeListen();
    await flushListens();
    const stats = await api("/api/stats?period="+node("statsPeriod").value,{auth:true});
    if (seq !== statsSeq) return;
    const seconds = Math.floor(stats.milliseconds/1000);
    text("statsTime",Math.floor(seconds/60)+" мин "+seconds%60+" сек");
    text("statsCount","Треков: "+stats.tracks);
    for (const [id,list] of [["statsTracks",stats.top_tracks],["statsArtists",stats.top_artists]]) {
      const rows = list.map(track => element("li","",[track.title,track.artist,fmt(track.milliseconds/1000)].filter(Boolean).join(" · ")));
      node(id).replaceChildren(...(rows.length ? rows : [element("li","","Пока нет прослушиваний.")]));
    }
    node("statsContent").hidden = false;
    text("statsStatus","");
  } catch (error) { if (seq === statsSeq) text("statsStatus",error.message); }
}

function isGuest() { return room?.host === false; }

async function createRoom() {
  try {
    await session();
    const data = await api("/api/rooms",{method:"POST",auth:true});
    node("roomCode").value = data.code;
    await joinRoom(data.code);
  } catch (error) { text("roomStatus",error.message); }
}
async function joinRoom(raw) {
  const code = raw.trim().toLowerCase();
  if (!/^[0-9a-f]{12}$/.test(code)) throw new Error("Введи 12 символов кода комнаты.");
  await session();
  leaveRoom();
  clearVideo();
  const address = new URL("/ws/room/"+code,location.href);
  address.protocol = address.protocol === "https:" ? "wss:" : "ws:";
  const socket = new WebSocket(address);
  const state = {code,socket,host:null,sound:false,desired:null,skew:0,rtt:Infinity,loading:null,trackRequest:0,timer:null};
  room = state;
  text("roomStatus","Подключаемся…");
  socket.addEventListener("open",() => {
    if (room === state) socket.send(JSON.stringify({type:"join",token}));
  });
  socket.addEventListener("message",event => {
    if (room !== state) return;
    let response;
    try { response = JSON.parse(event.data); } catch { return; }
    if (response.type === "joined") {
      state.host = response.host;
      state.skew = response.server_ms-Date.now();
      if (state.host) publish();
      else { audio.pause(); applyRemote(state,response.playback); }
      pingRoom();
      renderRoom();
    } else if (response.type === "playback" && !state.host) {
      applyRemote(state,response.playback);
    } else if (response.type === "pong") {
      const rtt = Date.now()-response.sent_ms;
      if (rtt >= 0 && rtt < state.rtt+8) {
        state.rtt = Math.min(state.rtt,rtt);
        state.skew = response.server_ms-(response.sent_ms+rtt/2);
      }
    } else if (response.type === "error") {
      text("roomStatus",response.message);
    }
  });
  socket.addEventListener("close",() => {
    if (room !== state) return;
    if (state.host === false) audio.pause();
    room = null;
    audio.playbackRate = rate;
    renderRoom(state.host === null ? "Комната недоступна. Проверь код." : "Соединение потеряно. Войди по тому же коду.");
  });
}
function renderRoom(message) {
  node("roomConnected").hidden = !room || room.host === null;
  node("enableRoomSound").hidden = !isGuest() || room.sound;
  text("roomStatus",message || (room?.host === true ? "Ведущий · "+room.code :
    room?.host === false ? "Слушатель · "+room.code : ""));
  updateButtons();
  if (node("queueSheet").open) renderQueue();
}
function leaveRoom() {
  if (!room) return;
  const old = room;
  if (old.host === true) publish(true);
  else if (old.host === false) audio.pause();
  room = null;
  clearTimeout(old.timer);
  old.socket.close();
  audio.playbackRate = rate;
  renderRoom();
}
function pingRoom() {
  if (room?.socket.readyState === WebSocket.OPEN) room.socket.send(JSON.stringify({type:"ping",sent_ms:Date.now()}));
}
function publish(paused=false) {
  if (!room?.host || room.socket.readyState !== WebSocket.OPEN) return;
  const direct = selected && selected.kind !== "youtube";
  room.socket.send(JSON.stringify({
    type:"playback",track_id:direct ? selected.id : null,
    position_ms:direct ? Math.round(Math.max(0,audio.currentTime || 0)*1000) : 0,
    playing:Boolean(direct && !audio.paused && !audio.ended && !paused),rate:audio.playbackRate
  }));
}
function publishSoon() {
  if (!room?.host) return;
  const state = room;
  clearTimeout(state.timer);
  state.timer = setTimeout(() => { if (room === state) publish(); },75);
}
async function applyRemote(state,playback) {
  if (state.desired && playback.revision < state.desired.revision) return;
  state.desired = playback;
  if (!playback.track_id) { audio.pause(); return; }
  if (selected?.id === playback.track_id) { syncRoom(); return; }
  if (state.loading === playback.track_id) return;
  state.loading = playback.track_id;
  const request = ++state.trackRequest;
  try {
    const track = known.get(playback.track_id) || await api("/api/tracks/"+encodeURIComponent(playback.track_id));
    if (room !== state || request !== state.trackRequest || state.desired?.track_id !== track.id) return;
    selectTrack(track,true);
    syncRoom();
  } catch (error) {
    if (room === state) text("roomStatus",error.message);
  } finally {
    if (request === state.trackRequest) state.loading = null;
  }
}
function syncRoom() {
  if (!isGuest() || !room.desired || selected?.id !== room.desired.track_id) return;
  const target = room.desired;
  const position = (target.position_ms+(target.playing ? Math.max(0,Date.now()+room.skew-target.at_ms)*target.rate : 0))/1000;
  if (Number.isFinite(audio.duration) && (Math.abs(audio.currentTime-position) > .25 || audio.paused && !target.playing)) {
    const next = Math.min(Math.max(0,position),audio.duration);
    if (Math.abs(audio.currentTime-next) > .05) audio.currentTime = next;
  }
  if (audio.playbackRate !== target.rate) audio.playbackRate = target.rate;
  if (!target.playing || !room.sound) audio.pause();
  else if (audio.paused && audio.readyState >= 1 && position < audio.duration) requestPlay();
}
function enableRoomSound() {
  if (!isGuest()) return;
  room.sound = true;
  if (room.desired?.playing) requestPlay();
  syncRoom();
  renderRoom();
}
function updateTimeline() {
  const duration = Number.isFinite(audio.duration) && audio.duration > 0 ? audio.duration : 0;
  seek.value = duration ? String(100*audio.currentTime/duration) : "0";
  text("tCur",fmt(audio.currentTime));
  text("tDur",fmt(duration));
}
node("qInput").addEventListener("input",() => {
  clearTimeout(searchTimer);
  searchRequest?.abort();
  searchTimer = setTimeout(search,350);
});
node("source").addEventListener("change",() => { clearTimeout(searchTimer); search(); });
node("statsPeriod").addEventListener("change",showStats);
node("queueSheet").addEventListener("close",syncBack);
node("joinRoom").addEventListener("submit",event => {
  event.preventDefault();
  joinRoom(node("roomCode").value).catch(error => text("roomStatus",error.message));
});
seek.addEventListener("input",() => {
  if (!isGuest() && Number.isFinite(audio.duration)) audio.currentTime = Number(seek.value)/100*audio.duration;
});
audio.addEventListener("timeupdate",() => { updateTimeline(); syncLyrics(); sampleListening(); });
audio.addEventListener("loadedmetadata",() => { updateTimeline(); updateButtons(); syncRoom(); });
audio.addEventListener("playing",() => { text("playerStatus",""); updateButtons(); publishSoon(); });
audio.addEventListener("pause",() => { meter = null; finalizeListen(); updateButtons(); publishSoon(); });
audio.addEventListener("seeking",() => { meter = null; });
audio.addEventListener("seeked",publishSoon);
audio.addEventListener("ended",() => {
  finalizeListen();
  if (isGuest()) return;
  if (repeat) { audio.currentTime = 0; requestPlay(); }
  else if (queue.length > 1) step(1);
  else { updateButtons(); publishSoon(); }
});
audio.addEventListener("error",() => {
  if (selected?.kind !== "youtube" && audio.getAttribute("src")) text("playerStatus","Аудиофайл недоступен.");
});
document.addEventListener("click",event => {
  const button = event.target.closest("[data-action]");
  if (!button || button.disabled) return;
  const {action,value} = button.dataset;
  if (event.isTrusted) window.Telegram?.WebApp?.HapticFeedback?.impactOccurred?.("light");
  switch (action) {
    case "tab": goTab(value); break;
    case "track": pick(Number(value)); break;
    case "queue-open": renderQueue(); node("queueSheet").showModal(); syncBack(); break;
    case "queue-close": node("queueSheet").close(); break;
    case "queue-track": node("queueSheet").close(); playQueue(Number(value)); break;
    case "play": playPause(); break;
    case "previous": step(-1); break;
    case "next": step(1); break;
    case "like": like(); break;
    case "shuffle": shuffle = !shuffle; pressed("shufBtn",shuffle,"active"); break;
    case "repeat": repeat = !repeat; pressed("repBtn",repeat,"active"); break;
    case "style": {
      const box = node("artBox");
      box.classList.remove(styles[style]);
      style = (style+1)%styles.length;
      box.classList.add(styles[style]);
      break;
    }
    case "settings-open": settings(true); break;
    case "settings-close": settings(false); break;
    case "rate": if (!isGuest()) setRate(value,button); break;
    case "sleep": if (!isGuest()) toggleSleep(); break;
    case "lyric": if (!isGuest() && lyricLines[Number(value)] && Number.isFinite(audio.duration)) {
      audio.currentTime = lyricLines[Number(value)].time;
      requestPlay();
    } break;
    case "create-room": createRoom(); break;
    case "leave-room": leaveRoom(); break;
    case "enable-room-sound": enableRoomSound(); break;
    case "copy-room":
      if (room) {
        const code = room.code;
        if (!navigator.clipboard) text("roomStatus","Код комнаты: "+code);
        else navigator.clipboard.writeText(code).then(
          () => text("roomStatus","Код скопирован"),
          () => text("roomStatus","Код комнаты: "+code)
        );
      }
      break;
  }
});
document.addEventListener("keydown",event => {
  if (event.key === "Escape" && !node("queueSheet").open) back();
});
window.addEventListener("pagehide",finalizeListen);
window.addEventListener("online",() => flushListens().catch(() => {}));
setInterval(() => { if (room?.host) publish(); else syncRoom(); },1000);
setInterval(pingRoom,5000);
search().then(() => {
  if (!selected && results.length) { queue = results.slice(); index = 0; selectTrack(queue[0]); }
});
session().then(flushListens).catch(() => {});
updateButtons();
