use super::{ChannelSink, Fault};
use anyhow::{anyhow, Result};
use crate::abi::WireValue;
use crate::embed::Limits;
use std::time::Duration;

/* what a run step left the interpreter waiting on */
pub enum Status {
    Done,
    PendingEvent,
    PendingHostCall,
    Error(String),
    Exit(u8),
    Preempted,
}

pub enum Completion {
    Value { id: u32, value: WireValue },
    Error { id: u32, fault: Fault },
}

/* one interpreter of the kit's runtime whichever engine its composition runs it on */
pub trait Engine {
    /* resolves imports and starts a fresh run with resolution failures read as compile errors */
    fn start(&mut self, src: &str, input: Option<&str>) -> Result<Status>;

    /* runs one more input on the persistent interpreter without running the history again */
    fn repl_eval(&mut self, src: &str, input: Option<&str>) -> Result<Status>;

    fn resume(&mut self) -> Result<Status>;

    /* runs the next input as `entry` whose directory resolves its relative imports */
    fn set_entry(&mut self, entry: &str);

    fn save_state(&mut self) -> Result<Option<Vec<u8>>>;

    /* boots from the source the blob embeds whose imports must resolve again */
    fn restore_state(&mut self, blob: &[u8]) -> Result<Status>;

    /* caps the next boot with the limits of a group or the sandbox profile */
    fn set_limits(&mut self, limits: &Limits) -> Result<()>;

    fn set_preempt_interval(&mut self, n: usize) -> Result<()>;

    /* drops every module registration so the next input starts in a fresh namespace */
    fn reset(&mut self) -> Result<()>;

    /* wakes a parked `receive()` or queues the message until the run parks on one */
    fn push_event(&mut self, message: &str) -> bool;

    /* moves the parked calls into the in-flight count whose answers arrive as completions */
    fn dispatch(&mut self);

    fn inflight(&self) -> usize;

    /* blocks for the next completion then injects all that arrived and returns the count */
    fn wait(&mut self, timeout: Option<Duration>) -> Result<usize>;

    /* hands queued messages to a run parked on an event and returns how many arrived */
    fn drain_buffered(&mut self) -> usize;

    /* whether a failure left the interpreter unfit to run again */
    fn poisoned(&self) -> bool;

    /* hands every line a run sends to `channel` the moment the program sends it */
    fn send_to(&mut self, channel: ChannelSink);

    /* why a channel of the run failed to take a line since the last ask */
    fn channel_error(&mut self) -> Option<String>;
}

/* the status a step's raw word names where `out` reads the error a failed step left */
#[cfg(feature = "cli-wasm")]
pub(crate) fn status_of(raw: u32, out: impl FnOnce() -> Vec<u8>) -> Result<Status> {
    const KIND_SHIFT: u32 = 29;
    Ok(match raw >> KIND_SHIFT {
        0 => Status::Done,
        3 => Status::PendingEvent,
        4 => Status::Error(String::from_utf8_lossy(&out()).into_owned()),
        5 => Status::PendingHostCall,
        6 => Status::Exit((raw & 0xFF) as u8),
        7 => Status::Preempted,
        kind => anyhow::bail!("unknown run status kind {kind}"),
    })
}

/* whether the runtime whose descriptor json is `described` wrote `blob` where a descriptor that names none passes */
#[cfg(feature = "cli-wasm")]
pub(crate) fn saved_by(described: &[u8], blob: &[u8]) -> bool {
    let Ok(d) = serde_json::from_slice::<serde_json::Value>(described) else { return true };
    let (Some(at), Some(id)) = (d["snapshot"]["runtime_at"].as_u64(), d["snapshot"]["runtime"].as_str()) else { return true };
    let at = at as usize;
    blob.get(at..at + 8).and_then(|w| w.try_into().ok()).map(u64::from_le_bytes).is_some_and(|w| format!("{w:016x}") == id)
}

/* the source a snapshot blob embeds read by the engine that wrote its header */
pub fn snapshot_source(blob: &[u8]) -> Result<String> {
    crate::embed::source_of(blob).map(String::from).map_err(|e| anyhow!(e))
}
