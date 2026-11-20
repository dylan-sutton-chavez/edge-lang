#![cfg_attr(not(feature = "std"), no_std)]
#![doc = include_str!("../README.md")]
// the build with every feature alone denies dead code since the others leave helpers idle
#![cfg_attr(not(all(feature = "cli", feature = "coverage", feature = "exports", feature = "full", feature = "plugin")), allow(dead_code))]
// the internals feature exports engine pieces no release promises to grow without a break
#![cfg_attr(feature = "unstable-internals", allow(clippy::exhaustive_enums, clippy::exhaustive_structs))]
// only the internals build exports every pub item so only it tells one unreachable
#![cfg_attr(not(feature = "unstable-internals"), allow(unreachable_pub))]

extern crate alloc;

// a method the engine's own tests call is public only under the internals feature
macro_rules! internal {
    // what only those tests call is absent from every other build
    (tests $(#[$m:meta])* pub $($item:tt)*) => {
        #[cfg(feature = "unstable-internals")]
        $(#[$m])* pub $($item)*
    };
    ($(#[$m:meta])* pub $($item:tt)*) => {
        #[cfg(feature = "unstable-internals")]
        $(#[$m])* pub $($item)*
        #[cfg(not(feature = "unstable-internals"))]
        $(#[$m])* pub(crate) $($item)*
    };
}

// the const setters a runtime chains from a base each named for its field
macro_rules! setters {
    ($($field:ident: $ty:ty),* $(,)?) => {
        $(pub const fn $field(mut self, $field: $ty) -> Self { self.$field = $field; self })*
    };
}

// the implementation behind the public modules below which take its module names
mod engine;

pub(crate) use crate::engine::util::fstr::s;

// the traits a user calls or names but only the crate implements take this unnameable bound
mod sealed {
    #[allow(unnameable_types)]
    pub trait Sealed {}
}

// the opcode table macro stays in the crate unless the internals feature exports it
#[cfg(not(feature = "unstable-internals"))]
pub(crate) use crate::engine::code::ops::ops;

/* an embedder compiles a program and runs it on a VM under limits with host modules */
pub use crate::engine::deps::{NativeClass, NoopResolver, Resolved, Resolver};
pub use crate::engine::code::Diagnostic;
#[cfg(feature = "unstable-internals")]
pub use crate::engine::code::{compile, compile_as};
pub use crate::engine::syntax::diag::Lines;
pub use crate::engine::value::{ExternFn as NativeBinding, Limits, SchedulerStatus, Val, VmErr};
pub use crate::engine::vm::VM;
#[cfg(feature = "unstable-internals")]
pub use crate::engine::vm::embed::call_native;

/* a VM saved to bytes and restored over a fresh VM of the same runtime */
pub mod snapshot {
    pub use crate::engine::vm::snapshot::{entry_of, format_of, limits_of, restore, save, SnapErr, FORMAT};
    #[cfg(feature = "unstable-internals")]
    pub use crate::engine::vm::snapshot::{inspect_globals, inspect_stack, saved_under, save_swept, source_of, FORMAT_AT, OTHER_RUNTIME, RUNTIME_AT};
}

/* the ready host a native binary runs one file of a runtime with */
#[cfg(feature = "std")]
pub mod native {
    pub use crate::engine::targets::native::{main, Script};
}

/* the compiled program and the builder a front end emits it with */
pub mod code {
    pub use crate::engine::deps::ImportSpec;
    pub use crate::engine::code::{build, constant_fold, Annotated, Annotation, Builder, Decl, DeclKind, ImportEntry, ImportKind, Node, NodeKind, Notes, Parsed, Recorded, SSAChunk, Unit, Value as Constant};
    #[cfg(feature = "unstable-internals")]
    pub use crate::engine::{code::{native_site, read_as, Instruction, NativeSite, OpCode, Rows, ops::{Class, Fast}, Operand, Operator, Parser, Program, SPREAD_BASES}, syntax::{parser::Arms, ssa::ssa_strip}, vm::{Binding, BodyKind, FnScope, Kind, ParamKind}};
}

/* the token table a runtime spells its language with */
pub mod lexer {
    pub use crate::engine::syntax::lexer::{index, lex_as, operators, Binding, Blocks, Comments, Cond, Continuation, Dot, Ends, Escapes, Interp, Keywords, Lambda, LexError, Literal, Mode as Conversion, Numbers, Quote, Shapes, Spelling, Strings, Suffix, Syntax, Token, TokenType, Unicode, Unknown, Zeros, BYTES};
    #[cfg(feature = "unstable-internals")]
    pub use crate::engine::syntax::lexer::{lex_forward, syntax::FORMAT, RAW};

    /* the rows a grammar reads expressions with beside the token table */
    pub mod grammar {
        pub use crate::engine::syntax::lexer::grammar::{Emit, Form, Grammar, Mode as Reading, Pattern, Row, Slot, Test};
    }
}

/* a runtime as a language the engine runs with its policy names and natives */
pub mod lang {
    pub use crate::engine::lang::{plain, Act, Args, Body, CallFault, Callee, Cmp, Cx, Descriptor, EngineError, ErrKind, Falsy, Fields, Fit, Front, Gen, IntDiv, IntMod, KindId, Language, MapIter, Misfit, Mode as Show, Names, NegPow, Native, NativeFn, NativeId, Natives, NumEq, Op, OpTable, Out, Overflow, ParseFn, Policy, Ret, Runtime, Sig, StrIter, Tree, Use, WordsFn, ZeroDiv};
    // the protocol a native calls back into user code with through `Out::Step`
    pub use crate::engine::lang::{Acc, Cont, Flow, Fold, FoldFn, Item, Sent, Step, TakeFn, Then, ThenFn};
    #[cfg(feature = "unstable-internals")]
    pub use crate::engine::lang::{kind_of, raised, OpSet, OwnOp, OwnTable, Access, CallRule, CallShape, Fast1, Fast2, FastOut, GetRule, Intrinsic, Key, Name, OpRule, ERRORS, ITERS, KINDS, STEPS};
}

/* the values and the heap natives read and build */
pub mod value {
    pub use crate::engine::value::{as_i128, HeapPool, KindIter as IterFrame};
    #[cfg(feature = "unstable-internals")]
    pub use crate::engine::value::{eq::hash_val_with_heap, meter, pool, Cause, DictMap, HeapObj, ValSet};

    /* the host types a runtime adds to the heap and how each behaves */
    pub mod foreign {
        pub use crate::engine::value::foreign::{BinOp, Built, CallFn, CutFn, GetFn, ItemFn, IterFn, Kind, KindToken, LenFn, Snapshot, Store, Transfer, UnOp};
    }
}

/* the wire a host and a plugin pass values and errors over */
pub mod abi {
    pub use crate::engine::abi::{ErrorKind, WireValue, NATIVE_ABI, PLUGIN_ABI, RUNTIME_ABI};
    #[cfg(feature = "unstable-internals")]
    pub use crate::engine::abi::{classify_decode, classify_encode, DecodeBits, EncodeRequest, ErrorStash, HandleTable, PrimitiveBytes, MAX_WIRE_DEPTH};
}

/* the kernels a runtime names in its own table with each module behind a feature */
#[cfg(feature = "lib")]
pub mod lib {
    pub use crate::engine::lib::{Kernel, Lang, Site};
    pub use crate::engine::lib::lang::{Centre, Codec, Endian, Machine, Numerals, OnError, Specs};
    #[cfg(feature = "unstable-internals")]
    pub use crate::engine::lib::lang::Tree as Classes;
    #[cfg(feature = "bytes")]
    pub mod bytes {
        pub use crate::engine::lib::bytes::{Buf, Fixed, Mutable, new};
        pub mod codec {
            pub use crate::engine::lib::bytes::codec::{decode, encode};
        }
        pub mod hex {
            pub use crate::engine::lib::bytes::hex::{from_hex, hex, unhex};
        }
        pub mod methods {
            pub use crate::engine::lib::bytes::methods::{capitalize, center, clear, copy, count, drop_prefix, drop_suffix, ends, extend, find, index, insert, is_alnum, is_alpha, is_ascii, is_digit, is_lower, is_space, is_title, is_upper, join, lines, lower, pad_end, pad_start, partition, pop, push, remove, replace, resize, reverse, rfind, rindex, rpartition, rsplit, split, starts, swap, tabs, take, title, trim, trim_end, trim_start, upper, zeros};
        }
    }
    #[cfg(feature = "data")]
    pub mod data {
        pub use crate::engine::lib::data::lines::lines;
        #[cfg(feature = "unstable-internals")]
        pub use crate::engine::lib::data::lines::Lines;
        pub mod csv {
            pub use crate::engine::lib::data::csv::{read, write};
        }
        pub mod json {
            pub use crate::engine::lib::data::json::{parse, read, write, write_into};
        }
    }
    #[cfg(feature = "date")]
    pub mod date {
        pub use crate::engine::lib::date::{Dates, day, days, duration_kind, hour, hours, minute, minutes, month, parse, second, seconds, time_kind, year};
        #[cfg(feature = "unstable-internals")]
        pub use crate::engine::lib::date::{Civil, Duration, Time, duration_val, part_of, time_val};
    }
    pub mod fail {
        pub use crate::engine::lib::fail::{At, Class, Coding, Csv, Error, Fail, Json, Spec, ToJson, english, raise};
        #[cfg(feature = "unstable-internals")]
        pub use crate::engine::lib::fail::fail;
    }
    pub mod fmt {
        pub use crate::engine::lib::fmt::render::{Escape, Floats, ascii_of, repr_of, show};
        #[cfg(feature = "spec")]
        pub use crate::engine::lib::fmt::spec::format;
        #[cfg(feature = "unstable-internals")]
        pub use crate::engine::lib::fmt::render::{ascii_escape, format_f64, push_bytes, push_repr_str};
    }
    #[cfg(feature = "io")]
    pub mod io {
        pub use crate::engine::lib::io::{exit, print, read_line, receive, send};
    }
    #[cfg(feature = "iter")]
    pub mod iter {
        pub use crate::engine::lib::iter::{backward, close, filtered, iter, mapped, next, numbered, range, range_count, range_index, send, throw, zipped};
        pub mod fold {
            pub use crate::engine::lib::iter::fold::{Adds, Joins, Refuse, every, least, most, some, sum};
        }
    }
    #[cfg(feature = "map")]
    pub mod map {
        pub use crate::engine::lib::map::{clear, copy, from_keys, get, items, keys, new, pop, pop_last, set_default, update, values};
    }
    #[cfg(feature = "num")]
    pub mod num {
        pub use crate::engine::lib::num::abs;
        pub use crate::engine::lib::num::big::int_kind;
        pub use crate::engine::lib::num::div::divmod;
        pub use crate::engine::lib::num::hash::{Scheme, hash};
        pub use crate::engine::lib::num::pow::pow;
        pub use crate::engine::lib::num::round::{HalfAway, HalfEven, Round, round};
        #[cfg(feature = "unstable-internals")]
        pub use crate::engine::lib::num::big::bigint::BigInt;
        pub mod float {
            pub use crate::engine::lib::num::float::{from_hex, hex, is_whole, new};
        }
        pub mod int {
            pub use crate::engine::lib::num::int::{Bin, Hex, Oct, Radix, bit_count, bit_length, bytes_int, from_bytes, int_bytes, new, radix, to_bytes};
            #[cfg(feature = "unstable-internals")]
            pub use crate::engine::lib::num::int::{Parse, literal, parse};
        }
    }
    #[cfg(feature = "types")]
    pub mod reflect {
        pub use crate::engine::lib::reflect::{delete, get, has, set};
    }
    #[cfg(feature = "seq")]
    pub mod seq {
        pub use crate::engine::lib::seq::{clear, copy, count, extend, index, insert, len, list, pop, push, remove, reverse, slice_kind, tuple};
        pub use crate::engine::lib::seq::sort::{sort, sorted};
        pub use crate::engine::lib::seq::top::top;
        #[cfg(feature = "unstable-internals")]
        pub use crate::engine::lib::seq::top::greatest;
    }
    #[cfg(feature = "set")]
    pub mod set {
        pub use crate::engine::lib::set::{add, clear, copy, difference, difference_update, discard, disjoint, intersection, intersection_update, new, pop, remove, subset, superset, symmetric, symmetric_update, union, update};
    }
    #[cfg(feature = "text")]
    pub mod text {
        pub use crate::engine::lib::text::{char_of, code_of, text};
        pub mod case {
            pub use crate::engine::lib::text::case::{Case, Full, Simple, capitalize, fold, lower, swap, title, upper};
        }
        pub mod class {
            pub use crate::engine::lib::text::class::{is_alnum, is_alpha, is_ascii, is_digit, is_lower, is_printable, is_space, is_title, is_upper};
        }
        pub mod cut {
            pub use crate::engine::lib::text::cut::{lines, partition, rpartition, rsplit, split};
            pub use crate::engine::lib::units::{Breaks, Newline, Unicode};
        }
        pub mod edit {
            pub use crate::engine::lib::text::edit::{drop_prefix, drop_suffix, join, replace};
        }
        pub mod find {
            pub use crate::engine::lib::text::find::{count, ends, find, index, rfind, rindex, starts};
        }
        pub mod pad {
            pub use crate::engine::lib::text::pad::{center, pad_end, pad_start, tabs, trim, trim_end, trim_start, zeros};
        }
    }
    pub mod types {
        pub use crate::engine::lib::types::handles;
        #[cfg(feature = "types")]
        pub use crate::engine::lib::types::{callable, is_a, is_sub, truth, type_of};
    }
    pub mod uni {
        pub use crate::engine::lib::uni::{Ascii, Decimal, Digit, Digits, Numeric};
    }
    // the shared kernels text and bytes forward which only the engine's tests reach whole
    #[cfg(all(feature = "bytes", feature = "unstable-internals"))]
    pub mod units {
        pub use crate::engine::lib::units::{Breaks, Family, Hay, Newline, Role, Unicode, center, count, drop_prefix, drop_suffix, ends, find, index, join, lines, pad_end, pad_start, partition, replace, rfind, rindex, rpartition, rsplit, split, starts, tabs, trim, trim_end, trim_start, zeros};
    }
    pub use crate::__lib_table as table;
    pub use crate::__lib_hooks as hooks;
}

/* the kit a WebAssembly plugin a program imports is written with */
#[cfg(feature = "plugin")]
pub mod plugin {
    pub use crate::engine::plugin::{call_id, plugin_class, plugin_fn, plugin_module, plugin_resume, Args, Bytes, Error, FromValue, Handle, IntoValue, Kwargs, PluginCell, Result};
}

/* the kit a command line over a runtime is built from and the commands the binary wires */
#[cfg(feature = "cli-kit")]
pub mod cli {
    pub use crate::engine::cli::{Access, Command, Field, Kit, LockFile, Runtime as Engine};
    pub use crate::engine::cli::manifest::FIELDS as MANIFEST_FIELDS;
    #[cfg(feature = "cli-docs")]
    pub use crate::engine::cli::docs::FIELD as DOCS_FIELD;
    #[cfg(feature = "cli-perms")]
    pub use crate::engine::cli::perms::Perms;
    pub use ::anyhow;
    pub use ::clap;
    pub use ::serde_json;

    /* the commands a composition wires each behind its own feature */
    pub mod cmd {
        #[cfg(feature = "cli-actors")]
        pub use crate::engine::cli::cmd::actors::Actor;
        #[cfg(feature = "cli-build")]
        pub use crate::engine::cli::cmd::build::Build;
        #[cfg(feature = "cli-init")]
        pub use crate::engine::cli::cmd::init::Init;
        #[cfg(feature = "cli-packages")]
        pub use crate::engine::cli::cmd::pkg::{Add, Lock, Remove};
        #[cfg(feature = "cli-run")]
        pub use crate::engine::cli::cmd::run::Run;
        #[cfg(feature = "cli-serve")]
        pub use crate::engine::cli::cmd::serve::Serve;
        #[cfg(feature = "cli-test")]
        pub use crate::engine::cli::cmd::test::Test;
        #[cfg(feature = "cli-uninstall")]
        pub use crate::engine::cli::cmd::uninstall::Uninstall;
    }

    /* the host modules a program calls out to and what each answers */
    pub mod host {
        pub use crate::engine::deps::rules::plain as is_plain_path;
        pub use crate::engine::cli::host::{Calls, Fault, Held, HostModule, Reply, Serve};
        #[cfg(feature = "cli-files")]
        pub use crate::engine::cli::files::Files;
        #[cfg(feature = "cli-net")]
        pub use crate::engine::cli::net::Net;
    }
}

/* what the exported macros expand to which no user names by hand */
#[doc(hidden)]
pub mod __private {
    pub use crate::engine::code::{native_site, ops, ITER_VALUE, SPREAD_BASES};
    pub use crate::engine::lang::{concat, count, count_foreign, count_kinds, foreign_runs, kind_name, method_index, name_index, slots, tables, MethodIndex};
    #[cfg(feature = "lib")]
    pub use crate::engine::lib::kernel::registered;
    pub use crate::engine::value::meter::Metered;
    pub use alloc::string::String;
    #[cfg(feature = "exports")]
    pub use crate::engine::targets::web::Entry;
    #[cfg(feature = "plugin")]
    pub use crate::engine::plugin::{Kind as ParamKind, Param, Ret, expand::{answer, check, construct, export, instance, BTreeMap, Cursor}};
    #[cfg(all(feature = "plugin", target_arch = "wasm32"))]
    pub use ::lol_alloc;
}

/* the engine pieces its own tests reach which no release promises */
#[cfg(feature = "unstable-internals")]
pub mod deps {
    pub use crate::engine::deps::{dir_of, join_relative, partition_bindings, scan_imports, walk, walk_up_dirs, DepHost, Deps, HostResolver, Manifest, ModuleEntry};
}

/* the hashes and helpers the engine's own tests reach */
#[cfg(feature = "unstable-internals")]
pub mod util {
    pub use crate::engine::util::{leak, sha256};
}
