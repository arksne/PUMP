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
- Проверки срезов: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace`, `python tests/browser.py` (+ новый `test_external_sources_shape_and_guards`), скриншоты `dist/ui-shots/` (mobile 390×844, desktop 1280×800). Все команды с `-j2` (тулчейн падает на полной параллельности — см. ограничения).
- Windows-база: `disk.rs` cfg для не-unix, `queue.rs` — любая ошибка `try_lock_exclusive` означает занятость (на Windows другой код, чем WouldBlock).
