#[macro_export]
macro_rules! s {
    (@b $s:ident;) => {};
    (@b $s:ident; $l:literal $(, $($r:tt)*)?) => { $s.push_str($l); $($crate::s!(@b $s; $($r)*);)? };
    (@b $s:ident; str $v:expr $(, $($r:tt)*)?) => { $s.push_str($v); $($crate::s!(@b $s; $($r)*);)? };
    (@b $s:ident; int $v:expr $(, $($r:tt)*)?) => {{ let mut _b = itoa::Buffer::new(); $s.push_str(_b.format($v)); $($crate::s!(@b $s; $($r)*);)? }};
    (@b $s:ident; char $v:expr $(, $($r:tt)*)?) => { $s.push($v); $($crate::s!(@b $s; $($r)*);)? };
    (@b $s:ident; bool $v:expr $(, $($r:tt)*)?) => { $s.push_str(if $v { "true" } else { "false" }); $($crate::s!(@b $s; $($r)*);)? };
    (cap: $c:expr; $($t:tt)*) => {{ let mut _s = alloc::string::String::with_capacity($c); $crate::s!(@b _s; $($t)*); _s }};
    // each piece binds once so the text takes their summed length in one allocation
    (@w [$($p:ident)*]) => {{ let mut _s = alloc::string::String::with_capacity(0 $(+ $p.len())*); $(_s.push_str($p);)* _s }};
    (@w [$($p:ident)*] $l:literal $(, $($r:tt)*)?) => {{ let p: &str = $l; $crate::s!(@w [$($p)* p] $($($r)*)?) }};
    (@w [$($p:ident)*] str $v:expr $(, $($r:tt)*)?) => {{ let p: &str = $v; $crate::s!(@w [$($p)* p] $($($r)*)?) }};
    (@w [$($p:ident)*] int $v:expr $(, $($r:tt)*)?) => {{ let mut b = itoa::Buffer::new(); let p: &str = b.format($v); $crate::s!(@w [$($p)* p] $($($r)*)?) }};
    (@w [$($p:ident)*] char $v:expr $(, $($r:tt)*)?) => {{ let mut b = [0u8; 4]; let p: &str = char::encode_utf8($v, &mut b); $crate::s!(@w [$($p)* p] $($($r)*)?) }};
    (@w [$($p:ident)*] bool $v:expr $(, $($r:tt)*)?) => {{ let p: &str = if $v { "true" } else { "false" }; $crate::s!(@w [$($p)* p] $($($r)*)?) }};
    ($($t:tt)*) => { $crate::s!(@w [] $($t)*) };
}
