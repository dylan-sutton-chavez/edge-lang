/* `fn` items exported as plugin functions under `__fn_<name>` */
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_fn {
    ($($(#[$m:meta])* $v:vis fn $name:ident ($($a:ident : $t:ty),* $(,)?) $(-> $r:ty)? $body:block)*) => { $(
        $(#[$m])* $v fn $name($($a: $t),*) $(-> $r)? $body
        $crate::__plugin_export!([concat!("__fn_", stringify!($name))] stringify!($name); ($($a: $t),*) => $name($($a),*));
    )* };
}

/* zero-param `fn` items exported as module constants under `__const_<name>` */
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_const {
    ($($(#[$m:meta])* $v:vis fn $name:ident () $(-> $r:ty)? $body:block)*) => { $(
        $(#[$m])* $v fn $name() $(-> $r)? $body
        $crate::__plugin_export!([concat!("__const_", stringify!($name))] concat!("__const_", stringify!($name)); () => $name());
    )* };
}

/* the `fn` a waiting system call settles into exported as `__interpreter_resume` */
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_resume {
    ($(#[$m:meta])* $v:vis fn $name:ident ($c:ident : $ct:ty, $h:ident : $ht:ty $(,)?) $(-> $r:ty)? $body:block) => {
        $(#[$m])* $v fn $name($c: $ct, $h: $ht) $(-> $r)? $body
        const _: () = {
            #[doc(hidden)]
            #[unsafe(export_name = "__interpreter_resume")]
            extern "C" fn __interpreter_resume_export(call: u32, answer: u32, out: *mut u32) -> i32 {
                let run = |_: &[u32]| $crate::plugin::Ret::settle($name(call, $crate::plugin::__internals::answer(answer)));
                // SAFETY the host gives a resume no argv and one writable slot at out
                unsafe { $crate::plugin::__internals::export(core::ptr::null(), 0, out, run) }
            }
        };
    };
}

/* a struct whose instances a plugin keeps by the id the runtime object holds */
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_class {
    ($(#[$m:meta])* $v:vis struct $name:ident $($rest:tt)*) => {
        $(#[$m])* $v struct $name $($rest)*

        impl $name {
            #[doc(hidden)]
            pub fn __interpreter_state() -> &'static mut $crate::plugin::__internals::BTreeMap<i64, $name> {
                static STATE: $crate::plugin::PluginCell<$crate::plugin::__internals::BTreeMap<i64, $name>> = $crate::plugin::PluginCell::new();
                STATE.get_or_init($crate::plugin::__internals::BTreeMap::new)
            }

            #[doc(hidden)]
            pub fn __interpreter_insert(value: $name) -> i64 {
                static NEXT: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(1);
                let id = NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                Self::__interpreter_state().insert(id, value);
                id
            }
        }
    };
}

/* an impl block whose methods export as `__class_<Name>_<method>` and whose `#[ctor]` as `__class_<Name>_init` */
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_methods {
    (impl $ty:ident { $($body:tt)* }) => {
        $crate::__plugin_methods!(@each $ty [] [] $($body)*);
    };
    (@each $ty:ident [$($impl:tt)*] [$($exports:tt)*]) => {
        impl $ty { $($impl)* }
        $($exports)*
    };
    (@each $ty:ident [$($impl:tt)*] [$($exports:tt)*] #[ctor] $(#[$m:meta])* $v:vis fn $f:ident () -> $r:ty $body:block $($rest:tt)*) => {
        $crate::__plugin_methods!(@each $ty [$($impl)* $(#[$m])* $v fn $f() -> $r $body] [$($exports)*
            $crate::__plugin_export!([concat!("__class_", stringify!($ty), "_init")] concat!("__class_", stringify!($ty), "_init");
                (self_h: $crate::plugin::Handle) => $crate::plugin::__internals::construct(&self_h, $ty::__interpreter_insert($ty::$f())));
        ] $($rest)*);
    };
    (@each $ty:ident [$($impl:tt)*] [$($exports:tt)*] $(#[$m:meta])* $v:vis fn $f:ident (&mut $s:tt $(, $a:ident : $t:ty)* $(,)?) $(-> $r:ty)? $body:block $($rest:tt)*) => {
        $crate::__plugin_methods!(@each $ty [$($impl)* $(#[$m])* $v fn $f(&mut $s $(, $a: $t)*) $(-> $r)? $body] [$($exports)*
            $crate::__plugin_export!([concat!("__class_", stringify!($ty), "_", stringify!($f))] concat!("__class_", stringify!($ty), "_", stringify!($f));
                (self_h: $crate::plugin::Handle $(, $a: $t)*) => $crate::plugin::__internals::instance($ty::__interpreter_state(), &self_h).and_then(|i| $crate::plugin::Ret::settle(i.$f($($a),*))));
        ] $($rest)*);
    };
    (@each $ty:ident [$($impl:tt)*] [$($exports:tt)*] $(#[$m:meta])* $v:vis fn $f:ident (& $s:tt $(, $a:ident : $t:ty)* $(,)?) $(-> $r:ty)? $body:block $($rest:tt)*) => {
        $crate::__plugin_methods!(@each $ty [$($impl)* $(#[$m])* $v fn $f(& $s $(, $a: $t)*) $(-> $r)? $body] [$($exports)*
            $crate::__plugin_export!([concat!("__class_", stringify!($ty), "_", stringify!($f))] concat!("__class_", stringify!($ty), "_", stringify!($f));
                (self_h: $crate::plugin::Handle $(, $a: $t)*) => $crate::plugin::__internals::instance($ty::__interpreter_state(), &self_h).and_then(|i| $crate::plugin::Ret::settle(i.$f($($a),*))));
        ] $($rest)*);
    };
}

/* one export reading its params off the call's handles and writing its value where the host asked */
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_export {
    ([$($export:tt)*] $label:expr; ($($a:ident : $t:ty),*) => $call:expr) => {
        const _: () = {
            const __INTERPRETER_KINDS: &[$crate::plugin::Kind] = &[$(<$t as $crate::plugin::Param>::KIND),*];
            const _: () = $crate::plugin::__internals::check(__INTERPRETER_KINDS);

            #[doc(hidden)]
            #[unsafe(export_name = $($export)*)]
            extern "C" fn __interpreter_export(argv: *const u32, argc: u32, out: *mut u32) -> i32 {
                let run = |slots: &[u32]| -> $crate::plugin::Result<$crate::plugin::Handle> {
                    #[allow(unused_mut, unused_variables)]
                    let mut at = $crate::plugin::__internals::Cursor::open($label, __INTERPRETER_KINDS, slots)?;
                    $(let $a: $t = <$t as $crate::plugin::Param>::take(&mut at)?;)*
                    $crate::plugin::Ret::settle($call)
                };
                // SAFETY the host stages argc handles at argv and one writable slot at out for this call
                unsafe { $crate::plugin::__internals::export(argv, argc, out, run) }
            }
        };
    };
}

/* the allocator and panic handler every wasm32 plugin needs */
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_module {
    () => {
        // a plugin has one thread so the free list reuses freed blocks without a lock
        #[cfg(target_arch = "wasm32")]
        #[global_allocator]
        static __PLUGIN_ALLOC: $crate::plugin::__lol_alloc::AssumeSingleThreaded<$crate::plugin::__lol_alloc::FreeListAllocator> =
            unsafe { $crate::plugin::__lol_alloc::AssumeSingleThreaded::new($crate::plugin::__lol_alloc::FreeListAllocator::new()) };

        #[cfg(target_arch = "wasm32")]
        #[panic_handler]
        fn __plugin_panic(_: &core::panic::PanicInfo) -> ! {
            core::arch::wasm32::unreachable()
        }
    };
}

/* `module!` on a 4 MiB pool that never grows memory and emits nothing off wasm32 */
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_module_fixed_pool {
    () => { $crate::__plugin_module_fixed_pool!(4 * 1024 * 1024); };
    ($pool:expr) => {
        #[cfg(target_arch = "wasm32")]
        mod __plugin_allocator {
            use core::alloc::{GlobalAlloc, Layout};
            use core::cell::UnsafeCell;
            use core::ptr::NonNull;
            use core::sync::atomic::{AtomicBool, Ordering};
            use $crate::plugin::__lla::Heap;

            const POOL_SIZE: usize = $pool;

            #[repr(align(16))]
            struct Pool(UnsafeCell<[u8; POOL_SIZE]>);
            unsafe impl Sync for Pool {}

            static POOL: Pool = Pool(UnsafeCell::new([0; POOL_SIZE]));

            struct HeapCell(UnsafeCell<Heap>);
            unsafe impl Sync for HeapCell {}

            static HEAP: HeapCell = HeapCell(UnsafeCell::new(Heap::empty()));
            static INIT: AtomicBool = AtomicBool::new(false);

            pub struct FreeListAlloc;

            unsafe impl GlobalAlloc for FreeListAlloc {
                unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                    if !INIT.load(Ordering::Relaxed) {
                        unsafe { (*HEAP.0.get()).init(POOL.0.get() as *mut u8, POOL_SIZE); }
                        INIT.store(true, Ordering::Relaxed);
                    }
                    unsafe {
                        (*HEAP.0.get())
                            .allocate_first_fit(layout)
                            .map_or(core::ptr::null_mut(), |p| p.as_ptr())
                    }
                }

                unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                    unsafe { (*HEAP.0.get()).deallocate(NonNull::new_unchecked(ptr), layout); }
                }
            }
        }

        #[cfg(target_arch = "wasm32")]
        #[global_allocator]
        static __PLUGIN_ALLOC: __plugin_allocator::FreeListAlloc = __plugin_allocator::FreeListAlloc;

        #[cfg(target_arch = "wasm32")]
        #[panic_handler]
        fn __plugin_panic(_: &core::panic::PanicInfo) -> ! {
            core::arch::wasm32::unreachable()
        }
    };
}
