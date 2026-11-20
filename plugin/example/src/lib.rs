#![cfg_attr(target_arch = "wasm32", no_std, no_main)]

extern crate alloc;

use alloc::{string::{String, ToString}, vec::Vec};
use interpreter::abi::WireValue;
use interpreter::plugin::*;

// host builds link std for workspace checks while wasm stays no_std with the kit allocator
plugin_module!();

plugin_fn! {
    // a module constant materialised once at import
    #[constant]
    fn version() -> i64 { 1 }

    // joins `parts` with `sep` writing each one as the runtime writes it
    fn join_all(sep: String, parts: Args) -> Result<String> {
        let mut out = String::new();
        for (i, h) in parts.iter().enumerate() {
            if i > 0 { out.push_str(&sep); }
            out.push_str(&h.text()?);
        }
        Ok(out)
    }

    // the slug of every item `items` yields joined by the `sep` keyword or a dash
    fn slug_all(items: Handle, kw: Kwargs) -> Result<String> {
        let sep = kw.get::<String>("sep")?.unwrap_or_else(|| "-".to_string());
        let it = items.iter()?;
        let mut slugs = Vec::new();
        while let Some(item) = it.iter_next()? {
            slugs.push(words(&item.text()?).join("-"));
        }
        Ok(slugs.join(&sep))
    }

    // the id of a request from a system call that answers at once
    fn request_id(url: String) -> Result<i64> {
        let (method, target) = (WireValue::Bytes(b"GET".to_vec()).into_handle()?, WireValue::Bytes(url.into_bytes()).into_handle()?);
        let id = Handle::sys("net.request", &[method.raw(), target.raw()])?;
        i64::from_handle(id.raw())
    }

    // the status `url` answers with where the response waits and `resume` finishes
    fn status_of(url: String) -> Result<i64> {
        let (method, target) = (WireValue::Bytes(b"GET".to_vec()).into_handle()?, WireValue::Bytes(url.into_bytes()).into_handle()?);
        let id = Handle::sys("net.request", &[method.raw(), target.raw()])?;
        status(Handle::sys("net.response", &[id.raw()])?)
    }
}

plugin_resume! {
    fn resume(_call: u32, head: Result<Handle>) -> Result<i64> {
        status(head?)
    }
}

// the lowercase runs of letters and digits in `text`
fn words(text: &str) -> Vec<String> {
    let lower: String = text.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    lower.split('-').filter(|p| !p.is_empty()).map(String::from).collect()
}

fn status(head: Handle) -> Result<i64> {
    i64::from_handle(head.get_item(&WireValue::Int(0).into_handle()?)?.raw())
}

plugin_class! {
    // slug parts accumulated across calls
    pub struct Slugger {
        parts: Vec<String>,
    }

    impl Slugger {
        #[ctor]
        pub fn new() -> Self {
            Self { parts: Vec::new() }
        }

        pub fn add(&mut self, s: String) {
            self.parts.extend(words(&s));
        }

        pub fn build(&self) -> String {
            self.parts.join("-")
        }

        pub fn shout(&self) -> String {
            let mut out = self.parts.join("-").to_uppercase();
            out.push('!');
            out
        }

        pub fn repeat(&self, n: i64) -> Result<String> {
            if n < 0 {
                return Err(Error::Value("repeat count must be non-negative".to_string()));
            }
            Ok(self.parts.join("-").repeat(n as usize))
        }

        pub fn total_len(&self) -> i64 {
            self.parts.iter().map(|p| p.len() as i64).sum()
        }

        pub fn pop(&mut self) -> Option<String> {
            self.parts.pop()
        }
    }
}

impl Default for Slugger {
    fn default() -> Self {
        Self::new()
    }
}
