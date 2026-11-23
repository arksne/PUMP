// node --test web/dsp.test.mjs
// DSP_WASM=web/pkg/pump_dsp.wasm node --test web/dsp.test.mjs — также настоящий WASM/AudioWorklet ABI.
import test from "node:test";
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import vm from "node:vm";
import {createRustDsp, defaultDspSettings, normalizeDspSettings, encodeDspSettings} from "./dsp-client.js";

test("настройки: низкая частота, partial patches и параметрический AutoEq", () => {
  const defaults = defaultDspSettings(8000);
  assert.equal(defaults.bands[9].frequency, 3600);
  const next = normalizeDspSettings({eqEnabled:true, gains:Array(10).fill(-3), pitchSemitones:12}, defaults, 8000);
  assert.equal(next.bands[0].gain, -3);
  assert.equal(defaults.bands[0].gain, 0);
  const profile = next.bands.map(b => ({...b, kind:"LowShelf", q:2}));
  const autoeq = normalizeDspSettings({bands:profile, preampDb:-6}, next, 8000);
  profile[0].gain = 20;
  assert.equal(autoeq.bands[0].gain, -3);
  assert.equal(autoeq.pitchSemitones, 12);
  const encoded = encodeDspSettings(autoeq);
  assert.equal(encoded.length, 57);
  assert.deepEqual(Array.from(encoded.slice(0, 7)), [1, -6, 0, 0.15, 1.2, 0.4, 12]);
  assert.deepEqual(Array.from(encoded.slice(7, 12)), [1, 31, -3, 2, 1]);
});

test("неверные настройки отклоняются до передачи аудиопотоку", () => {
  const defaults = defaultDspSettings(48000);
  for (const patch of [{pitchSemitones:13}, {pitchSemitones:NaN}, {wet:Infinity}, {eqEnabled:1},
    {gains:[1]}, {gains:Array(10).fill("1")}, {gains:Array(10).fill(0), bands:defaults.bands},
    {bands:Array(10)}, {bands:defaults.bands.map(b => ({...b, frequency:24000}))}, {speed:2}]) {
    assert.throws(() => normalizeDspSettings(patch, defaults, 48000));
  }
});

test("отсутствие AudioWorklet оставляет источник нетронутым и сообщает об ошибке", async () => {
  const context = {sampleRate:48000};
  let connects = 0, errors = 0;
  const source = {context, connect() { connects++; }};
  const dsp = await createRustDsp(context, source, {onError() { errors++; }});
  assert.equal(dsp.available, false);
  assert.equal(dsp.output, source);
  assert.equal(connects, 0);
  assert.equal(errors, 1);
  await assert.rejects(dsp.configure({concertMode:true}), /AudioWorklet/);
  dsp.destroy();
});

let wasmModule;
if (process.env.DSP_WASM) wasmModule = await WebAssembly.compile(await readFile(process.env.DSP_WASM));
const workletSource = await readFile(new URL("./dsp-worklet.js", import.meta.url), "utf8");

function worklet(module = wasmModule) {
  let Processor;
  const messages = [];
  const context = vm.createContext({WebAssembly, Float32Array, Float64Array, sampleRate:48000,
    AudioWorkletProcessor:class {
      constructor() { this.port = {postMessage(message) { messages.push(message); }}; }
    },
    registerProcessor(name, cls) { assert.equal(name, "pump-rust-dsp"); Processor = cls; }
  });
  vm.runInContext(workletSource, context);
  return {node:new Processor({processorOptions:{module}}), messages};
}

test("worklet не выдаёт успех для отсутствующего DSP ABI", () => {
  const emptyWasm = new WebAssembly.Module(new Uint8Array([0,97,115,109,1,0,0,0]));
  const {node, messages} = worklet(emptyWasm);
  assert.equal(messages[0].type, "fatal");
  const output = [[new Float32Array(128), new Float32Array(128)]];
  const input = new Float32Array(128).fill(0.25);
  assert.equal(node.process([[input]], output), true);
  assert.equal(output[0][0][10], 0.25);
  assert.equal(output[0][1][10], 0.25);
  node.command({type:"dispose", id:4});
  assert.equal(node.process([[input]], output), false);
});

test("настоящий WASM: EQ меняет PCM, mono/stereo, RMS, подтверждение configure", {skip:!wasmModule}, () => {
  const {node, messages} = worklet();
  assert.equal(messages[0].type, "ready");
  const settings = normalizeDspSettings({eqEnabled:true, gains:Array(10).fill(0), preampDb:-6}, defaultDspSettings(48000), 48000);
  node.command({type:"configure", id:1, values:encodeDspSettings(settings)});
  assert.equal(messages.at(-1).type, "ack");
  const input = new Float32Array(128).fill(0.5);
  const output = [[new Float32Array(128), new Float32Array(128)]];
  for (let i = 0; i < 40; i++) node.process([[input]], output);
  assert.ok(Math.abs(output[0][0][64] - 0.5 * 10 ** (-6 / 20)) < 1e-6);
  assert.equal(output[0][0][64], output[0][1][64]);
  assert.ok(messages.some(m => m.type === "meter" && m.rms[0] > 0.2));
  const invalid = encodeDspSettings(settings);
  invalid[6] = NaN;
  node.command({type:"configure", id:2, values:invalid});
  assert.equal(messages.at(-1).type, "error");
  node.process([[input]], output);
  assert.ok(Math.abs(output[0][0][64] - 0.5 * 10 ** (-6 / 20)) < 1e-6);
  // Фильтр должен ослабить синус на своей центральной частоте на 12 dB.
  settings.preampDb = 0;
  settings.bands[5].gain = -12;
  node.command({type:"configure", id:3, values:encodeDspSettings(settings)});
  let energy = 0;
  for (let block = 0; block < 100; block++) {
    for (let i = 0; i < 128; i++) input[i] = 0.5 * Math.sin(2 * Math.PI * 1000 * (block * 128 + i) / 48000);
    node.process([[input]], output);
    if (block > 50) for (const sample of output[0][0]) energy += sample * sample;
  }
  const rms = Math.sqrt(energy / (49 * 128));
  assert.ok(rms > 0.08 && rms < 0.1, "фактический EQ RMS=" + rms);
});

test("настоящий WASM: Concert tail, reset, лимит блока, dispose", {skip:!wasmModule}, () => {
  const {node, messages} = worklet();
  node.command({type:"configure", id:1, values:encodeDspSettings(normalizeDspSettings(
    {concertMode:true, wet:1}, defaultDspSettings(48000), 48000))});
  const input = new Float32Array(128);
  input[0] = 1;
  const output = [[new Float32Array(128), new Float32Array(128)]];
  node.process([[input]], output);
  let energy = 0;
  for (let i = 0; i < 30; i++) {
    node.process([[]], output);
    for (const sample of output[0][0]) energy += sample * sample;
  }
  assert.ok(energy > 0.005);
  node.command({type:"reset", id:2});
  node.process([[]], output);
  assert.ok(output[0][0].every(x => x === 0));
  const huge = [[new Float32Array(2049), new Float32Array(2049)]];
  node.process([[]], huge);
  assert.equal(messages.at(-1).type, "fatal");
  node.command({type:"dispose", id:3});
  assert.equal(node.process([[]], output), false);
  const fresh = worklet();
  fresh.node.command({type:"dispose", id:3});
  assert.equal(fresh.node.process([[]], output), false);
});
