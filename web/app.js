"use strict";

const node = id => document.getElementById(id);
const audio = node("audio"), seek = node("seekBar"), cover = node("cover"), frame = node("videoFrame");
const tabs = {player:["scrPlayer","tabPlayer"], search:["scrSearch","tabSearch"], lyrics:["scrLyrics","tabLyrics"], rooms:["scrRooms","tabRooms"], stats:["scrStats","tabStats"]};
const styles = ["card", "vinyl", "clover", "classic-glass", "full-bleed"];
const labels = {full:"Полная запись", preview:"Превью", youtube:"Видео"};
const known = new Map();
let results = [], queue = [], selected = null, index = -1, currentTab = "player";
let style = 0, shuffle = false, repeat = false, rate = 1, sleep = null;
let mountedVideo = null, lyricLines = [], lyricIndex = -1, lyricRequest = null;
let searchRequest = null, searchTimer = null, statsSeq = 0;
let token = stored("pump.session", ""), pending = stored("pump.listens", []);
let gettingSession = null, activeSession = null, sending = null, listening = null, meter = null;
let localOnly=stored("pump.localOnly",false);
let room = null;
let scrobbleState=null,lastfmConnected=false;
let streamCandidates=[], streamIndex=0, streamSelection=0;
const streamVideo=node("streamVideo");

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
  if(localOnly)throw new Error("Включён режим «Только офлайн».");
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
      connectControls();
      return result;
    })().finally(() => { gettingSession = null; });
  }
  return gettingSession;
}

async function search() {
  if(localOnly){const records=await PumpUX.transaction("downloads","getAll");const query=node("qInput").value.toLocaleLowerCase();results=records.filter(record=>(record.track.title+" "+record.track.artist).toLocaleLowerCase().includes(query)).map(record=>({...record.track,offlineBlob:record.blob}));renderResults("Офлайн-медиатека");return;}

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
  streamVideo.pause();streamVideo.removeAttribute("src");streamVideo.hidden=true;
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
  streamSelection++;streamCandidates=[];
  if(canvasUrl){node("canvasVideo").pause();node("canvasVideo").removeAttribute("src");node("canvasVideo").hidden=true;URL.revokeObjectURL(canvasUrl);canvasUrl=null;}
  selected = track;
  scrobbleState={started_at_unix:Math.floor(Date.now()/1000),played_ms:0,queued:false};
  node("streamTools").hidden=track.kind!=="youtube";
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
    audio.src = track.kind === "preview" ? "/preview/"+encodeURIComponent(track.id) : url;
    audio.playbackRate = rate;
  }
  if(track.offlineBlob){const blobUrl=URL.createObjectURL(track.offlineBlob);audio.src=blobUrl;audio.addEventListener("emptied",()=>URL.revokeObjectURL(blobUrl),{once:true});}
  audio.load();
  updateButtons();
  if (node("queueSheet").open) renderQueue();
  if(localOnly)node("lyricsBox").replaceChildren(element("p","plain-lyrics",track.offlineLyrics||"Текст не сохранён."));else loadLyrics(track);
}
cover.addEventListener("error", () => { cover.hidden = true; node("coverFallback").hidden = false; });

function requestPlay() {
  startAudioGraph();
  if (selected?.kind === "youtube") {
    if(!streamCandidates.length){loadStreams();return;}
    playStreamCandidate();return;
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
  if (selected.kind === "youtube") { if (!audio.paused && !streamVideo.src) audio.pause(); else if(!streamVideo.paused)streamVideo.pause();else requestPlay(); return; }
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
  seek.disabled = guest || !selected || (selected.kind === "youtube" && !audio.getAttribute("src")) || !Number.isFinite(audio.duration) || audio.duration <= 0;
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
  if (sleep) { clearInterval(sleep); sleep = null; audio.volume = 1; }
  else {
    const deadline = Date.now()+Number(node("sleepDuration").value)*60000;
    sleep = setInterval(() => {
      const remaining = deadline-Date.now();
      audio.volume = Math.min(1,Math.max(0,remaining/15000));
      if (remaining <= 0) { audio.pause(); clearVideo(); toggleSleep(); updateButtons(); }
    },100);
  }
  text("sleepBtn",sleep ? "Отменить таймер" : "Таймер: выкл.");
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
    if(data.document?.lines?.length) {
      lyricLines=data.document.lines.filter(line=>line.start_ms!=null).map(line=>({...line,time:line.start_ms/1000}));
    } else {
      for (const row of (data.synced || "").split(/\r?\n/)) {
        const match=/^\[(\d{1,3}):(\d{2})(?:\.(\d{1,3}))?\]\s*(.+)$/u.exec(row);
        if(match)lyricLines.push({time:Number(match[1])*60+Number(match[2])+Number("0."+(match[3]||"0")),text:match[4]});
      }
    }
    lyricLines.sort((a,b)=>a.time-b.time);box.replaceChildren();
    if(!lyricLines.length)box.append(element("p","plain-lyrics",data.plain||"Текст не найден."));
    else lyricLines.forEach((line,i)=>{
      const button=element("button","l-line");button.type="button";button.dataset.action="lyric";button.dataset.value=String(i);
      if(line.singer_ids?.length){button.append(element("span","lyric-singer",line.singer_ids.join(" · ")));button.dataset.singerSide=line.alignment==="right"||line.alignment==="end"?"right":"left";}
      if(line.words?.length)for(const word of line.words){const span=element("span","lyric-word",word.text);span.dataset.start=String(word.start_ms);span.dataset.end=String(word.end_ms??Infinity);button.append(span);}
      else button.append(document.createTextNode(line.text));box.append(button);
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
      if(scrobbleState){scrobbleState.played_ms+=Math.round(Math.min(elapsed,delta));const duration_ms=Math.round(audio.duration*1000);
        if(lastfmConnected&&!scrobbleState.queued&&duration_ms>30000&&scrobbleState.played_ms>=duration_ms/2){scrobbleState.queued=true;api("/api/lastfm/scrobble",{method:"POST",auth:true,body:{track_id:selected.id,duration_ms,started_at_unix:scrobbleState.started_at_unix,played_ms:scrobbleState.played_ms}}).catch(error=>text("settingsStatus",error.message));}
      }
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
      node("designSelect").value = styles[style];
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

// Дополнительные возможности Mini App используют тот же элемент audio и сессию.
let graph=null, remoteSocket=null, reconnectTimer=null;
let eqGains=stored("pump.eq",Array(10).fill(0));
if (!Array.isArray(eqGains)||eqGains.length!==10) eqGains=Array(10).fill(0);
function startAudioGraph() {
  if (!graph && window.AudioContext && (!audio.src || audio.src.startsWith("blob:") || new URL(audio.src).origin === location.origin)) {
    const context=new AudioContext();
    const source=context.createMediaElementSource(audio);
    const preamp=context.createGain();
    let tail=source;
    const filters=PumpUX.frequencies.map(f=>{
      const filter=context.createBiquadFilter();filter.type="peaking";filter.frequency.value=f;filter.Q.value=1.4;
      tail.connect(filter);tail=filter;return filter;
    });
    tail.connect(preamp);preamp.connect(context.destination);graph={context,filters,preamp};applyEq();
  }
  graph?.context.resume().catch(()=>{});
}
function applyEq() {
  if (!graph) return;
  const enabled=node("eqEnabled").checked;
  graph.filters.forEach((filter,i)=>filter.gain.setTargetAtTime(enabled?eqGains[i]:0,graph.context.currentTime,.02));
  graph.preamp.gain.setTargetAtTime(enabled?Math.pow(10,Number(node("preamp").value)/20):1,graph.context.currentTime,.02);
  text("preampValue",node("preamp").value+" dB");
  persist("pump.eq",eqGains);
}
PumpUX.frequencies.forEach((frequency,i)=>{
  const label=element("label","eq-band",frequency>=1000?frequency/1000+"k":String(frequency));
  const slider=element("input");slider.type="range";slider.min="-12";slider.max="12";slider.step=".5";slider.value=String(eqGains[i]);slider.setAttribute("aria-label","EQ "+frequency+" Гц");
  slider.addEventListener("input",()=>{eqGains[i]=Number(slider.value);applyEq();});label.append(slider);node("eqBands").append(label);
});
node("eqEnabled").addEventListener("change",()=>{startAudioGraph();applyEq();});
node("preamp").addEventListener("input",applyEq);
node("autoEqFile").addEventListener("change",async event=>{
  try {const file=event.target.files[0];if(!file)return;if(file.size>1024*1024)throw new Error("Профиль больше 1 МБ.");
    const raw=await file.text();
    if(/^\s*(Preamp:|Filter\s)/mi.test(raw)){
      startAudioGraph();const profile=await api("/api/eq/profile",{method:"POST",body:{text:raw,sample_rate:graph?.context.sampleRate||48000}});
      node("eqEnabled").checked=true;node("preamp").value=String(Math.max(-24,Math.min(0,profile.preamp)));
      eqGains=profile.bands.map(b=>b.enabled?b.gain:0);applyEq();
      graph.filters.forEach((filter,i)=>{const band=profile.bands[i];filter.frequency.value=band.frequency;filter.Q.value=band.q;filter.type={Peak:"peaking",LowShelf:"lowshelf",HighShelf:"highshelf"}[band.kind];});
      text("settingsStatus","Параметрический профиль применён.");return;
    }
    const profile=PumpUX.autoEq(raw);eqGains=profile.gains;node("preamp").value=String(profile.preamp);node("eqEnabled").checked=true;
    node("eqBands").querySelectorAll("input").forEach((slider,i)=>slider.value=String(eqGains[i]));startAudioGraph();applyEq();text("settingsStatus","Профиль применён.");
  }catch(error){text("settingsStatus",error.message);}
});
node("rateSelect").addEventListener("change",()=>{if(!isGuest())setRate(node("rateSelect").value);});
node("designSelect").addEventListener("change",()=>{
  node("artBox").classList.remove(...styles);style=styles.indexOf(node("designSelect").value);node("artBox").classList.add(styles[style]);
});
node("settingsSearch").addEventListener("input",()=>{
  for(const section of node("settingsCategories").children)section.hidden=!PumpUX.matches(node("settingsSearch").value,section.textContent+" "+section.dataset.category);
});
function updatePalette(){const rgb=PumpUX.palette(cover);if(rgb&&node("dynamicPalette").checked)document.documentElement.style.setProperty("--cover-rgb",rgb);}
cover.addEventListener("load",updatePalette);node("dynamicPalette").addEventListener("change",updatePalette);
node("lyricsBlur").addEventListener("change",()=>node("lyricsBox").classList.toggle("lyrics-blur",node("lyricsBlur").checked));
node("lyricsGlow").addEventListener("change",()=>node("lyricsBox").classList.toggle("lyrics-glow",node("lyricsGlow").checked));
node("splitView").addEventListener("change",()=>document.body.classList.toggle("split-view",node("splitView").checked));
node("fontSelect").addEventListener("change",()=>{const families={system:"inherit",serif:"Georgia, serif",mono:"monospace",custom:"PumpCustom"};node("lyricsBox").style.fontFamily=families[node("fontSelect").value];});
node("fontFile").addEventListener("change",async event=>{
 try{const file=event.target.files[0];if(!file)return;if(file.size>8*1024*1024)throw new Error("Шрифт больше 8 МБ.");const font=new FontFace("PumpCustom",await file.arrayBuffer());await font.load();document.fonts.add(font);node("fontSelect").querySelector('[value="custom"]').disabled=false;node("fontSelect").value="custom";node("lyricsBox").style.fontFamily="PumpCustom";}catch(error){text("settingsStatus",error.message);}
});
async function refreshOutputs(){
 try{if(typeof audio.setSinkId!=="function"||!navigator.mediaDevices?.enumerateDevices)throw new Error("Выбор аудиовыхода недоступен в этом браузере.");
 if(navigator.mediaDevices.selectAudioOutput){const device=await navigator.mediaDevices.selectAudioOutput();await audio.setSinkId(device.deviceId);}
 const devices=await navigator.mediaDevices.enumerateDevices();node("outputDevice").replaceChildren(...devices.filter(d=>d.kind==="audiooutput").map(d=>{const option=element("option","",d.label||"Аудиовыход");option.value=d.deviceId;return option;}));text("outputStatus","");
 }catch(error){text("outputStatus",error.message);}
}
node("outputDevice").addEventListener("change",()=>audio.setSinkId?.(node("outputDevice").value).catch(error=>text("outputStatus",error.message)));
async function showDownloads(){
 const records=await PumpUX.transaction("downloads","getAll");node("downloadsList").replaceChildren(...records.map(record=>{
  const button=element("button","btn chip",record.track.title);button.addEventListener("click",()=>{if(isGuest())return;const url=URL.createObjectURL(record.blob);selectTrack({...record.track,audio_url:null});audio.src=url;audio.addEventListener("emptied",()=>URL.revokeObjectURL(url),{once:true});requestPlay();settings(false);});return button;
 }));
}
document.addEventListener("click",async event=>{
 const action=event.target.closest("[data-action]")?.dataset.action;
 try{
  if(action==="eq-reset"){eqGains.fill(0);node("preamp").value="0";node("eqBands").querySelectorAll("input").forEach(input=>input.value="0");applyEq();}
  if(action==="output-refresh")await refreshOutputs();
  if(action==="download"){if(!selected||!audio.src)throw new Error("Выбери аудиотрек.");text("offlineStatus","Скачиваем…");await PumpUX.download(selected,audio.src);await showDownloads();text("offlineStatus","Запись доступна на этом устройстве.");}
  if(action==="offline-enable"){if(!navigator.serviceWorker)throw new Error("Офлайн-интерфейс недоступен.");await navigator.serviceWorker.register("/sw.js");text("offlineStatus","Офлайн-интерфейс включён.");await showDownloads();}
  if(action==="history-clear"){persist("pump.history",[]);text("settingsStatus","История очищена.");}
  if(action==="show-history"){results=stored("pump.history",[]);renderResults("История прослушиваний");settings(false);goTab("search");}
 }catch(error){text("settingsStatus",error.message);text("offlineStatus",error.message);}
});
audio.addEventListener("playing",()=>{
 if(!selected)return;const history=stored("pump.history",[]).filter(track=>track.id!==selected.id);history.unshift(selected);persist("pump.history",history.slice(0,100));
 if(navigator.mediaSession){navigator.mediaSession.metadata=new MediaMetadata({title:selected.title,artist:selected.artist,artwork:asset(selected.artwork)?[{src:asset(selected.artwork)}]:[]});}
});
if(navigator.mediaSession){for(const [action,handler] of Object.entries({play:()=>requestPlay(),pause:()=>audio.pause(),nexttrack:()=>step(1),previoustrack:()=>step(-1),seekto:event=>{if(!isGuest()&&event)audio.currentTime=event.seekTime;}})){try{navigator.mediaSession.setActionHandler(action,event=>{if(!isGuest())handler(event);});}catch{}}}
function connectControls(){
 if(localOnly)return;
 if(remoteSocket&&remoteSocket.readyState<=1)return;
 const url=new URL("/ws/player",location.href);url.protocol=location.protocol==="https:"?"wss:":"ws:";
 const socket=new WebSocket(url);remoteSocket=socket;
 socket.addEventListener("open",()=>socket.send(JSON.stringify({type:"join",token})));
 socket.addEventListener("close",()=>{if(remoteSocket===socket){remoteSocket=null;clearTimeout(reconnectTimer);reconnectTimer=setTimeout(connectControls,5000);}});
 socket.addEventListener("message",async event=>{try{
  const command=JSON.parse(event.data);if(command.type!=="command"||isGuest())return;
  if(command.track_id){const track=known.get(command.track_id)||await api("/api/tracks/"+encodeURIComponent(command.track_id));queue.push(track);if(command.action==="play"){index=queue.length-1;selectTrack(track);}}
  switch(command.action){case"play":requestPlay();break;case"pause":audio.pause();break;case"next":step(1);break;case"previous":step(-1);break;case"seek":if(Number.isFinite(audio.duration))audio.currentTime=Math.min(command.position_ms/1000,audio.duration);break;case"eq":if(Array.isArray(command.bands)&&command.bands.length===10){eqGains=command.bands;node("eqEnabled").checked=true;startAudioGraph();applyEq();node("eqBands").querySelectorAll("input").forEach((input,i)=>input.value=String(eqGains[i]));}break;}
 }catch(error){text("playerStatus",error.message);}});
}
showDownloads().catch(()=>{});

async function loadStreams(){
 const track=selected,version=++streamSelection;text("playerStatus","Получаем форматы…");
 try{const data=await api("/api/streams/"+encodeURIComponent(track.id)+"?mode="+node("streamMode").value+"&quality=balanced");if(version!==streamSelection||selected!==track)return;streamCandidates=data.streams;streamIndex=0;playStreamCandidate();}
 catch(error){if(version===streamSelection)text("playerStatus",error.message);}
}
function playStreamCandidate(){
 const stream=streamCandidates[streamIndex];if(!stream)return;
 const video=node("streamMode").value==="video";const target=video?streamVideo:audio;
 if(target.getAttribute("src")!==stream.url){audio.pause();streamVideo.pause();target.src=stream.url;}
 streamVideo.hidden=!video;node("artBox").classList.toggle("video",video);
 target.play().then(()=>text("playerStatus","")).catch(error=>{if(error.name!=="AbortError")text("playerStatus","Нажми Play, чтобы начать воспроизведение.");});
}
function fallbackStream(){if(selected?.kind!=="youtube")return;if(++streamIndex<streamCandidates.length){playStreamCandidate();}else{text("playerStatus","Доступные форматы закончились. Можно открыть YouTube.");streamCandidates=[];}}
audio.addEventListener("error",fallbackStream);streamVideo.addEventListener("error",fallbackStream);
node("streamMode").addEventListener("change",()=>{streamCandidates=[];audio.pause();streamVideo.pause();loadStreams();});
document.addEventListener("click",event=>{if(event.target.closest('[data-action="youtube-embed"]')&&selected?.video_id){audio.pause();streamVideo.pause();streamVideo.hidden=true;frame.src="https://www.youtube-nocookie.com/embed/"+selected.video_id+"?autoplay=1";frame.hidden=false;mountedVideo=selected.id;node("artBox").classList.add("video");updateButtons();}});
function animateLyrics(){if(currentTab==="lyrics"||document.body.classList.contains("split-view")){syncLyrics();const time=audio.currentTime*1000;for(const word of node("lyricsBox").querySelectorAll(".lyric-word")){word.classList.toggle("current",time>=Number(word.dataset.start)&&time<Number(word.dataset.end));word.classList.toggle("sung",time>=Number(word.dataset.end));}}requestAnimationFrame(animateLyrics);}
requestAnimationFrame(animateLyrics);

node("localOnly").checked=localOnly;
node("localOnly").addEventListener("change",()=>{localOnly=node("localOnly").checked;persist("pump.localOnly",localOnly);if(localOnly){leaveRoom();remoteSocket?.close();audio.pause();audio.removeAttribute("src");audio.load();clearVideo();}else session().then(connectControls).catch(()=>{});search().catch(error=>text("settingsStatus",error.message));});

let canvasUrl=null;
node("canvasFile").addEventListener("change",async event=>{try{const file=event.target.files[0];if(!file)return;if(!["video/mp4","video/webm"].includes(file.type)||file.size>32*1024*1024)throw new Error("Для Canvas нужен MP4/WebM до 32 МБ.");if(canvasUrl)URL.revokeObjectURL(canvasUrl);canvasUrl=URL.createObjectURL(file);const video=node("canvasVideo");video.src=canvasUrl;video.hidden=false;await video.play();}catch(error){text("settingsStatus",error.message);}});
node("iconPreset").addEventListener("change",()=>{document.body.dataset.icons=node("iconPreset").value;persist("pump.iconPreset",node("iconPreset").value);});
document.body.dataset.icons=stored("pump.iconPreset","default");node("iconPreset").value=document.body.dataset.icons;

document.addEventListener("click",async event=>{if(event.target.closest('[data-action="lastfm-connect"]')){try{await session();await api("/api/lastfm/connect",{method:"POST",auth:true,body:{session_key:node("lastfmKey").value}});node("lastfmKey").value="";lastfmConnected=true;text("settingsStatus","Last.fm подключён к текущей сессии.");}catch(error){text("settingsStatus",error.message);}}});

document.addEventListener("click",async event=>{
 const action=event.target.closest("[data-action]")?.dataset.action;
 if(!["spotify-connect","playlist-import"].includes(action))return;
 try{
  await session();
  if(action==="spotify-connect"){
   const result=await api("/api/spotify/authorize",{method:"POST",auth:true});
   const url=new URL(result.url);if(url.protocol!=="https:"||url.hostname!=="accounts.spotify.com")throw new Error("Неверный адрес авторизации.");
   const link=element("a","","Авторизоваться в Spotify");link.href=url.href;link.target="_blank";link.rel="noopener";node("settingsStatus").replaceChildren(link);return;
  }
  const link=node("importLink").value.trim();const url=new URL(link);
  const source=url.hostname==="open.spotify.com"?"spotify":url.hostname==="music.apple.com"?"apple":null;
  if(!source)throw new Error("Нужна ссылка Spotify или Apple Music.");
  text("settingsStatus","Переносим треки…");const imported=await api("/api/import",{method:"POST",auth:true,body:{source,link}});
  const matched=imported.filter(track=>track.resolution.status==="matched");
  const tracks=[];for(const track of matched)tracks.push(await api("/api/tracks/"+encodeURIComponent(track.resolution.id)));
  if(tracks.length){queue=tracks;index=0;selectTrack(queue[0]);renderQueue();}
  text("settingsStatus","Добавлено в очередь: "+tracks.length+". Без однозначного совпадения: "+(imported.length-tracks.length)+".");
 }catch(error){text("settingsStatus",error.message);}
});
