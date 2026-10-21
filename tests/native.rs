/* The GC sanity check of native code, over the `op_` handlers a glue calls. */

// Every run meters what it allocates, as the native build does.
#[global_allocator]
static METERED: compiler::value::meter::Metered<std::alloc::System> = compiler::value::meter::Metered(std::alloc::System);

use compiler::code::{OpCode, effect};
use compiler::native::{self, is_err};

const UNDEF: i64 = 0x7FFC_0000_0000_0000;
const NONE: i64 = 0x7FFC_0000_0000_0001;

/* A collection leaves every live value at its bits and its object, and a root naming a free or a reused slot harms nothing.
   Native code keeps values in registers between safepoints and stores them only there. */
#[test]
fn gc_keeps_values_in_place() {
    const WHY: &str = "edge build's native code stores live values at safepoints, update it if the GC moves objects or collects elsewhere";
    let src = "a = [1, 2]\nb = 'x' * 3\nc = {'k': a}\nd = [3]\n";
    let rt = unsafe { native::boot(src.as_ptr(), src.len(), None, 1 << 26, true) };
    let call = move |op: OpCode, site: i64, a: &[i64]| {
        let at = |k: usize| a.get(k).copied().unwrap_or(UNDEF);
        unsafe { native::handler(op).unwrap()(rt, site, at(0), at(1), at(2), std::ptr::null_mut(), std::ptr::null_mut(), 0) }
    };
    // The module runs straight through, each instruction through its handler as the glue calls it.
    let (chunk, mut stack) = (native::chunk(unsafe { &*rt }, 0), Vec::new());
    for (ip, ins) in chunk.instructions.iter().enumerate().filter(|(_, i)| i.opcode != OpCode::ReturnValue) {
        let (pops, pushes) = effect(ins, chunk).unwrap();
        let args = stack.split_off(stack.len() - pops as usize);
        let r = call(ins.opcode, (ip as i64) << 16 | ins.operand as i64, &args);
        assert!(!is_err(r), "{:?}", ins.opcode);
        if pushes == 1 { stack.push(r); }
    }
    let load = |slot: i64| call(OpCode::LoadName, slot, &[]);
    let (live, kinds): (Vec<i64>, Vec<u32>) = (0..3).map(|s| (load(s), unsafe { native::heap_type(rt, load(s)) })).unzip();
    // The list `d` held is garbage once it holds None, its slot free once collected.
    let freed = load(3);
    call(OpCode::StoreName, 3, &[NONE]);
    native::collect(unsafe { &mut *rt }, &mut live.clone());
    assert_eq!(unsafe { native::heap_type(rt, freed) }, u32::MAX, "{WHY}");
    native::collect(unsafe { &mut *rt }, &mut [live.clone(), vec![freed, freed + 0x7FF0]].concat());
    let again: Vec<i64> = (0..3).map(load).collect();
    assert_eq!(again, live, "{WHY}");
    assert_eq!(again.iter().map(|&v| unsafe { native::heap_type(rt, v) }).collect::<Vec<_>>(), kinds, "{WHY}");
    assert_eq!(unsafe { native::heap_type(rt, freed) }, u32::MAX, "{WHY}");
}
