/* The answers a program's imports got, each (importer dir, spec) to the module it reached and its source, as a bundle
   under hex keys, so a native build and the binary it makes answer them again the same. */

use alloc::{boxed::Box, rc::Rc, string::String, vec};
use core::cell::RefCell;
use crate::{s, util::sha256::hex_encode};
use super::{bundle::{Bundle, Entry}, dir_of, Resolved, Resolver};

/* Answers imports from the bundle, or with `inner` as `inner` answers them, writing each answer down: a refusal or a
   plugin as `!` and why. The entry dir sits under `d`. */
pub struct Table { pub bundle: Rc<RefCell<Bundle>>, dir: String, inner: Option<Box<dyn Resolver>> }

fn key(tag: &str, text: &str) -> String { s!(str tag, str &hex_encode(text.as_bytes())) }

impl Table {
    pub fn recording(dir: &str, inner: Box<dyn Resolver>) -> Table {
        Table { bundle: Rc::new(RefCell::new(Bundle { entry: "d".into(), files: vec![Entry { path: "d".into(), bytes: dir.into() }] })), dir: dir.into(), inner: Some(inner) }
    }
    pub fn new(bytes: &[u8]) -> Result<Table, String> { let t = Table { bundle: Rc::new(RefCell::new(Bundle::decode(bytes)?)), dir: String::new(), inner: None }; Ok(t.at(&t.get("d").unwrap_or_default())) }
    /* The same answers from module dir `dir`, for code that eval or exec compiles there. */
    pub fn at(&self, dir: &str) -> Table { Table { bundle: self.bundle.clone(), dir: dir.into(), inner: None } }
    fn get(&self, path: &str) -> Option<String> { self.bundle.borrow().files.iter().find(|f| f.path == path).map(|f| String::from_utf8_lossy(&f.bytes).into_owned()) }
}

impl Resolver for Table {
    fn resolve(&mut self, spec: &str) -> Result<Resolved, String> {
        let at = key("k", &s!(str &self.dir, "\0", str spec));
        let Some(inner) = self.inner.as_mut() else {
            let answer = self.get(&at).ok_or_else(|| s!("module '", str spec, "' was not imported when the program was built"))?;
            if let Some(why) = answer.strip_prefix('!') { return Err(why.into()); }
            let src = self.get(&key("m", &answer)).ok_or_else(|| s!("module '", str spec, "' was built without its source"))?;
            return Ok(Resolved::Code { src, canonical: answer });
        };
        let got = inner.resolve(spec);
        let answer = match &got {
            Ok(Resolved::Code { src, canonical }) => { self.bundle.borrow_mut().files.push(Entry { path: key("m", canonical), bytes: src.clone().into_bytes() }); canonical.clone() }
            Ok(Resolved::Native { .. }) => s!("!edge build runs Python modules only; '", str spec, "' is a plugin"),
            Err(e) => s!("!", str e),
        };
        self.bundle.borrow_mut().files.push(Entry { path: at, bytes: answer.into_bytes() });
        got
    }
    fn child(&self, spec: &str) -> Box<dyn Resolver> { Box::new(Table { bundle: self.bundle.clone(), dir: dir_of(spec), inner: self.inner.as_ref().map(|i| i.child(spec)) }) }
}
