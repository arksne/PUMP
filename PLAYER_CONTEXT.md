# PLAYER_CONTEXT.md — PUMP (фактический контекст, проверено 25.11.2026)

Браузерный музыкальный плеер + Telegram Mini App. Сервер Rust (axum) раздаёт `index.html`, `/assets/app.js`, каталог, WebSocket-комнаты и статистику SQLite. Бот — Teloxide. Аудиодвижок — нативный `HTMLAudioElement` (`<audio id="audio">`), видео YouTube — iframe `youtube-nocookie.com`.

## Стек (проверено)
- Rust edition 2024, axum 0.8 (ws), tokio 1 (fs/net/rt-multi-thread/signal/sync/time), sqlx 0.8 (sqlite, WAL, pool 5), reqwest 0.12 (rustls-native-roots, json, stream), lofty 0.22, serde/serde_json 1, teloxide 0.17 (rustls), tower-http 0.6 (fs/trace), `Cargo.lock` зафиксирован.
- Frontend: ванильный JS (`web/app.js`, ~641 строка, `"use strict"`), без фреймворка; CSS Liquid Glass в `index.html`; AudioContext/WebAudio/DSP/WASM **нет**.
- E2E: Python `unittest` + Playwright Chromium, профиль Pixel 7, две мобильные сессии; фикстуры — синтез FFmpeg sine 440/550 Гц, 40 c.
- Деплой: `Dockerfile` (build `rust:1-bookworm` → runtime `debian:bookworm-slim`, UID 10001, `/app/data`), `render.yaml` (Docker, free, `healthCheckPath: /healthz`, `TELOXIDE_TOKEN` sync:false). CI: `check.yml` (fmt/clippy/test/build + ffmpeg + playwright + docker healthz), `publish.yml`.
- Версии окружения проверки: cargo 1.99.1 / rustc 1.99.1, ffmpeg N-128212, Python 3.12.4, playwright 1.63.0 (в CI 1.62.0 — дрейф версии), headless-shell Chromium 153.0.8010.12.

## Карта файлов
- `src/main.rs` — вход, `App { catalog, stats, rooms, telegram }`, роуты `/`, `/assets/app.js` (inline `include_str!`), `/healthz`, `/api/search|tracks/{id}|lyrics/{id}|session|listens|stats|rooms`, `/media/{id}`, `/covers/{id}`, `/ws/room/{code}`, `/telegram/webhook` (лимит тела 256 KiB; глобальный 24 KiB). `MAX_TRACK_MS = 86_400_000`. CSP: `media-src 'self' https://*.mzstatic.com https://*.itunes.apple.com`, `frame-src youtube-nocookie`, `frame-ancestors 'self' web.telegram.org *.telegram.org`.
- `src/catalog.rs` — `Kind { Full, Preview, Youtube }`, `Source { All, Full, Preview, Youtube }`, поиск ≤140 символов, full ≤40, remote кэш 180 c (≤256), lookup 6 ч (≤8192). Локальные: mp3/m4a/aac/ogg/opus/flac/wav/webm, лимит обхода 100 000, пропуск dot-файлов/пустых, id `mp3-<sha256[:16]>` / `file-<…>`, теги lofty, обложка ≤2 MiB (jpeg/png/webp/gif). Демо `soundhelix-{1..3}` → прокси SoundHelix с пробросом `Range`, `206/Content-Range/Accept-Ranges` (таймаут 120 с). Превью iTunes (лимит 16, allowlist хостов `*.mzstatic.com`/`*.itunes.apple.com`, SSRF-защита через `apple_asset`). YouTube: по ссылке (`video_from_input`, 11 символов `[A-Za-z0-9_-]`), по `YOUTUBE_API_KEY` (v3 search, 16), иначе InnerTube `WEB_REMIX` POST. Лирика LRCLIB `/api/get`, cap 64 KiB.
- `src/rooms.rs` — комнаты в памяти (`Mutex<HashMap>`, retain по `LIFETIME = 6 ч`), лимиты 8/владелец и 256 всего, код `Uuid simple[..12]` (hex). WS: `max_message_size/frame 4096`, join ≤5 c, токен сессии обязателен, только ведущий шлёт `playback` (throttle 45 мс, `rate 0.5–2.0`, `position ≤ MAX_TRACK_MS`, трек обязан существовать и быть не-YouTube). Broadcast 32, heartbeat 20 c / dead 55 c, при уходе ведущего — пауза с доначислением позиции. Origin-check: Telegram-origin либо Host совпадает.
- `src/stats.rs` — SQLite `sessions(token_hash, owner)`, `listens(owner,event,track,title,artist,timestamp,milliseconds 100–30000)` с идемпотентностью `ON CONFLICT DO NOTHING`, токен 64 hex (sha256), сессии `guest:<uuid>` / `telegram:<id>`, периоды 7/30 дней, топ-5. YouTube в статистике запрещён (400).
- `src/telegram.rs` — `/start` (WebApp-кнопка), `/stats` (7 дней), inline — только `Full` + только `mp3-` из медиатеки и SoundHelix (≤20, `cache_time 60`). `initData` HMAC (`WebAppData`, окно −12 ч/+60 с, лимит 16 KiB, строгий разбор). Webhook-секрет HMAC(token, `pump/telegram-webhook/v1\0` + url), сравнение 64-hex.
- `web/app.js` — единый источник `selected` + `<audio>`; события `timeupdate/loadedmetadata/playing/pause/seeking/seeked/ended/error`; `play()` через `requestPlay()` с `catch AbortError`; гость: контролы disabled, `playBtn` disabled, очередь из 1 трека. Очередь/шаффл/повтор/скорость (0.85/1.0/1.15)/слип 15 мин/лайки (`pump.favorites`, `pump.favorite-tracks`)/стили обложки (card/vinyl/clover)/тексты (LRC-парсинг, клик → seek). Статистика: семплирование `timeupdate` (окно 2.5 с, допуск +400 мс, порог 5 с), очередь `pump.listens` (≤256), flush по `pagehide/online`. Комнаты: ping 5 с, publish 1 с + debounce 75 мс, `skew/rtt` по pong, допуск рассинхрона 0.25 с / seek-порог 0.05 с, гостю нужен явный «Включить звук» (autoplay policy). Поиск debounce 350 мс + `AbortController`; гонки трека закрыты `trackRequest`/revision.
- `index.html` — 6 экранов (player/search/lyrics/rooms/stats + очередь-dialog), `aria-label/pressed/current`, `role=status`, focus-visible, touch-targets ≥48px, safe-area, `prefers-reduced-motion`, media 372px.
- `tests/browser.py` — `test_catalog_and_persistent_statistics` (Range 206/50 байт, YouTube-ссылка kind/video_id/audio_url, идемпотентность listens, персистентность после рестарта), `test_two_people_listen_together` (play → seek 50% → pause, Δ позиций <0.35 с, очередь, статистика, `pageerror == []`).

## Команды
```sh
cp .env.example .env
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
./target/debug/pump  # http://127.0.0.1:8080
python tests/browser.py  # нужны FFmpeg + Playwright Chromium
```

## Поток данных
Браузер `/` → `POST /api/session` → `GET /api/search` → `GET /api/tracks/{id}` → `<audio src=/media/{id}>` (Range 206) + `/covers/{id}` + `/api/lyrics/{id}` → `timeupdate` → `POST /api/listens` (батч) → `GET /api/stats`. Комната: `POST /api/rooms` → `WS /ws/room/{code}` (`join/playback/ping`) → гость `applyRemote/syncRoom`.

## Реализовано (доказано кодом и тестами)
Play/pause/resume/stop, seek/scrub/progress, volume через системный (свой слайдер громкости отсутствует — см. ниже), очередь/shuffle/repeat, next/previous, тексты LRC + plain, скорость, слип-таймер, избранное, 3 стиля обложки, комнаты host/guest с синхронизацией, статистика 7/30 дней + `/stats` в боте, inline-MP3, персистентность SQLite, Range/206, retry/идемпотентность, Origin/CSP/HMAC-защита.

## Отсутствует / ограничения (осознанно, не чинить без задачи)
- Нет слайдера громкости/mute в UI (громкость — системная/железная). Матрица: Volume — ABSENT (UI), движок поддерживает.
- Нет Waveform/visualizer, нет DSP/EQ (только playbackRate), нет gapless/crossfade, нет Media Session API, нет офлайн-кэша/ServiceWorker.
- Демо-аудио зависит от внешнего SoundHelix (сеть); без сети — только локальные файлы.
- Комнаты — память процесса (сброс при рестарте/деплоe free); история — SQLite на эфемерном диске без volume.
- Playwright в окружении 1.63.0 против 1.62.0 в CI — принять или зафиксировать.

## Риски
- `audio.play()` rejection обработан; ложного «playing» нет (кнопка читает `audio.paused/ended`).
- Смена источника: `finalizeListen` + `pause` + `removeAttribute(src)` + `load()` — гонки закрыты; YouTube отделён (`clearVideo`/iframe).
- Секреты (`TELOXIDE_TOKEN`, `YOUTUBE_API_KEY`) только сервер, в логи/коммиты не попадают; `.env` в `.gitignore`.

## Дополнения ветки feature/full-spec (26.11.2026, проверено)
- Сеть: `core-streaming/src/network.rs` — `PUMP_HTTP_PROXY` (http/https/socks5/socks5h, секреты не в ошибках), `PUMP_DNS_MODE=system|cloudflare` (DoH с кэшем ≤256/TTL≤300с, прокси/localhost — системный DNS). DNS не меняет страну сервера — для регионов нужен прокси. reqwest `socks` включён в core-streaming.
- SoundCloud: `core-streaming/src/soundcloud.rs` — api-v2 по протоколу веб-плеера (см. OPEN_SOURCE.md), `client_id` извлекается из бандлов и кэшируется на 24 ч, обновление после 401; только `progressive`-потоки, HLS/блокировки — недоступно без обхода. `Source::Soundcloud`, поиск/ссылки, прокси `GET /source/sc/{id}` со свежей подписью на запрос, Range, `audio/*`-валидация, `private, no-store`.
- Spotify: `social-integrations/src/spotify_app.rs` — Client Credentials (`SPOTIFY_CLIENT_ID` + `SPOTIFY_CLIENT_SECRET`), кэш токена; `Source::Spotify` возвращает только совпадения каталога. Без ключей — статическое уведомление.
- Поиск `All`: локальное + SoundCloud параллельно (лимит 6 с) + Spotify-совпадения (лимит 8 с), топ-64, Full первыми, склейка уведомлений.
- Статистика: при открытии БД удаляются события старше 35 дней (неделя/месяц не меняются); сессии не трогаются.
- Тексты: сервер ограничивает каждый источник 6 с; клиент кэширует слова один раз и подсвечивает только окрестность текущей позиции через RAF, gated видимостью/воспроизведением.
- Проверки срезов: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace` (55 passed, 0 failed — 21 набор), `python tests/browser.py` (12 тестов: 9 ok + 3 honest skip без сети; stub SC-цепочка и stub-комната детерминированы), скриншоты `dist/ui-shots/`, CDP `dist/cdp-metrics.json` (heap 2.07→3.39 МБ, long tasks 0, Layout 0.039 с / RecalcStyle 0.020 с суммарно).
- Windows-база: `disk.rs` cfg для не-unix, `queue.rs` — любая ошибка `try_lock_exclusive` означает занятость (на Windows другой код, чем WouldBlock).

## Этап 1 — стабилизация (26.11.2026, частично проверено)
- SQLite Windows: первопричина — незакрытое `sqlite3`-соединение самого теста (`with` не закрывает, блокирует rmtree); лечится явным `db.close()`. Доказано: падает и на чистом `0c308b9`. E2E снова зелёный 6/6 + новые тесты.
- SoundCloud: прямое воспроизведение через `/source/sc/{id}` — 200 + `audio/mpeg`, Range 206 с побайтовым совпадением, свежие подписи на запрос (без кэша), метаданные для комнат/статистики. Stub-E2E `test_soundcloud_stub_chain_search_resolve_stream` (локальный api-v2 + CDN-заглушка, настоящий decode в браузере). Живые тесты пропускаются при недоступности сети (`skip`, не фейк). Комнаты + SC: гость следует за ведущим (live-тест со skip).
- Безопасность (аудит quality-оркестратора, частично — лимит шагов): применено — userinfo/порты в `valid_stream_url`/`check_api_url`, `apple_asset` без портов/фрагментов, cap 64 KiB на resolve-ответ, 30 с таймауты NAS/downloads (send + idle chunk), `download` принимает только same-origin или iTunes-allowlist. Решения primary: NAS-тикет 6ч и plugin allowlist на совести админа — принято (тикет нужен очереди, allowlist — env админа); OAuth state `!=` — принято (43-char random, практически неэксплуатируемо, без новой зависимости); download-SSRF закрыт проверкой origin+allowlist (провайдеры отдают только same-origin lease-URL, NAS — same-origin тикеты, iTunes — allowlist). Открыто: `plugin-spine` внутренности (runtime/registry), `innertube.rs` upstream-валидация, `recognition.rs` (AUDD-токен), `scrobbling.rs`, `discord/fingerprint/import`, `dlna.rs` (LAN-SSRF), `controls/rooms/stats`, полный grep секретов.
- UI-перф (CDP, headless, non-isolated): heap 2.07→3.39 МБ, nodes 2116→2770, long tasks 0 за скролл и за окно подсветки слов, Layout 0.032 с / RecalcStyle 0.022 с суммарно. Кадр подсветки — O(окрестность), не O(document). Воспроизводимый замер: `python tests/cdp_perf.py` → `dist/cdp-metrics.json` (диагностика, не gate CI).
- Находки независимого ревью Этапа 1 (исправлено, ждёт сборки для прогона): deadlock `track()` на read-guard через await+write (скоуп guard), Full только при progressive (иначе HlsOnly/Blocked + честный notice), общий таймаут discovery 15 с, проверка статуса на 401-ретрае, single-flight client_id и Spotify-токена, negative-кэш SC (5 мин/512), семафор 8 + валидация Range на `/source/sc`, userinfo/порты/фрагменты в `valid_stream_url`/`check_api_url`/`apple_asset`, idle-таймауты NAS/SMB/обложек, guard загрузок (same-origin|iTunes) + allowlist обложек, индекс `listens_ts`, honest skip live-SC-тестов по notice, детерминированные lyric-тесты.
- Тексты: сервер — лимит 6 с на источник; E2E — seek/rate(1.15)/pause-resume на LRC + word-уровень (current/sung) на document-моке.
- Этапы 2–4 (решения primary по OSS-отчёту): бенчмарку `web/bench/*` — да; `analysis`-feature + `realfft` — да (Этап 3); timestretch EVAL — отложен; поставка считается закрытой (GPL/AGPL-код не входит, только формулы); true peak/LRA — входят в Этап 3.
- Ограничения окружения: память 1.2/16 ГБ — `cargo test --workspace` и полный `cargo build` OOM; проверка покрейтово (`-p`, `-j1`, `RUSTFLAGS=-C debuginfo=0`), E2E — после чистой пересборки; `cargo clean` при порче кэша после OOM-киллов. NB 26.11: память исчерпана полностью (rustc не выделяет даже 1 МБ) — сборка бинаря и E2E-прогон заблокированы до освобождения RAM; код верифицирован через fmt/clippy/test покрейтово.

## GATE 1 — бенчмарк JS vs Rust/WASM (26.11.2026, PASS)
- Стенд `web/bench/*` + `tests/bench_dsp.py`: одинаковый тракт (10 RBJ-биквадов transposed-DF2 + preamp), детерминированный вход (xorshift), 15 пачек x 256 блоков x 128 фреймов, warmup 3 пачки. JS-зеркало побитово совпадает с WASM (checksum rel 0, 3/3 прогона + throttled).
- Результаты (HeadlessChrome 153, non-isolated): JS p50 100.7 нс/фрейм (xRT ~207), WASM p50 54.9 нс/фрейм (xRT ~379); ratio wasm/js 0.545 (1.8x быстрее WASM). 4x CPU-throttle: 442/250 нс, xRT 47/83 — соотношение то же. Холодный старт: compile 0.5 мс + instantiate 0.1 mc (wasm 61951 байт). Память WASM статична (1638400 байт, mem_grew=false).
- Квант 128 фреймов = 2.67 мс: блок WASM ~7 мкс, JS ~13 мкс — оба <0.5% ядра. Вывод: EQ-ядро переносить некуда (уже WASM и корректно); WASM/Rust — для compute-heavy анализа (FFT/chroma/onset, SIMD-дружелюбно), UI/состояние — JS. Порог регрессии: checksum_ok обязателен; perf — медианы из baseline.json, тревога при >2x.
- Файлы: web/bench/{index.html,harness.js,js-dsp.js,fixtures.js,README.md,baseline.json}, tests/bench_dsp.py.
- Приёмка GATE 1 (26.11.2026, повтор после ревью-фиксов, `-j2`): `cargo fmt --check` чисто; `cargo clippy --workspace --locked --all-targets -D warnings` — 0 ошибок; `cargo test --workspace --locked` — 58 passed, 0 failed; `cargo build --workspace --locked` — 0; `python tests/browser.py` — 9 ok + 3 honest skip (сеть). Бенч после симметризации копирования входа: core JS p50 103.76 / WASM 54.93 нс/фрейм (ratio 0.529, speedup 1.89x, xRT 201/379); full 103.76/57.98 (ABI +3.05 нс, +5.6%); p95/p99 в baseline.json; mem 1638400 статична; throttle-4x то же соотношение (460.82/241.09); checksum_rel 0 везде; WASM raw 61951 / gzip 25154 байт; cold start compile 0.4 + instantiate 0.2 мс. Решение зафиксировано: docs/ADR-001-wasm-dsp-boundary.md.

## GATE 2 — независимый анализ аудио (26.11.2026, реализовано, ревью назначено)
- Модуль `crates/audio-dsp-engine/src/music_analysis.rs` за feature `analysis` (`dep:realfft 3.5.1`): только std+realfft, без Axum/Tokio/DOM/Telegram; native + wasm32 (`cargo check --target wasm32-unknown-unknown` чисто, один детерминированный код).
- STFT 4096/1024 Hann → flux с фикс. лог-компрессией + kick-band 160 Гц + ВЧ-контроль >2 кГц → onset (median+2·MAD, ≥0.15·max, ≥80 мс, сильные доли для IOI) → BPM 50–220 автокорреляцией с параболой + октавный снап по IOI (значение от автокорреляции) → beatgrid фазовым поиском (метка = конец кадра, aubio-конвенция) → HPCP-chroma (пики+парабола, вес энергией, октава 0.3, без 3f) → ключ KS с лог-компрессией → LUFS (`LoudnessMeter`) + true peak 4x → энергия/RMS-100мс, тишина −60dBFS/200мс, секции кластеризацией (cos ≥0.93, cap 8), переходы MixIn/MixOut/SectionBoundary/EnergyDip.
- Память: лимит 14.4M фреймов, хранятся chroma+энергия (13 f32/кадр), план/буферы переиспользуются (`MusicAnalyzer::reset`). Отмена `AtomicBool`, `ANALYSIS_VERSION = 1`, гейты: крест ≥3.0, hf_ratio ≥0.02, conf BPM ≥0.35, ключ при r1 ≥0.55. Ориентиры: docs/ADR-002, OPEN_SOURCE.md (Mixxx/Essentia/aubio/librosa/realfft/libebur128/Signalsmith — формулы, код не копировался, поставка закрытая).
- Приёмка на синтетике (`cargo test -p audio-dsp-engine --features analysis --lib`, 15/15): BPM 60–174 ≤1%, half/double/weak/синкопа/дрейф/44.1-96кГц/стерео в норме; beatgrid медиана ≤20 мс; ключ 4/4; LUFS vs эталон ≤0.5 LU; пусто/NaN/отмена без panic; секции 2-половины/1-однородный; переходы MixIn/Boundary. Старый `analyze_track` не тронут.
- Тест-матрица: `cargo test --workspace --locked -j2` (58) + `cargo test -p audio-dsp-engine --features analysis -j2` (15) + `cargo clippy -p audio-dsp-engine --features analysis --all-targets -D warnings` + wasm32-check. Реальная музыка — отдельным размеченным набором, не выдавать синтетику за доказательство.
