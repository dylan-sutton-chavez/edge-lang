use super::Text;
use crate::{Kernel, Site, units};

/* the text centred in a width with an odd pad split as the runtime chooses */
pub const fn center<S: Site>() -> Kernel { units::center::<S, Text>() }

/* the text padded at its end to a width */
pub const fn pad_end<S: Site>() -> Kernel { units::pad_end::<S, Text>() }

/* the text padded at its start to a width */
pub const fn pad_start<S: Site>() -> Kernel { units::pad_start::<S, Text>() }

/* zeros before the digits and after a leading sign up to a width */
pub const fn zeros<S: Site>() -> Kernel { units::zeros::<S, Text>() }

/* every tab padded to the next stop where a line break starts the column again */
pub const fn tabs<S: Site>() -> Kernel { units::tabs::<S, Text>() }

/* white space or the argument's characters dropped from both ends */
pub const fn trim<S: Site>() -> Kernel { units::trim::<S, Text>() }

/* white space or the argument's characters dropped from the start */
pub const fn trim_start<S: Site>() -> Kernel { units::trim_start::<S, Text>() }

/* white space or the argument's characters dropped from the end */
pub const fn trim_end<S: Site>() -> Kernel { units::trim_end::<S, Text>() }
