use crate::s;

use alloc::{string::String, vec::Vec};

/* a diagnostic at byte offsets whose line and column wait for the render */
#[derive(Clone)]
pub struct Diagnostic {
    pub start: usize,
    pub end: usize,
    pub msg: String,
}

/* the UAX#11 display width that keeps a caret aligned under wide and combining chars */
const fn char_width(c: char) -> usize {
    let cp = c as u32;
    if matches!(cp,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x05BF
        | 0x05C1..=0x05C2 | 0x05C4..=0x05C5 | 0x05C7 | 0x0610..=0x061A
        | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC | 0x06DF..=0x06E4
        | 0x06E7..=0x06E8 | 0x06EA..=0x06ED | 0x0711 | 0x0730..=0x074A
        | 0x07A6..=0x07B0 | 0x07EB..=0x07F3 | 0x200B..=0x200F | 0x202A..=0x202E
        | 0x2060..=0x206F | 0xFE00..=0xFE0F | 0xFEFF | 0xE0100..=0xE01EF)
    {
        0
    } else if matches!(cp,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF | 0x20000..=0x3FFFD)
    {
        2
    } else {
        1
    }
}

#[inline]
fn display_width(s: &str) -> usize {
    // ascii text takes one column a byte
    if s.is_ascii() { return s.len(); }
    s.chars().map(char_width).sum()
}

// the spaces a report pads with and a longer caret pushes one at a time
const SPACES: &str = "                                                                                                ";

// long lines show `SHOWN` chars with `LEAD` before the error and reports `MAX_SHOWN` errors
const SHOWN: usize = 80;
const LEAD: usize = 40;
const MAX_SHOWN: usize = 20;

/* how many newlines `head` holds and where the line after the last one starts */
fn newlines(head: &[u8]) -> (usize, usize) {
    const LOW: u64 = 0x7F7F_7F7F_7F7F_7F7F;
    let (mut n, mut start) = (0, 0);
    let (words, rest) = head.as_chunks::<8>();
    for (k, w) in words.iter().enumerate() {
        let x = u64::from_le_bytes(*w) ^ 0x0A0A_0A0A_0A0A_0A0A;
        // the top bit of each byte that was a newline and of no other
        let hit = !(((x & LOW) + LOW) | x | LOW);
        if hit != 0 {
            n += hit.count_ones() as usize;
            start = k * 8 + (63 - hit.leading_zeros() as usize) / 8 + 1;
        }
    }
    let tail = head.len() - rest.len();
    for (i, &b) in rest.iter().enumerate() {
        if b == b'\n' { (n, start) = (n + 1, tail + i + 1); }
    }
    (n, start)
}

/* where each line of a source starts so many diagnostics find their lines without rescanning */
pub struct Lines<'s> { src: &'s str, starts: core::cell::OnceCell<Vec<usize>>, asked: core::cell::Cell<bool> }

impl<'s> Lines<'s> {
    pub fn new(src: &'s str) -> Self { Self { src, starts: core::cell::OnceCell::new(), asked: core::cell::Cell::new(false) } }

    pub fn source(&self) -> &'s str { self.src }

    /* the 1-indexed line of `byte` and the byte that line starts at */
    fn line_of(&self, byte: usize) -> (usize, usize) {
        // a first ask scans up to its byte and a second builds the line index
        if self.starts.get().is_none() && !self.asked.replace(true) {
            let (n, start) = newlines(&self.src.as_bytes()[..byte.min(self.src.len())]);
            return (n + 1, start);
        }
        let starts = self.starts.get_or_init(|| core::iter::once(0).chain(self.src.bytes().enumerate().filter(|&(_, b)| b == b'\n').map(|(i, _)| i + 1)).collect());
        let i = starts.partition_point(|&s| s <= byte).max(1);
        (i, starts[i - 1])
    }
}

impl Diagnostic {
    /* a rustc style render of the error and its source line with a caret */
    pub fn render(&self, src: &str, path: Option<&str>) -> alloc::string::String { self.render_in(&Lines::new(src), path) }

    /* `render` on a source whose lines are already found */
    pub fn render_in(&self, lines: &Lines, path: Option<&str>) -> alloc::string::String {
        let mut o = alloc::string::String::new();
        self.render_to(&mut o, lines, path, "error: ");
        o
    }

    /* the report led by `label` onto `o` so an outer frame reads as a note */
    pub fn render_to(&self, o: &mut alloc::string::String, lines: &Lines, path: Option<&str>, label: &str) {
        let src = lines.src;
        let path = path.unwrap_or("<input>");
        // a span off a character boundary widens to the characters it touches
        let s_off = src.floor_char_boundary(self.start.min(src.len()));
        let e_off = src.ceil_char_boundary(self.end.min(src.len()).max(s_off));
        let (line_no, line_start) = lines.line_of(s_off);
        let col = display_width(&src[line_start..s_off]) + 1;
        let line_end = src[s_off..].find('\n').map_or(src.len(), |p| s_off + p);
        let line_txt = &src[line_start..line_end];
        let at = s_off - line_start;
        let (from, to) = if line_txt.len() <= SHOWN { (0, line_txt.len()) } else {
            let from = line_txt[..at].char_indices().rev().nth(LEAD - 1).map_or(0, |(i, _)| i);
            (from, line_txt[from..].char_indices().nth(SHOWN).map_or(line_txt.len(), |(i, _)| from + i))
        };
        let (head, tail) = (if from > 0 { "..." } else { "" }, if to < line_txt.len() { "..." } else { "" });
        let mark = display_width(&src[s_off..e_off.min(line_start + to)]).max(1);
        let mut buf = itoa::Buffer::new();
        let pad_len = buf.format(line_no).len();
        let pad = &SPACES[..pad_len.min(SPACES.len())];
        let caret = if from == 0 { col - 1 } else { head.len() + display_width(&line_txt[from..at]) };
        // the whole report fits the room taken once
        o.reserve(label.len() + self.msg.len() + path.len() + (to - from) + caret + mark + 5 * pad_len + 64);
        o.push_str(label); o.push_str(&self.msg); o.push('\n');
        o.push_str(pad); o.push_str(" --> ");
        o.push_str(path); o.push(':');
        o.push_str(buf.format(line_no)); o.push(':'); o.push_str(buf.format(col)); o.push('\n');
        o.push_str(pad); o.push_str(" |\n");
        o.push_str(buf.format(line_no)); o.push_str(" | "); o.push_str(head); o.push_str(&line_txt[from..to]); o.push_str(tail); o.push('\n');
        o.push_str(pad); o.push_str(" | ");
        // a caret within the run of spaces lands in one copy
        match SPACES.get(..caret) { Some(run) => o.push_str(run), None => for _ in 0..caret { o.push(' '); } }
        for _ in 0..mark { o.push('^'); }
        o.push('\n');
    }

    /* a whole report of the first `MAX_SHOWN` diagnostics and how many more there are */
    pub fn render_all(errs: &[Diagnostic], src: &str, path: Option<&str>) -> String {
        let mut out = String::new();
        let lines = Lines::new(src);
        for (i, e) in errs.iter().take(MAX_SHOWN).enumerate() {
            if i > 0 { out.push('\n'); }
            out.push_str(&e.render_in(&lines, path));
        }
        let more = errs.len().saturating_sub(MAX_SHOWN);
        if more > 0 { out.push_str(&s!("\nand ", int more, str if more == 1 { " more error\n" } else { " more errors\n" })); }
        out
    }
}
