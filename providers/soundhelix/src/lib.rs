//! Рабочий пример ABI v1 с тремя открытыми записями SoundHelix.
//! Гость не делает сетевых запросов. Разрешённые URL читает прокси сервера PUMP.
use serde::Deserialize;
use serde_json::{Value, json};

const ABI: u32 = 1;
const MAX_REQUEST: usize = 65536;

#[derive(Deserialize)]
struct Search {
    abi_version: u32,
    query: String,
    limit: usize,
}
#[derive(Deserialize)]
struct Resolve {
    abi_version: u32,
    track_id: String,
}

/// Хост создаёт отдельный экземпляр на вызов и освобождает всю память после ответа.
fn reply(value: Value) -> u64 {
    let bytes = value.to_string().into_bytes().into_boxed_slice();
    let len = bytes.len() as u64;
    let pointer = Box::into_raw(bytes) as *mut u8 as usize as u64;
    (pointer << 32) | len
}

/// Ошибку хост распознает как ответ, не соответствующий контракту успешного вызова.
fn failure(message: &str) -> u64 {
    reply(json!({"error": message}))
}

#[unsafe(no_mangle)]
pub extern "C" fn plugin_abi_version() -> u32 {
    ABI
}

#[unsafe(no_mangle)]
pub extern "C" fn alloc(length: usize) -> *mut u8 {
    if length == 0 || length > MAX_REQUEST {
        return std::ptr::null_mut();
    }
    Box::into_raw(vec![0_u8; length].into_boxed_slice()) as *mut u8
}

/// # Safety
/// Хост передаёт участок, полученный от alloc, с исходной длиной, в памяти этого гостя.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn search(pointer: *const u8, length: usize) -> u64 {
    if pointer.is_null() || length == 0 || length > MAX_REQUEST {
        return failure("Некорректный буфер запроса");
    }
    // Указатель проверяет хост. Срез не сохраняется за пределами вызова.
    let input = unsafe { std::slice::from_raw_parts(pointer, length) };
    let Ok(request) = serde_json::from_slice::<Search>(input) else {
        return failure("Некорректный JSON поиска");
    };
    if request.abi_version != ABI
        || request.query.trim().is_empty()
        || !(1..=100).contains(&request.limit)
    {
        return failure("Некорректные параметры поиска");
    }
    let query = request.query.to_lowercase();
    let tracks: Vec<_> = (1..=3)
        .filter(|number| {
            query
                .split_whitespace()
                .all(|word| format!("soundhelix song {number}").contains(word))
        })
        .take(request.limit)
        .map(|number| {
            json!({
                "id": format!("song-{number}"), "title": format!("SoundHelix Song {number}"),
                "artist": "SoundHelix", "kind": "full", "duration_ms": null, "artwork": null,
            })
        })
        .collect();
    reply(json!({"abi_version": ABI, "tracks": tracks}))
}

/// # Safety
/// Хост передаёт участок, полученный от alloc, с исходной длиной, в памяти этого гостя.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn resolve(pointer: *const u8, length: usize) -> u64 {
    if pointer.is_null() || length == 0 || length > MAX_REQUEST {
        return failure("Некорректный буфер запроса");
    }
    let input = unsafe { std::slice::from_raw_parts(pointer, length) };
    let Ok(request) = serde_json::from_slice::<Resolve>(input) else {
        return failure("Некорректный JSON разрешения потока");
    };
    if request.abi_version != ABI {
        return failure("Несовместимая версия ABI");
    }
    let Some(number) = (1..=3).find(|number| request.track_id == format!("song-{number}")) else {
        return failure("Запись не найдена");
    };
    reply(json!({"abi_version": ABI, "candidates": [{
        "url": format!("https://www.soundhelix.com/examples/mp3/SoundHelix-Song-{number}.mp3"),
        "quality": "standard", "mime_type": "audio/mpeg", "bitrate_kbps": null,
    }]}))
}
