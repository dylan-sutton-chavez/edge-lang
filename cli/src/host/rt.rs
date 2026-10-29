use super::{read, read_u32, stage, unstage, Exports, State};
use edge::abi::{WireValue, TAG_INVALID};
use wasmtime::AsContextMut;

/* reads a handle as a transit value refusing sets and instances as the wire does */
pub fn decode(cx: &mut impl AsContextMut<Data = State>, ex: &Exports, handle: u32) -> Result<WireValue, String> {
    let tag_ptr = alloc(cx, ex, 4)?;
    let mut cap = 256;
    let mut dst = alloc(cx, ex, cap)?;
    let mut n = ex.host_edge_decode.call(&mut *cx, (handle as i32, tag_ptr, dst, cap)).map_err(|e| e.to_string())?;
    if n < 0 {
        free(cx, ex, dst, cap);
        cap = -n;
        dst = alloc(cx, ex, cap)?;
        n = ex.host_edge_decode.call(&mut *cx, (handle as i32, tag_ptr, dst, cap)).map_err(|e| e.to_string())?;
    }
    let tag = read_u32(cx, ex.memory, tag_ptr);
    let bytes = if n > 0 { read(cx, ex.memory, dst, n) } else { Vec::new() };
    free(cx, ex, tag_ptr, 4);
    free(cx, ex, dst, cap);
    if tag == TAG_INVALID {
        return Err("value is not a transit value".to_string());
    }
    WireValue::decode_body(tag, &bytes).ok_or_else(|| "malformed wire value".to_string())
}

/* materializes a transit value in the interpreter and returns its handle */
pub fn encode(cx: &mut impl AsContextMut<Data = State>, ex: &Exports, value: &WireValue) -> Result<u32, String> {
    let mut body = Vec::new();
    value.encode_body(&mut body);
    let ptr = stage(cx, ex, &body).map_err(|e| e.to_string())?;
    let handle = ex.host_edge_encode.call(&mut *cx, (value.tag() as i32, ptr, body.len() as i32)).map_err(|e| e.to_string());
    unstage(cx, ex, ptr, body.len());
    match handle? {
        0 => Err("encode failed".to_string()),
        h => Ok(h as u32),
    }
}

fn alloc(cx: &mut impl AsContextMut<Data = State>, ex: &Exports, len: i32) -> Result<i32, String> {
    ex.wasm_alloc.call(&mut *cx, len.max(1)).map_err(|e| e.to_string())
}

fn free(cx: &mut impl AsContextMut<Data = State>, ex: &Exports, ptr: i32, len: i32) {
    let _ = ex.wasm_free.call(&mut *cx, (ptr, len.max(1)));
}
