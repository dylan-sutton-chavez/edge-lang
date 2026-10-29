use super::{read, Access, Exports, Fault, Held, HostModule, Packages, State};
use std::collections::HashMap;
use std::sync::Arc;
use wasmtime::Caller;

// the channel names whose rest is the file the host writes the lines of
pub const FILE: &str = "file:";

/* takes one line a program sent or the text the channel failed with */
pub type ChannelSink = Box<dyn FnMut(&str, &str) -> Result<(), String> + Send>;

/* the packages of a run and what each may write through a file channel */
pub struct Senders {
    packages: Packages,
    access: Arc<dyn Access>,
    modules: Vec<Arc<dyn HostModule>>,
    // what each package holds of each module once a send of it asked
    held: HashMap<String, Vec<Option<Held>>>,
}

impl Senders {
    pub fn new(packages: Packages, access: Arc<dyn Access>, modules: Vec<Arc<dyn HostModule>>) -> Senders {
        Senders { packages, access, modules, held: HashMap::new() }
    }

    /* whether the root package is the only one so every send is its own */
    pub fn alone(&self) -> bool {
        self.packages.dirs.len() <= 1
    }

    /* refuses `path` to the package at `dir` unless it is the root or granted it */
    pub fn writes(&mut self, dir: &str, path: &str) -> Result<(), Fault> {
        let dir = plain(dir);
        if dir == plain(&self.packages.root) {
            return Ok(());
        }
        let Some(package) = self.packages.dirs.iter().find(|package| plain(&package.dir) == dir) else {
            return Err(Fault::denied(format!("code at '{dir}' belongs to no package of the run so it writes no {FILE}{path}")));
        };
        if !self.held.contains_key(dir) {
            let held = self.modules.iter().map(|module| self.access.held(&self.packages, package, module.name()).ok()).collect();
            self.held.insert(dir.to_string(), held);
        }
        // a package writes below its own directory where its grants read their paths
        let held = &self.held[dir];
        let below = path.strip_prefix(dir).filter(|_| !dir.is_empty());
        if let Some(below) = below
            && self.modules.iter().zip(held).any(|(module, held)| held.as_ref().is_some_and(|held| module.writes(held, below)))
        {
            return Ok(());
        }
        Err(Fault::denied(format!("'{}' has no grant that reaches {FILE}{path}, only the root package writes a file channel without one", package.name)))
    }
}

// a dir in one spelling since `./lib/` and `lib/` name the same package
fn plain(dir: &str) -> &str {
    dir.trim_start_matches("./")
}

/* hands a send to the channels or the outbox and is false if neither can */
pub(super) fn deliver(caller: &mut Caller<'_, State>, ex: &Exports, group: String, body: String) -> Result<bool, Fault> {
    if let Some(path) = group.strip_prefix(FILE)
        && caller.data().senders.as_ref().is_some_and(|senders| !senders.alone())
        && let Some(dir) = sender(caller, ex)
        && let Some(senders) = caller.data_mut().senders.as_mut()
    {
        senders.writes(&dir, path)?;
    }
    let state = caller.data_mut();
    if let Some(channel) = state.channel.as_mut() {
        // a channel that failed takes nothing more and the run ends after its step
        if state.channel_error.is_none()
            && let Err(e) = channel(&group, &body)
        {
            state.channel_error = Some(e);
        }
        return Ok(true);
    }
    let Some(outbox) = state.outbox.as_mut() else {
        state.unsent.get_or_insert(group);
        return Ok(false);
    };
    let bytes = group.len() + body.len();
    if state.outbox_bytes.saturating_add(bytes) > state.outbox_cap {
        return Err(Fault::runtime(format!("the messages sent and not yet delivered would pass the {} MB the run may hold", state.outbox_cap >> 20)));
    }
    state.outbox_bytes += bytes;
    outbox.push((group, body));
    Ok(true)
}

/* the package dir of the code sending now as the runtime names it */
fn sender(caller: &mut Caller<'_, State>, ex: &Exports) -> Option<String> {
    let asked = ex.send_package.as_ref()?.call(&mut *caller, ()).ok()?;
    let at = ex.out_ptr.call(&mut *caller, ()).ok()?;
    Some(String::from_utf8_lossy(&read(caller, ex.memory, at, asked)).into_owned())
}

#[cfg(all(test, feature = "files", feature = "perms"))]
mod tests {
    use super::*;
    use serde_json::json;

    /* a run whose root grants util files below its out dir and other nothing */
    fn senders() -> Senders {
        let dirs = json!([["", "main", [[{}, "main"]]], ["./util/", "util", [[{ "util": ["files:out/"] }, "util"]]], ["./other/", "other", [[{}, "other"]]]]);
        let packages = Packages::read(&json!({ "root": "", "needed": true, "dirs": dirs, "permissions": {} }), Some("permissions"));
        Senders::new(packages, Arc::new(crate::perms::Perms), vec![Arc::new(crate::files::Files)])
    }

    #[test]
    fn only_the_root_writes_a_file_channel_unless_a_grant_reaches_the_file() {
        let mut senders = senders();
        assert!(senders.writes("", "anything.txt").is_ok() && senders.writes("./", "sub/any.txt").is_ok());
        assert!(senders.writes("./util/", "util/out/report.txt").is_ok());
        let refused = |text: &str, name: &str| format!("'{name}' has no grant that reaches file:{text}, only the root package writes a file channel without one");
        for (dir, path, name) in [("./util/", "util/report.txt", "util"), ("./util/", "out/report.txt", "util"), ("./other/", "other/x.txt", "other")] {
            assert_eq!(senders.writes(dir, path).unwrap_err().text, refused(path, name), "{dir} {path}");
        }
        assert_eq!(senders.writes("./nowhere/", "x").unwrap_err().text, "code at 'nowhere/' belongs to no package of the run so it writes no file:x");
    }
}

#[cfg(test)]
mod outbox {
    use crate::host::{Host, Project, Status};
    use edge::embed::Limits;

    // what a pool's run sends between steps counts against what it may hold
    #[test]
    fn an_outbox_holds_no_more_than_the_run_may() {
        let kit = crate::kit::tests::bare();
        let host = Host::new(kit.load().unwrap()).unwrap();
        let mut vm = host.vm(Box::new(|_: &str| {}), Project::disk("", None), None, None).unwrap();
        vm.instance().borrow_mut().accept_sends();
        vm.set_limits(&Limits { memory: 1 << 20, ..Limits::sandbox() }).unwrap();
        let Status::Error(refused) = vm.start("line = 'x' * 1000\nfor i in range(5000):\n    send('g', line)\n", None).unwrap() else { panic!("the outbox took every send") };
        assert!(refused.contains("the messages sent and not yet delivered would pass the 1 MB the run may hold"), "{refused}");
        assert!(vm.take_sends().len() < 1100);
    }
}
