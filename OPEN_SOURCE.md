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

[Exportify](https://github.com/watsonbox/exportify), MIT, предоставляет пользователю экспорт плейлистов в CSV. PUMP умеет читать этот формат самостоятельно. Exportify работает как отдельный сайт, его код в PUMP не включён.

DejaVu для PNG-карточек распространяется с [исходной лицензией](crates/telegram-bridge/assets/DejaVu-LICENSE.txt). Демозаписи SoundHelix отмечены в интерфейсе и README. Исходный HTML-дизайн предоставлен пользователем.
