use super::{Completion, Fault};
use edge::abi::WireValue;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};

/* the calls of a host module one package opened answered by index of their names */
pub trait Calls: Send {
    /* answers the call at `index` through `reply` now or later from any thread */
    fn call(&mut self, index: usize, args: Vec<WireValue>, reply: Reply);

    /* aborts what a finished run left open */
    fn close(&mut self) {}
}

// whether the answer came during the call or the run parked waiting for a completion
enum Slot {
    Calling,
    Parked,
    Answered(Result<WireValue, Fault>),
    // known during the call yet delivered only once the run parks for it
    Later(Result<WireValue, Fault>),
}

fn lock(slot: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    slot.lock().unwrap_or_else(|e| e.into_inner())
}

/* where one host call answers once whether before its call returns or long after */
pub struct Reply {
    inner: Option<Inner>,
}

struct Inner {
    id: u32,
    events: Sender<Completion>,
    slot: Arc<Mutex<Slot>>,
    name: Arc<str>,
}

impl Reply {
    /* a reply for call `id` and the settle the host reads once the call returns */
    pub(crate) fn new(id: u32, events: Sender<Completion>, name: Arc<str>) -> (Reply, Settle) {
        let slot = Arc::new(Mutex::new(Slot::Calling));
        (Reply { inner: Some(Inner { id, events, slot: slot.clone(), name }) }, Settle(slot))
    }

    pub fn ok(self, value: WireValue) {
        self.answer(Ok(value));
    }

    pub fn fail(self, fault: Fault) {
        self.answer(Err(fault));
    }

    pub fn answer(mut self, result: Result<WireValue, Fault>) {
        if let Some(inner) = self.inner.take() {
            inner.finish(result);
        }
    }

    /* answers as a waiting call does so the run parks even for a known answer */
    pub fn later(mut self, result: Result<WireValue, Fault>) {
        let Some(inner) = self.inner.take() else { return };
        let mut slot = lock(&inner.slot);
        if let Slot::Calling = *slot {
            *slot = Slot::Later(result);
            return;
        }
        drop(slot);
        inner.finish(result);
    }
}

impl Inner {
    fn finish(self, result: Result<WireValue, Fault>) {
        let mut slot = lock(&self.slot);
        if let Slot::Calling = *slot {
            *slot = Slot::Answered(result);
            return;
        }
        drop(slot);
        let completion = match result {
            Ok(value) => Completion::Value { id: self.id, value },
            Err(fault) => Completion::Error { id: self.id, fault },
        };
        let _ = self.events.send(completion);
    }
}

// a module that drops a reply unanswered fails the call rather than hang the run
impl Drop for Reply {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            let fault = Fault::runtime(format!("{} dropped its answer", inner.name));
            inner.finish(Err(fault));
        }
    }
}

/* what the host reads of a reply once the call that took it returns */
pub(crate) struct Settle(Arc<Mutex<Slot>>);

impl Settle {
    /* the answer given during the call or none once the run parks for it */
    pub(crate) fn settle(self, id: u32, events: &Sender<Completion>) -> Option<Result<WireValue, Fault>> {
        match std::mem::replace(&mut *lock(&self.0), Slot::Parked) {
            Slot::Answered(result) => Some(result),
            // the completion waits in the channel until the run parks and drains it
            Slot::Later(result) => {
                let _ = events.send(match result {
                    Ok(value) => Completion::Value { id, value },
                    Err(fault) => Completion::Error { id, fault },
                });
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn reply() -> (Reply, Settle, Sender<Completion>, std::sync::mpsc::Receiver<Completion>) {
        let (events, rx) = channel();
        let (reply, settle) = Reply::new(7, events.clone(), "probe.call".into());
        (reply, settle, events, rx)
    }

    /* an answer during the call returns at once and sends no completion */
    #[test]
    fn an_answer_before_the_call_returns_is_read_at_once() {
        let (reply, settle, events, rx) = reply();
        reply.ok(WireValue::Int(1));
        assert!(matches!(settle.settle(7, &events), Some(Ok(WireValue::Int(1)))));
        assert!(rx.try_recv().is_err());
    }

    /* an answer after the run parked arrives as a completion for its call */
    #[test]
    fn an_answer_after_the_park_arrives_as_a_completion() {
        let (reply, settle, events, rx) = reply();
        assert!(settle.settle(7, &events).is_none());
        std::thread::spawn(move || reply.fail(Fault::runtime("late"))).join().unwrap();
        assert!(matches!(rx.try_recv(), Ok(Completion::Error { id: 7, fault }) if fault.text == "late"));
    }

    /* a known answer given as later parks the run and waits in the channel */
    #[test]
    fn a_later_answer_parks_the_run_even_when_known() {
        let (reply, settle, events, rx) = reply();
        reply.later(Ok(WireValue::Int(2)));
        assert!(settle.settle(7, &events).is_none());
        assert!(matches!(rx.try_recv(), Ok(Completion::Value { id: 7, value: WireValue::Int(2) })));
    }

    #[test]
    fn a_dropped_reply_fails_its_call() {
        let (reply, settle, events, rx) = reply();
        assert!(settle.settle(7, &events).is_none());
        drop(reply);
        assert!(matches!(rx.try_recv(), Ok(Completion::Error { id: 7, fault }) if fault.text == "probe.call dropped its answer"));
    }
}
