# Музыкальные дополнения PUMP

Администратор кладёт WASM-файлы в `PUMP_PLUGIN_DIR`. Имя файла без расширения становится именем источника. После запуска он появляется в поиске. Поиск возвращает метаданные, а разрешение трека — упорядоченные качества, в том числе lossless и резервные форматы.

Рабочий пример `soundhelix` написан на Rust. Он ищет три открытые полные записи SoundHelix и возвращает их MP3-адреса. Это небольшой встроенный каталог; он не заявляет поддержку остальных музыкальных сервисов.

```sh
rustup target add wasm32-unknown-unknown
cargo build --manifest-path providers/soundhelix/Cargo.toml --target wasm32-unknown-unknown --release --locked
node --test providers/soundhelix/provider.test.mjs
mkdir -p plugins
cp providers/soundhelix/target/wasm32-unknown-unknown/release/pump_provider_soundhelix.wasm plugins/soundhelix.wasm
```

Добавь в `.env` и перезапусти PUMP.

```dotenv
PUMP_PLUGIN_DIR=./plugins
PUMP_PLUGIN_ALLOWED_HOSTS=www.soundhelix.com
```

В поиске выбери `soundhelix` и введи `song 2`. Для других провайдеров перечисляй точные хосты через запятую. Шаблон `*.example.org` разрешает поддомены, сам `example.org` указывается отдельно. Это конфигурация администратора. В неё можно включить адрес собственного медиасервера.

Браузер получает ссылки `/addon/stream/...`, действующие 15 минут. Каждое разрешение независимо, поэтому два слушателя могут выбирать разные форматы. Один URL всегда соответствует одному формату и поддерживает Range. Клиент выбирает следующий формат при ошибке и обновляет истёкшее разрешение. При сбое посреди трека позиция переносится в новый формат после загрузки метаданных.

WASM исполняется с ограничением памяти, топлива и времени. Сетевых импортов в госте пока нет. Он может искать во встроенных данных и возвращать HTTP(S)-потоки. Прокси проверяет настроенные хосты, не следует перенаправлениям, не передаёт источнику cookies и Authorization браузера. Полный контракт описан в [ABI](../crates/plugin-spine/ABI.md).

Музыка примера предоставлена [SoundHelix](https://www.soundhelix.com/audio-examples).
