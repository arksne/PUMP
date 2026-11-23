//! Изолированные WASM-провайдеры поиска и аудио. Контракт описан в `ABI.md`.
//! Синхронные вызовы следует выполнять через `tokio::task::spawn_blocking`.

mod model;
mod registry;
mod runtime;

pub use model::*;
pub use registry::*;
pub use runtime::*;

pub type Result<T> = std::result::Result<T, PluginError>;

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("неверная конфигурация: {0}")]
    Configuration(String),
    #[error("ошибка загрузки WASM: {0}")]
    Load(String),
    #[error("нарушение ABI: {0}")]
    Abi(String),
    #[error("неверный запрос: {0}")]
    Request(String),
    #[error("неверный ответ: {0}")]
    Response(String),
    #[error("исчерпан запас топлива")]
    FuelExhausted,
    #[error("истёк срок выполнения")]
    Timeout,
    #[error("превышен лимит памяти или таблицы либо неверный доступ к памяти")]
    MemoryLimit,
    #[error("ловушка WASM: {0}")]
    Trap(String),
    #[error("провайдер уже зарегистрирован: {0}")]
    DuplicateProvider(String),
    #[error("провайдер не найден: {0}")]
    ProviderNotFound(String),
    #[error("нет зарегистрированных провайдеров")]
    NoProviders,
    #[error("все провайдеры завершились ошибкой: {0:?}")]
    ProvidersFailed(Vec<ProviderFailure>),
    #[error("нет подходящих аудиокандидатов")]
    NoCandidates,
}

/// Контракт провайдера; WASM-реализация не получает доступ к сети или файлам хоста.
pub trait MusicProviderPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn search(&self, request: &SearchRequest) -> Result<SearchResponse>;
    fn resolve(&self, request: &ResolveRequest) -> Result<ResolveResponse>;
}
