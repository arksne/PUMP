// Отдельный экземпляр WASM на узел; никакого fetch или wasm-bindgen glue на аудиопотоке.
class PumpRustDsp extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.live = true;
    this.failed = false;
    this.meterFrames = 0;
    this.port.onmessage = event => this.command(event.data);
    try {
      const module = options.processorOptions.module;
      let instance;
      const imports = Object.create(null);
      for (const item of WebAssembly.Module.imports(module)) {
        // Пакет ui-liquid-glass 0.2.130 использует только инициализацию externref.
        // Неизвестные импорты отклоняются: фиктивные функции скрыли бы несовместимый ABI.
        if (item.kind !== "function" || item.name !== "__wbindgen_init_externref_table") {
          throw new Error("Неподдерживаемый WASM import: " + item.module + "." + item.name);
        }
        imports[item.module] ??= Object.create(null);
        imports[item.module][item.name] = () => {
          const table = instance.exports.__wbindgen_externrefs;
          if (!(table instanceof WebAssembly.Table)) throw new Error("Нет таблицы externref WASM.");
          const offset = table.grow(4);
          table.set(0, undefined);
          table.set(offset, undefined);
          table.set(offset + 1, null);
          table.set(offset + 2, true);
          table.set(offset + 3, false);
        };
      }
      instance = new WebAssembly.Instance(module, imports);
      const wasm = instance.exports;
      const required = ["pump_dsp_abi_version", "pump_dsp_create", "pump_dsp_capacity",
        "pump_dsp_config_len", "pump_dsp_config_ptr", "pump_dsp_pcm_ptr", "pump_dsp_configure",
        "pump_dsp_process", "pump_dsp_reset", "pump_dsp_rms", "pump_dsp_destroy"];
      if (required.some(name => typeof wasm[name] !== "function") || !(wasm.memory instanceof WebAssembly.Memory)) {
        throw new Error("WASM-пакет собран без DSP ABI. Пересоберите ui-liquid-glass.");
      }
      if (wasm.pump_dsp_abi_version() !== 1 || wasm.pump_dsp_config_len() !== 57) {
        throw new Error("Несовместимая версия DSP ABI.");
      }
      wasm.__wbindgen_start?.();
      if (wasm.pump_dsp_create(sampleRate) !== 0) throw new Error("Неподдерживаемая частота DSP: " + sampleRate);
      this.wasm = wasm;
      this.capacity = wasm.pump_dsp_capacity();
      this.refreshViews();
      this.port.postMessage({type:"ready", capacity:this.capacity, sampleRate, abi:1});
    } catch (error) {
      this.fail(error);
    }
  }

  refreshViews() {
    if (this.buffer !== this.wasm.memory.buffer) {
      this.buffer = this.wasm.memory.buffer;
      this.pcm = new Float32Array(this.buffer, this.wasm.pump_dsp_pcm_ptr(), this.capacity * 2);
      this.config = new Float64Array(this.buffer, this.wasm.pump_dsp_config_ptr(), 57);
    }
  }

  command(message) {
    if (!message) return;
    const {type, id} = message;
    try {
      // Освобождение работает и после fatal/ошибки конструктора.
      if (type === "dispose") {
        this.wasm?.pump_dsp_destroy();
        this.live = false;
        this.port.postMessage({type:"ack", id});
        return;
      }
      if (!this.wasm || this.failed) return;
      let code;
      if (type === "configure") {
        if (!(message.values instanceof Float64Array) || message.values.length !== 57) {
          throw new Error("Неверный размер конфигурации DSP.");
        }
        this.refreshViews();
        this.config.set(message.values);
        code = this.wasm.pump_dsp_configure();
      } else if (type === "reset") {
        code = this.wasm.pump_dsp_reset();
        this.meterFrames = 0;
      } else {
        throw new Error("Неизвестная команда DSP.");
      }
      if (code !== 0) throw new Error("DSP " + type + ": код " + code);
      this.port.postMessage({type:"ack", id});
    } catch (error) {
      this.port.postMessage({type:"error", id, message:String(error.message || error)});
    }
  }

  fail(error) {
    this.failed = true;
    this.port.postMessage({type:"fatal", message:String(error.message || error)});
  }

  process(inputs, outputs) {
    if (!this.live) return false;
    const input = inputs[0], output = outputs[0];
    if (!output?.length) return true;
    const left = input?.[0], right = input?.[1] || left;
    const outLeft = output[0], outRight = output[1];
    const count = outLeft.length;
    if (!this.failed) {
      try {
        if (count > this.capacity) throw new Error("Аудиоблок превышает лимит DSP.");
        this.refreshViews();
        // Не создаём subarray/массивы на каждом render quantum; моно дублируется в стерео.
        for (let i = 0; i < count; i++) {
          this.pcm[i * 2] = left ? left[i] : 0;
          this.pcm[i * 2 + 1] = right ? right[i] : 0;
        }
        const code = this.wasm.pump_dsp_process(count);
        if (code !== 0) throw new Error("Обработка PCM: код " + code);
        for (let i = 0; i < count; i++) {
          const l = this.pcm[i * 2], r = this.pcm[i * 2 + 1];
          if (!Number.isFinite(l) || !Number.isFinite(r)) throw new Error("Неконечный выход DSP.");
          outLeft[i] = l;
          if (outRight) outRight[i] = r;
        }
        this.meterFrames += count;
        if (this.meterFrames >= sampleRate / 10) {
          this.meterFrames = 0;
          this.port.postMessage({type:"meter", rms:[this.wasm.pump_dsp_rms(0), this.wasm.pump_dsp_rms(1)]});
        }
        return true;
      } catch (error) {
        this.fail(error);
      }
    }
    // До переключения клиентом на обход сохраняем исходный сигнал.
    for (let i = 0; i < count; i++) {
      outLeft[i] = Number.isFinite(left?.[i]) ? left[i] : 0;
      if (outRight) outRight[i] = Number.isFinite(right?.[i]) ? right[i] : 0;
    }
    return true;
  }
}

registerProcessor("pump-rust-dsp", PumpRustDsp);
