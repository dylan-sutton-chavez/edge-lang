/* What Edge Python adds on top of the engine as FOREIGN kinds, the engine itself knowing none of them. */
pub mod bigint;
pub mod complex;
pub mod sentinel;
pub mod int;

// Wide ints register first so no constant boxes one before them.
pub fn install(heap: &mut crate::value::HeapPool) {
    heap.set_wide_int(&int::KIND);
    heap.know_kind(&complex::KIND);
    heap.know_kind(&sentinel::KIND);
}
