mod kind;

use alloc::rc::Rc;
use alloc::string::String;
use core::any::Any;

use edge::lang::{Args, Cx, Ret, Sig};
use edge::value::foreign::Kind;
use edge::value::{HeapPool, Val, VmErr};

use crate::fail::{Fail, fail};
use crate::{Kernel, Site};

pub use kind::{duration_kind, time_kind};

// nanoseconds in each unit a time or a duration counts
const SECOND: i128 = 1_000_000_000;
const MINUTE: i128 = 60 * SECOND;
const HOUR: i128 = 60 * MINUTE;
const DAY: i128 = 24 * HOUR;
// the first local instant of year 0000 and the one past year 9999
const EARLIEST: i128 = days_from_civil(0, 1, 1) * DAY;
const LATEST: i128 = days_from_civil(10_000, 1, 1) * DAY;

/* an instant as nanoseconds since the epoch and the offset in minutes its text gave */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Time {
    pub nanos: i128,
    pub offset: i16,
}

/* a span of time in nanoseconds */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Duration {
    pub nanos: i128,
}

/* the civil fields of a time read in its own offset */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub nanos: u32,
}

impl Time {
    /* the time RFC 3339 text spells and none for any other text */
    pub fn parse(s: &str) -> Option<Time> {
        let b = s.as_bytes();
        let num = |at: usize, n: usize| b.get(at..at + n)?.iter().try_fold(0u32, |v, &c| c.is_ascii_digit().then(|| v * 10 + u32::from(c - b'0')));
        let mark = |at: usize, c: u8| b.get(at) == Some(&c);
        let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
        let (hour, minute, second) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
        if !(mark(4, b'-') && mark(7, b'-') && matches!(b.get(10), Some(b'T' | b't' | b' ')) && mark(13, b':') && mark(16, b':')) { return None; }
        // a 60th second is the start of the next minute
        if !(1..=12).contains(&month) || day == 0 || day > days_in(year, month) || hour > 23 || minute > 59 || second > 60 { return None; }
        let (mut at, mut frac) = (19, 0);
        if mark(at, b'.') {
            let digits = b.get(at + 1..).map_or(0, |r| r.iter().take_while(|c| c.is_ascii_digit()).count());
            if digits == 0 { return None; }
            // digits past the ninth are dropped
            for k in 0..9 { frac = frac * 10 + if k < digits { i128::from(b[at + 1 + k] - b'0') } else { 0 }; }
            at += 1 + digits;
        }
        let offset = match b.get(at) {
            Some(b'Z' | b'z') if at + 1 == b.len() => 0,
            Some(&sign @ (b'+' | b'-')) if at + 6 == b.len() && mark(at + 3, b':') => {
                let (h, m) = (num(at + 1, 2)?, num(at + 4, 2)?);
                if h > 23 || m > 59 { return None; }
                let o = (h * 60 + m) as i16;
                if sign == b'-' { -o } else { o }
            }
            _ => return None,
        };
        let clock = i128::from(hour) * HOUR + i128::from(minute) * MINUTE + i128::from(second) * SECOND + frac;
        let local = days_from_civil(i128::from(year), month, day) * DAY + clock;
        Some(Time { nanos: local - i128::from(offset) * MINUTE, offset })
    }

    /* the nanoseconds since the epoch the clock of its own offset reads */
    fn local(&self) -> i128 { self.nanos.saturating_add(i128::from(self.offset) * MINUTE) }

    /* whether the time reads in years 0000 to 9999 with an offset under a day */
    pub fn valid(&self) -> bool { self.offset.unsigned_abs() < 24 * 60 && (EARLIEST..LATEST).contains(&self.local()) }

    /* the civil fields of the time in its own offset */
    pub fn civil(&self) -> Civil {
        let local = self.local();
        let (days, clock) = (local.div_euclid(DAY), local.rem_euclid(DAY));
        let (year, month, day) = civil_from_days(days);
        Civil {
            year: i64::try_from(year).unwrap_or(if year < 0 { i64::MIN } else { i64::MAX }),
            month,
            day,
            hour: (clock / HOUR) as u32,
            minute: (clock % HOUR / MINUTE) as u32,
            second: (clock % MINUTE / SECOND) as u32,
            nanos: (clock % SECOND) as u32,
        }
    }

    /* the time as RFC 3339 text in its own offset with the fraction trimmed */
    pub fn text(&self) -> String {
        let c = self.civil();
        let mut out = String::with_capacity(36);
        if c.year < 0 { out.push('-'); }
        padded(&mut out, u128::from(c.year.unsigned_abs()), 4);
        for (sep, n) in [('-', c.month), ('-', c.day), ('T', c.hour), (':', c.minute), (':', c.second)] {
            out.push(sep);
            padded(&mut out, u128::from(n), 2);
        }
        fraction(&mut out, c.nanos);
        if self.offset == 0 { out.push('Z'); return out; }
        let o = self.offset.unsigned_abs();
        out.push(if self.offset < 0 { '-' } else { '+' });
        padded(&mut out, u128::from(o / 60), 2);
        out.push(':');
        padded(&mut out, u128::from(o % 60), 2);
        out
    }
}

impl Duration {
    /* the span as its total seconds then `s` with the fraction trimmed */
    pub fn text(&self) -> String {
        let n = self.nanos.unsigned_abs();
        let mut out = String::new();
        if self.nanos < 0 { out.push('-'); }
        out.push_str(itoa::Buffer::new().format(n / SECOND as u128));
        fraction(&mut out, (n % SECOND as u128) as u32);
        out.push('s');
        out
    }
}

/* `n` in decimal onto `out` with zeros before it up to `width` digits */
fn padded(out: &mut String, n: u128, width: usize) {
    let mut buf = itoa::Buffer::new();
    let digits = buf.format(n);
    for _ in digits.len()..width { out.push('0'); }
    out.push_str(digits);
}

/* a fraction of a second after a point with its trailing zeros trimmed */
fn fraction(out: &mut String, nanos: u32) {
    if nanos == 0 { return; }
    let (mut n, mut width) = (nanos, 9);
    while n % 10 == 0 { n /= 10; width -= 1; }
    out.push('.');
    padded(out, u128::from(n), width);
}

/* whether `year` of the proleptic gregorian calendar has a 29th of february */
const fn leap(year: u32) -> bool { year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) }

/* the days of `month` in `year` */
const fn days_in(year: u32, month: u32) -> u32 {
    match month {
        2 if leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/* the days from 1970-01-01 to a civil date by the days from civil algorithm */
const fn days_from_civil(year: i128, month: u32, day: u32) -> i128 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month as i128 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i128 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/* the civil date `days` after 1970-01-01 falls on */
fn civil_from_days(days: i128) -> (i128, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i128::from(month <= 2), month, day)
}

/* the kinds a runtime boxes times and durations in whose failures it words */
pub trait Dates: crate::Lang {
    const TIME: &'static Kind;
    const DURATION: &'static Kind;
}

/* the payload of type `T` a foreign value holds and none for any other value */
fn held<T: Any>(heap: &HeapPool, v: Val) -> Option<&T> {
    let kind = heap.foreign_kind(v)?;
    heap.foreign_payload::<T>(Cx::kind_token(heap, kind)?, v)
}

/* the payload of type `T` a value of exactly `kind` holds */
fn held_by<'h, T: Any>(heap: &'h HeapPool, kind: &'static Kind, v: Val) -> Option<&'h T> { heap.foreign_payload::<T>(Cx::kind_token(heap, kind)?, v) }

/* `payload` boxed as a value of `kind` which registers on its first box */
fn boxed<T: Any>(heap: &mut HeapPool, kind: &'static Kind, payload: T) -> Result<Val, VmErr> {
    let token = match Cx::kind_token(heap, kind) { Some(t) => t, None => heap.register_kind(kind)? };
    heap.foreign_box(token, Rc::new(payload))
}

/* `t` boxed as a time of the dates `D` */
pub fn time_val<D: Dates>(heap: &mut HeapPool, t: Time) -> Result<Val, VmErr> { boxed(heap, D::TIME, t) }

/* `d` boxed as a duration of the dates `D` */
pub fn duration_val<D: Dates>(heap: &mut HeapPool, d: Duration) -> Result<Val, VmErr> { boxed(heap, D::DURATION, d) }

/* the time RFC 3339 text spells */
pub const fn parse<S: Site, D: Dates>() -> Kernel { Kernel::new(Sig::exact(1), parse_run::<S, D>) }

fn parse_run<S: Site, D: Dates>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let v = a.pos[0];
    let Some(s) = cx.str_of(v) else { return Err(fail::<S>(cx, Fail::NeedText(v))) };
    match Time::parse(s) {
        Some(t) => Ok(time_val::<D>(cx.heap_mut(), t)?.into()),
        None => Err(fail::<S>(cx, Fail::BadTime(v))),
    }
}

/* a field of a time or duration by the neutral name a field read finds */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field { Days, Hours, Minutes, Seconds, Year, Month, Day, Hour, Minute, Second }

impl Field {
    pub const ALL: [Field; 10] = [Field::Days, Field::Hours, Field::Minutes, Field::Seconds, Field::Year, Field::Month, Field::Day, Field::Hour, Field::Minute, Field::Second];

    /* the neutral name a field read finds the field by */
    pub const fn name(self) -> &'static str {
        match self {
            Field::Days => "days", Field::Hours => "hours", Field::Minutes => "minutes", Field::Seconds => "seconds",
            Field::Year => "year", Field::Month => "month", Field::Day => "day", Field::Hour => "hour", Field::Minute => "minute", Field::Second => "second",
        }
    }

    /* whether a time holds the field where a duration holds the others */
    pub const fn of_time(self) -> bool { !matches!(self, Field::Days | Field::Hours | Field::Minutes | Field::Seconds) }

    /* a duration's total in the field's unit as a float */
    pub fn of_duration(self, d: &Duration) -> f64 {
        let unit = match self { Field::Days => DAY, Field::Hours => HOUR, Field::Minutes => MINUTE, _ => SECOND };
        // whole units stay exact and only the part of one rounds
        (d.nanos / unit) as f64 + (d.nanos % unit) as f64 / unit as f64
    }

    /* a time's civil field in its own offset */
    pub fn of_civil(self, c: &Civil) -> i64 {
        match self {
            Field::Year => c.year,
            Field::Month => i64::from(c.month),
            Field::Day => i64::from(c.day),
            Field::Hour => i64::from(c.hour),
            Field::Minute => i64::from(c.minute),
            _ => i64::from(c.second),
        }
    }

    /* the field read off `t` or `d` whichever holds it and none when missing */
    fn read(self, t: Option<&Time>, d: Option<&Duration>) -> Option<Val> {
        if self.of_time() { return Val::int_checked(self.of_civil(&t?.civil())); }
        Some(Val::float(self.of_duration(d?)))
    }
}

/* a marker naming the field a part kernel reads */
pub trait Part: 'static { const FIELD: Field; }

// one marker per field which a runtime names in its table
macro_rules! parts {
    ($($name:ident),*) => {$(
        pub struct $name;
        impl Part for $name { const FIELD: Field = Field::$name; }
    )*};
}

parts!(Days, Hours, Minutes, Seconds, Year, Month, Day, Hour, Minute, Second);

/* a time's civil field or a duration's total as a method of no arguments */
pub const fn part<S: Site, D: Dates, P: Part>() -> Kernel { Kernel::new(Sig::exact(0), part_run::<S, D, P>) }

fn part_run<S: Site, D: Dates, P: Part>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let kind = if P::FIELD.of_time() { D::TIME } else { D::DURATION };
    let (heap, f) = (cx.heap(), P::FIELD);
    let got = if f.of_time() { f.read(held_by(heap, D::TIME, a.recv), None) } else { f.read(None, held_by(heap, D::DURATION, a.recv)) };
    match got {
        Some(v) => Ok(v.into()),
        None => Err(fail::<S>(cx, Fail::FieldOf { recv: a.recv, want: kind.type_name })),
    }
}

/* the field `field` of a time or duration or none for other values or names */
pub fn part_of(heap: &HeapPool, v: Val, field: &str) -> Option<Val> {
    let f = Field::ALL.into_iter().find(|f| f.name() == field)?;
    if f.of_time() { f.read(held(heap, v), None) } else { f.read(None, held(heap, v)) }
}
