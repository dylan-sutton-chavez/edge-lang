use std::path::Path;

/* the threads cpus memory and disk a box gives whoever runs on it */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    // what the scheduler lets this process run on at once
    pub threads: u64,
    // whole cpus once the cgroup quota is applied as the box's sizing reads them
    pub cpu: u64,
    pub memory: u64,
    // bytes an unprivileged writer may still use and the size of that disk
    pub disk_free: u64,
    pub disk_total: u64,
}

// a cgroup limit as a number where max or a missing file means none
fn cgroup(file: &str) -> Option<f64> {
    let text = std::fs::read_to_string(file).ok()?;
    let mut words = text.split_whitespace();
    let limit = words.next()?;
    let period: f64 = words.next().map_or(Some(1.0), |p| p.parse().ok())?;
    if limit == "max" { None } else { limit.parse::<f64>().ok().map(|l| l / period) }
}

fn meminfo() -> u64 {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kb = text.lines().find_map(|l| l.strip_prefix("MemTotal:")).and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok());
    kb.map_or(0, |kb| kb << 10)
}

/* what a box holds read the same rule its sizing uses with disk taken at `dir` */
pub fn measure(dir: &Path) -> std::io::Result<Capacity> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()) as u64;
    let quota = cgroup("/sys/fs/cgroup/cpu.max").unwrap_or(f64::INFINITY);
    let cpu = (threads as f64).min(quota).floor().max(1.0) as u64;
    let limit = cgroup("/sys/fs/cgroup/memory.max").or_else(|| cgroup("/sys/fs/cgroup/memory/memory.limit_in_bytes"));
    let memory = limit.map_or(meminfo(), |l| (l as u64).min(meminfo()));
    let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).map_err(std::io::Error::other)?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // safety the path is a valid c string and st is a writable statvfs
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let block = st.f_frsize as u64;
    Ok(Capacity { threads, cpu, memory, disk_free: st.f_bavail as u64 * block, disk_total: st.f_blocks as u64 * block })
}
