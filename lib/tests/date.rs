use std::any::Any;
use std::cmp::Ordering;

use edge::embed::{Limits, VM};
use edge::lang::{Native, Natives, Runtime};
use edge::value::foreign::{BinOp, Snapshot, UnOp};
use edge::value::{HeapPool, Val};
use lib::date::{self, Civil, Day, Days, Duration, Field, Hour, Hours, Minute, Minutes, Month, Second, Seconds, Time, Year};
use mini::Mini;
use mini::natives::{ARRAY, ATTR, BUFFER, BYTES, CONVERT, DICT, FLOAT, FOLDS, GEN, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};

// every run meters what it allocates as edge.wasm does
edge::allocator!();

const DATE: &[Native] = &lib::table![Mini; "time" => date::parse[Mini]];

// the fields as methods of no arguments which every foreign value finds
const PARTS: &[Native] = &lib::table![Mini;
    "days" => date::part[Mini, Days],
    "hours" => date::part[Mini, Hours],
    "minutes" => date::part[Mini, Minutes],
    "seconds" => date::part[Mini, Seconds],
    "year" => date::part[Mini, Year],
    "month" => date::part[Mini, Month],
    "day" => date::part[Mini, Day],
    "hour" => date::part[Mini, Hour],
    "minute" => date::part[Mini, Minute],
    "second" => date::part[Mini, Second],
];

static NATIVES: Natives = edge::natives! {
    language: Over,
    fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, ATTR, TYPES, DATE],
    methods: [
        Str => [STR],
        Bytes => [BUFFER],
        ByteArray => [ARRAY],
        List => [LIST, LIST_SORT],
        Tuple => [TUPLE],
        Dict => [DICT],
        Set => [SET, SET_MANY],
        Int => [INT],
        Float => [FLOAT],
        Range => [RANGE],
        Coro => [GEN],
        Foreign => [PARTS],
    ],
    types: mini::types::NAMES,
};

static RUNTIME: Runtime = Runtime::of::<Over>();

// mini's language over these natives
struct Over;
mini::over!(Over, &NATIVES);

const SECOND: i128 = 1_000_000_000;
const DAY: i128 = 86_400 * SECOND;

/* what `src` printed and its error after it when one ended the run */
fn run(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<Mini>(src);
    assert!(errors.is_empty(), "{src}: {:?}", errors.iter().map(|e| &e.msg).collect::<Vec<_>>());
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    let ran = vm.run();
    let mut out = vm.output_text();
    if let Err(e) = ran { out.push_str(&e.to_string()); }
    out
}

// each call's repr or its error as the class then the words
const TRY: &str = "fn t(f):\n    try:\n        print(repr(f()))\n    except Exception as e:\n        print(type(e).__name__ + ':', e)\n";

fn tried(calls: &str) -> String { run(&(String::from(TRY) + calls)) }

fn time(s: &str) -> Time { Time::parse(s).unwrap_or_else(|| panic!("{s} does not parse")) }

fn leap(y: i64) -> bool { y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) }

#[test]
fn rfc_3339_text_parses_to_its_instant_and_offset() {
    let base = 1_790_867_045 * SECOND;
    assert_eq!(Time::parse("2026-10-01T15:04:05Z"), Some(Time { nanos: base, offset: 0 }));
    assert_eq!(Time::parse("2026-10-01t15:04:05z"), Some(Time { nanos: base, offset: 0 }));
    assert_eq!(Time::parse("2026-10-01 17:04:05+02:00"), Some(Time { nanos: base, offset: 120 }));
    assert_eq!(Time::parse("2026-10-01T10:04:05.5-05:00"), Some(Time { nanos: base + SECOND / 2, offset: -300 }));
    assert_eq!(Time::parse("2026-10-01T15:04:05.123456789123Z").map(|t| t.nanos - base), Some(123_456_789));
    assert_eq!(Time::parse("2026-10-01T15:04:05.000000001+00:00").map(|t| (t.nanos - base, t.offset)), Some((1, 0)));
    assert_eq!(Time::parse("1970-01-01T00:00:00Z"), Some(Time { nanos: 0, offset: 0 }));
    assert_eq!(Time::parse("0000-01-01T00:00:00Z").map(|t| t.nanos), Some(-719_528 * DAY));
    assert_eq!(Time::parse("9999-12-31T23:59:59Z").map(|t| t.nanos), Some(253_402_300_799 * SECOND));
    // the 60th second of a minute is the first of the next
    assert_eq!(time("2016-12-31T23:59:60Z").nanos, 1_483_228_800 * SECOND);
    assert_eq!(time("2016-12-31T23:59:60.25Z").nanos, time("2017-01-01T00:00:00.25Z").nanos);
    for bad in [
        "", "2026-10-01T15:04Z", "2026-10-01T15:04:05", "2026-10-01T24:00:00Z", "2026-10-01T15:60:00Z", "2026-10-01T15:04:61Z",
        "2026-10-01T15:04:05+24:00", "2026-10-01T15:04:05+02:60", "2026-10-01T15:04:05.Z", "2026-10-01T15:04:05Zx", "2026-10-01T15:04:05+0200",
        "26-10-01T15:04:05Z", "2026-1-01T15:04:05Z", "2026-10-01X15:04:05Z", "+2026-10-01T15:04:05Z", "2026-10-01T15:04:05.5", "2026/10/01T15:04:05Z",
        "2026-02-29T00:00:00Z", "2023-02-29T00:00:00Z", "1900-02-29T00:00:00Z", "2026-04-31T00:00:00Z", "2026-13-01T00:00:00Z", "2026-00-01T00:00:00Z",
        "2026-01-00T00:00:00Z", "\u{ff12}026-10-01T15:04:05Z", "2026-10-01T15:04:05 Z",
    ] {
        assert_eq!(Time::parse(bad), None, "{bad}");
    }
    for y in 0..=9999 {
        let text = format!("{y:04}-02-29T12:00:00+05:30");
        assert_eq!(Time::parse(&text).is_some(), leap(y), "{text}");
    }
}

#[test]
fn a_time_writes_its_own_offset_and_a_trimmed_fraction() {
    for (src, text) in [
        ("2026-10-01T17:04:05+02:00", "2026-10-01T17:04:05+02:00"),
        ("2026-10-01T15:04:05.500Z", "2026-10-01T15:04:05.5Z"),
        ("2026-10-01t15:04:05+00:00", "2026-10-01T15:04:05Z"),
        ("2026-10-01T15:04:05.000000001-09:30", "2026-10-01T15:04:05.000000001-09:30"),
        ("0000-01-01T00:00:00Z", "0000-01-01T00:00:00Z"),
        ("9999-12-31T23:59:59.999999999+14:00", "9999-12-31T23:59:59.999999999+14:00"),
        ("2016-12-31T23:59:60Z", "2017-01-01T00:00:00Z"),
        ("2024-02-29 23:59:59.120-00:00", "2024-02-29T23:59:59.12Z"),
    ] {
        assert_eq!(time(src).text(), text, "{src}");
    }
    let c = time("2026-10-01T17:04:05.25+02:00").civil();
    assert_eq!(c, Civil { year: 2026, month: 10, day: 1, hour: 17, minute: 4, second: 5, nanos: 250_000_000 });
    assert_eq!(time("2026-10-01T23:30:00-02:30").civil().day, 1);
    assert_eq!(Time { nanos: time("2026-10-01T23:30:00-02:30").nanos, offset: 0 }.text(), "2026-10-02T02:00:00Z");
    // every seventh day of the years a time reads in round trips through its text
    for day in (-719_528..2_932_897).step_by(7) {
        let t = Time { nanos: day * DAY + (day % 86_400) * SECOND + day % 997, offset: (day % 1439) as i16 };
        assert_eq!(Time::parse(&t.text()), t.valid().then_some(t), "{}", t.text());
    }
    for (nanos, text) in [(5400 * SECOND, "5400s"), (3 * SECOND / 2, "1.5s"), (-SECOND / 2, "-0.5s"), (0, "0s"), (1, "0.000000001s"), (-90_061_250_000_000, "-90061.25s")] {
        assert_eq!(Duration { nanos }.text(), text);
    }
}

#[test]
fn fields_read_in_the_neutral_names() {
    assert_eq!(Field::ALL.map(Field::name), ["days", "hours", "minutes", "seconds", "year", "month", "day", "hour", "minute", "second"]);
    assert!(Field::ALL.iter().filter(|f| f.of_time()).count() == 6 && !Field::Days.of_time() && Field::Year.of_time());
    let d = Duration { nanos: 5400 * SECOND };
    assert_eq!([Field::Days, Field::Hours, Field::Minutes, Field::Seconds].map(|f| f.of_duration(&d)), [0.0625, 1.5, 90.0, 5400.0]);
    let c = time("2026-10-01T15:04:05Z").civil();
    assert_eq!([Field::Year, Field::Month, Field::Day, Field::Hour, Field::Minute, Field::Second].map(|f| f.of_civil(&c)), [2026, 10, 1, 15, 4, 5]);
    let mut h = HeapPool::new(1 << 24);
    let a = date::time_val::<Mini>(&mut h, time("2026-10-01T17:04:05+02:00")).unwrap_or_else(|e| panic!("{e}"));
    let d = date::duration_val::<Mini>(&mut h, d).unwrap_or_else(|e| panic!("{e}"));
    let read = |v: Val, f: &str| date::part_of(&h, v, f).map(|v| if v.is_float() { v.as_float() } else { v.as_int() as f64 });
    assert_eq!((read(a, "hour"), read(a, "year"), read(d, "hours"), read(d, "days")), (Some(17.0), Some(2026.0), Some(1.5), Some(0.0625)));
    assert_eq!((read(a, "hours"), read(d, "hour"), read(a, "week"), read(Val::int(1), "year")), (None, None, None, None));
}

/* a heap whose wide ints the reference runtime's kind boxes */
fn heap() -> HeapPool {
    let mut h = HeapPool::new(1 << 24);
    h.set_wide_int(&mini::lang::WIDE);
    h
}

fn op(h: &mut HeapPool, o: BinOp, a: Val, b: Val) -> Option<Val> { h.foreign_binop(o, a, b).unwrap_or_else(|e| panic!("{o:?} {e}")) }

fn repr(h: &HeapPool, v: Val) -> String {
    match h.foreign_repr(v) { Some(Ok(s)) => s, _ => format!("{v:?}") }
}

#[test]
fn the_kinds_compute_compare_hash_and_save() {
    let mut h = heap();
    let boxed = |h: &mut HeapPool, s: &str| date::time_val::<Mini>(h, time(s)).unwrap_or_else(|e| panic!("{e}"));
    let (a, b, same) = (boxed(&mut h, "2026-10-01T15:04:05Z"), boxed(&mut h, "2026-10-01T17:34:05+01:00"), boxed(&mut h, "2026-10-01T16:04:05+01:00"));
    let d = op(&mut h, BinOp::Sub, b, a).unwrap_or_else(|| panic!("no difference"));
    assert_eq!((repr(&h, d), h.type_name(a), h.type_name(d)), (String::from("5400s"), "time", "duration"));
    let moved = op(&mut h, BinOp::Sub, b, d).unwrap_or_else(|| panic!("no move"));
    assert_eq!(repr(&h, moved), "2026-10-01T16:04:05+01:00");
    let half = op(&mut h, BinOp::Mul, d, Val::float(0.5)).unwrap_or_else(|| panic!("no product"));
    assert_eq!(repr(&h, half), "2700s");
    let third = op(&mut h, BinOp::TrueDiv, d, Val::int(7)).unwrap_or_else(|| panic!("no quotient"));
    assert_eq!(repr(&h, third), "771.428571429s");
    assert!(op(&mut h, BinOp::Pow, d, Val::int(2)).is_none() && op(&mut h, BinOp::Add, a, Val::int(1)).is_none() && op(&mut h, BinOp::Sub, d, a).is_none());
    assert!(op(&mut h, BinOp::Mul, a, Val::int(2)).is_none() && op(&mut h, BinOp::Mod, d, d).is_none());
    let neg = h.foreign_unop(UnOp::Neg, d).ok().flatten().unwrap_or_else(|| panic!("no negation"));
    assert_eq!(repr(&h, neg), "-5400s");
    assert!(h.foreign_unop(UnOp::Neg, a).is_ok_and(|v| v.is_none()) && h.foreign_unop(UnOp::Invert, d).is_ok_and(|v| v.is_none()));
    assert_eq!((h.foreign_eq(a, same), h.foreign_eq(a, b), h.foreign_eq(a, d), h.foreign_eq(d, Val::int(5400))), (Some(true), Some(false), Some(false), Some(false)));
    assert_eq!(h.foreign_hash(a), h.foreign_hash(same));
    assert_eq!((h.foreign_cmp(a, b), h.foreign_cmp(d, half), h.foreign_cmp(a, d), h.foreign_cmp(Val::int(1), d)), (Some(Ordering::Less), Some(Ordering::Greater), None, None));
    let zero = op(&mut h, BinOp::Sub, d, d).unwrap_or_else(|| panic!("no zero"));
    assert_eq!((h.foreign_truthy(zero), h.foreign_truthy(d), h.foreign_truthy(a)), (Some(false), Some(true), Some(true)));
    let far = date::time_val::<Mini>(&mut h, time("9999-12-31T23:00:00Z")).unwrap_or_else(|e| panic!("{e}"));
    assert!(h.foreign_binop(BinOp::Add, far, d).is_err_and(|e| e.to_string().contains("time out of range")));
    assert!(h.foreign_binop(BinOp::Mul, d, Val::float(f64::INFINITY)).is_err_and(|e| e.to_string().contains("duration out of range")));
    assert!(h.foreign_binop(BinOp::TrueDiv, d, Val::float(0.0)).is_err() && h.foreign_binop(BinOp::TrueDiv, d, zero).is_err());
}

#[test]
fn a_snapshot_saves_each_value_in_a_few_bytes() {
    let (Snapshot::Serialize { save: save_time, load: load_time }, Snapshot::Serialize { save: save_span, load: load_span }) = (&mini::lang::TIME.snapshot, &mini::lang::DURATION.snapshot) else { panic!("the kinds refuse snapshots") };
    for s in ["2026-10-01T15:04:05.123456789+05:45", "0000-01-01T00:00:00Z", "9999-12-31T23:59:59.999999999-23:59"] {
        let t = time(s);
        let mut out = Vec::new();
        save_time(&t as &dyn Any, &mut out);
        assert!(out.len() <= 12, "{s} took {} bytes", out.len());
        assert_eq!(load_time(&out).and_then(|p| p.downcast_ref::<Time>().copied()), Some(t));
        assert!(load_time(&out[..out.len() - 1]).is_none());
    }
    for nanos in [0, 1, -1, 5400 * SECOND, i128::MAX, i128::MIN] {
        let mut out = Vec::new();
        save_span(&Duration { nanos } as &dyn Any, &mut out);
        assert_eq!(load_span(&out).and_then(|p| p.downcast_ref::<Duration>().copied()), Some(Duration { nanos }));
    }
    let mut out = Vec::new();
    save_time(&Time { nanos: 0, offset: 1440 } as &dyn Any, &mut out);
    assert!(load_time(&out).is_none() && load_time(&[]).is_none() && load_span(&[0x80]).is_none());
}

#[test]
fn programs_take_differences_across_offsets_and_read_fields() {
    let src = concat!(
        "a = time('2026-10-01T15:04:05Z')\nb = time('2026-10-01T17:34:05+01:00')\nd = b - a\n",
        "print(a, b, d, d.hours(), d.minutes(), d.days(), d.seconds(), type(a).__name__, type(d).__name__)\n",
        "print(a + d, d + a, b - d, d * 2, 2 * d, d / 2, d / d, d * 1.5, d / 4.0, -d, abs(-d), +d, d - d, d * true, d * 0.1)\n",
        "print(bool(d - d), bool(d), bool(a), a == time('2026-10-01T16:04:05+01:00'), a != b, a < b, d > d - d, sorted([b, a]) == [a, b])\n",
        "print({a: 1}[time('2026-10-01T10:04:05-05:00')], {d: 2}[b - a], a == d, d == 5400, (d * 1.5) / d)\n",
        "print(a.year(), a.month(), a.day(), a.hour(), a.minute(), a.second(), b.hour(), time('2026-10-01T23:30:00-02:30').day())\n",
        "print((time('2026-10-02T01:00:00+09:00') - time('2026-10-01T12:00:00-04:00')).hours(), (time('2026-10-01T23:30:00-02:30') - time('2026-10-02T00:00:00Z')).hours(),\n",
        "    (time('2026-03-29T03:00:00+02:00') - time('2026-03-29T01:00:00+01:00')).hours(), (time('2026-10-01T00:00:00Z') - time('2026-10-02T06:00:00+06:00')).hours())\n",
        "print(time('2024-03-01T00:00:00Z') - time('2024-02-28T00:00:00Z'), (time('2023-03-01T00:00:00Z') - time('2023-02-28T00:00:00Z')).days(),\n",
        "    (time('2025-01-01T00:00:00Z') - time('2024-01-01T00:00:00Z')).days(), time('2024-02-28T12:00:00Z') + (time('2024-01-02T00:00:00Z') - time('2024-01-01T00:00:00Z')))\n",
        "print(time('2016-12-31T23:59:60Z'), time('2016-12-31T23:59:60Z') - time('2016-12-31T23:59:59Z'), time('2016-12-31T23:59:60.5Z').second())\n",
        "ns = time('2026-10-01T00:00:00.000000003Z') - time('2026-10-01T00:00:00Z')\n",
        "print(time('2026-10-01T00:00:01.25Z') - time('2026-10-01T00:00:00.5Z'), ns / 2, (ns - ns / 2 - ns / 2) / 2, ns * 0.5)\n",
    );
    let want = concat!(
        "2026-10-01T15:04:05Z 2026-10-01T17:34:05+01:00 5400s 1.5 90.0 0.0625 5400.0 time duration\n",
        "2026-10-01T16:34:05Z 2026-10-01T16:34:05Z 2026-10-01T16:04:05+01:00 10800s 10800s 2700s 1.0 8100s 1350s -5400s 5400s 5400s 0s 5400s 540s\n",
        "False True True True True True True True\n",
        "1 2 False False 1.5\n",
        "2026 10 1 15 4 5 17 1\n",
        "0.0 2.0 1.0 -24.0\n",
        "172800s 1.0 366.0 2024-02-29T12:00:00Z\n",
        "2017-01-01T00:00:00Z 1s 0\n",
        "0.75s 0.000000002s 0s 0.000000002s\n",
    );
    assert_eq!(run(src), want);
}

#[test]
fn programs_get_the_failures_in_the_runtimes_words() {
    let got = tried(concat!(
        "a = time('2026-10-01T15:04:05Z')\nd = time('2026-10-01T16:34:05Z') - a\n",
        "t(lambda: time('2026-02-30T00:00:00Z'))\nt(lambda: time('nope'))\nt(lambda: time(5))\nt(lambda: a - 1)\nt(lambda: a + a)\n",
        "t(lambda: d / 0)\nt(lambda: d / (d - d))\nt(lambda: -a)\nt(lambda: d.year())\nt(lambda: a.hours())\n",
        "t(lambda: time('9999-12-31T23:59:59Z') + d)\nt(lambda: a < d)\nt(lambda: d * 1e400)\nt(lambda: d * 2 ** 200)\n",
    ));
    let want = concat!(
        "ValueError: invalid RFC 3339 time: '2026-02-30T00:00:00Z'\nValueError: invalid RFC 3339 time: 'nope'\n",
        "TypeError: time() argument must be str, not int\n",
        "TypeError: unsupported operand type(s) for -: 'time' and 'int'\nTypeError: unsupported operand type(s) for +: 'time' and 'time'\n",
        "ZeroDivisionError: division by zero\nZeroDivisionError: division by zero\nTypeError: bad operand type for unary -: 'time'\n",
        "TypeError: 'year' applies to 'time' values, not 'duration'\nTypeError: 'hours' applies to 'duration' values, not 'time'\n",
        "ValueError: time out of range\nTypeError: '<' not supported between instances of 'time' and 'duration'\n",
        "ValueError: duration out of range\nValueError: duration out of range\n",
    );
    assert_eq!(got, want);
}

// a runtime of mini's classes that keeps lib's own words for every failure
struct Neutral;

impl lib::fmt::render::Style for Neutral {}

impl edge::lang::Language for Neutral {
    const VERSION: u32 = 1;
    const POLICY: edge::lang::Policy = <Mini as edge::lang::Language>::POLICY;
    const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    const NAMES: &'static edge::lang::Names = &mini::NAMES;
    const FRONT: edge::lang::Front = mini::FRONT;
    const DESCRIPTOR: &'static edge::lang::Descriptor = &mini::DESCRIPTOR;
    const WIDE: Option<&'static edge::value::foreign::Kind> = Some(&mini::lang::WIDE);
    const BUILT: edge::value::foreign::Built = <mini::Mini as edge::lang::Language>::BUILT;
    type Ops = ();

    fn show(cx: &mut edge::lang::Cx<'_, '_>, v: edge::value::Val, mode: edge::lang::Mode) -> Result<String, edge::value::VmErr> { lib::fmt::render::hook::<Self>(cx, v, mode) }
}

impl lib::Lang for Neutral {
    const TREE: &'static lib::lang::Tree = &mini::types::TREE;

    fn class(c: lib::Class) -> &'static str { <Mini as lib::Lang>::class(c) }
}

static NEUTRAL_TIME: edge::value::foreign::Kind = date::time_kind::<Neutral>("time");
static NEUTRAL_DURATION: edge::value::foreign::Kind = date::duration_kind::<Neutral>("duration");

impl date::Dates for Neutral {
    const TIME: &'static edge::value::foreign::Kind = &NEUTRAL_TIME;
    const DURATION: &'static edge::value::foreign::Kind = &NEUTRAL_DURATION;
}

#[test]
fn a_time_or_duration_out_of_range_raises_in_the_words_of_its_runtime() {
    // a heap registers kinds by name so each runtime takes its own heap
    let (mut h, mut n) = (heap(), heap());
    let span = Duration { nanos: 3600 * SECOND };
    let (far, d) = (date::time_val::<Mini>(&mut h, time("9999-12-31T23:30:00Z")).unwrap_or_else(|e| panic!("{e}")), date::duration_val::<Mini>(&mut h, span).unwrap_or_else(|e| panic!("{e}")));
    let (lone, ld) = (date::time_val::<Neutral>(&mut n, time("9999-12-31T23:30:00Z")).unwrap_or_else(|e| panic!("{e}")), date::duration_val::<Neutral>(&mut n, span).unwrap_or_else(|e| panic!("{e}")));
    let said = |h: &mut HeapPool, a: Val, b: Val| h.foreign_binop(BinOp::Add, a, b).err().map(|e| e.to_string()).unwrap_or_default();
    let long = |h: &mut HeapPool, v: Val| h.foreign_binop(BinOp::Mul, v, Val::float(f64::INFINITY)).err().map(|e| e.to_string()).unwrap_or_default();
    assert!(said(&mut h, far, d).ends_with(": time out of range") && long(&mut h, d).ends_with(": duration out of range"));
    assert_eq!(said(&mut n, lone, ld), "ValueError: the time falls outside the years 0000 to 9999");
    assert_eq!(long(&mut n, ld), "ValueError: the duration is longer than a duration holds");
}
