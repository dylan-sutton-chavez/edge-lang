#[cfg(target_os = "linux")]
mod linux {
    use cli::prep::caps::{tree, Caps, Tree};
    use cli::prep::capacity::{measure, Capacity};
    use cli::prep::run::{run, Step};
    use cli::prep::scope::Scope;
    use cli::prep::seal::seal;
    use cli::prep::sweep::alive;
    use cli::prep::User;
    use std::io::Write;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    // the variable that holds the parent's secret the way a box holds its token
    const TOKEN: &str = "PREP_TOKEN";
    // every token starts so so a hostile child can look for one it never saw
    const SHAPE: &str = "preptok-";
    // an id no account on a test machine holds so the sweep ends only this test's processes
    const USER: User = User { uid: 64123, gid: 64123 };

    fn say(capacity: &Capacity) -> String {
        format!("{} {} {} {} {}", capacity.threads, capacity.cpu, capacity.memory, capacity.disk_free, capacity.disk_total)
    }

    fn holds(bytes: &[u8]) -> bool {
        bytes.windows(SHAPE.len()).any(|w| w == SHAPE.as_bytes())
    }

    /* what this binary does when a step runs it as the unprivileged user */
    pub fn child(mode: &str, arg: &str) -> ! {
        match mode {
            "who" => {
                let status = std::fs::read_to_string("/proc/self/status").unwrap();
                let field = |name: &str| status.lines().find_map(|l| l.strip_prefix(name)).unwrap_or("").split_whitespace().collect::<Vec<_>>().join(",");
                let fds: Vec<String> = std::fs::read_dir("/proc/self/fd").unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
                let env: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
                println!("uid {}\ngid {}\ngroups {}\nnnp {}\nfds {}\nenv {}", field("Uid:"), field("Gid:"), field("Groups:"), field("NoNewPrivs:"), fds.len(), env.join(" "));
            }
            "capacity" => println!("{}", say(&measure(Path::new(arg)).unwrap())),
            "steal" => {
                let (mut tried, mut read, mut hits) = (0, 0, Vec::new());
                for (k, v) in std::env::vars() {
                    if holds(format!("{k}={v}").as_bytes()) { hits.push("env".to_string()); }
                }
                for entry in std::fs::read_dir("/proc").unwrap().flatten() {
                    // its own descriptors are its own pipes and reading one would wait forever
                    let pid = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok());
                    if pid.is_none() || pid == Some(std::process::id()) { continue; }
                    for file in ["environ", "cmdline", "mem", "maps"] {
                        tried += 1;
                        if let Ok(bytes) = std::fs::read(entry.path().join(file)) {
                            read += 1;
                            if holds(&bytes) { hits.push(format!("{}/{file}", entry.path().display())); }
                        }
                    }
                    // a descriptor the parent left open would be a way to its memory or files
                    if let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) {
                        for fd in fds.flatten() {
                            if let Ok(bytes) = std::fs::read(fd.path()) && holds(&bytes) { hits.push(fd.path().display().to_string()); }
                        }
                    }
                }
                let mut dirs = vec![PathBuf::from(arg), PathBuf::from("/tmp")];
                while let Some(dir) = dirs.pop() {
                    let Ok(list) = std::fs::read_dir(&dir) else { continue };
                    for entry in list.flatten() {
                        let kind = entry.file_type().unwrap();
                        if kind.is_dir() { dirs.push(entry.path()); continue; }
                        if !kind.is_file() || entry.metadata().map_or(true, |m| m.len() > 16 << 20) { continue; }
                        tried += 1;
                        if let Ok(bytes) = std::fs::read(entry.path()) {
                            read += 1;
                            if holds(&bytes) { hits.push(entry.path().display().to_string()); }
                        }
                    }
                }
                println!("tried {tried}\nread {read}\nhits {}", hits.join(" "));
            }
            "linger" => {
                std::fs::write(Path::new(arg).join("a.txt"), "good").unwrap();
                let exe = std::env::current_exe().unwrap();
                // the background process outlives this one and rewrites the output after the step
                #[allow(clippy::zombie_processes)]
                std::process::Command::new(exe).args(["rewrite", arg]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
            }
            "rewrite" => {
                // it waits past the step's end so only the sweep can stop it
                std::thread::sleep(Duration::from_millis(300));
                for _ in 0..500 {
                    std::thread::sleep(Duration::from_millis(10));
                    let _ = std::fs::write(Path::new(arg).join("a.txt"), "bad");
                    let _ = std::fs::write(Path::new(arg).join("late.txt"), "late");
                }
            }
            "link" => symlink("/etc/passwd", Path::new(arg).join("passwd")).unwrap(),
            "spin" => loop { std::thread::sleep(Duration::from_secs(1)) },
            "stubborn" => {
                unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
                loop { std::thread::sleep(Duration::from_secs(1)) }
            }
            "count" => {
                let mut hold = Vec::new();
                loop {
                    hold.extend_from_slice(&[1u8; 1 << 20]);
                    println!("line {} {}", (hold.len() >> 20) - 1, hold.len());
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            "fill" => {
                let mut file = std::fs::File::create(Path::new(arg).join("big")).unwrap();
                for _ in 0..64 { if file.write_all(&[7u8; 1 << 20]).is_err() { std::process::exit(9) } file.sync_data().ok(); std::thread::sleep(Duration::from_millis(50)); }
            }
            "say" => print!("{}", "x".repeat(1 << 20)),
            _ => std::process::exit(2),
        }
        std::process::exit(0)
    }

    fn step(exe: &Path, args: &[&str], cwd: &Path) -> Step {
        let mut argv = vec![exe.display().to_string()];
        argv.extend(args.iter().map(|a| a.to_string()));
        Step { time: Duration::from_secs(60), ..Step::new(argv, cwd.to_path_buf(), USER) }
    }

    fn text(step: &Step) -> String {
        let ran = run(step).unwrap();
        assert!(ran.ok(), "{:?} ended {:?}", step.argv, ran);
        String::from_utf8(ran.log.kept).unwrap()
    }

    fn line<'a>(said: &'a str, key: &str) -> &'a str {
        said.lines().find_map(|l| l.strip_prefix(key).map(str::trim)).unwrap_or("")
    }

    fn own(path: &Path) {
        std::os::unix::fs::chown(path, Some(USER.uid), Some(USER.gid)).unwrap();
    }

    /* every case as root where each child is this binary run as the unprivileged user */
    pub fn cases() {
        let scratch = std::env::temp_dir().join(format!("prep-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(scratch.join("bin")).unwrap();
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o755)).unwrap();
        // a copy the user can reach since the build tree may sit under a closed home
        let exe = scratch.join("bin/prep");
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(alive(USER.uid).is_empty(), "uid {} already runs processes on this machine", USER.uid);
        let token = std::env::var(TOKEN).unwrap();

        // g1 the child is the user with no groups no privileges and only the env it was given
        let mut who = step(&exe, &["who"], &scratch);
        who.env = vec![("ONLY".into(), "this".into())];
        let said = text(&who);
        assert_eq!(line(&said, "uid"), "64123,64123,64123,64123", "{said}");
        assert_eq!(line(&said, "gid"), "64123,64123,64123,64123", "{said}");
        assert_eq!(line(&said, "groups"), "", "{said}");
        assert_eq!(line(&said, "nnp"), "1", "{said}");
        assert_eq!(line(&said, "env"), "ONLY=this", "{said}");
        // stdio and the one the listing itself opened
        assert_eq!(line(&said, "fds"), "4", "{said}");
        println!("ok: the step runs as uid 64123 with no groups and only its env");

        // g1 capacity the child reads is what the parent measures
        let parent = measure(&scratch).unwrap();
        let seen: Vec<u64> = text(&step(&exe, &["capacity", scratch.to_str().unwrap()], &scratch)).split_whitespace().map(|n| n.parse().unwrap()).collect();
        assert_eq!((seen[0], seen[1], seen[2], seen[4]), (parent.threads, parent.cpu, parent.memory, parent.disk_total), "child {seen:?} parent {parent:?}");
        // free disk moves with the rest of the machine so it only has to agree closely
        assert!(seen[3].abs_diff(parent.disk_free) < 512 << 20, "child free {} parent free {}", seen[3], parent.disk_free);
        println!("ok: the child sees {} threads {} cpu {} bytes memory and the same disk as the parent", parent.threads, parent.cpu, parent.memory);

        // g1 a hostile child finds no token where the parent can
        let secret = scratch.join("secret");
        std::fs::create_dir(&secret).unwrap();
        std::fs::write(secret.join("token"), &token).unwrap();
        std::fs::set_permissions(secret.join("token"), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(holds(&std::fs::read("/proc/self/environ").unwrap()), "the parent holds no token so the case proves nothing");
        let said = text(&step(&exe, &["steal", scratch.to_str().unwrap()], &scratch));
        assert_eq!(line(&said, "hits"), "", "the child found the token in {said}");
        let (tried, read): (u64, u64) = (line(&said, "tried").parse().unwrap(), line(&said, "read").parse().unwrap());
        assert!(tried > 10 && read > 0 && read < tried, "{said}");
        assert!(!said.contains(&token));
        println!("ok: the child tried {tried} environ cmdline mem and files read {read} and found no token");

        // g2 a process left behind is ended and the sealed output is the step's own
        let out = scratch.join("out");
        std::fs::create_dir(&out).unwrap();
        own(&out);
        let ran = run(&step(&exe, &["linger", out.to_str().unwrap()], &scratch)).unwrap();
        assert!(ran.ok(), "{ran:?}");
        assert!(alive(USER.uid).is_empty(), "the step left processes");
        std::thread::sleep(Duration::from_millis(800));
        assert!(!out.join("late.txt").exists(), "the background process outlived the step");
        let sealed = seal(&out, &scratch.join("sealed"), &Caps::default()).unwrap();
        assert_eq!(sealed.len(), 1, "{sealed:?}");
        assert_eq!(sealed[0].sha256, edge::util::sha256::sha256(b"good"));
        assert_eq!(std::fs::read(scratch.join("sealed/a.txt")).unwrap(), b"good");
        assert_eq!(std::fs::metadata(scratch.join("sealed")).unwrap().permissions().mode() & 0o022, 0);
        println!("ok: the background rewriter was ended and the sealed a.txt hashes to good");

        // g2 a link among the outputs is refused rather than read as root
        let linked = scratch.join("linked");
        std::fs::create_dir(&linked).unwrap();
        own(&linked);
        text(&step(&exe, &["link", linked.to_str().unwrap()], &scratch));
        assert!(seal(&linked, &scratch.join("linked.sealed"), &Caps::default()).is_err());
        println!("ok: an output that links to /etc/passwd is refused");

        // g2 a step past its time ends with 124 and leaves nothing
        let ran = run(&Step { time: Duration::from_millis(500), ..step(&exe, &["spin"], &scratch) }).unwrap();
        assert!(ran.timed_out && ran.exit() == 124 && alive(USER.uid).is_empty(), "{ran:?}");
        assert_eq!(ran.signal, Some(libc::SIGTERM), "{ran:?}");
        let started = std::time::Instant::now();
        let ran = run(&Step { time: Duration::from_millis(300), grace: Duration::from_millis(700), ..step(&exe, &["stubborn"], &scratch) }).unwrap();
        assert!(ran.timed_out && ran.signal == Some(libc::SIGKILL) && started.elapsed() >= Duration::from_secs(1) && alive(USER.uid).is_empty(), "{ran:?}");
        println!("ok: a step past its time is stopped then killed past its grace and ends with 124");

        // a line handler sees each line live and ending the step reads its peak memory
        let mut heard = Vec::new();
        let ran = cli::prep::run::run_with(&step(&exe, &["count"], &scratch), |line| {
            heard.push(String::from_utf8_lossy(line).into_owned());
            !line.starts_with(b"line 5 ")
        })
        .unwrap();
        assert!(ran.stopped && !ran.ok() && alive(USER.uid).is_empty(), "{ran:?}");
        assert_eq!(heard.first().map(String::as_str), Some("line 0 1048576"));
        assert!(heard.iter().any(|l| l.starts_with("line 5 ")) && ran.peak >= 6 << 20, "{heard:?} peak {}", ran.peak);
        println!("ok: the handler ended the step at line 5 with a peak of {} MB", ran.peak >> 20);

        // g5 the watched tree past the repo cap ends the step
        let filled = scratch.join("filled");
        std::fs::create_dir(&filled).unwrap();
        own(&filled);
        let small = Caps { repo: 8 << 20, ..Caps::default() };
        let ran = run(&Step { caps: small, watch: Some(filled.clone()), ..step(&exe, &["fill", filled.to_str().unwrap()], &scratch) }).unwrap();
        assert!(matches!(ran.over, Some(Tree::Over { cap, .. }) if cap == 8 << 20) && !ran.ok(), "{ran:?}");
        assert!(std::fs::metadata(filled.join("big")).unwrap().len() < 32 << 20);
        println!("ok: a tree past its repo cap ends the step");
        // a file past its size limit stops the writer
        std::fs::remove_file(filled.join("big")).unwrap();
        let ran = run(&Step { fsize: Some(1 << 20), ..step(&exe, &["fill", filled.to_str().unwrap()], &scratch) }).unwrap();
        assert!(!ran.ok() && std::fs::metadata(filled.join("big")).unwrap().len() <= 1 << 20, "{ran:?}");
        assert!(matches!(tree(&filled, &Caps { file: 512 << 10, ..Caps::default() }, &[]).unwrap(), Tree::Over { cap, .. } if cap == 512 << 10));
        assert!(matches!(tree(&filled, &Caps::default(), &[]).unwrap(), Tree::Held { files: 1, .. }));
        println!("ok: a file past its limit stops and the tree check names it");
        // the log keeps its cap and counts the rest
        let ran = run(&Step { caps: Caps { log: 1000, ..Caps::default() }, ..step(&exe, &["say"], &scratch) }).unwrap();
        assert!(ran.ok() && ran.log.kept.len() == 1000 && ran.log.dropped == (1 << 20) - 1000, "{:?}", (ran.log.kept.len(), ran.log.dropped));
        assert_eq!(Caps::default(), Caps { repo: 5 << 30, file: 100 << 20, log: 100 << 20 });
        println!("ok: the log keeps 1000 bytes and counts the rest");

        // a step as root is refused outright
        assert!(run(&Step { user: User { uid: 0, gid: 0 }, ..step(&exe, &["who"], &scratch) }).is_err());
        scope(&scratch);
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    /* g3 reads writes and walks stay inside the root whatever a path or link says */
    fn scope(scratch: &Path) {
        let root = scratch.join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(scratch.join("beside"), "outside").unwrap();
        std::fs::write(root.join("a"), "a").unwrap();
        std::fs::write(root.join("sub/b"), "b").unwrap();
        symlink("sub/b", root.join("in")).unwrap();
        symlink("../beside", root.join("up")).unwrap();
        symlink("/etc/passwd", root.join("out")).unwrap();
        symlink("/etc", root.join("etc")).unwrap();
        symlink(root.join("a"), root.join("whole")).unwrap();
        symlink("..", root.join("sub/parent")).unwrap();
        symlink("loop", root.join("loop")).unwrap();
        let scope = Scope::new(&root).unwrap();
        let read = |p: &str| scope.read(Path::new(p), 1 << 20).map(|b| String::from_utf8(b).unwrap());
        for (path, want) in [("a", "a"), ("sub/b", "b"), ("sub/../a", "a"), ("./sub/./b", "b"), ("in", "b"), ("whole", "a"), ("sub/parent/a", "a"), ("sub/parent/sub/parent/in", "b")] {
            assert_eq!(read(path).unwrap(), want, "{path}");
        }
        assert_eq!(read(root.join("a").to_str().unwrap()).unwrap(), "a");
        for path in ["../beside", "sub/../../beside", "up", "out", "etc/passwd", "/etc/passwd", "loop", "sub/parent/../beside", "sub/parent/up"] {
            assert!(read(path).is_err(), "{path} was read");
        }
        assert!(scope.write(Path::new("out"), b"x").is_err() && scope.write(Path::new("up"), b"x").is_err());
        assert!(scope.write(Path::new("../written"), b"x").is_err() && scope.write(Path::new("/tmp/written"), b"x").is_err());
        assert_eq!(std::fs::read(scratch.join("beside")).unwrap(), b"outside");
        assert!(!scratch.join("written").exists());
        scope.write(Path::new("sub/new"), b"new").unwrap();
        assert_eq!(read("sub/new").unwrap(), "new");
        assert!(scope.read(Path::new("a"), 0).is_err());
        let walked: Vec<String> = scope.walk().unwrap().iter().map(|p| p.display().to_string()).collect();
        assert_eq!(walked, ["a", "sub/b", "sub/new"]);
        println!("ok: the scope reads writes and walks only inside its root");
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    {
        let args: Vec<String> = std::env::args().collect();
        if args.len() > 1 && !args[1].starts_with('-') {
            linux::child(&args[1], args.get(2).map_or("", String::as_str));
        }
        // the cases need root to drop to another user so a plain run says it skipped
        if unsafe { libc::geteuid() } != 0 {
            println!("skipped: the prep cases run as root on linux");
            return;
        }
        // the parent starts again holding a token in its own environment
        if std::env::var_os("PREP_TOKEN").is_none() {
            let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let code = std::process::Command::new(std::env::current_exe().unwrap()).env("PREP_TOKEN", format!("preptok-{seed:x}")).status().unwrap();
            std::process::exit(code.code().unwrap_or(1));
        }
        linux::cases();
    }
    #[cfg(not(target_os = "linux"))]
    println!("skipped: the prep cases run as root on linux");
}
