use super::{Instruction, OpCode, SSAChunk, ITER_VALUE, SPREAD_BASES};

/* The values instruction `ins` of `chunk` takes off the stack and leaves on it, None for an opcode the parser never writes.
   A `ReturnValue` met with the stack empty takes nothing and returns None, as the module body's last one does. */
pub fn effect(ins: &Instruction, chunk: &SSAChunk) -> Option<(u32, u32)> {
    let operand = ins.operand;
    let pos = (operand & 0xFF) as u32;
    let kw = ((operand >> 8) & 0xFF) as u32;
    // A fused builtin keeps its top two bits for the spread and keyword flags, a plain call counts with them.
    let fused_kw = kw & 0x3F;
    use OpCode::*;
    Some(match ins.opcode {
        Add | Sub | Mul | Div | FloorDiv | Mod | Pow | MatMul | InPlaceAdd | InPlaceSub | InPlaceBitOr | InPlaceBitAnd
        | InPlaceBitXor | BitAnd | BitOr | BitXor | Shl | Shr | Eq | NotEq | Lt | Gt | LtEq | GtEq | Is | IsNot | In
        | NotIn | And | Or | GetItem | DictUpdate | SetUpdate | ListExtend | ListAppend | SetAdd => (2, 1),
        BitNot | Minus | Pos | Not | MatchSeq | MatchMap | MakeTypeAlias | LoadAttr | UnpackArgs | Yield | Await | GetAwaitable => (1, 1),
        // The iterator a comprehension is handed stays on the stack, where every other form moves it to the loop.
        GetIter if operand == ITER_VALUE => (1, 1),
        MatchClass => (3, 1),
        LoadConst | LoadTrue | LoadFalse | LoadNone | LoadEllipsis | LoadName | LoadGlobal | LoadModule | LoadYieldFrom | MakeTypeVar => (0, 1),
        StoreName | StoreGlobal | PopTop | DelAttr | GetIter | Assert | WithJudge | ReturnValue | JumpIfFalse => (1, 0),
        Phi | Global | Nonlocal | BeginArgs | Del | PopIter | SetupExcept | SetupFinally | PopExcept | BeginFinally | EndFinally
        | UnwindFinally | Jump | JumpIfFalseOrPop | JumpIfTrueOrPop => (0, 0),
        Dup => (1, 2),
        Dup2 => (2, 4),
        Swap => (2, 2),
        Rot3 => (3, 3),
        StoreAttr | DelItem => (2, 0),
        StoreItem => (3, 0),
        MapAdd => (3, 1),
        ForIter => (0, 1),
        // A bare raise, operand 1, raises again what is being handled and takes nothing.
        Raise => (u32::from(operand != 1), 0),
        RaiseFrom => (2, 0),
        WithEnter => (1, 2),
        WithExit => (0, 5),
        Call | CallSpread => (pos + 2 * kw + 1, 1),
        CallPrint => (pos + 2 * fused_kw, 0),
        CallExtern => ((operand & 0xF) as u32 + 2 * ((operand >> 4) & 0xF) as u32, 1),
        MakeFunction | MakeCoroutine => (chunk.functions.get(operand as usize)?.2 as u32, 1),
        // A class pops its keyword pairs above its bases, which come one by one or gathered in one list.
        MakeClass => {
            let body = chunk.classes.get(operand as usize)?;
            (if body.bases == SPREAD_BASES { 1 } else { u32::from(body.bases) } + 2 * u32::from(body.class_kw), 1)
        }
        BuildList | BuildTuple | BuildSet | BuildString | BuildSlice => (operand as u32, 1),
        BuildDict => (operand as u32 * 2, 1),
        FormatValue => (if operand & 1 != 0 { 2 } else { 1 }, 1),
        UnpackSequence => (1, operand as u32),
        UnpackEx => (1, (operand >> 8) as u32 + 1 + (operand & 0xFF) as u32),
        CallLen | CallAbs | CallType | CallChr | CallOrd | CallRepr | CallBin | CallOct | CallHex | CallReversed | CallCallable
        | CallIsInstance | CallDivmod | CallInput | CallRange | CallRound | CallInt | CallStr | CallFloat | CallBool | CallList
        | CallTuple | CallSet | CallSum | CallPow | CallZip | CallAll | CallAny | CallMin | CallMax | CallDict | CallEnumerate => (pos + 2 * fused_kw, 1),
        // Only the VM's fusing writes a method call, and the lowering every register form.
        _ => return None,
    })
}
