/* tslint:disable */
/* eslint-disable */

/**
 * Непрозрачные пиксели обложки дают спокойный цвет для стекла.
 */
export function palette_rgb(rgba: Uint8Array | number[]): number;

/**
 * Все слова запроса должны встречаться в описании настройки.
 */
export function settings_match(query: string, description: string): boolean;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly palette_rgb: (a: number, b: number) => number;
    readonly pump_dsp_abi_version: () => number;
    readonly pump_dsp_capacity: () => number;
    readonly pump_dsp_config_len: () => number;
    readonly pump_dsp_config_ptr: () => number;
    readonly pump_dsp_configure: () => number;
    readonly pump_dsp_create: (a: number) => number;
    readonly pump_dsp_destroy: () => void;
    readonly pump_dsp_pcm_ptr: () => number;
    readonly pump_dsp_process: (a: number) => number;
    readonly pump_dsp_reset: () => number;
    readonly pump_dsp_rms: (a: number) => number;
    readonly settings_match: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_export: (a: number, b: number) => number;
    readonly __wbindgen_export2: (a: number, b: number, c: number) => void;
    readonly __wbindgen_export3: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
