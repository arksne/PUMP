// Замер JS vs WASM на одном ядре: одинаковый EQ-каскад, одинаковый вход.
// Режимы: "core" (без копирования, чистые ядра) и "full" (с interleave-копированием,
// как цикл dsp-worklet.js:111-122 — цена границы JS/WASM).
import { makeJsDsp } from "./js-dsp.js";
import { benchConfig, benchInput, BLOCK_FRAMES, BLOCKS_PER_BATCH, BATCHES, WARMUP_BATCHES } from "./fixtures.js";

function stats(samples) {
  const sorted = [...samples].sort((a, b) => a - b);
  const at = q => sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))];
  const mean = samples.reduce((a, b) => a + b, 0) / samples.length;
  return { mean, p50: at(0.5), p95: at(0.95), p99: at(0.99), min: sorted[0], max: sorted[sorted.length - 1] };
}

async function loadWasm(wasmUrl, sampleRate) {
  const bytes = await (await fetch(wasmUrl)).arrayBuffer();
  const t0 = performance.now();
  const module = await WebAssembly.compile(bytes);
  const compileMs = performance.now() - t0;
  const imports = Object.create(null);
  let instance;
  for (const item of WebAssembly.Module.imports(module)) {
    if (item.kind !== "function" || item.name !== "__wbindgen_init_externref_table") {
      throw new Error("Неподдерживаемый WASM import: " + item.module + "." + item.name);
    }
    imports[item.module] ??= Object.create(null);
    imports[item.module][item.name] = () => {
      const table = instance.exports.__wbindgen_externrefs;
      const offset = table.grow(4);
      table.set(0, undefined); table.set(offset, undefined);
      table.set(offset + 1, null); table.set(offset + 2, true); table.set(offset + 3, false);
    };
  }
  instance = await WebAssembly.instantiate(module, imports);
  const instantiateMs = performance.now() - t0 - compileMs;
  const w = instance.exports;
  for (const name of ["pump_dsp_abi_version", "pump_dsp_create", "pump_dsp_capacity",
    "pump_dsp_config_len", "pump_dsp_config_ptr", "pump_dsp_pcm_ptr",
    "pump_dsp_configure", "pump_dsp_process", "pump_dsp_destroy"]) {
    if (typeof w[name] !== "function") throw new Error("Нет экспорта " + name);
  }
  if (w.pump_dsp_abi_version() !== 1 || w.pump_dsp_config_len() !== 57) throw new Error("ABI mismatch");
  w.__wbindgen_start?.();
  if (w.pump_dsp_create(sampleRate) !== 0) throw new Error("create failed");
  if (w.pump_dsp_capacity() < BLOCK_FRAMES) throw new Error("capacity too small");
  return { instance, w, startupMs: { compileMs, instantiateMs } };
}

function viewFor(w) {
  return {
    pcm: new Float32Array(w.memory.buffer, w.pump_dsp_pcm_ptr(), w.pump_dsp_capacity() * 2),
    config: new Float64Array(w.memory.buffer, w.pump_dsp_config_ptr(), w.pump_dsp_config_len()),
  };
}

export async function runBench({ wasmUrl, sampleRate, config, throttleNote }) {
  const totalFrames = BLOCK_FRAMES * BLOCKS_PER_BATCH * BATCHES;
  const input = benchInput(BLOCK_FRAMES * BLOCKS_PER_BATCH);
  const { w, startupMs } = await loadWasm(wasmUrl, sampleRate);
  const memBefore = w.memory.buffer.byteLength;
  let views = viewFor(w);
  const refresh = () => { if (views.pcm.buffer !== w.memory.buffer) views = viewFor(w); };
  views.config.set(config);
  if (w.pump_dsp_configure() !== 0) throw new Error("configure failed");
  const js = makeJsDsp(sampleRate, config);
  const jsOut = new Float32Array(BLOCK_FRAMES * 2);

  // Разогрев: истории фильтров должны устаканиться до замера.
  for (let b = 0; b < WARMUP_BATCHES; b++) {
    js.processInto(input, jsOut, BLOCK_FRAMES);
    refresh();
    views.pcm.set(input.subarray(0, BLOCK_FRAMES * 2));
    if (w.pump_dsp_process(BLOCK_FRAMES) !== 0) throw new Error("warmup failed");
  }

  const coreJs = [], coreWasm = [], fullJs = [], fullWasm = [];
  let checksumJs = 0, checksumWasm = 0, sumSqJs = 0, sumSqWasm = 0, n = 0;
  const tmp = new Float32Array(BLOCK_FRAMES * 2);
  let firstOut = null;
  for (let b = 0; b < BATCHES; b++) {
    // Замер пачкой: один quantum 128 фреймов короче кванта таймера (100 мкс без изоляции).
    let t0 = performance.now();
    for (let k = 0; k < BLOCKS_PER_BATCH; k++) {
      const off = (k * BLOCK_FRAMES * 2) % input.length;
      js.processInto(input.subarray(off, off + BLOCK_FRAMES * 2), jsOut, BLOCK_FRAMES);
      for (let i = 0; i < BLOCK_FRAMES * 2; i++) {
        checksumJs += jsOut[i];
        sumSqJs += jsOut[i] * jsOut[i];
      }
    }
    coreJs.push((performance.now() - t0) * 1e6 / (BLOCK_FRAMES * BLOCKS_PER_BATCH));
    t0 = performance.now();
    for (let k = 0; k < BLOCKS_PER_BATCH; k++) {
      const off = (k * BLOCK_FRAMES * 2) % input.length;
      refresh();
      views.pcm.set(input.subarray(off, off + BLOCK_FRAMES * 2));
      if (w.pump_dsp_process(BLOCK_FRAMES) !== 0) throw new Error("wasm process failed");
      for (let i = 0; i < BLOCK_FRAMES * 2; i++) {
        checksumWasm += views.pcm[i];
        sumSqWasm += views.pcm[i] * views.pcm[i];
      }
    }
    coreWasm.push((performance.now() - t0) * 1e6 / (BLOCK_FRAMES * BLOCKS_PER_BATCH));
    n += BLOCK_FRAMES * BLOCKS_PER_BATCH;
    t0 = performance.now();
    for (let k = 0; k < BLOCKS_PER_BATCH; k++) {
      const off = (k * BLOCK_FRAMES * 2) % input.length;
      for (let i = 0; i < BLOCK_FRAMES; i++) {
        tmp[i * 2] = input[(off + i * 2) % input.length];
        tmp[i * 2 + 1] = input[(off + i * 2 + 1) % input.length];
      }
      js.processInto(tmp, jsOut, BLOCK_FRAMES);
      for (let i = 0; i < BLOCK_FRAMES * 2; i++) {
        checksumJs += jsOut[i];
        sumSqJs += jsOut[i] * jsOut[i];
        tmp[i] = jsOut[i];
      }
    }
    fullJs.push((performance.now() - t0) * 1e6 / (BLOCK_FRAMES * BLOCKS_PER_BATCH));
    if (b === 0) firstOut = Array.from(jsOut.slice(0, 4));
    t0 = performance.now();
    for (let k = 0; k < BLOCKS_PER_BATCH; k++) {
      const off = (k * BLOCK_FRAMES * 2) % input.length;
      refresh();
      for (let i = 0; i < BLOCK_FRAMES; i++) {
        views.pcm[i * 2] = input[(off + i * 2) % input.length];
        views.pcm[i * 2 + 1] = input[(off + i * 2 + 1) % input.length];
      }
      if (w.pump_dsp_process(BLOCK_FRAMES) !== 0) throw new Error("wasm process failed");
      for (let i = 0; i < BLOCK_FRAMES * 2; i++) {
        checksumWasm += views.pcm[i];
        sumSqWasm += views.pcm[i] * views.pcm[i];
        tmp[i] = views.pcm[i];
      }
    }
    fullWasm.push((performance.now() - t0) * 1e6 / (BLOCK_FRAMES * BLOCKS_PER_BATCH));
  }
  const memAfter = w.memory.buffer.byteLength;
  const rel = Math.abs(checksumJs - checksumWasm) / Math.max(1e-12, Math.abs(checksumJs));
  const ns = s => stats(s);
  const xr = nsPerFrame => (BLOCK_FRAMES / sampleRate) / (nsPerFrame * BLOCK_FRAMES / 1e9);
  const out = (arr) => ({ ns_per_frame: ns(arr), xrt_median: xr(ns(arr).p50) });
  return {
    engine: "eq10+preamp", abi: 1, config_len: 57, sample_rate: sampleRate,
    channels: 2, frames_per_block: BLOCK_FRAMES, blocks_per_batch: BLOCKS_PER_BATCH,
    batches: BATCHES, warmup_batches: WARMUP_BATCHES, throttle: throttleNote || "none",
    checksum_js: checksumJs, checksum_wasm: checksumWasm, checksum_rel_diff: rel,
    checksum_ok: Number.isFinite(rel) && rel < 1e-6,
    startup_ms: startupMs,
    mem_bytes_before: memBefore, mem_bytes_after: memAfter,
    mem_grew: memAfter !== memBefore,
    core: { js: out(coreJs), wasm: out(coreWasm) },
    full: { js: out(fullJs), wasm: out(fullWasm) },
  };
}

export { benchConfig, BLOCK_FRAMES };
