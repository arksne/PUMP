//! Проверка выполнения настоящего WASM-модуля через ABI провайдера.
use plugin_spine::{MusicProviderPlugin, RuntimeLimits, SearchRequest, WasmRuntime};
#[test]
fn executes_search_fixture() {
    let runtime = WasmRuntime::new(RuntimeLimits::default()).unwrap();
    let wasm = wat::parse_str(include_str!("fixtures/provider.wat")).unwrap();
    let provider = runtime.load_bytes("test", &wasm).unwrap();
    let response = provider.search(&SearchRequest::new("needle", 10)).unwrap();
    assert_eq!(response.tracks[0].id, "fixture-1");
}
#[test]
fn rejects_incompatible_module() {
    let runtime = WasmRuntime::new(RuntimeLimits::default()).unwrap();
    let wasm = wat::parse_str("(module (memory (export \"memory\") 1))").unwrap();
    assert!(runtime.load_bytes("bad", &wasm).is_err());
}

#[test]
fn terminates_unbounded_guest_code() {
    let limits = RuntimeLimits {
        fuel: 10000,
        ..Default::default()
    };
    let runtime = WasmRuntime::new(limits).unwrap();
    let wasm=wat::parse_str(r#"(module (memory (export "memory") 1) (func (export "plugin_abi_version") (result i32) i32.const 1) (func (export "alloc") (param i32) (result i32) i32.const 0) (func (export "search") (param i32 i32) (result i64) (loop br 0) i64.const 0) (func (export "resolve") (param i32 i32) (result i64) i64.const 0))"#).unwrap();
    let provider = runtime.load_bytes("loop", &wasm).unwrap();
    assert!(matches!(
        provider.search(&SearchRequest::new("test", 1)),
        Err(plugin_spine::PluginError::FuelExhausted)
    ));
}
