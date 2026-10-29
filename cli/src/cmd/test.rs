use anyhow::{bail, Context, Result};
use clap::{ArgMatches, Args, FromArgMatches};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::host::driver::{path_spec, Session};
#[cfg(feature = "web")]
use crate::host::browser;
use crate::host::RunLimits;
use crate::kit::{self, Command, Kit};
use crate::ui;

#[derive(Args)]
struct TestArgs {
    /// Directory or file to run, the tree is searched when omitted.
    path: Option<PathBuf>,
    /// Run them on the browser host in headless Chrome instead of the native engine.
    #[cfg(feature = "web")]
    #[arg(long)]
    web: bool,
    #[command(flatten)]
    limits: RunLimits,
}

/* `test` over every file the runtime's test suffix names */
pub struct Test;

impl Command for Test {
    fn spec(&self, kit: &Kit) -> clap::Command {
        let command = TestArgs::augment_args(clap::Command::new("test").about(format!("Run *{} files", kit.descriptor().test_suffix)));
        RunLimits::shown(command, kit.run_limits())
    }

    fn summary(&self, kit: &Kit) -> (String, String) {
        ("test [path]".to_string(), format!("Run *{} files{}", kit.descriptor().test_suffix, crate::web::FLAG))
    }

    fn compiles(&self) -> bool {
        true
    }

    // the tests under a path resolve through the manifest nearest above it
    fn manifest(&self, m: &ArgMatches) -> PathBuf {
        let dir = match m.try_get_one::<PathBuf>("path").ok().flatten() {
            Some(path) if path.is_dir() => format!("{}/", path_spec(path).trim_end_matches('/')),
            Some(path) => edge::deps::dir_of(&path_spec(path)),
            None => String::new(),
        };
        kit::root_manifest(m, &dir)
    }

    fn run(&self, kit: &Kit, m: &ArgMatches) -> Result<i32> {
        let args = TestArgs::from_arg_matches(m)?;
        args.limits.set();
        #[cfg(feature = "web")]
        let web = args.web;
        #[cfg(not(feature = "web"))]
        let web = false;
        run(kit, &kit::manifest_path(m), kit::manifest_flag(m).as_deref(), args.path.as_deref(), web)?;
        Ok(0)
    }
}

// one browser or one session opened once for the whole suite
enum Engine {
    Native(Session),
    #[cfg(feature = "web")]
    Web(browser::Host),
}

/* the engine the suite runs on which is a browser only where one is wired */
#[cfg(feature = "web")]
fn open_engine(kit: &Kit, manifest_path: &Path, web: bool, open: &dyn Fn() -> Result<Session>) -> Result<Engine> {
    match web {
        true => Ok(Engine::Web(browser::Host::open(kit, Some(manifest_path))?)),
        false => Ok(Engine::Native(open_or_die(open))),
    }
}

#[cfg(not(feature = "web"))]
fn open_engine(_kit: &Kit, _manifest_path: &Path, _web: bool, open: &dyn Fn() -> Result<Session>) -> Result<Engine> {
    Ok(Engine::Native(open_or_die(open)))
}

// drives each test file through one engine with verdicts only from exit codes
fn run(kit: &Kit, manifest_path: &Path, manifest: Option<&Path>, path: Option<&Path>, web: bool) -> Result<()> {
    let suffix = kit.descriptor().test_suffix;
    let target = path.unwrap_or(Path::new("."));
    let files = if target.is_file() { vec![target.to_path_buf()] } else { discover_tests(target, suffix) };
    if files.is_empty() {
        bail!("no *{suffix} files found under {}", target.display());
    }

    let open = || Session::open(kit, manifest);
    let mut engine = open_engine(kit, manifest_path, web, &open)?;

    let started = std::time::Instant::now();
    let mut failed = 0usize;
    for (i, file) in files.iter().enumerate() {
        // a browser gets a fresh page per file and a session is wiped between files
        if let (Engine::Native(session), true) = (&mut engine, i > 0)
            && session.reset().is_err()
        {
            engine = Engine::Native(open_or_die(&open));
        }
        let result = std::fs::read_to_string(file)
            .with_context(|| format!("reading {}", file.display()))
            .and_then(|src| match &mut engine {
                Engine::Native(session) => run_file(session, &src, file),
                #[cfg(feature = "web")]
                Engine::Web(host) => web_file(host, &src, file),
            });
        let (ok, reason) = match result {
            Ok(v) => v,
            // a wedged session would poison later files so it reopens
            Err(e) => {
                ui::error(&e);
                if native(&engine) {
                    engine = Engine::Native(open_or_die(&open));
                }
                (false, Some("error"))
            }
        };
        let name = file.strip_prefix(".").unwrap_or(file);
        ui::test_verdict(ok, &name.display().to_string(), reason);
        if !ok {
            failed += 1;
        }
    }

    ui::test_summary(files.len() - failed, files.len(), started.elapsed().as_secs_f64());
    drop(engine);
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/* test files under `dir` sorted and without dist or hidden entries */
fn discover_tests(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(dir, suffix, &mut HashSet::new(), &mut found);
    found.sort();
    found
}

fn walk(dir: &Path, suffix: &str, seen: &mut HashSet<PathBuf>, out: &mut Vec<PathBuf>) {
    if !crate::host::first_visit(seen, dir) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') || name == "dist" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk(&path, suffix, seen, out);
        } else if name.ends_with(suffix) {
            out.push(path);
        }
    }
}

// exit 2 keeps infra failures distinct from red tests
fn open_or_die(open: &dyn Fn() -> Result<Session>) -> Session {
    match open() {
        Ok(s) => s,
        Err(e) => {
            ui::error(&e);
            std::process::exit(2);
        }
    }
}

/* whether the engine is a session which a file that wedged it reopens */
#[cfg(feature = "web")]
fn native(engine: &Engine) -> bool {
    matches!(engine, Engine::Native(_))
}

#[cfg(not(feature = "web"))]
fn native(_engine: &Engine) -> bool {
    true
}

/* a file passes when its page exits 0 */
#[cfg(feature = "web")]
fn web_file(host: &browser::Host, src: &str, file: &Path) -> Result<(bool, Option<&'static str>)> {
    Ok((host.run(src, &path_spec(file))? == 0, None))
}

// a file passes when it ends cleanly or exits 0
fn run_file(session: &mut Session, src: &str, file: &Path) -> Result<(bool, Option<&'static str>)> {
    let outcome = session.eval(src, Some(&path_spec(file)), None)?;
    if let Some(err) = outcome.err {
        ui::traceback(&err);
        return Ok((false, None));
    }
    Ok((outcome.exit_code.is_none_or(|code| code == 0), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_skips_dist_and_hidden() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["a_test.mini", "sub/b_test.mini", "dist/c_test.mini", ".git/d_test.mini", "util.mini"] {
            let p = dir.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
        }
        let names: Vec<_> = discover_tests(dir.path(), "_test.mini").iter().map(|p| p.strip_prefix(dir.path()).unwrap().to_str().unwrap().to_string()).collect();
        assert_eq!(names, ["a_test.mini", "sub/b_test.mini"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_back_up_the_tree_is_walked_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a_test.mini"), "").unwrap();
        std::os::unix::fs::symlink("..", dir.path().join("sub/up")).unwrap();
        std::os::unix::fs::symlink(".", dir.path().join("loop")).unwrap();
        let names: Vec<_> = discover_tests(dir.path(), "_test.mini").iter().map(|p| p.strip_prefix(dir.path()).unwrap().to_str().unwrap().to_string()).collect();
        assert_eq!(names, ["sub/a_test.mini"]);
    }
}
