use alloc::{string::String, sync::Arc};
use core::cell::RefCell;

use crate::value::VmErr;

/* the host call or send running now as a host that runs the vm in its own process reads it */
#[derive(Clone)]
pub struct Calling {
    // the id the call parks under once its binding defers it
    pub id: u64,
    // the file of the code that called the binding or sent
    pub file: Option<Arc<String>>,
}

std::thread_local! {
    static NOW: RefCell<Option<Calling>> = const { RefCell::new(None) };
    static REFUSED: RefCell<Option<VmErr>> = const { RefCell::new(None) };
}

/* what `calling` reads while a binding or a send hook runs and what it read before after */
pub(crate) struct Now(Option<Calling>);

impl Now {
    pub(crate) fn enter(id: u64, file: Option<Arc<String>>) -> Now {
        Now(NOW.with_borrow_mut(|now| now.replace(Calling { id, file })))
    }
}

impl Drop for Now {
    fn drop(&mut self) {
        NOW.with_borrow_mut(|now| *now = self.0.take());
    }
}

/* the host call or send running now on this thread or none outside one */
pub fn calling() -> Option<Calling> {
    NOW.with_borrow(Clone::clone)
}

/* the error the send running now raises in place of returning whether the host took it */
pub fn refuse(e: VmErr) {
    REFUSED.with_borrow_mut(|refused| *refused = Some(e));
}

/* the refusal a send hook left which the send raises */
pub(crate) fn refused() -> Option<VmErr> {
    REFUSED.with_borrow_mut(Option::take)
}
