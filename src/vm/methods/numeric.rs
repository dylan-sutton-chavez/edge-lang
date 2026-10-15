use super::prelude::*;

pub fn bit_length(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let (bits, _) = crate::frontend::int::bits(&vm.heap, recv).ok_or_else(|| cold_type("bit_length() requires an int"))?;
    vm.push(Val::int(bits as i64)); Ok(())
}

pub fn bit_count(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let (_, ones) = crate::frontend::int::bits(&vm.heap, recv).ok_or_else(|| cold_type("bit_count() requires an int"))?;
    vm.push(Val::int(ones as i64)); Ok(())
}

// `int.to_bytes(length=1, byteorder='big')`, unsigned (signed=False), errors if it doesn't fit.
pub fn to_bytes(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let length = match pos.first() { Some(v) if v.is_int() => v.as_int().max(0) as usize, None => 1, _ => return Err(cold_type("length must be an integer")) };
    // Length is user-controlled, cap it against the memory left so a huge count errors instead of aborting.
    vm.heap.reserve(length)?;
    let big = byteorder_is_big(vm, pos.get(1))?;
    let out = crate::frontend::int::to_bytes(&vm.heap, recv, length, big, false).ok_or_else(|| cold_type("to_bytes() requires an int"))??;
    let val = vm.heap.alloc(HeapObj::Bytes(out))?;
    vm.push(val); Ok(())
}

// `int.from_bytes(bytes, byteorder='big')` classmethod, unsigned.
pub fn from_bytes(vm: &mut VM, _recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let buf = recv_bytes_ref(vm, pos[0])?.to_vec();
    let big = byteorder_is_big(vm, pos.get(1))?;
    let v = crate::frontend::int::from_bytes(&mut vm.heap, &buf, big, false)?;
    vm.push(v); Ok(())
}

pub fn is_integer(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let f = if recv.is_float() { recv.as_float() } else { return Err(cold_type("is_integer() requires a float")); };
    vm.push(Val::bool(f.is_finite() && f == libm::trunc(f))); Ok(())
}

fn byteorder_is_big(vm: &VM, arg: Option<&Val>) -> Result<bool, VmErr> {
    match arg {
        None => Ok(true), // default is 'big'
        Some(&v) => match vm.heap.try_get(v) {
            Some(HeapObj::Str(s)) if s == "big" => Ok(true),
            Some(HeapObj::Str(s)) if s == "little" => Ok(false),
            _ => Err(cold_value("byteorder must be 'little' or 'big'")),
        },
    }
}
