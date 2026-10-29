use edge::lang::{Native, Natives};
use edge::value::foreign::Kind;
use lib::{data, io, seq};

use crate::Uptime;

// the four natives a check calls each a lib kernel under its own name
const TABLE: &[Native] = &lib::table![Uptime;
    "print" => io::print,
    "exit" => io::exit,
    "json" => data::json::read,
    "size" => seq::len,
];

pub static NATIVES: Natives = edge::natives! {
    language: Uptime,
    fns: [TABLE],
    methods: [],
    types: &crate::names::TYPES,
};

// the kind that boxes an int too wide for an inline value
pub static WIDE: Kind = lib::num::big::kind::<Uptime>("int");
