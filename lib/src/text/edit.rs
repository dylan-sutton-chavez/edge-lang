use super::Text;
use crate::{Kernel, Site, units};

/* the text with its first matches up to a count replaced */
pub const fn replace<S: Site>() -> Kernel { units::replace::<S, Text>() }

/* the parts of an iterable joined by the text */
pub const fn join<S: Site>() -> Kernel { units::join::<S, Text>() }

/* the text without a prefix when it starts with one */
pub const fn drop_prefix<S: Site>() -> Kernel { units::drop_prefix::<S, Text>() }

/* the text without a suffix when it ends with one */
pub const fn drop_suffix<S: Site>() -> Kernel { units::drop_suffix::<S, Text>() }
