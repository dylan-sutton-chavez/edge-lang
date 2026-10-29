use super::Text;
use crate::{Kernel, Site, units};

/* whether a window of the text starts with an affix or one of a tuple */
pub const fn starts<S: Site>() -> Kernel { units::starts::<S, Text>() }

/* whether a window of the text ends with an affix or one of a tuple */
pub const fn ends<S: Site>() -> Kernel { units::ends::<S, Text>() }

/* the character index of the first match in a window or minus one */
pub const fn find<S: Site>() -> Kernel { units::find::<S, Text>() }

/* the character index of the last match in a window or minus one */
pub const fn rfind<S: Site>() -> Kernel { units::rfind::<S, Text>() }

/* the character index of the first match in a window or a failure */
pub const fn index<S: Site>() -> Kernel { units::index::<S, Text>() }

/* the character index of the last match in a window or a failure */
pub const fn rindex<S: Site>() -> Kernel { units::rindex::<S, Text>() }

/* how many matches a window holds without overlap */
pub const fn count<S: Site>() -> Kernel { units::count::<S, Text>() }
