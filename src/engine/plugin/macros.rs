/* `fn` items exported as plugin functions under `__fn_<name>` and those marked `#[constant]` as module constants under `__const_<name>` */
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_fn {
    () => {};
    (#[constant] $(#[$m:meta])* $v:vis fn $name:ident () $(-> $r:ty)? $body:block $($rest:tt)*) => {
        $(#[$m])* $v fn $name() $(-> $r)? $body
        $crate::__plugin_export!([concat!("__const_", stringify!($name))] concat!("__const_", stringify!($name)); () => $name());
        $crate::__plugin_fn!($($rest)*);
    };
    ($(#[$m:meta])* $v:vis fn $name:ident ($($a:ident : $t:ty),* $(,)?) $(-> $r:ty)? $body:block $($rest:tt)*) => {
        $(#[$m])* $v fn $name($($a: $t),*) $(-> $r)? $body
        $crate::__plugin_export!([concat!("__fn_", stringify!($name))] stringify!($name); ($($a: $t),*) => $name($($a),*));
        $crate::__plugin_fn!($($rest)*);
    };
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
                let run = |_: &[u32]| $crate::__private::Ret::settle($name(call, $crate::__private::answer(answer)));
                // SAFETY the host gives a resume no argv and one writable slot at out
                unsafe { $crate::__private::export(core::ptr::null(), 0, out, run) }
            }
        };
    };
}

/* a struct whose instances a plugin keeps by the id the runtime object holds and its `impl` of exported methods */
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_class {
    (@state $name:ident) => {
        impl $name {
            #[doc(hidden)]
            pub fn __interpreter_state() -> &'static mut $crate::__private::BTreeMap<i64, $name> {
                static STATE: $crate::plugin::PluginCell<$crate::__private::BTreeMap<i64, $name>> = $crate::plugin::PluginCell::new();
                STATE.get_or_init($crate::__private::BTreeMap::new)
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
    ($(#[$m:meta])* $v:vis struct $name:ident { $($fields:tt)* } $(impl $ty:ident { $($body:tt)* })?) => {
        $(#[$m])* $v struct $name { $($fields)* }
        $crate::__plugin_class!(@state $name);
        $($crate::__plugin_methods!(impl $ty { $($body)* });)?
    };
    ($(#[$m:meta])* $v:vis struct $name:ident ( $($fields:tt)* ); $(impl $ty:ident { $($body:tt)* })?) => {
        $(#[$m])* $v struct $name ( $($fields)* );
        $crate::__plugin_class!(@state $name);
        $($crate::__plugin_methods!(impl $ty { $($body)* });)?
    };
    ($(#[$m:meta])* $v:vis struct $name:ident; $(impl $ty:ident { $($body:tt)* })?) => {
        $(#[$m])* $v struct $name;
        $crate::__plugin_class!(@state $name);
        $($crate::__plugin_methods!(impl $ty { $($body)* });)?
    };
}

/* the methods of a `plugin_class!` exported as `__class_<Name>_<method>` and its `#[ctor]` as `__class_<Name>_init` */
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
                (self_h: $crate::plugin::Handle) => $crate::__private::construct(&self_h, $ty::__interpreter_insert($ty::$f())));
        ] $($rest)*);
    };
    (@each $ty:ident [$($impl:tt)*] [$($exports:tt)*] $(#[$m:meta])* $v:vis fn $f:ident (&mut $s:tt $(, $a:ident : $t:ty)* $(,)?) $(-> $r:ty)? $body:block $($rest:tt)*) => {
        $crate::__plugin_methods!(@each $ty [$($impl)* $(#[$m])* $v fn $f(&mut $s $(, $a: $t)*) $(-> $r)? $body] [$($exports)*
            $crate::__plugin_export!([concat!("__class_", stringify!($ty), "_", stringify!($f))] concat!("__class_", stringify!($ty), "_", stringify!($f));
                (self_h: $crate::plugin::Handle $(, $a: $t)*) => $crate::__private::instance($ty::__interpreter_state(), &self_h).and_then(|i| $crate::__private::Ret::settle(i.$f($($a),*))));
        ] $($rest)*);
    };
    (@each $ty:ident [$($impl:tt)*] [$($exports:tt)*] $(#[$m:meta])* $v:vis fn $f:ident (& $s:tt $(, $a:ident : $t:ty)* $(,)?) $(-> $r:ty)? $body:block $($rest:tt)*) => {
        $crate::__plugin_methods!(@each $ty [$($impl)* $(#[$m])* $v fn $f(& $s $(, $a: $t)*) $(-> $r)? $body] [$($exports)*
            $crate::__plugin_export!([concat!("__class_", stringify!($ty), "_", stringify!($f))] concat!("__class_", stringify!($ty), "_", stringify!($f));
                (self_h: $crate::plugin::Handle $(, $a: $t)*) => $crate::__private::instance($ty::__interpreter_state(), &self_h).and_then(|i| $crate::__private::Ret::settle(i.$f($($a),*))));
        ] $($rest)*);
    };
}

/* one export reading its params off the call's handles and writing its value where the host asked */
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_export {
    ([$($export:tt)*] $label:expr; ($($a:ident : $t:ty),*) => $call:expr) => {
        const _: () = {
            const __INTERPRETER_KINDS: &[$crate::__private::ParamKind] = &[$(<$t as $crate::__private::Param>::KIND),*];
            const _: () = $crate::__private::check(__INTERPRETER_KINDS);

            #[doc(hidden)]
            #[unsafe(export_name = $($export)*)]
            extern "C" fn __interpreter_export(argv: *const u32, argc: u32, out: *mut u32) -> i32 {
                let run = |slots: &[u32]| -> $crate::plugin::Result<$crate::plugin::Handle> {
                    #[allow(unused_mut, unused_variables)]
                    let mut at = $crate::__private::Cursor::open($label, __INTERPRETER_KINDS, slots)?;
                    $(let $a: $t = <$t as $crate::__private::Param>::take(&mut at)?;)*
                    $crate::__private::Ret::settle($call)
                };
                // SAFETY the host stages argc handles at argv and one writable slot at out for this call
                unsafe { $crate::__private::export(argv, argc, out, run) }
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
        static __PLUGIN_ALLOC: $crate::__private::lol_alloc::AssumeSingleThreaded<$crate::__private::lol_alloc::FreeListAllocator> =
            unsafe { $crate::__private::lol_alloc::AssumeSingleThreaded::new($crate::__private::lol_alloc::FreeListAllocator::new()) };

        #[cfg(target_arch = "wasm32")]
        #[panic_handler]
        fn __plugin_panic(_: &core::panic::PanicInfo) -> ! {
            core::arch::wasm32::unreachable()
        }
    };
}
