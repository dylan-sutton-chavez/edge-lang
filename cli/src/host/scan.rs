use anyhow::{anyhow, Result};
use std::cell::RefCell;
use std::rc::Rc;

use super::resolver::{step, Step};
use super::{linear_cap, Host, Instance, Project};
use crate::kit::{Descriptor, Kit};
use edge::embed::Limits;

/* what sources import as the runtime reads them so the host knows no spelling */
pub struct Scan<'k> {
    kit: &'k Kit,
    // opened on the first source so a build without packages never boots the runtime
    inst: Option<Rc<RefCell<Instance>>>,
}

impl<'k> Scan<'k> {
    pub fn new(kit: &'k Kit) -> Scan<'k> {
        Scan { kit, inst: None }
    }

    pub fn descriptor(&self) -> &'static Descriptor {
        self.kit.descriptor()
    }

    /* every module `src` at `address` imports relative to itself joined onto that address */
    pub fn relative(&mut self, address: &str, src: &str) -> Result<Vec<String>> {
        let inst = match &self.inst {
            Some(inst) => inst.clone(),
            None => {
                let host = Host::new(self.kit.load()?)?;
                let inst = host.instance(Box::new(|_: &str| {}), Project::default(), None, linear_cap(Limits::sandbox().memory))?;
                self.inst.insert(inst).clone()
            }
        };
        let mut inst = inst.borrow_mut();
        inst.name_entry(address)?;
        let found = walk(&mut inst, src).map_err(|e| anyhow!("reading the imports of '{address}': {e}"))?;
        // the walk leaves a refusal behind for every name it could not reach
        inst.reset_modules()?;
        Ok(found)
    }
}

/* drives the walk with every fetch missing so it names each import and reads none */
fn walk(inst: &mut Instance, src: &str) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    let mut out = inst.walk_start(src, "")?;
    loop {
        out = match step(&out)? {
            Step::Fetch(spec) => {
                if !spec.ends_with("edge.json") {
                    found.push(spec);
                }
                inst.walk_fetched(&[], 1)?
            }
            Step::Plugin { .. } => inst.walk_plugin(2, "")?,
            Step::System(_) => inst.walk_served("")?,
            Step::Undeclared => inst.walk_known("")?,
            Step::Done(_) => return Ok(found),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /* a relative import lands on the address of its file and nothing else is fetched */
    #[test]
    fn the_runtime_names_what_a_source_imports() {
        let kit = crate::kit::tests::kit();
        let mut scan = Scan::new(&kit);
        let src = "from .util import shout\nfrom ..up.word import WORD\nimport json\nimport pkg.mod\n// from .commented import nothing\nprint(\"from .quoted import nothing\")\n";
        let found = scan.relative("https://cdn.test/pkg/greet/0.1.0/lib/main.mini", src).unwrap();
        assert_eq!(found, ["https://cdn.test/pkg/greet/0.1.0/lib/util.mini", "https://cdn.test/pkg/greet/0.1.0/up/word.mini"]);
        let found = scan.relative("https://cdn.test/pkg/greet/0.1.0/main.mini", "from .lib.main import shout\n").unwrap();
        assert_eq!(found, ["https://cdn.test/pkg/greet/0.1.0/lib/main.mini"]);
    }
}
