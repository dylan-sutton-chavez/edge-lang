use super::Text;
use crate::lib::units;

units::forward! { Text, [],
    /* the text centred in a width with an odd pad split as the runtime chooses */
    center,
    /* the text padded at its end to a width */
    pad_end,
    /* the text padded at its start to a width */
    pad_start,
    /* zeros before the digits and after a leading sign up to a width */
    zeros,
    /* every tab padded to the next stop where a line break starts the column again */
    tabs,
    /* white space or the argument's characters dropped from both ends */
    trim,
    /* white space or the argument's characters dropped from the start */
    trim_start,
    /* white space or the argument's characters dropped from the end */
    trim_end,
}
