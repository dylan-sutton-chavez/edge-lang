use super::Text;
use crate::engine::lib::{Kernel, Site, units};

pub(crate) use crate::engine::lib::units::Breaks;

units::forward! { Text, [],
    /* the pieces between separators or white space runs cut from the start */
    split,
    /* the pieces between separators or white space runs cut from the end */
    rsplit,
    /* the head the separator and the tail around the first match or the whole text */
    partition,
    /* the head the separator and the tail around the last match or the whole text */
    rpartition,
}

/* the lines at every break `B` names where a crlf counts as one */
pub const fn lines<S: Site, B: Breaks>() -> Kernel { units::lines::<Text, B>() }
