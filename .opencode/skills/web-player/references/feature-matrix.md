# Матрица покрытия PUMP (заполнена по аудиту 25.11.2026, baseline весь PASS)

Шаблон v2 заменён фактической матрицей. ABSENT без явного задания не реализовывать. Строка считается зелёной только при PASS соответствующих тестов.

| Область | Статус | Пути-доказательства | Тест / критерий | Итог |
|---|---|---|---|---|
| Source / load / metadata | PRESENT | `src/catalog.rs` (scan, lofty, `mp3-/file-` id), `src/main.rs` (`/api/search`, `/api/tracks/{id}`, `/covers/{id}`) | `browser.py::test_catalog…` — `North Loop`, `duration_ms > 39 000` | PASS |
| Play / pause / resume / stop | PRESENT | `web/app.js` (`requestPlay`, `playPause`, события `playing/pause`), `index.html#audio` | E2E `currentTime > 0.6`, пауза останавливает | PASS |
| Seek / scrub / progress | PRESENT | `web/app.js` (`seek input`, `timeupdate`, `loadedmetadata`) | E2E seek 50% → гость `≥19.8 с` | PASS |
| Volume / mute | ABSENT (UI) | Свой слайдер отсутствует; громкость системная | N/A — не реализовывать без задания | N/A |
| Buffering / retry / offline | PRESENT | `ServeFile` Range/206 (`src/main.rs`), прокси SoundHelix с пробросом `Range`, батч listens + `pagehide/online` | E2E Range `206`, 50 байт `bytes=100-149`; рестарт-сервер | PASS |
| Queue / shuffle / repeat | PRESENT | `web/app.js` (`queue/index/shuffle/repeat`, `renderQueue`, `queueSheet`) | E2E очередь + `aria-current` | PASS |
| Next / previous / gapless | PRESENT / ABSENT | `step(±1)`, порог 3 с; gapless/crossfade нет | E2E переходы; gapless N/A | PASS / N/A |
| Waveform / visualizer | ABSENT | Нет | N/A | N/A |
| DSP / EQ / speed | PARTIAL | Только `playbackRate` 0.85/1.0/1.15 (`web/app.js`), валидация комнат 0.5–2.0 (`src/rooms.rs`); EQ/DSP нет | E2E скорость не ломает синк | PASS / N/A |
| Media Session / shortcuts | ABSENT | Media Session API нет; `Escape`/назад Mini App есть (`web/app.js`) | N/A | N/A |
| Persistence / resume | PRESENT | `src/stats.rs` (SQLite WAL, `sessions`/`listens`, `ON CONFLICT DO NOTHING`) | E2E `stop → start`, `(tracks, ms) == (1, 1200)` | PASS |
| Upload / library / search | PRESENT | `src/catalog.rs` (медиатека + iTunes + YouTube), избранное localStorage | E2E поиск full/YouTube-ссылка; unit `parses_public_video_links…` | PASS |
| SoundCloud search / playback | PRESENT | `crates/core-streaming/src/soundcloud.rs` (api-v2, progressive only), `Source::Soundcloud`, `GET /source/sc/{id}` (прокси + Range) | unit-парсинг/валидация 3/3; E2E `test_external_sources_shape_and_guards` | PASS |
| Spotify search (app keys) | PRESENT | `crates/social-integrations/src/spotify_app.rs` (Client Credentials), только совпадения каталога | unit-парсинг 1/1; E2E notice без ключей | PASS |
| Backend / storage / CDN | PRESENT | axum + SQLite; `ServeFile` HEAD/Range; `apple_asset` allowlist против SSRF | unit + E2E Range/206; clippy | PASS |
| UI / mobile / keyboard | PRESENT | `index.html` (6 экранов, `aria-*`, ≥48px, safe-area, reduced-motion) | E2E Pixel 7 ×2, `pageerror == []` | PASS |
| Compatibility | PARTIAL | Кодек зависит от браузера (README); проверен Chromium 153 | Safari/Firefox — NOT RUN | PASS / NOT RUN |
| Lifecycle / performance | PRESENT | `finalizeListen`, очистка abort/timer/WS; отдельного бюджета нет | E2E + unit; замеры — по `performance.md` при задачах | PASS |
| Security | PRESENT | CSP/`nosniff`/лимиты тел, Origin-check, HMAC `initData`/webhook, sha256-токены | unit `webhook_rejects…`, `signed_data_rejects…` | PASS |
| Rooms sync | PRESENT | `src/rooms.rs` (код 12 hex, throttle 45 мс, broadcast 32, heartbeat), `web/app.js` (`applyRemote/syncRoom/publish`, допуски 0.25/0.05) | E2E двое, Δ позиций <0.35 с | PASS |
| Telegram bot / inline | PRESENT | `src/telegram.rs` (`/start`, `/stats`, inline только `mp3-` Full ≤20) | unit `inline_query_supplies…` | PASS |
| Lyrics | PRESENT | `src/catalog.rs` (LRCLIB, cap 64 KiB), `web/app.js` (LRC-парсинг, клик → seek) | E2E мок `/api/lyrics` | PASS |
