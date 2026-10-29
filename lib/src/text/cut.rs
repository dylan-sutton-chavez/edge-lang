use super::Text;
use crate::{Kernel, Site, units};

pub use crate::units::{Breaks, Newline, Unicode};

/* the pieces between separators or white space runs cut from the start */
pub const fn split<S: Site>() -> Kernel { units::split::<S, Text>() }

/* the pieces between separators or white space runs cut from the end */
pub const fn rsplit<S: Site>() -> Kernel { units::rsplit::<S, Text>() }

/* the lines at every break `B` names where a crlf counts as one */
pub const fn lines<S: Site, B: Breaks>() -> Kernel { units::lines::<S, Text, B>() }

/* the head the separator and the tail around the first match or the whole text */
pub const fn partition<S: Site>() -> Kernel { units::partition::<S, Text>() }

/* the head the separator and the tail around the last match or the whole text */
pub const fn rpartition<S: Site>() -> Kernel { units::rpartition::<S, Text>() }
