use std::path::Path;

use crate::kit::Descriptor;

// the compiled host a dist vendors and a headless page imports which only web embeds
#[cfg(feature = "web")]
include!(concat!(env!("OUT_DIR"), "/js_host.rs"));

// the help flag of the browser host which a composition without it never offers
#[cfg(feature = "web")]
pub const FLAG: &str = "  (--web)";
#[cfg(not(feature = "web"))]
pub const FLAG: &str = "";

/* the Host header a request to one of the kit's servers carries */
#[cfg(any(feature = "serve", feature = "web"))]
pub fn host_of(req: &tiny_http::Request) -> Option<&str> {
    req.headers().iter().find(|header| header.field.equiv("Host")).map(|header| header.value.as_str())
}

/* the content type of a served file with runtime sources sent as text */
pub fn content_type(path: &Path, d: &Descriptor) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html",
        Some("js") | Some("mjs") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("json") => "application/json",
        Some("css") => "text/css",
        Some(ext) if ext == d.extension => "text/plain",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

#[cfg(all(test, feature = "web"))]
mod tests {
    use super::*;

    // keys are urls a page imports so a bad key breaks pages and never builds
    #[test]
    fn the_embedded_host_is_keyed_by_its_cdn_path() {
        assert!(!JS_HOST.is_empty());
        assert!(JS_HOST.iter().all(|(key, _)| key.starts_with("src/") && key.ends_with(".js") && !key.contains('\\')));
        assert!(JS_HOST.iter().any(|(key, _)| *key == "src/index.js"));
    }
}
