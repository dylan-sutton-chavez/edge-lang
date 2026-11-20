use interpreter::abi::WireValue;
use interpreter::code::SSAChunk;
use interpreter::lang::{Descriptor, Front, Natives, Policy, Runtime};
use interpreter::lexer::Syntax;
use interpreter::value::HeapPool;
use interpreter::{Diagnostic, Limits, NativeBinding, Val, VM, VmErr};

const fn shared<T: Send + Sync>() {}

// a type that cannot move between threads meets one impl alone so the call resolves
trait NotSend<A> { fn held() {} }
impl<T: ?Sized> NotSend<()> for T {}
impl<T: ?Sized + Send> NotSend<u8> for T {}

trait NotSync<A> { fn held() {} }
impl<T: ?Sized> NotSync<()> for T {}
impl<T: ?Sized + Sync> NotSync<u8> for T {}

// the tables and values a host hands between threads may cross and be shared
const _: () = {
    shared::<Val>();
    shared::<VmErr>();
    shared::<Limits>();
    shared::<Diagnostic>();
    shared::<Runtime>();
    shared::<Front>();
    shared::<Natives>();
    shared::<Syntax>();
    shared::<Policy>();
    shared::<Descriptor>();
    shared::<NativeBinding>();
    shared::<WireValue>();
};

// a machine its heap and its compiled code stay on the thread that made them
#[test]
fn a_machine_stays_on_its_thread() {
    <VM<'static> as NotSend<_>>::held();
    <VM<'static> as NotSync<_>>::held();
    <HeapPool as NotSend<_>>::held();
    <HeapPool as NotSync<_>>::held();
    <SSAChunk as NotSend<_>>::held();
    <SSAChunk as NotSync<_>>::held();
}
