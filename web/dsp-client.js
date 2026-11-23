// Этот модуль владеет только DSP-веткой. Элемент audio, destination и API плеера — у приложения.
const FREQUENCIES = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
const FIELDS = new Set(["eqEnabled", "preampDb", "gains", "bands", "concertMode", "wet",
  "decaySeconds", "damping", "pitchSemitones"]);
const loadedWorklets = new WeakMap();
const activeSources = new WeakSet();

function number(value, min, max, label) {
  if (typeof value !== "number" || !Number.isFinite(value) || value < min || value > max) {
    throw new RangeError(label + ": допустимо " + min + "…" + max);
  }
  return value;
}

function boolean(value, label) {
  if (typeof value !== "boolean") throw new TypeError(label + ": требуется boolean");
  return value;
}

export function defaultDspSettings(sampleRate) {
  number(sampleRate, 8000, 192000, "sampleRate");
  return {
    eqEnabled:false, preampDb:0, concertMode:false, wet:0.15, decaySeconds:1.2,
    damping:0.4, pitchSemitones:0,
    bands:FREQUENCIES.map(frequency => ({kind:"Peak", frequency:Math.min(frequency, sampleRate * 0.45), gain:0, q:1.4, enabled:true}))
  };
}

// Проверка на основном потоке дублируется Rust: ошибочные настройки не попадают в аудиосостояние.
export function normalizeDspSettings(patch, current, sampleRate) {
  if (!patch || typeof patch !== "object" || Array.isArray(patch)) throw new TypeError("Нужен объект DSP settings.");
  for (const field of Object.keys(patch)) if (!FIELDS.has(field)) throw new TypeError("Неизвестная настройка DSP: " + field);
  if (patch.gains !== undefined && patch.bands !== undefined) throw new TypeError("Передайте gains или bands.");
  const next = {...current, ...patch};
  if (patch.gains !== undefined) {
    if (!Array.isArray(patch.gains) || patch.gains.length !== 10) throw new TypeError("Нужно 10 EQ gains.");
    next.bands = current.bands.map((band, i) => ({...band, gain:patch.gains[i]}));
  }
  delete next.gains;
  boolean(next.eqEnabled, "eqEnabled");
  boolean(next.concertMode, "concertMode");
  number(next.preampDb, -60, 24, "preampDb");
  number(next.wet, 0, 1, "wet");
  number(next.decaySeconds, 0.1, 10, "decaySeconds");
  number(next.damping, 0, 0.95, "damping");
  number(next.pitchSemitones, -12, 12, "pitchSemitones");
  if (!Array.isArray(next.bands) || next.bands.length !== 10) throw new TypeError("Нужно 10 EQ bands.");
  next.bands = Array.from(next.bands, band => {
    if (!band || !["Peak", "LowShelf", "HighShelf"].includes(band.kind)) throw new TypeError("Неверный EQ kind.");
    return {kind:band.kind, frequency:number(band.frequency, 10, sampleRate * 0.499, "frequency"),
      gain:number(band.gain, -24, 24, "gain"), q:number(band.q, 0.1, 20, "q"), enabled:boolean(band.enabled, "band.enabled")};
  });
  return next;
}

export function encodeDspSettings(settings) {
  const values = new Float64Array(57);
  values.set([Number(settings.eqEnabled), settings.preampDb, Number(settings.concertMode),
    settings.wet, settings.decaySeconds, settings.damping, settings.pitchSemitones]);
  settings.bands.forEach((band, i) => values.set([
    ["Peak", "LowShelf", "HighShelf"].indexOf(band.kind), band.frequency, band.gain, band.q, Number(band.enabled)
  ], 7 + i * 5));
  return values;
}

function loadWorklet(context, url) {
  let modules = loadedWorklets.get(context);
  if (!modules) loadedWorklets.set(context, modules = new Map());
  if (!modules.has(url)) {
    modules.set(url, context.audioWorklet.addModule(url).catch(error => { modules.delete(url); throw error; }));
  }
  return modules.get(url);
}

/**
 * source — уже созданный AudioNode в context. Не создаёт MediaElementSource и не подключает destination.
 * Успех: source → worklet → output; fallback: available=false, output=source, исходные связи не тронуты.
 * configure(patch) и reset() возвращают Promise с подтверждением аудиопотока; destroy()/dispose() освобождают узлы.
 * options: wasmUrl, workletUrl, timeoutMs (100..30000), settings, onError(Error), onMeter({rms}).
 * Скорость задаёт вызывающий код: audio.preservesPitch=true; audio.playbackRate=0.5..2.
 */
export async function createRustDsp(context, source, options = {}) {
  let node = null, output = null, connected = false, disposed = false, available = false, failure = null;
  let id = 0, desired;
  const pending = new Map();
  const fallback = error => ({available:false, input:source, output:source, error,
    configure:async () => { throw error; }, reset:async () => {}, dispose:() => {}, destroy:() => {}});
  const timeoutMs = options.timeoutMs ?? 5000;
  const notify = (name, value) => {
    try { options[name]?.(value); } catch { /* Ошибка UI callback не выключает звук. */ }
  };
  function rejectPending(error) {
    for (const item of pending.values()) { clearTimeout(item.timer); item.reject(error); }
    pending.clear();
  }
  function fail(error) {
    if (disposed || failure) return;
    failure = error;
    available = false;
    rejectPending(error);
    if (connected) {
      // Стабильный output сохраняет связи с destination и анализаторами приложения.
      try { source.disconnect(node); } catch {}
      try { node.disconnect(); } catch {}
      source.connect(output);
      node.port.postMessage({type:"dispose"});
    }
    notify("onError", error);
  }
  function command(type, values) {
    if (disposed || failure || !available) return Promise.reject(failure || new Error("DSP недоступен."));
    if (pending.size >= 64) return Promise.reject(new Error("Слишком много ожидающих команд DSP."));
    const key = ++id;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => fail(new Error("Нет подтверждения команды DSP: " + type)), timeoutMs);
      pending.set(key, {resolve, reject, timer});
      try { node.port.postMessage({type, id:key, values}); }
      catch (error) { fail(error); }
    });
  }
  try {
    number(timeoutMs, 100, 30000, "timeoutMs");
    if (!context || !source || source.context !== context || typeof source.connect !== "function") {
      throw new TypeError("Нужен AudioNode из указанного AudioContext.");
    }
    if (activeSources.has(source)) throw new Error("DSP для этого источника уже создан.");
    if (!context.audioWorklet || typeof globalThis.AudioWorkletNode !== "function" || typeof WebAssembly !== "object") {
      throw new Error("Браузер не поддерживает AudioWorklet/WASM; используйте обычный WebAudio EQ.");
    }
    desired = normalizeDspSettings(options.settings || {}, defaultDspSettings(context.sampleRate), context.sampleRate);
    const workletUrl = options.workletUrl || "/assets/dsp-worklet.js";
    const wasmUrl = options.wasmUrl || "/assets/ui-liquid-glass/pkg/pump_dsp.wasm";
    // Резервируем источник до первого await, чтобы два вызова не удвоили сигнал.
    activeSources.add(source);
    const abort = new AbortController();
    let loadTimer;
    let module;
    try {
      module = await Promise.race([
        Promise.all([
          loadWorklet(context, workletUrl),
          fetch(wasmUrl, {signal:abort.signal, credentials:"same-origin"}).then(async response => {
            if (!response.ok) throw new Error("DSP WASM: HTTP " + response.status);
            return WebAssembly.compile(await response.arrayBuffer());
          })
        ]).then(([, module]) => module),
        new Promise((_, reject) => { loadTimer = setTimeout(() => reject(new Error("Истекло время загрузки DSP.")), timeoutMs); })
      ]);
    } finally {
      clearTimeout(loadTimer);
      abort.abort();
    }
    node = new AudioWorkletNode(context, "pump-rust-dsp", {
      numberOfInputs:1, numberOfOutputs:1, outputChannelCount:[2],
      channelCount:2, channelCountMode:"explicit", channelInterpretation:"speakers",
      processorOptions:{module}
    });
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("DSP не подтвердил готовность.")), timeoutMs);
      node.onprocessorerror = () => { clearTimeout(timer); reject(new Error("Ошибка инициализации AudioWorklet.")); };
      node.port.onmessage = event => {
        const data = event.data;
        if (data?.type === "ready") { clearTimeout(timer); resolve(); }
        else if (data?.type === "fatal") { clearTimeout(timer); reject(new Error(data.message)); }
      };
      node.port.start();
    });
    available = true;
    node.onprocessorerror = () => fail(new Error("AudioWorklet остановлен браузером; включён обход DSP."));
    node.port.onmessage = event => {
      const data = event.data;
      if (data?.type === "fatal") { fail(new Error(data.message)); return; }
      if (data?.type === "meter") { notify("onMeter", {rms:data.rms}); return; }
      const item = pending.get(data?.id);
      if (!item) return;
      pending.delete(data.id);
      clearTimeout(item.timer);
      if (data.type === "ack") item.resolve();
      else item.reject(new Error(data.message || "Ошибка команды DSP."));
    };
    await command("configure", encodeDspSettings(desired));
    if (failure) throw failure;
    output = context.createGain();
    node.connect(output);
    source.connect(node);
    connected = true;
    function dispose() {
      if (disposed) return;
      disposed = true;
      available = false;
      rejectPending(new Error("DSP освобождён."));
      try { source.disconnect(failure ? output : node); } catch {}
      node.disconnect();
      output.disconnect();
      node.port.postMessage({type:"dispose"});
      node.port.close();
      activeSources.delete(source);
    }
    return {
      get available() { return available; },
      get error() { return failure; },
      input:source, output,
      configure(patch) {
        try {
          if (pending.size >= 64) throw new Error("Слишком много ожидающих команд DSP.");
          const next = normalizeDspSettings(patch, desired, context.sampleRate);
          const result = command("configure", encodeDspSettings(next));
          // Очередь сообщений упорядочена: последующие partial patches видят уже запрошенное состояние.
          if (!disposed && !failure) desired = next;
          return result;
        } catch (error) { return Promise.reject(error); }
      },
      reset:() => command("reset"),
      dispose, destroy:dispose
    };
  } catch (error) {
    available = false;
    rejectPending(error);
    if (connected) { try { source.disconnect(node); } catch {} }
    if (node) {
      node.disconnect();
      node.port.postMessage({type:"dispose"});
      node.port.close();
    }
    output?.disconnect();
    // Не снимаем чужую резервацию при повторном вызове для уже занятого source.
    if (desired) activeSources.delete(source);
    notify("onError", error);
    return fallback(error);
  }
}
