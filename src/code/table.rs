crate::ops! {
    table OpCode;

    // the ops the parser emits in bytecode order and the two the lowering fuses
    stack {
    //  name              pops      pushes  operand   arm         flow    edge   spill  operator   fold  form          branch
        LoadConst         0         1       constant  values      -       -      -      -          -     -             -;
        LoadName          0         1       slot      values      -       -      -      -          -     -             -;
        StoreName         1         0       slot      values      -       -      -      -          -     -             -;
        Call              args      1       call      calls       -       -      under  -          -     CallR         -;
        PopTop            1         0       none      values      -       -      -      -          -     -             -;
        ReturnValue       1         0       none      calls       leave   -      -      -          -     ReturnR       -;
        BuildString       items     1       count     containers  -       -      -      -          -     BuildR        -;
        FormatValue       spec      1       flags     containers  -       -      -      -          -     FormatR       -;
        Minus             1         1       none      values      -       -      -      unary      fold  MinusR        -;
        Phi               0         0       slot      values      -       -      every  -          -     -             -;
        MakeFunction      defaults  1       function  calls       -       -      -      -          -     OpR           -;
        Add               2         1       flags     values      -       -      -      binary     fold  AddR          -;
        Sub               2         1       flags     values      -       -      -      binary     fold  SubR          -;
        Mul               2         1       flags     values      -       -      -      binary     fold  MulR          -;
        Div               2         1       flags     values      -       -      -      binary     fold  DivR          -;
        Eq                2         1       flags     values      -       -      -      binary     fold  EqR           JumpUnlessEq;
        BuildDict         pairs     1       count     containers  -       -      under  -          -     BuildR        -;
        BuildList         items     1       count     containers  -       -      -      -          -     BuildR        -;
        NotEq             2         1       flags     values      -       -      -      binary     fold  NotEqR        JumpUnlessNotEq;
        Lt                2         1       flags     values      -       -      -      binary     fold  LtR           JumpUnlessLt;
        Gt                2         1       flags     values      -       -      -      binary     fold  GtR           JumpUnlessGt;
        LtEq              2         1       flags     values      -       -      -      binary     fold  LtEqR         JumpUnlessLtEq;
        GtEq              2         1       flags     values      -       -      -      binary     fold  GtEqR         JumpUnlessGtEq;
        // the parser writes these two as jumps so no compiled code holds them
        And               ?         ?       none      -           -       -      -      -          -     -             -;
        Or                ?         ?       none      -           -       -      -      -          -     -             -;
        Not               1         1       none      values      -       -      -      unary      fold  NotR          -;
        JumpIfFalse       1         0       jump      flow        fork    pop    -      -          -     JumpIfFalseR  -;
        Jump              0         0       jump      flow        goto    keep   every  -          -     -             -;
        GetIter           1         iter    flags     flow        -       -      -      -          -     GetIterR      -;
        ForIter           0         1       jump      flow        fork    keep   every  -          -     ForIterR      -;
        GetItem           2         1       none      containers  -       -      -      -          -     GetItemR      -;
        Mod               2         1       flags     values      -       -      -      binary     fold  ModR          -;
        Pow               2         1       flags     values      -       -      -      binary     -     PowR          -;
        FloorDiv          2         1       flags     values      -       -      -      binary     fold  FloorDivR     -;
        LoadTrue          0         1       none      values      -       -      -      -          -     -             -;
        LoadFalse         0         1       none      values      -       -      -      -          -     -             -;
        LoadNone          0         1       none      values      -       -      -      -          -     -             -;
        LoadAttr          1         1       attr      containers  -       -      -      -          -     GetAttrR      -;
        StoreAttr         2         0       attr      containers  -       -      -      -          -     SetAttrR      -;
        BuildSlice        items     1       count     containers  -       -      -      -          -     BuildR        -;
        MakeClass         bases     1       class     calls       -       -      -      -          -     OpR           -;
        SetupExcept       0         0       jump      flow        guard   catch  every  -          -     -             -;
        PopExcept         0         0       none      flow        -       -      every  -          -     -             -;
        Raise             raised    0       flags     flow        leave   -      -      -          -     RaiseR        -;
        BitAnd            2         1       flags     values      -       -      -      binary     fold  BitAndR       -;
        BitOr             2         1       flags     values      -       -      -      binary     fold  BitOrR        -;
        BitXor            2         1       flags     values      -       -      -      binary     fold  BitXorR       -;
        BitNot            1         1       none      values      -       -      -      unary      -     UnaryR        -;
        Shl               2         1       flags     values      -       -      -      binary     fold  ShlR          -;
        Shr               2         1       flags     values      -       -      -      binary     fold  ShrR          -;
        In                2         1       flags     values      -       -      -      binary     -     InR           -;
        NotIn             2         1       flags     values      -       -      -      binary     -     NotInR        -;
        Is                2         1       flags     values      -       -      -      binary     -     IsR           -;
        IsNot             2         1       flags     values      -       -      -      binary     -     IsNotR        -;
        UnpackSequence    1         items   count     containers  -       -      -      -          -     UnpackR       -;
        BuildTuple        items     1       count     containers  -       -      -      -          -     BuildR        -;
        Yield             1         1       flags     flow        -       -      under  -          -     -             -;
        // `SetupFinally` pushes a cleanup frame then `BeginFinally` marks normal entry and `EndFinally` resumes the exit
        SetupFinally      0         0       jump      flow        guard   keep   every  -          -     -             -;
        BeginFinally      0         0       none      flow        -       -      every  -          -     -             -;
        EndFinally        0         0       none      flow        -       -      every  -          -     -             -;
        // unwinds the operand count of finally blocks before a break or continue jumps
        UnwindFinally     0         0       count     flow        unwind  -      every  -          -     -             -;
        Del               0         0       slot      values      -       -      every  -          -     -             -;
        Assert            1         0       none      values      -       -      -      -          -     OpR           -;
        Global            0         0       name      values      -       -      -      -          -     -             -;
        Outer             0         0       name      values      -       -      -      -          -     -             -;
        UnpackArgs        1         1       flags     calls       -       -      under  -          -     -             -;
        ListAppend        1         0       none      containers  -       -      -      -          -     ListAppendR   -;
        SetAdd            1         0       none      containers  -       -      -      -          -     SetAddR       -;
        MapAdd            2         0       none      containers  -       -      -      -          -     MapAddR       -;
        BuildSet          items     1       count     containers  -       -      under  -          -     BuildR        -;
        UnpackEx          1         rest    count     containers  -       -      -      -          -     OpR           -;
        LoadEllipsis      0         1       none      values      -       -      -      -          -     OpR           -;
        // `await` where GetAwaitable makes what Await steps by sending in the value under it
        Await             2         1       none      flow        -       -      under  -          -     -             -;
        GetAwaitable      1         2       none      flow        -       -      under  -          -     -             -;
        MakeCoroutine     defaults  1       function  calls       -       -      -      -          -     OpR           -;
        StoreItem         3         0       none      containers  -       -      -      -          -     StoreItemR    -;
        Dup2              2         4       none      values      -       -      -      -          -     -             -;
        JumpIfFalseOrPop  1         0       jump      flow        fork    keep   every  -          -     JumpIfFalseR  -;
        JumpIfTrueOrPop   1         0       jump      flow        fork    keep   every  -          -     JumpIfFalseR  -;
        Dup               1         2       none      values      -       -      -      -          -     -             -;
        CallMethod        args      1       method    calls       -       -      under  -          -     CallMethodR   -;
        CallMethodArgs    0         0       call      calls       -       -      -      -          -     -             -;
        PopIter           0         0       none      flow        -       -      every  -          -     -             -;
        DelItem           2         0       none      containers  -       -      -      -          -     OpR           -;
        DelAttr           1         0       attr      containers  -       -      -      -          -     OpR           -;
        CallExtern        packed    1       binding   calls       -       -      under  -          -     -             -;
        // the module of `chunk.imports[operand]` read from `vm.module_table` in constant time
        LoadModule        0         1       module    values      -       -      -      -          -     OpR           -;
        // a name declared `global` read or written in `self.globals` by its bare name in `chunk.names`
        LoadGlobal        0         1       name      values      -       -      -      -          -     LoadGlobalR   -;
        StoreGlobal       1         0       name      values      -       -      -      -          -     StoreGlobalR  -;
        // `{**m}` `{*s}` and `[*it]` merge the popped value into the container below it
        DictUpdate        1         0       none      containers  -       -      under  -          -     -             -;
        SetUpdate         1         0       none      containers  -       -      under  -          -     -             -;
        ListExtend        1         0       none      containers  -       -      under  -          -     -             -;
        // whether the popped value matches a sequence pattern which str and bytes never do
        MatchSeq          1         1       none      containers  -       -      -      -          -     OpR           -;
        // extends a left list in place so aliases see it and adds otherwise
        InPlaceAdd        2         1       flags     values      -       -      under  augmented  -     InPlaceAddR   -;
        // removes from a left set in place so aliases see it and subtracts otherwise
        InPlaceSub        2         1       flags     values      -       -      -      augmented  -     InPlaceSubR   -;
        // unary plus where a bool becomes an int and other numbers stay as they are
        Pos               1         1       none      values      -       -      -      unary      -     UnaryR        -;
        // pushes the return value of the finished subiterator as the value of `yield from`
        LoadYieldFrom     0         1       none      flow        -       -      -      -          -     OpR           -;
        // `|=` `&=` and `^=` mutating a left set in place
        InPlaceBitOr      2         1       flags     values      -       -      -      augmented  -     BitOrR        -;
        InPlaceBitAnd     2         1       flags     values      -       -      -      augmented  -     BitAndR       -;
        InPlaceBitXor     2         1       flags     values      -       -      -      augmented  -     BitXorR       -;
        // pushes a fresh spread delta frame
        BeginArgs         0         0       none      calls       -       -      under  -          -     -             -;
        Swap              2         2       none      values      -       -      -      -          -     -             -;
        // lifts the third stack value to the top
        Rot3              3         3       none      values      -       -      -      -          -     -             -;
        // `a @ b` which no value supports
        MatMul            2         1       flags     values      -       -      -      binary     -     -             -;
        // a class pattern pushing the matched attributes or None with the positional count as operand
        MatchClass        3         1       count     containers  -       -      -      -          -     OpR           -;
        // whether the popped value matches a mapping pattern
        MatchMap          1         1       none      containers  -       -      -      -          -     OpR           -;
        // a call with a `*` or `**` argument closing the frame its first spread opened
        CallSpread        args      1       call      calls       -       -      under  -          -     CallSpreadR   -;
        // a call of the native its operand names with its count or through `chunk.native_sites`
        CallNative        site      gives   native    natives     -       -      under  -          -     CallNativeR   -;
        // the length of the sequence a pattern matched which no binding redirects
        Len               1         1       none      values      -       -      -      -          -     LenR          -;
        // the tuple of the list a starred display built item by item
        ListToTuple       1         1       none      values      -       -      -      -          -     ListToTupleR  -;
        // whether the exception under the class a handler names is of that class
        MatchExc          2         1       none      values      -       -      -      -          -     MatchExcR     -;
        // a runtime's own opcode over the values the high byte of its operand counts
        RuntimeOp         high      1       own       values      -       -      -      -          -     RuntimeR      -;
        // the cut of the object under the bounds its operand counts building no slice
        Cut               items     1       count     containers  -       -      -      -          -     OpR           -;
        // stores the value under the object and its bounds over that cut
        StoreCut          items     0       count     containers  -       -      -      -          -     OpR           -;
        // deletes the cut of the object under its bounds
        DelCut            items     0       count     containers  -       -      -      -          -     OpR           -;
    }

    // register forms only the VM lowering writes each reading and writing frame slots
    register {
    //  name             stack       rows        fast
        Move             -           -           move;
        PushRegs         -           -           -;
        // the iterator of register `b` a loop steps with `x` the GetIter operand
        GetIterR         -           -           -;
        AddR             Add         -           arith;
        SubR             Sub         -           arith;
        MulR             Mul         -           arith;
        DivR             Div         -           arith;
        ModR             Mod         -           arith;
        FloorDivR        FloorDiv    -           arith;
        InPlaceAddR      InPlaceAdd  -           arith;
        InPlaceSubR      InPlaceSub  -           arith;
        BitAndR          BitAnd      -           bits;
        BitOrR           BitOr       -           bits;
        BitXorR          BitXor      -           bits;
        ShlR             Shl         -           bits;
        ShrR             Shr         -           bits;
        PowR             Pow         -           -;
        EqR              Eq          -           -;
        NotEqR           NotEq       -           -;
        LtR              Lt          -           order;
        LtEqR            LtEq        -           order;
        GtR              Gt          -           order;
        GtEqR            GtEq        -           order;
        MinusR           Minus       -           -;
        NotR             Not         -           -;
        JumpIfFalseR     -           -           -;
        ReturnR          -           -           -;
        GetItemR         GetItem     -           -;
        StoreItemR       -           -           -;
        GetAttrR         -           -           -;
        SetAttrR         -           -           -;
        LoadGlobalR      -           -           -;
        StoreGlobalR     -           -           -;
        LoadCellR        -           -           -;
        StoreCellR       -           -           -;
        InR              In          -           -;
        NotInR           NotIn       -           -;
        IsR              Is          -           -;
        IsNotR           IsNot       -           -;
        LenR             Len         -           -;
        // adds registers to the comprehension accumulator on the stack top
        ListAppendR      -           -           -;
        SetAddR          -           -           -;
        MapAddR          -           -           -;
        // unpacks `b` into `c` registers from `a` or as a pair `c` into `a` `b`
        UnpackR          -           unpack      -;
        // pops the stack top into a slot or a module binding when `x` is set
        StoreTopR        -           -           -;
        // steps the innermost iterator into a register and jumps out once it ends
        ForIterR         -           -           -;
        // stack opcode `x` over registers from `b` its values from `a` and `c` its operand
        OpR              -           op          -;
        // jump when the comparison of two registers is false
        JumpUnlessEq     Eq          -           -;
        JumpUnlessNotEq  NotEq       -           -;
        JumpUnlessLt     Lt          -           test;
        JumpUnlessLtEq   LtEq        -           test;
        JumpUnlessGt     Gt          -           test;
        JumpUnlessGtEq   GtEq        -           test;
        // calls register `a` with `x` arguments from `b` and `c` for the next StoreTopR
        CallR            -           -           -;
        // calls method `a` of register `b` with `x` arguments from `c` for the next StoreTopR
        CallMethodR      -           -           -;
        // arguments in `a` `b` and `c` past those of the CallR or CallMethodR before it
        ArgsR            -           -           -;
        // grows in place the string owned by a local that its loop only appends to
        AppendR          -           -           -;
        // writes to `a` the unary stack opcode `x` of register `b`
        UnaryR           -           -           -;
        // raises register `b` or with `x` set the handled exception again
        RaiseR           -           -           -;
        // writes to `a` what build op `x` makes of the `c` registers from `b`
        BuildR           -           items       -;
        // writes to `a` field `b` formatted by operand `x` with its spec in `c`
        FormatR          -           -           -;
        // calls native `a` on `x` registers from `b` for the next StoreTopR
        CallNativeR      -           -           -;
        // writes to `a` whether exception `b` is of the class or tuple `c`
        MatchExcR        -           -           -;
        // writes to `a` the tuple of the items of list `b`
        ListToTupleR     -           -           -;
        // writes to `a` what runtime opcode `x` gives of the `c` registers from `b`
        RuntimeR         RuntimeOp   items       -;
        // disowns the string of `a` as an appending loop starts and stays where `ALL` ends
        OwnResetR        -           -           -;
    }

    // the forms sections and calls lay which `ALL` leaves out
    internal {
        // a CallR reading its callee from module binding `a` so no register carries it
        CallGlobalR               -      -;
        // jumps into the laid section while callee `a` still holds its function and calls otherwise
        CallSectionR              -      -;
        // the section a CallSectionR enters with entry `a` parameter `b` index `c` and flags `x`
        SectionR                  -      -;
        // a section return sending `b` to the register and return point held by link `a`
        ReturnSectionR            -      -;
        // before a recursive call saves the `b` words at `a` the level reads after it
        TurnR                     -      -;
        // after that call restores the level from the words its TurnR saved
        UnturnR                   -      -;
        // reads binding `b` of module `c` into `a` or writes `b` into its binding `a`
        LoadGlobalAtR             -      -;
        StoreGlobalAtR            -      -;
        // unbinds register `a` for a local the section checks as each call enters it
        UnbindR                   -      -;
        // wraps register `a` in a fresh cell as the section enters
        WrapCellR                 -      -;
        // first in a register call stub entering the section laid for its function if any
        CallValueR                -      -;
        // a positional CallSectionR of a module binding keeping nothing and checking only the binding
        CallGlobalSectionR        -      -;
        // a positional CallSectionR of a method keeping nothing and checking the class of receiver `b`
        CallMethodSectionR        -      -;
        // a positional CallSectionR of a register keeping nothing and checking it holds the section function
        CallValueSectionR         -      -;
        // a positional CallSectionR of a class whose init method is laid checking binding and class
        CallCtorSectionR          -      -;
        // a CallMethodSectionR on the module binding its stub SectionR holds plus one in `c`
        CallMethodGlobalSectionR  -      -;
        // `a(*b)` calling register `a` with the items of `b` for the next StoreTopR
        CallSpreadR               -      -;
        // a laid exception init storing the `c` registers from `b` as the `args` of `a`
        ExcArgsR                  -      -;
        // a ReturnSectionR inside the `try` of a section leaving through its handlers
        ReturnTriedR              -      -;
        // a ReturnSectionR of a value always bound like an undeleted parameter or a constant
        ReturnBoundR              -      -;
        // `a.append(b)` on a local only ever holding its first list with the result unread
        ListPushR                 -      -;
        // a stepless jump to head `a` laid past the end of grown code or back
        JumpR                     -      -;
        // stack opcode `x` of operand `b` over the `c` registers from `a`
        RowsR                     -      row;
        // takes back into the row at `a` what the op before left aside
        RowsBackR                 -      -;
    }
}
