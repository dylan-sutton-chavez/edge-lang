use super::User;
use std::io;
use std::time::Duration;

/* the processes of `uid` still running where a zombie holds nothing and is skipped */
pub fn alive(uid: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return found };
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        // the status file names real uids even of a process that made itself undumpable
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else { continue };
        let field = |name: &str| status.lines().find_map(|l| l.strip_prefix(name)).map(str::trim).unwrap_or("");
        let zombie = field("State:").starts_with('Z');
        let real = field("Uid:").split_whitespace().next().and_then(|u| u.parse::<u32>().ok());
        if real == Some(uid) && !zombie {
            found.push(pid);
        }
    }
    found
}

// one signal from a child that became the user ends every process the user owns
fn kill_as(user: User) -> io::Result<()> {
    // safety the child calls only async signal safe functions before it exits
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            let ok = libc::setgroups(0, std::ptr::null()) == 0 && libc::setgid(user.gid) == 0 && libc::setuid(user.uid) == 0;
            if ok {
                libc::kill(-1, libc::SIGKILL);
            }
            libc::_exit(if ok { 0 } else { 1 });
        }
    }
    let mut status = 0;
    // safety pid is the child just forked
    unsafe { libc::waitpid(pid, &mut status, 0) };
    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 { Ok(()) } else { Err(io::Error::other("could not become the step's user to end its processes")) }
}

/* ends every process of `user` and returns once none is left */
pub fn sweep(user: User) -> io::Result<()> {
    if user.uid == 0 {
        return Err(io::Error::other("a step never runs as root so root is never swept"));
    }
    for _ in 0..100 {
        if alive(user.uid).is_empty() {
            return Ok(());
        }
        kill_as(user)?;
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(io::Error::other(format!("{} processes of uid {} would not end", alive(user.uid).len(), user.uid)))
}
