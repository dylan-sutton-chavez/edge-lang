#![cfg_attr(target_arch = "wasm32", no_std, no_main)]

extern crate alloc;

use alloc::{string::{String, ToString}, vec::Vec};
use wasm_pdk::*;

// Host builds link std for the workspace checks, wasm stays no_std with the allocator of the kit.
module!();

/// Module constant materialised once at import. Showcases #[plugin_const]. Build the module with `cargo build --release --target wasm32-unknown-unknown -p slugify-mod`.
#[plugin_const]
fn version() -> i64 { 1 }

/// Variadic join over `parts`, separated by `sep`, each written as `"{}".format` writes it. Showcases the trailing `Args` param and `Handle::call`.
#[plugin_fn]
fn join_all(sep: String, parts: Args) -> Result<String> {
    let template = "{}".into_handle()?;
    let mut out = String::new();
    for (i, h) in parts.0.iter().enumerate() {
        if i > 0 { out.push_str(&sep); }
        out.push_str(&String::from_handle(template.call("format", &[h.raw()])?.raw())?);
    }
    Ok(out)
}

/// Nanoseconds on the wall clock, a system call that answers at once. Showcases `Handle::sys`.
#[plugin_fn]
fn wall_ns() -> Result<i64> {
    let now = Handle::sys("time.now", &[])?;
    i64::from_handle(now.raw())
}

/// The status `url` answers with, the response waits and `resume` finishes. Showcases `#[plugin_resume]`.
#[plugin_fn]
fn status_of(url: String) -> Result<i64> {
    let (method, target) = (encode(Value::Bytes(b"GET".to_vec()))?, encode(Value::Bytes(url.into_bytes()))?);
    let id = Handle::sys("net.request", &[method.raw(), target.raw()])?;
    status(Handle::sys("net.response", &[id.raw()])?)
}

#[plugin_resume]
fn resume(_call: u32, head: Result<Handle>) -> Result<i64> {
    status(head?)
}

fn status(head: Handle) -> Result<i64> {
    i64::from_handle(head.get_item(&encode(Value::Int(0))?)?.raw())
}

/// Accumulates slug parts across calls. Exercises mutable state and Option/Result returns.
#[plugin_class]
pub struct Slugger {
    parts: Vec<String>,
}

#[plugin_methods]
impl Slugger {
    #[plugin_ctor]
    pub fn new() -> Self {
        Self { parts: Vec::new() }
    }

    pub fn add(&mut self, s: String) {
        let normalized: String = s.to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        for part in normalized.split('-').filter(|p| !p.is_empty()) {
            self.parts.push(part.to_string());
        }
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

impl Default for Slugger {
    fn default() -> Self {
        Self::new()
    }
}
