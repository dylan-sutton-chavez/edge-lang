use crate::lang::{Descriptor, Front, Runtime};
use crate::deps::json::quote;
use crate::vm::snapshot::{ENTRY_LEN_AT, MAGIC, RUNTIME_AT, SOURCE_AT, SOURCE_LEN_AT};
use alloc::string::String;

use super::write_out;

/* the runtime edge.wasm boots with its front end and the names tools give it */
pub struct Entry {
    rt: &'static Runtime,
}

impl Entry {
    pub const fn new(rt: &'static Runtime) -> Self { Self { rt } }

    /* read at each boot rather than passed to the bodies so lto folds the load */
    #[inline(always)]
    pub(crate) fn runtime(&self) -> &'static Runtime { self.rt }

    #[inline(always)]
    pub(crate) fn front(&self) -> &'static Front { self.rt.front.as_ref().unwrap_or(&Front::NONE) }

    #[inline(always)]
    fn descriptor(&self) -> &'static Descriptor { self.rt.descriptor }
}

/* the descriptor as JSON so a host learns the runtime from its artifact */
pub(crate) fn runtime_descriptor() -> u32 {
    let entry = crate::r#unsafe::web::entry();
    write_out(&json(entry.descriptor(), entry.runtime().identity)) as u32
}

fn json(d: &Descriptor, identity: u64) -> String {
    let mut out = String::from("{");
    let texts = [("language", d.language), ("extension", d.extension), ("test_suffix", d.test_suffix), ("scaffold", d.scaffold), ("example_fence", d.example_fence), ("manifest_fence", d.manifest_fence), ("type_error", d.type_error), ("value_error", d.value_error), ("runtime_error", d.runtime_error), ("denied_error", d.denied_error), ("failed_error", d.failed_error)];
    for (key, value) in texts {
        quote(&mut out, key);
        out.push(':');
        quote(&mut out, value);
        out.push(',');
    }
    quote(&mut out, "entries");
    out.push_str(":[");
    for (j, item) in d.entries.iter().enumerate() {
        if j > 0 { out.push(','); }
        quote(&mut out, item);
    }
    // the snapshot header so a host reads a blob and checks its runtime first
    let mut hex = String::with_capacity(16);
    for k in (0..16).rev() { hex.push(char::from(b"0123456789abcdef"[(identity >> (k * 4) & 15) as usize])); }
    out.push_str(&crate::s!("],\"snapshot\":{\"magic\":", int MAGIC, ",\"runtime_at\":", int RUNTIME_AT, ",\"runtime\":\"", str &hex, "\",\"source_len_at\":", int SOURCE_LEN_AT, ",\"entry_len_at\":", int ENTRY_LEN_AT, ",\"source_at\":", int SOURCE_AT, "}}"));
    out
}
