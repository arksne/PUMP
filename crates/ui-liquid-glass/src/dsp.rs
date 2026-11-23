//! Числовой WASM ABI: не требует TextEncoder, JS-массивов или wasm-bindgen glue.
//! Один экземпляр WASM принадлежит одному AudioWorklet, состояние между ними не общее.
use audio_dsp_engine::wasm::{CONFIG_LEN, MAX_BLOCK_FRAMES, WorkletDsp};
use std::cell::RefCell;

thread_local! {
    static DSP: RefCell<Option<WorkletDsp>> = const { RefCell::new(None) };
}

pub const DSP_ABI_VERSION: u32 = 1;

#[unsafe(no_mangle)]
pub extern "C" fn pump_dsp_abi_version() -> u32 {
    DSP_ABI_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn pump_dsp_capacity() -> usize {
    MAX_BLOCK_FRAMES
}

#[unsafe(no_mangle)]
pub extern "C" fn pump_dsp_config_len() -> usize {
    CONFIG_LEN
}

/// 0 — успех, 1 — неверная частота, 2 — настройки, 3 — блок/PCM, 4 — не создан.
#[unsafe(no_mangle)]
pub extern "C" fn pump_dsp_create(rate: u32) -> u32 {
    match WorkletDsp::new(rate) {
        Ok(dsp) => {
            DSP.with(|slot| *slot.borrow_mut() = Some(dsp));
            0
        }
        Err(_) => 1,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pump_dsp_pcm_ptr() -> *mut f32 {
    DSP.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .map_or(std::ptr::null_mut(), |dsp| dsp.pcm_mut().as_mut_ptr())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pump_dsp_config_ptr() -> *mut f64 {
    DSP.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .map_or(std::ptr::null_mut(), |dsp| dsp.config_mut().as_mut_ptr())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pump_dsp_configure() -> u32 {
    DSP.with(|slot| match slot.borrow_mut().as_mut() {
        Some(dsp) => {
            if dsp.configure().is_ok() {
                0
            } else {
                2
            }
        }
        None => 4,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pump_dsp_process(frames: usize) -> u32 {
    DSP.with(|slot| match slot.borrow_mut().as_mut() {
        Some(dsp) => {
            if dsp.process(frames).is_ok() {
                0
            } else {
                3
            }
        }
        None => 4,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pump_dsp_rms(channel: usize) -> f64 {
    DSP.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or(0.0, |dsp| dsp.rms().get(channel).copied().unwrap_or(0.0))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pump_dsp_reset() -> u32 {
    DSP.with(|slot| match slot.borrow_mut().as_mut() {
        Some(dsp) => {
            dsp.reset();
            0
        }
        None => 4,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pump_dsp_destroy() {
    DSP.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_checks_lifecycle_and_bounds() {
        pump_dsp_destroy();
        assert_eq!(pump_dsp_abi_version(), 1);
        assert_eq!(pump_dsp_config_len(), 57);
        assert_eq!(pump_dsp_process(128), 4);
        assert!(pump_dsp_pcm_ptr().is_null());
        assert_eq!(pump_dsp_create(0), 1);
        assert_eq!(pump_dsp_create(48000), 0);
        assert!(!pump_dsp_pcm_ptr().is_null());
        assert_eq!(pump_dsp_configure(), 0);
        assert_eq!(pump_dsp_process(128), 0);
        assert_eq!(pump_dsp_process(pump_dsp_capacity() + 1), 3);
        assert_eq!(pump_dsp_reset(), 0);
        pump_dsp_destroy();
        assert_eq!(pump_dsp_configure(), 4);
    }
}
