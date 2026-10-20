/* How many loops run, never more than one, and the generic frames pushed, which `tests/nested.rs` holds at none. */
#[cfg(any(debug_assertions, feature = "coverage"))]
#[derive(Default)]
pub(crate) struct Loops { pub depth: u32, pub generic: u64 }

#[cfg(any(debug_assertions, feature = "coverage"))]
impl Loops {
    /* Opens the loop, which fails when another runs but for a call a plugin or the host makes, `hosted`. */
    pub(crate) fn open(&mut self, hosted: bool) {
        if self.depth != 0 && !hosted {
            panic!("an instruction loop opened inside another: a call into Python from Rust is a section, a job or a frame of the running loop, see docs/05-internals/01-design.mdx");
        }
        self.depth += 1;
    }

    pub(crate) fn close(&mut self) { self.depth -= 1; }
}

#[cfg(all(test, any(debug_assertions, feature = "coverage")))]
mod tests {
    use super::*;

    #[test]
    fn first_loop_opens() {
        let mut l = Loops::default();
        l.open(false);
        l.close();
        assert_eq!(l.depth, 0);
    }

    #[test]
    fn hosted_loop_nests() {
        let mut l = Loops::default();
        l.open(false);
        l.open(true);
        assert_eq!(l.depth, 2);
    }

    #[test]
    #[should_panic(expected = "opened inside another")]
    fn loop_inside_another_panics() {
        let mut l = Loops::default();
        l.open(false);
        l.open(false);
    }
}
