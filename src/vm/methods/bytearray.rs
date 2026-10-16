use super::prelude::*;

/* The bytes of bytearray `recv`, changed in place. */
fn buf_mut<'v>(vm: &'v mut VM, recv: Val) -> Result<&'v mut Vec<u8>, VmErr> {
    match vm.heap.try_get_mut(recv) {
        Some(HeapObj::ByteArray(b)) => Ok(b),
        _ => Err(cold_type("method requires a bytearray receiver")),
    }
}

// `ba.append(x)`, one byte at the end.
pub fn append(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let byte = vm.byte_value(pos[0])?;
    buf_mut(vm, recv)?.push(byte);
    vm.push(Val::none()); Ok(())
}

// `ba.extend(iterable)`, the bytes of a bytes-like value or the ints an iterable yields.
pub fn extend(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let more: Vec<u8> = match vm.heap.bytes_of(pos[0]) {
        Some(b) => b.to_vec(),
        None => {
            let items = vm.extract_iter(pos[0])?;
            items.into_iter().map(|v| vm.byte_value(v)).collect::<Result<_, _>>()?
        }
    };
    vm.heap.reserve(more.len())?;
    buf_mut(vm, recv)?.extend_from_slice(&more);
    vm.push(Val::none()); Ok(())
}

// `ba.pop(i=-1)`, the byte removed as an int.
pub fn pop(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let i = match pos.first() { Some(v) if v.is_int() => v.as_int(), None => -1, _ => return Err(cold_type("an integer is required")) };
    let b = buf_mut(vm, recv)?;
    if b.is_empty() { return Err(cold_index("pop from empty bytearray")); }
    let at = if i < 0 { i + b.len() as i64 } else { i };
    if at < 0 || at as usize >= b.len() { return Err(cold_index("pop index out of range")); }
    let byte = b.remove(at as usize);
    vm.push(Val::int(byte as i64)); Ok(())
}

// `ba.insert(i, x)`, clamped to the ends like `list.insert`.
pub fn insert(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let i = if pos[0].is_int() { pos[0].as_int() } else { return Err(cold_type("an integer is required")) };
    let byte = vm.byte_value(pos[1])?;
    let b = buf_mut(vm, recv)?;
    let len = b.len() as i64;
    let at = if i < 0 { (i + len).max(0) } else { i.min(len) };
    b.insert(at as usize, byte);
    vm.push(Val::none()); Ok(())
}

// `ba.remove(x)`, the first byte equal to `x`.
pub fn remove(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let byte = vm.byte_value(pos[0])?;
    let b = buf_mut(vm, recv)?;
    let Some(at) = b.iter().position(|&x| x == byte) else { return Err(cold_value("value not found in bytearray")) };
    b.remove(at);
    vm.push(Val::none()); Ok(())
}

pub fn clear(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> { buf_mut(vm, recv)?.clear(); vm.push(Val::none()); Ok(()) }

pub fn reverse(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> { buf_mut(vm, recv)?.reverse(); vm.push(Val::none()); Ok(()) }

pub fn copy(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let b = recv_bytes(vm, recv)?;
    let v = vm.heap.alloc(HeapObj::ByteArray(b))?;
    vm.push(v); Ok(())
}

// `ba.resize(size)`, cut to `size` or grown with zero bytes.
pub fn resize(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let size = index_arg(vm, pos[0])?;
    if size < 0 { return Err(VmErr::Raised(crate::s!("ValueError: Can only resize to positive sizes, got ", int size))); }
    vm.heap.reserve(size as usize)?;
    buf_mut(vm, recv)?.resize(size as usize, 0);
    vm.push(Val::none()); Ok(())
}

// `ba.take_bytes([n])`, the first `n` bytes taken out as bytes, all of them by default, a negative `n` counted from the end.
pub fn take_bytes(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let n = match pos.first().filter(|v| !v.is_none()) { Some(&v) => Some(index_arg(vm, v)?), None => None };
    let buf = buf_mut(vm, recv)?;
    let len = buf.len() as i64;
    let take = match n { None => len, Some(k) if k < 0 => len + k, Some(k) => k };
    if !(0..=len).contains(&take) { return Err(VmErr::Raised(crate::s!("IndexError: can't take ", int n.unwrap_or(take), " bytes outside size ", int len))); }
    let taken: Vec<u8> = buf.drain(..take as usize).collect();
    let v = vm.heap.alloc(HeapObj::Bytes(taken))?;
    vm.push(v); Ok(())
}
