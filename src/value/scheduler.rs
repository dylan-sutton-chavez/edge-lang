/* Why `drive_root` handed control back, surfaced via `VmErr::HostYield` for embedder resume. */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerStatus {
    /// The module body parked in `receive()`, host waits for `run_push_event`.
    PendingEvent,
    /// The module body parked mid-`CallExtern`, host wakes via `set_host_result_by_id`.
    PendingHostCall,
    /// Preempt tick, resumes with no host action.
    Preempted,
}
