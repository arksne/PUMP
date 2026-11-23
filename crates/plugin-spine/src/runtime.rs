//! Изоляция WASM-провайдера с ограничениями памяти, времени и топлива.
use std::{
    fs::File,
    io::Read,
    path::Path,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde::{Serialize, de::DeserializeOwned};
use wasmtime::{
    Config, Engine, ExternType, Instance, Module, ResourceLimiter, Store, StoreLimits,
    StoreLimitsBuilder, Trap, UpdateDeadline,
};

use crate::{
    ABI_VERSION, MusicProviderPlugin, PluginError, ResolveRequest, ResolveResponse, Result,
    SearchRequest, SearchResponse, model::valid_id,
};

/// Лимиты одного вызова, включая стартовую функцию и аллокатор гостя.
#[derive(Clone, Debug)]
pub struct RuntimeLimits {
    pub fuel: u64,
    pub timeout: Duration,
    pub memory_bytes: usize,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    pub max_module_bytes: usize,
    pub table_elements: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            fuel: 5_000_000,
            timeout: Duration::from_millis(250),
            memory_bytes: 16 * 1024 * 1024,
            max_input_bytes: 64 * 1024,
            max_output_bytes: 1024 * 1024,
            max_module_bytes: 8 * 1024 * 1024,
            table_elements: 4096,
        }
    }
}

impl RuntimeLimits {
    fn validate(&self) -> Result<()> {
        if self.fuel == 0
            || self.timeout.is_zero()
            || self.timeout > Duration::from_secs(3600)
            || self.memory_bytes < 65536
            || self.memory_bytes as u64 > u32::MAX as u64
            || self.max_input_bytes == 0
            || self.max_input_bytes > self.memory_bytes
            || self.max_output_bytes == 0
            || self.max_output_bytes > self.memory_bytes
            || self.max_module_bytes == 0
            || self.max_module_bytes > 64 * 1024 * 1024
            || self.table_elements == 0
        {
            return Err(PluginError::Configuration(
                "нулевой или несогласованный лимит".into(),
            ));
        }
        Ok(())
    }
}

/// Компилирует обычный WASM; сериализованные нативные модули не принимаются.
#[derive(Clone)]
pub struct WasmRuntime {
    engine: Engine,
    limits: RuntimeLimits,
}

impl WasmRuntime {
    pub fn new(limits: RuntimeLimits) -> Result<Self> {
        limits.validate()?;
        let mut config = Config::new();
        config
            .consume_fuel(true)
            .epoch_interruption(true)
            .wasm_multi_memory(false)
            .wasm_memory64(false)
            .max_wasm_stack(256 * 1024)
            .memory_reservation(0)
            .memory_guard_size(65536);
        let engine = Engine::new(&config).map_err(|e| PluginError::Configuration(e.to_string()))?;
        Ok(Self { engine, limits })
    }

    pub fn load_bytes(&self, id: impl Into<String>, wasm: &[u8]) -> Result<WasmMusicProvider> {
        let id = id.into();
        if !valid_id(&id) {
            return Err(PluginError::Configuration(
                "неверный идентификатор провайдера".into(),
            ));
        }
        if wasm.len() > self.limits.max_module_bytes {
            return Err(PluginError::Load("превышен размер модуля".into()));
        }
        let module = Module::from_binary(&self.engine, wasm)
            .map_err(|e| PluginError::Load(e.to_string()))?;
        if module.imports().next().is_some() {
            return Err(PluginError::Abi(
                "импорты запрещены; сеть, WASI и файловая система недоступны".into(),
            ));
        }
        match module.get_export("memory") {
            Some(ExternType::Memory(memory)) if !memory.is_64() && !memory.is_shared() => {}
            _ => {
                return Err(PluginError::Abi(
                    "нужна экспортированная память wasm32 memory".into(),
                ));
            }
        }
        for name in ["plugin_abi_version", "alloc", "search", "resolve"] {
            if !matches!(module.get_export(name), Some(ExternType::Func(_))) {
                return Err(PluginError::Abi(format!("нет функции {name}")));
            }
        }
        Ok(WasmMusicProvider {
            id,
            runtime: self.clone(),
            module,
        })
    }

    /// Путь выбирает сервер; гостевой модуль никогда не читает файлы хоста.
    pub fn load_file(
        &self,
        id: impl Into<String>,
        path: impl AsRef<Path>,
    ) -> Result<WasmMusicProvider> {
        let file = File::open(path).map_err(|e| PluginError::Load(e.to_string()))?;
        let mut bytes = Vec::new();
        file.take(self.limits.max_module_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| PluginError::Load(e.to_string()))?;
        self.load_bytes(id, &bytes)
    }
}

/// Скомпилированный провайдер безопасно разделяется между потоками.
pub struct WasmMusicProvider {
    id: String,
    runtime: WasmRuntime,
    module: Module,
}

impl WasmMusicProvider {
    fn invoke<I: Serialize, O: DeserializeOwned>(&self, operation: &str, input: &I) -> Result<O> {
        let input = serde_json::to_vec(input).map_err(|e| PluginError::Request(e.to_string()))?;
        let limits = &self.runtime.limits;
        if input.len() > limits.max_input_bytes {
            return Err(PluginError::Request("превышен размер JSON запроса".into()));
        }
        let deadline = Instant::now() + limits.timeout;
        let mut store = Store::new(
            &self.runtime.engine,
            GuestLimits {
                inner: StoreLimitsBuilder::new()
                    .memory_size(limits.memory_bytes)
                    .table_elements(limits.table_elements)
                    .instances(1)
                    .memories(1)
                    .tables(1)
                    .trap_on_grow_failure(true)
                    .build(),
                exceeded: false,
            },
        );
        store.limiter(|state| state);
        store
            .set_fuel(limits.fuel)
            .map_err(|e| PluginError::Configuration(e.to_string()))?;
        store.set_epoch_deadline(1);
        // Общий Engine допустим: чужой таймер не прерывает вызов до его срока.
        store.epoch_deadline_callback(move |_| {
            if Instant::now() >= deadline {
                Err(Trap::Interrupt.into())
            } else {
                Ok(UpdateDeadline::Continue(1))
            }
        });
        let _watchdog = Watchdog::start(self.runtime.engine.clone(), deadline)?;
        let instance =
            Instance::new(&mut store, &self.module, &[]).map_err(|e| execution_error(e, &store))?;
        let version = instance
            .get_typed_func::<(), i32>(&mut store, "plugin_abi_version")
            .map_err(|e| PluginError::Abi(e.to_string()))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|e| PluginError::Abi(e.to_string()))?;
        let search = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "search")
            .map_err(|e| PluginError::Abi(e.to_string()))?;
        let resolve = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "resolve")
            .map_err(|e| PluginError::Abi(e.to_string()))?;
        let actual = version
            .call(&mut store, ())
            .map_err(|e| execution_error(e, &store))?;
        if actual as u32 != ABI_VERSION {
            return Err(PluginError::Abi(format!(
                "версия гостя {actual}, ожидалась {ABI_VERSION}"
            )));
        }
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| PluginError::Abi("нет памяти".into()))?;
        let ptr = alloc
            .call(&mut store, input.len() as i32)
            .map_err(|e| execution_error(e, &store))? as u32 as usize;
        memory
            .write(&mut store, ptr, &input)
            .map_err(|_| PluginError::Abi("alloc вернул неверный диапазон".into()))?;
        let function = if operation == "search" {
            search
        } else {
            resolve
        };
        let packed = function
            .call(&mut store, (ptr as i32, input.len() as i32))
            .map_err(|e| execution_error(e, &store))? as u64;
        let output_ptr = (packed >> 32) as usize;
        let output_len = (packed & 0xffff_ffff) as usize;
        if output_len == 0 || output_len > limits.max_output_bytes {
            return Err(PluginError::Response(
                "пустой или слишком большой ответ".into(),
            ));
        }
        let end = output_ptr
            .checked_add(output_len)
            .ok_or_else(|| PluginError::Abi("переполнение диапазона ответа".into()))?;
        let bytes = memory
            .data(&store)
            .get(output_ptr..end)
            .ok_or_else(|| PluginError::Abi("ответ вне памяти гостя".into()))?;
        let output =
            serde_json::from_slice(bytes).map_err(|e| PluginError::Response(e.to_string()))?;
        if Instant::now() >= deadline {
            return Err(PluginError::Timeout);
        }
        Ok(output)
    }
}

impl MusicProviderPlugin for WasmMusicProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn search(&self, request: &SearchRequest) -> Result<SearchResponse> {
        request.validate()?;
        let response: SearchResponse = self.invoke("search", request)?;
        response.validate(request)?;
        Ok(response)
    }

    fn resolve(&self, request: &ResolveRequest) -> Result<ResolveResponse> {
        request.validate()?;
        let response: ResolveResponse = self.invoke("resolve", request)?;
        response.validate()?;
        Ok(response)
    }
}

struct GuestLimits {
    inner: StoreLimits,
    exceeded: bool,
}

impl ResourceLimiter for GuestLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        let result = self.inner.memory_growing(current, desired, maximum);
        if !matches!(result, Ok(true)) {
            self.exceeded = true;
        }
        result
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        let result = self.inner.table_growing(current, desired, maximum);
        if !matches!(result, Ok(true)) {
            self.exceeded = true;
        }
        result
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }
    fn tables(&self) -> usize {
        self.inner.tables()
    }
    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

fn execution_error(error: anyhow::Error, store: &Store<GuestLimits>) -> PluginError {
    if store.data().exceeded {
        return PluginError::MemoryLimit;
    }
    match error.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => PluginError::FuelExhausted,
        Some(Trap::Interrupt) => PluginError::Timeout,
        Some(Trap::MemoryOutOfBounds | Trap::TableOutOfBounds) => PluginError::MemoryLimit,
        _ => PluginError::Trap(error.to_string()),
    }
}

// Таймер будится при завершении вызова, поэтому быстрый запрос не ждёт timeout.
struct Watchdog {
    cancel: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    fn start(engine: Engine, deadline: Instant) -> Result<Self> {
        let cancel = Arc::new((Mutex::new(false), Condvar::new()));
        let state = Arc::clone(&cancel);
        let thread = thread::Builder::new()
            .name("plugin-deadline".into())
            .spawn(move || {
                let (lock, wake) = &*state;
                let mut cancelled = lock.lock().unwrap_or_else(|e| e.into_inner());
                while !*cancelled {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        engine.increment_epoch();
                    }
                    // Повторный тик исключает гонку с callback другого вызова того же Engine.
                    let wait = if remaining.is_zero() {
                        Duration::from_millis(1)
                    } else {
                        remaining
                    };
                    cancelled = wake
                        .wait_timeout(cancelled, wait)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            })
            .map_err(|e| PluginError::Configuration(e.to_string()))?;
        Ok(Self {
            cancel,
            thread: Some(thread),
        })
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        let (lock, wake) = &*self.cancel;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
