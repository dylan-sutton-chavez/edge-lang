/* Exports of compiler.wasm shared by every worker-side module, optional members appear only on newer builds. */
export interface CompilerExports {
    memory: WebAssembly.Memory
    out_ptr(): number
    out_len(): number
    wasm_alloc(size: number): number
    wasm_free(ptr: number, size: number): void
    register_native_module(spec_ptr: number, spec_len: number, names_ptr: number, names_len: number, base_id: number): void
    register_module_error(spec_ptr: number, spec_len: number, msg_ptr: number, msg_len: number): void
    reset_modules(): void
    walk_start(src_ptr: number, src_len: number, system_ptr: number, system_len: number): number
    walk_fetched(ptr: number, len: number, kind: number): number
    walk_plugin_bytes(): number
    walk_plugin(kind: number, ptr: number, len: number): number
    walk_served(ptr: number, len: number): number
    walk_known(ptr: number, len: number): number
    set_entry(ptr: number, len: number): void
    set_input?(ptr: number, len: number): void
    repl_eval(ptr: number, len: number): number
    compile_warnings?(ptr: number, len: number): number
    run_start(ptr: number, len: number): number
    run_resume(): number
    run_push_event(ptr: number, len: number): number
    set_host_result_by_id(id: number, handle: number): number
    set_host_error_by_id(id: number, kind: number, msg_handle: number): number
    set_preempt_interval?(n: number): void
    set_limits?(memory: bigint, ops: bigint): void
    save_state(): bigint
    restore_state(ptr: number, len: number): number
    state_globals(): number
    state_stack(): number
    vm_create(): number
    vm_select(id: number): number
    vm_drop(id: number): number
    host_edge_op(op: number, recv: number, name_ptr: number, name_len: number, argv_ptr: number, argc: number, out_handle: number): number
    host_edge_encode(tag: number, ptr: number, len: number): number
    host_edge_decode(h: number, out_tag: number, dst: number, dst_max: number): number
    host_edge_release(h: number): void
    host_edge_throw(kind: number, msg_ptr: number, msg_len: number): void
    host_edge_take_error(out_kind: number, dst: number, dst_max: number): number
}
