use edge::lang::Native;
use lib::bytes::{self, Fixed, Mutable};
use lib::iter::fold::Joins;
use lib::num::int::{Bin, Hex, Oct};
use lib::num::round::HalfEven;
use lib::text::case::Full;
use lib::uni::{Decimal, Digit, Numeric};
use lib::{date, fmt, io, iter, map, num, reflect, seq, set, text, types};

use crate::Mini;
use crate::lang::{Lines, Modular};

pub const IO: &[Native] = &lib::table![Mini;
    "print" => io::print{"sep", "end", "file", "flush"},
    "input" => io::read_line,
    "receive" => io::receive,
    "send" => io::send,
    "exit" => io::exit,
];

pub const ITER: &[Native] = &lib::table![Mini;
    "len" => seq::len,
    "range" => iter::range,
    "iter" => iter::iter,
    "enumerate" => iter::numbered("iterable", "start"),
    "zip" => iter::zipped{"strict"},
    "map" => iter::mapped,
    "filter" => iter::filtered,
    "reversed" => iter::backward,
    "all" => iter::fold::every,
    "any" => iter::fold::some,
];

// a time read from RFC 3339 text whose kinds the runtime declares for a restore
pub const DATE: &[Native] = &lib::table![Mini; "time" => date::parse[Mini]];

// next resumes a generator through the engine's next path
pub const NEXT: &[Native] = &lib::table![Mini; "next" => iter::next];

pub const NUM: &[Native] = &lib::table![Mini;
    "abs" => num::abs,
    "round" => num::round::round[HalfEven]("number", "ndigits"),
    "divmod" => num::div::divmod,
    "pow" => num::pow::pow("base", "exp", "mod"),
    "int_from_bytes" => num::int::bytes_int,
    "int_to_bytes" => num::int::int_bytes,
];

// the least the greatest and the total fold their iterable through a drain
pub const FOLDS: &[Native] = &lib::table![Mini;
    "min" => iter::fold::least{"key", "default"},
    "max" => iter::fold::most{"key", "default"},
    // the iterable is positional only and the start may come by name
    "sum" => iter::fold::sum[Joins]("", "start"),
];

pub const CONVERT: &[Native] = &lib::table![Mini;
    "str" => text::text("object", "encoding", "errors"),
    "int" => num::int::new[Decimal]("", "base"),
    "float" => num::float::new[Decimal],
    "bool" => types::truth,
    "list" => seq::list,
    "tuple" => seq::tuple,
    // keywords past the source are entries put after its own
    "dict" => map::new,
    "set" => set::new,
    "chr" => text::char_of,
    "ord" => text::code_of,
    "bin" => num::int::radix[Bin],
    "oct" => num::int::radix[Oct],
    "hex" => num::int::radix[Hex],
    "repr" => fmt::render::repr_of,
    "ascii" => fmt::render::ascii_of,
    "hash" => num::hash::hash[Modular],
    "callable" => types::callable,
];

pub const BYTES: &[Native] = &lib::table![Mini;
    "bytes" => bytes::new[Fixed]("source", "encoding", "errors"),
    "bytearray" => bytes::new[Mutable]("source", "encoding", "errors"),
    "bytes_fromhex" => bytes::hex::from_hex_fn,
];

pub const SORT: &[Native] = &lib::table![Mini; "sorted" => seq::sort::sorted{"key", "reverse"}];

pub const ATTR: &[Native] = &lib::table![Mini;
    "getattr" => reflect::get,
    "hasattr" => reflect::has,
    "setattr" => reflect::set,
    "delattr" => reflect::delete,
];

pub const TYPES: &[Native] = &lib::table![Mini;
    "isinstance" => types::is_a,
    "issubclass" => types::is_sub,
    "type" => types::type_of,
];

pub const STR: &[Native] = &lib::table![Mini;
    "encode" => bytes::codec::encode("encoding", "errors"),
    "upper" => text::case::upper[Full],
    "lower" => text::case::lower[Full],
    "strip" => text::pad::trim,
    "capitalize" => text::case::capitalize,
    "title" => text::case::title,
    "lstrip" => text::pad::trim_start,
    "rstrip" => text::pad::trim_end,
    "isdigit" => text::class::digit[Digit],
    "isalpha" => text::class::alpha,
    "isalnum" => text::class::alnum,
    "startswith" => text::find::starts,
    "endswith" => text::find::ends,
    "find" => text::find::find,
    "count" => text::find::count,
    "split" => text::cut::split("sep", "maxsplit"),
    "join" => text::edit::join,
    "replace" => text::edit::replace("", "", "count"),
    "removeprefix" => text::edit::drop_prefix,
    "removesuffix" => text::edit::drop_suffix,
    "splitlines" => text::cut::lines[Lines]("keepends"),
    "partition" => text::cut::partition,
    "rpartition" => text::cut::rpartition,
    "center" => text::pad::center,
    "zfill" => text::pad::zeros,
    "rsplit" => text::cut::rsplit("sep", "maxsplit"),
    "casefold" => text::case::fold,
    "swapcase" => text::case::swap,
    "ljust" => text::pad::pad_end,
    "rjust" => text::pad::pad_start,
    "expandtabs" => text::pad::tabs("tabsize"),
    "rfind" => text::find::rfind,
    "index" => text::find::index,
    "rindex" => text::find::rindex,
    "isspace" => text::class::space,
    "isupper" => text::class::upper,
    "islower" => text::class::lower,
    "istitle" => text::class::title,
    "isascii" => text::class::ascii,
    "isdecimal" => text::class::digit[Decimal],
    "isnumeric" => text::class::digit[Numeric],
    "isprintable" => text::class::printable,
];

// each shared row registers for both buffers with `$b` choosing mutable results
macro_rules! rows {
    ($b:ty; $($own:tt)*) => {
        lib::table![Mini;
            "decode" => bytes::codec::decode[$b]("encoding", "errors"),
            "hex" => bytes::hex::hex[$b]("sep", "bytes_per_sep"),
            "startswith" => bytes::methods::starts[$b],
            "endswith" => bytes::methods::ends[$b],
            "find" => bytes::methods::find[$b],
            "index" => bytes::methods::index[$b],
            "count" => bytes::methods::count[$b],
            "replace" => bytes::methods::replace[$b]("", "", "count"),
            "split" => bytes::methods::split[$b]("sep", "maxsplit"),
            "lower" => bytes::methods::lower[$b],
            "upper" => bytes::methods::upper[$b],
            "strip" => bytes::methods::trim[$b],
            "lstrip" => bytes::methods::trim_start[$b],
            "rstrip" => bytes::methods::trim_end[$b],
            "join" => bytes::methods::join[$b],
            "fromhex" => bytes::hex::from_hex[$b],
            "rfind" => bytes::methods::rfind[$b],
            "rindex" => bytes::methods::rindex[$b],
            "rsplit" => bytes::methods::rsplit[$b]("sep", "maxsplit"),
            "splitlines" => bytes::methods::lines[$b]("keepends"),
            "partition" => bytes::methods::partition[$b],
            "rpartition" => bytes::methods::rpartition[$b],
            "removeprefix" => bytes::methods::drop_prefix[$b],
            "removesuffix" => bytes::methods::drop_suffix[$b],
            "center" => bytes::methods::center[$b],
            "ljust" => bytes::methods::pad_end[$b],
            "rjust" => bytes::methods::pad_start[$b],
            "zfill" => bytes::methods::zeros[$b],
            "expandtabs" => bytes::methods::tabs[$b]("tabsize"),
            "capitalize" => bytes::methods::capitalize[$b],
            "title" => bytes::methods::title[$b],
            "swapcase" => bytes::methods::swap[$b],
            "isalnum" => bytes::methods::is_alnum[$b],
            "isalpha" => bytes::methods::is_alpha[$b],
            "isascii" => bytes::methods::is_ascii[$b],
            "isdigit" => bytes::methods::is_digit[$b],
            "islower" => bytes::methods::is_lower[$b],
            "isspace" => bytes::methods::is_space[$b],
            "istitle" => bytes::methods::is_title[$b],
            "isupper" => bytes::methods::is_upper[$b],
            $($own)*
        ]
    };
}

pub const BUFFER: &[Native] = &rows!(Fixed;);

pub const ARRAY: &[Native] = &rows!(Mutable;
    "append" => bytes::methods::push,
    "extend" => bytes::methods::extend,
    "pop" => bytes::methods::pop,
    "insert" => bytes::methods::insert,
    "remove" => bytes::methods::remove,
    "clear" => bytes::methods::clear,
    "reverse" => bytes::methods::reverse,
    "copy" => bytes::methods::copy,
    "resize" => bytes::methods::resize,
    "take_bytes" => bytes::methods::take,
);

pub const LIST: &[Native] = &lib::table![Mini;
    "index" => seq::index,
    "count" => seq::count,
    "copy" => seq::copy,
    "append" => seq::push,
    "clear" => seq::clear,
    "reverse" => seq::reverse,
    "extend" => seq::extend,
    "insert" => seq::insert,
    "remove" => seq::remove,
    "pop" => seq::pop,
];

// the body checks its positionals before any other keyword in its own words
pub const LIST_SORT: &[Native] = &lib::table![Mini; "sort" => seq::sort::sort{"key", "reverse"}];

pub const TUPLE: &[Native] = &lib::table![Mini;
    "index" => seq::index,
    "count" => seq::count,
];

pub const DICT: &[Native] = &lib::table![Mini;
    "get" => map::get,
    "keys" => map::keys,
    "values" => map::values,
    "items" => map::items,
    "pop" => map::pop,
    "popitem" => map::pop_last,
    "setdefault" => map::set_default,
    // keywords past the source come as pairs merged after it
    "update" => map::update,
    "copy" => map::copy,
    "clear" => map::clear,
    "fromkeys" => map::from_keys,
];

pub const SET: &[Native] = &lib::table![Mini;
    "add" => set::add,
    "remove" => set::remove,
    "discard" => set::discard,
    "pop" => set::pop,
    "clear" => set::clear,
    "copy" => set::copy,
    "symmetric_difference" => set::symmetric,
    "symmetric_difference_update" => set::symmetric_update,
    "issubset" => set::subset,
    "issuperset" => set::superset,
    "isdisjoint" => set::disjoint,
];

// the methods taking any number of operands which drain one after another
pub const SET_MANY: &[Native] = &lib::table![Mini;
    "update" => set::update,
    "union" => set::union,
    "intersection" => set::intersection,
    "difference" => set::difference,
    "intersection_update" => set::intersection_update,
    "difference_update" => set::difference_update,
];

pub const INT: &[Native] = &lib::table![Mini;
    "bit_length" => num::int::bit_length,
    "bit_count" => num::int::bit_count,
    "to_bytes" => num::int::to_bytes("length", "byteorder", "signed"),
    "from_bytes" => num::int::from_bytes("bytes", "byteorder", "signed"),
];

pub const FLOAT: &[Native] = &lib::table![Mini;
    "is_integer" => num::float::is_whole,
    "hex" => num::float::hex,
    "fromhex" => num::float::from_hex,
];

pub const RANGE: &[Native] = &lib::table![Mini;
    "index" => iter::range_index,
    "count" => iter::range_count,
];

// generators and coroutines both resume through the engine
pub const GEN: &[Native] = &lib::table![Mini;
    "send" => iter::send,
    "throw" => iter::throw,
    "close" => iter::close,
];
