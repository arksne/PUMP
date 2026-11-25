# Использованные open-source проекты

PUMP использует Rust-крейты как зависимости. Их точные версии закреплены в Cargo.lock. Лицензии распространяются на соответствующие компоненты.

| Проект | Применение | Лицензия |
| --- | --- | --- |
| [Symphonia](https://github.com/pdeljanov/Symphonia) | Декодирование аудио в PCM | MPL-2.0 |
| [Rubato](https://github.com/HEnquist/rubato) | Ресемплинг | MIT |
| [Wasmtime](https://github.com/bytecodealliance/wasmtime) | Изолированное исполнение музыкальных WASM-провайдеров | Apache-2.0 WITH LLVM-exception |
| [Axum](https://github.com/tokio-rs/axum) | HTTP API и WebSocket | MIT |
| [Teloxide](https://github.com/teloxide/teloxide) | Telegram Bot API | MIT |
| [SQLx](https://github.com/launchbadge/sqlx) | SQLite | MIT OR Apache-2.0 |
| [Lofty](https://github.com/Serial-ATA/lofty-rs) | Аудиотеги и обложки | MIT OR Apache-2.0 |
| [RustFFT](https://github.com/ejmahler/RustFFT) | Спектральные отпечатки | MIT OR Apache-2.0 |
| [Reqwest](https://github.com/seanmonstar/reqwest) | HTTP-клиенты и потоки | MIT OR Apache-2.0 |

Для импорта Spotify без собственного Client ID изучен [SpotifyScraper](https://github.com/AliAkhtari78/SpotifyScraper), MIT. Использован подход чтения публичного Spotify Embed. Парсер PUMP написан на Rust, Python-проект не является зависимостью.

Протокол SoundCloud (поиск, разрешение транскодингов, извлечение `client_id`) повторяет поведение веб-плеера и задокументирован в открытом каталоге [soundcloud-protocol](https://github.com/KlugesBuch1/soundcloud-protocol) (MIT) и документации [Resona API](https://savo-o.github.io/resona/api.html). Реализация PUMP написана самостоятельно поверх reqwest с учётом прокси и лимитов сервера; внешние Rust-крейты для SoundCloud не используются.

[Exportify](https://github.com/watsonbox/exportify), MIT, предоставляет пользователю экспорт плейлистов в CSV. PUMP умеет читать этот формат самостоятельно. Exportify работает как отдельный сайт, его код в PUMP не включён.

DejaVu для PNG-карточек распространяется с [исходной лицензией](crates/telegram-bridge/assets/DejaVu-LICENSE.txt). Демозаписи SoundHelix отмечены в интерфейсе и README. Исходный HTML-дизайн предоставлен пользователем.

## GATE 2 — ориентиры музыкального анализа (изучено 26.11.2026, код не копировался)

Поставка PUMP считается закрытой: GPL/AGPL-код в репозиторий не входит, заимствуются только опубликованные формулы и алгоритмические идеи. Реализация `crates/audio-dsp-engine/src/music_analysis.rs` написана с нуля.

| Проект | Версия / ревизия | Что взято за ориентир | Лицензия |
| --- | --- | --- | --- |
| [Mixxx](https://github.com/mixxxdj/mixxx) | ветка 2.4–2.5 | Темп автокорреляцией onset-огибающей, фазовый beatgrid, октавная неоднозначность | GPL-2.0-or-later |
| [Essentia](https://github.com/MTG/essentia) | 2.1 | HPCP (пики спектра + парабола), RhythmExtractor, ключ по профилям Krumhansl–Schmuckler, гейты громкости EBU R128 | AGPL-3.0 |
| [aubio](https://github.com/aubio/aubio) | 0.4.9 | Spectral flux с логарифмическим сжатием, пикирование onset, конвенция метки onset концом кадра | GPL-3.0 |
| [librosa](https://github.com/librosa/librosa) | 0.10 | Chroma-нормализация и лог-компрессия, структурная самоподобность (рекуррентность) | ISC |
| [realfft](https://github.com/HEnquist/realfft) | 3.5.1 (в Cargo.lock) | Прямая зависимость `analysis`: вещественное FFT поверх rustfft, переиспользование плана/буферов | MIT |
| [rustfft](https://github.com/ejmahler/RustFFT) | 6.4.1 (в Cargo.lock) | Транзитивно через realfft | MIT OR Apache-2.0 |
| [libebur128](https://github.com/jiixyj/libebur128) | 1.2.x | Двухступенчатые гейты −70 LU / −10 LU, тарсет K-взвешивания (свой `LoudnessMeter` уже следовал BS.1770) | MIT |
| [Signalsmith Stretch](https://github.com/Signalsmith-Audio/signalsmith-stretch) | 1.x | Только референс фазовой обработки для будущего timestretch-EVAL Automix; в анализе не используется | MIT |

Профили Krumhansl–Schmuckler — эмпирические константы из литературы (Krumhansl–Kessler), не код.
