# Бенчмарк DSP: Vanilla JS vs Rust/WASM (1:1 EQ-каскад)

Сравнивает один и тот же вычислительный тракт — 10 полос RBJ transposed-DF2
+ preamp (см. `crates/audio-dsp-engine/src/eq.rs`) — в двух реализациях:
`web/bench/js-dsp.js` (точная копия формул) и `web/pkg/pump_dsp.wasm`
через числовой ABI v1 (`config_len=57`, `capacity=2048`).

## Что измеряется

- **core**: чистые ядра с симметричным копированием входного блока
  (JS — в свой `Float32Array`, WASM — в свой pcm-буфер через `.set`);
  сравнивается именно математика EQ, а не чтение subarray.
- **full**: с interleave-копированием L/R, как цикл `dsp-worklet.js:111-122`
  (цена границы JS/WASM).
- Метрики на блок 128 фреймов: `ns_per_frame` {mean,p50,p95,p99,min,max},
  `xRT` (во сколько раз быстрее realtime), `checksum` сходимости,
  `memory.buffer.byteLength` до/после (рост = баг).
- `performance.now()` клампится (100 мкс без изоляции, 5 мкс при COOP/COEP),
  поэтому меряем пачки по 256 блоков; перцентили — грубые, медиана — основная.
- В `AudioWorkletGlobalScope` тайминг не документирован — бенч идёт в page,
  не в worklet; это сравнение ядер, а не планировщика.

## Контракт результата (одна JSON-строка)

`{engine:"eq10+preamp", abi:1, config_len:57, sample_rate, channels:2,
frames_per_block:128, blocks_per_batch:256, batches:15, warmup_batches:3,
throttle, checksum_js, checksum_wasm, checksum_rel_diff, checksum_ok,
mem_bytes_before, mem_bytes_after, mem_grew, core:{js,wasm}, full:{js,wasm}}`,
где `js`/`wasm` = `{ns_per_frame:{...}, xrt_median}`.

## Пороги

- `checksum_ok` обязателен (`rel_diff < 1e-6`): расхождение — баг зеркала/ABI.
- `mem_grew == false`: рост Linear Memory за прогон запрещён.
- `wasm_js_ratio_ns_per_frame` и p99-пороги — НЕ выдумываются: фиксируются
  из 3 прогонов на одной машине в `baseline.json` и пересматриваются только
  вместе с изменением алгоритма.

## Запуск

`python tests/bench_dsp.py` — поднимает локальный static-сервер на `web/`,
прогоняет обычный режим и 4x CPU-throttle (эмуляция мобильного CPU),
сохраняет отчёт в `web/bench/baseline.json`, проверяет checksum и отсутствие
ошибок страницы. WASM собирается отдельно (`README.md`: пересборка WASM).
