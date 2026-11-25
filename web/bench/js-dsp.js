// JS-зеркало Rust EQ 1:1 (crates/audio-dsp-engine/src/eq.rs):
// preamp (f64) -> 10 биквадов RBJ transposed-DF2 с f64-состоянием -> f32 выход.
// Любое расхождение с WASM сверх допуска — баг зеркала или ABI, а не «погрешность».
const FREQUENCIES_FALLBACK = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];

function rbj(kind, frequency, gainDb, q, rate) {
  const a = Math.pow(10, gainDb / 40);
  const omega = 2 * Math.PI * frequency / rate;
  const c = Math.cos(omega);
  const alpha = Math.sin(omega) / (2 * q);
  const beta = 2 * Math.sqrt(a) * alpha;
  let b, d;
  if (kind === 0) {
    b = [1 + alpha * a, -2 * c, 1 - alpha * a];
    d = [1 + alpha / a, -2 * c, 1 - alpha / a];
  } else if (kind === 1) {
    b = [a * ((a + 1) - (a - 1) * c + beta), 2 * a * ((a - 1) - (a + 1) * c), a * ((a + 1) - (a - 1) * c - beta)];
    d = [(a + 1) + (a - 1) * c + beta, -2 * ((a - 1) + (a + 1) * c), (a + 1) + (a - 1) * c - beta];
  } else {
    b = [a * ((a + 1) + (a - 1) * c + beta), -2 * a * ((a - 1) + (a + 1) * c), a * ((a + 1) + (a - 1) * c - beta)];
    d = [(a + 1) - (a - 1) * c + beta, 2 * ((a - 1) - (a + 1) * c), (a + 1) - (a - 1) * c - beta];
  }
  // Нормировка a0 = 1, как в Biquad::from_band.
  const b0 = b.map(v => v / d[0]);
  return { b: b0, a: [d[1] / d[0], d[2] / d[0]], z: [[0, 0], [0, 0]] };
}

function tick(filter, x, ch) {
  const y = filter.b[0] * x + filter.z[ch][0];
  filter.z[ch][0] = filter.b[1] * x - filter.a[0] * y + filter.z[ch][1];
  filter.z[ch][1] = filter.b[2] * x - filter.a[1] * y;
  return y;
}

export function makeJsDsp(rate, config) {
  if (config.length !== 57) throw new RangeError("config_len must be 57");
  const enabled = config[0] === 1;
  const gain = Math.pow(10, (enabled ? config[1] : 0) / 20);
  const filters = [];
  for (let i = 0; i < 10; i++) {
    const o = 7 + i * 5;
    const active = enabled && config[o + 4] === 1 && config[o + 2] !== 0;
    // Неактивная полоса — identity, как Biquad::from_band.
    filters.push(active
      ? rbj(config[o], config[o + 1], config[o + 2], config[o + 3], rate)
      : { b: [1, 0, 0], a: [0, 0], z: [[0, 0], [0, 0]] });
  }
  return {
    // input/output: Float32Array interleaved, frames <= 2048.
    processInto(input, output, frames) {
      for (let i = 0; i < frames; i++) {
        for (let ch = 0; ch < 2; ch++) {
          let v = input[i * 2 + ch] * gain;
          for (const f of filters) v = tick(f, v, ch);
          output[i * 2 + ch] = Math.fround(v);
        }
      }
    },
    reset() {
      for (const f of filters) { f.z = [[0, 0], [0, 0]]; }
    },
  };
}
