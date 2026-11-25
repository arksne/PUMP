// Детерминированный вход и конфигурация бенчмарка. Никакой сети, один seed.
export const SAMPLE_RATE = 48000;
export const BLOCK_FRAMES = 128;
export const BLOCKS_PER_BATCH = 256;
export const BATCHES = 15;
export const WARMUP_BATCHES = 3;

// Ненулевой EQ, чтобы измерять вычисления, а не проход identity:
// preamp +3 дБ, смесь Peak/LowShelf/HighShelf, Q 0.7..2.0.
export function benchConfig() {
  const values = new Float64Array(57);
  values.set([1, 3, 0, 0.15, 1.2, 0.4, 0]);
  const bands = [
    [1, 31, 2, 0.7, 1], [0, 62, -3, 1.4, 1], [0, 125, 4, 1.0, 1],
    [0, 250, -2, 2.0, 1], [0, 500, 5, 0.7, 1], [0, 1000, -4, 1.4, 1],
    [0, 2000, 3, 1.0, 1], [0, 4000, -6, 2.0, 1], [0, 8000, 2, 0.7, 1],
    [2, 16000, -3, 1.4, 1],
  ];
  bands.forEach((band, i) => values.set(band, 7 + i * 5));
  return values;
}

// xorshift32: воспроизводимый псевдошум ±0.5, интерливинг LRLR.
export function benchInput(frames, seed = 0xC0FFEE) {
  const out = new Float32Array(frames * 2);
  let state = seed >>> 0 || 1;
  for (let i = 0; i < out.length; i++) {
    state ^= state << 13; state >>>= 0;
    state ^= state >>> 17; state ^= state << 5; state >>>= 0;
    out[i] = (state / 4294967296 - 0.5);
  }
  return out;
}
