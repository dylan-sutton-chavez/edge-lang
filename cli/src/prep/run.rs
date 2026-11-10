use super::caps::{tree, Caps, Log, Tree};
use super::{sweep::sweep, User};
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/* one program a step runs with every input it may see named here */
#[derive(Clone, Debug)]
pub struct Step {
    // the program and its arguments with no shell between
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    // the whole environment since nothing of the caller's is inherited
    pub env: Vec<(String, String)>,
    pub user: User,
    pub time: Duration,
    // how long a step past its time has between the stop signal and the kill
    pub grace: Duration,
    pub caps: Caps,
    // a directory whose disk the step may not grow past the repo cap
    pub watch: Option<PathBuf>,
    // data segment and largest file in bytes and processes the user may hold
    pub memory: Option<u64>,
    pub fsize: Option<u64>,
    pub procs: Option<u64>,
}

impl Step {
    pub fn new(argv: Vec<String>, cwd: PathBuf, user: User) -> Step {
        Step { argv, cwd, env: Vec::new(), user, time: Duration::from_secs(40 * 60), grace: Duration::from_secs(10), caps: Caps::default(), watch: None, memory: None, fsize: None, procs: None }
    }
}

/* how a step ended and what it said */
#[derive(Debug)]
pub struct Ran {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    // the line handler ended it on purpose
    pub stopped: bool,
    // the cap the watched directory went over when that ended the step
    pub over: Option<Tree>,
    pub log: Log,
    // the largest resident set of the step and the children it waited for in bytes
    pub peak: u64,
}

impl Ran {
    pub fn ok(&self) -> bool {
        self.code == Some(0) && !self.timed_out && !self.stopped && self.over.is_none()
    }

    /* the exit code a shell would show so 124 137 and 139 keep their meaning */
    pub fn exit(&self) -> i32 {
        if self.timed_out { 124 } else { self.code.unwrap_or(128 + self.signal.unwrap_or(0)) }
    }
}

// sets one limit where the resource type differs between libcs
macro_rules! limit {
    ($resource:expr, $value:expr) => {
        if let Some(value) = $value {
            let lim = libc::rlimit { rlim_cur: value as libc::rlim_t, rlim_max: value as libc::rlim_t };
            if libc::setrlimit($resource, &lim) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    };
}

// what the child does between fork and exec where only async signal safe calls run
fn drop_to(step: &Step) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static + use<> {
    let (user, memory, fsize, procs) = (step.user, step.memory, step.fsize, step.procs);
    move || unsafe {
        // its own session so a timeout ends the whole group and no terminal is shared
        if libc::setsid() < 0 {
            return Err(io::Error::last_os_error());
        }
        // data rather than address space so large reservations that stay untouched still work
        limit!(libc::RLIMIT_DATA, memory);
        limit!(libc::RLIMIT_FSIZE, fsize);
        limit!(libc::RLIMIT_NPROC, procs);
        limit!(libc::RLIMIT_CORE, Some(0u64));
        if libc::setgroups(0, std::ptr::null()) != 0 || libc::setgid(user.gid) != 0 || libc::setuid(user.uid) != 0 {
            return Err(io::Error::last_os_error());
        }
        // a dropped step can never take root back and errors here must not allocate
        if libc::getuid() != user.uid || libc::geteuid() != user.uid || libc::getegid() != user.gid || libc::setuid(0) == 0 {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        // every descriptor the caller holds past stdio closes at exec
        if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, libc::CLOSE_RANGE_CLOEXEC) != 0 {
            for fd in 3..4096 {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        Ok(())
    }
}

/* runs `step` as its user then ends every process that user left behind */
pub fn run(step: &Step) -> io::Result<Ran> {
    run_with(step, |_| true)
}

// a line handed on whole up to this so one endless line holds no more
const LINE: usize = 64 << 10;

/* runs `step` handing `on` each line it prints where false ends the step at once */
pub fn run_with(step: &Step, mut on: impl FnMut(&[u8]) -> bool) -> io::Result<Ran> {
    if step.user.uid == 0 || step.user.gid == 0 {
        return Err(io::Error::other("a step never runs as root"));
    }
    let program = step.argv.first().ok_or_else(|| io::Error::other("a step needs a program"))?;
    let (mut reader, writer) = io::pipe()?;
    let mut command = Command::new(program);
    command.args(&step.argv[1..]).env_clear().envs(step.env.iter().map(|(k, v)| (k, v))).current_dir(&step.cwd);
    command.stdin(Stdio::null()).stdout(writer.try_clone()?).stderr(writer);
    // safety drop_to calls only async signal safe functions
    unsafe { command.pre_exec(drop_to(step)) };
    let child = command.spawn();
    // the parent's copy of the pipe goes so the log ends when the step's processes do
    drop(command);
    let child = child?;
    let pid = child.id() as libc::pid_t;
    let (cap, (lines, heard)) = (step.caps.log, std::sync::mpsc::channel::<Vec<u8>>());
    let logger = std::thread::spawn(move || {
        let (mut log, mut line) = (Log::new(cap), Vec::new());
        let mut chunk = [0u8; 64 << 10];
        while let Ok(n @ 1..) = reader.read(&mut chunk) {
            log.push(&chunk[..n]);
            for &byte in &chunk[..n] {
                if byte == b'\n' {
                    let _ = lines.send(std::mem::take(&mut line));
                } else if line.len() < LINE {
                    line.push(byte);
                }
            }
        }
        if !line.is_empty() {
            let _ = lines.send(line);
        }
        log
    });
    let (start, mut looked) = (Instant::now(), Instant::now());
    let (mut timed_out, mut stopped, mut over, mut termed) = (false, false, None, None::<Instant>);
    let mut status = 0;
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        while let Ok(line) = heard.try_recv() {
            if !stopped && !on(&line) {
                stopped = true;
            }
        }
        // safety pid is our child and status and usage are writable
        let done = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut usage) };
        if done == pid {
            break;
        }
        if done < 0 {
            return Err(io::Error::last_os_error());
        }
        if let Some(dir) = step.watch.as_ref().filter(|_| looked.elapsed() > Duration::from_secs(1)) {
            looked = Instant::now();
            over = tree(dir, &step.caps, &[]).ok().filter(|t| matches!(t, Tree::Over { .. }));
        }
        // a step past its time is asked to stop and killed once its grace runs out
        if start.elapsed() > step.time && termed.is_none() {
            timed_out = true;
            termed = Some(Instant::now());
            unsafe { libc::kill(-pid, libc::SIGTERM) };
        }
        if stopped || over.is_some() || termed.is_some_and(|t| t.elapsed() > step.grace) {
            // safety the group is the step's own session
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            sweep(step.user)?;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    sweep(step.user)?;
    let log = logger.join().map_err(|_| io::Error::other("the log reader failed"))?;
    for line in heard.try_iter() {
        if !stopped && !on(&line) {
            stopped = true;
        }
    }
    let (code, signal) = if libc::WIFEXITED(status) { (Some(libc::WEXITSTATUS(status)), None) } else { (None, Some(libc::WTERMSIG(status))) };
    Ok(Ran { code, signal, timed_out, stopped, over, log, peak: (usage.ru_maxrss as u64) << 10 })
}
