use super::Text;
use crate::lib::units;

units::forward! { Text, [],
    /* whether a window of the text starts with an affix or one of a tuple */
    starts,
    /* whether a window of the text ends with an affix or one of a tuple */
    ends,
    /* the character index of the first match in a window or minus one */
    find,
    /* the character index of the last match in a window or minus one */
    rfind,
    /* the character index of the first match in a window or a failure */
    index,
    /* the character index of the last match in a window or a failure */
    rindex,
    /* how many matches a window holds without overlap */
    count,
}
