#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerStatus {
    // the module body parked on an empty event queue until the host pushes one
    PendingEvent,
    // the module body parked inside a host call until the host answers its id
    PendingHostCall,
    // a preempt tick which resumes with nothing from the host
    Preempted,
}
