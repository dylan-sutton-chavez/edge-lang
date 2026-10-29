use wasmtime::Config;

/* the settings the precompiled artifacts and the runtime engine must agree on */
pub fn base() -> Config {
    let mut cfg = Config::new();
    cfg.epoch_interruption(true);
    cfg.consume_fuel(false);
    // a full 4 GiB reservation lets Cranelift drop bounds checks on loads and stores
    cfg.memory_reservation(4 << 30);
    cfg.memory_guard_size(32 << 20);
    cfg.memory_init_cow(true);
    // `MAX_NESTED` engine loops take up to about 620 KiB of native stack on x86
    cfg.max_wasm_stack(1 << 20);
    cfg
}
