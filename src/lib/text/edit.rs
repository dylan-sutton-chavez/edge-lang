use super::Text;
use crate::lib::units;

units::forward! { Text, [],
    /* the text with its first matches up to a count replaced */
    replace,
    /* the parts of an iterable joined by the text */
    join,
    /* the text without a prefix when it starts with one */
    drop_prefix,
    /* the text without a suffix when it ends with one */
    drop_suffix,
}
