use edge::lang::{Fast1, Fast2, FastOut, Intrinsic, Native, NativeFn, Sig, VAR};

/* the runtime and the name one table entry gives the kernel it registers */
pub trait Site: 'static {
    // the runtime whose choices the kernel follows
    type Lang: crate::Lang;
    // the name the entry registers which the kernel's failures name
    const NAME: &'static str;
    // names for the positionals a keyword may fill where an empty name stays positional
    const PARAMS: &'static [&'static str] = &[];
    // names for the keyword only slots the kernel reads in order
    const KEYWORDS: &'static [&'static str] = &[];
}

/* a native's body with its signature fast paths and flags and no name yet */
#[derive(Clone, Copy)]
pub struct Kernel {
    native: Native,
    // keyword only slots the body reads in order
    slots: u8,
}

impl Kernel {
    /* a kernel taking the positionals `sig` counts with no keyword named yet */
    pub const fn new(sig: Sig, run: NativeFn) -> Self { Kernel { native: Native::new("", sig, run), slots: 0 } }

    /* the kernel reading `n` keyword only slots which the runtime names */
    pub const fn reads(self, n: u8) -> Self { Kernel { slots: n, ..self } }

    pub const fn fast(self, f: Fast1) -> Self { Kernel { native: self.native.fast(f), ..self } }

    pub const fn fast2(self, f: Fast2) -> Self { Kernel { native: self.native.fast2(f), ..self } }

    pub const fn fast_out(self, f: FastOut) -> Self { Kernel { native: self.native.fast_out(f), ..self } }

    pub const fn flags(self, flags: u8) -> Self { Kernel { native: self.native.flags(flags), ..self } }

    pub const fn intrinsic(self, intrinsic: Intrinsic) -> Self { Kernel { native: self.native.intrinsic(intrinsic), ..self } }

    /* names for positionals a keyword may fill where an empty name stays positional */
    pub const fn params(self, names: &'static [&'static str]) -> Self {
        let sig = self.native.sig;
        assert!(sig.max == VAR || names.len() <= sig.max as usize, "more parameter names than positionals");
        Kernel { native: Native { sig: sig.named(names), ..self.native }, ..self }
    }

    /* names for the keyword only slots in order where extra names are taken and ignored */
    pub const fn keywords(self, names: &'static [&'static str]) -> Self {
        assert!(names.is_empty() || names.len() >= self.slots as usize, "a keyword slot of the kernel is left unnamed");
        Kernel { native: Native { sig: self.native.sig.kw(names), ..self.native }, ..self }
    }

    /* the native a table registers under `name` */
    pub const fn named(self, name: &'static str) -> Native { Native { name, ..self.native } }
}
