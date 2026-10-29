mod test {
    use edge::lang::{self, Act, Body, EngineError, ErrKind, Falsy, Gen, Language, Lock, Names, Policy, Runtime};
    use edge::value::Val;
    use mini::Mini;

    // a language stating a rule the loop does not read yet
    struct Loose;

    impl Language for Loose {
        const VERSION: u32 = 1;
        const POLICY: Policy = Policy { identity_first: !Mini::POLICY.identity_first, ..Mini::POLICY };
        const NATIVES: &'static lang::Natives = &mini::NATIVES;
        const NAMES: &'static Names = &mini::NAMES;
        const FRONT: lang::Front = mini::FRONT;
        const DESCRIPTOR: &'static lang::Descriptor = &mini::DESCRIPTOR;
        const WIDE: Option<&'static edge::value::foreign::Kind> = Some(&mini::lang::WIDE);
        const BUILT: edge::value::foreign::Built = <mini::Mini as Language>::BUILT;
        type Ops = ();
    }

    #[test]
    fn a_runtime_carries_the_rules_its_language_states() {
        let rt = Runtime::of::<Mini>();
        assert!(core::ptr::eq(rt.natives, &mini::NATIVES) && core::ptr::eq(rt.names(), &mini::NAMES));
        assert_eq!(*rt.policy(), Mini::POLICY);
        assert_eq!(Policy { max_int_digits: None, ..Mini::POLICY }.digits(), u32::MAX);
    }

    #[test]
    #[should_panic(expected = "reads no identity_first yet")]
    fn a_runtime_refuses_a_rule_the_loop_does_not_read() { let _ = Runtime::of::<Loose>(); }

    // a language reading zero as true over natives built where zero reads false
    struct Mixed;

    impl Language for Mixed {
        const VERSION: u32 = 1;
        const POLICY: Policy = Policy { falsy: Falsy::NIL.or(Falsy::FALSE), ..Mini::POLICY };
        const NATIVES: &'static lang::Natives = &mini::NATIVES;
        const NAMES: &'static Names = &mini::NAMES;
        const FRONT: lang::Front = mini::FRONT;
        const DESCRIPTOR: &'static lang::Descriptor = &mini::DESCRIPTOR;
        const WIDE: Option<&'static edge::value::foreign::Kind> = Some(&mini::lang::WIDE);
        const BUILT: edge::value::foreign::Built = <mini::Mini as Language>::BUILT;
        type Ops = ();
    }

    // the loop would read one mask and the slow paths and the folder another
    #[test]
    #[should_panic(expected = "builds its natives and folds its constants under the rules it states")]
    fn a_runtime_refuses_natives_built_under_other_rules() { let _ = Runtime::of::<Mixed>(); }

    // a class tree whose parents loop is refused before a lookup walks it
    #[test]
    #[should_panic(expected = "a class tree holds a cycle")]
    fn a_class_tree_refuses_parents_that_loop() { let _ = lang::Tree::of(&[("Timeout", "Retry"), ("Retry", "Timeout"), ("Fault", "")]); }

    #[test]
    fn an_engine_error_names_its_kind_and_a_signal_names_none() {
        let v = Val::int(1);
        let kinds = [
            (EngineError::ZeroDiv, Some(ErrKind::ZeroDiv)),
            (EngineError::Unbound { name: None, free: true }, Some(ErrKind::Name)),
            (EngineError::Unbound { name: None, free: false }, Some(ErrKind::Unbound)),
            (EngineError::Cannot { act: Act::Complex, recv: None }, Some(ErrKind::Value)),
            (EngineError::Cannot { act: Act::Iterate, recv: Some(v) }, Some(ErrKind::Type)),
            (EngineError::ReadOnly { recv: v, name: String::from("x"), lock: Lock::Type }, Some(ErrKind::Type)),
            (EngineError::ReadOnly { recv: v, name: String::from("x"), lock: Lock::Plain }, Some(ErrKind::Attribute)),
            (EngineError::Generator(Gen::Running(Body::Generator)), Some(ErrKind::Value)),
            (EngineError::Stop(v), Some(ErrKind::Stop)),
            (EngineError::Of(ErrKind::Permission, String::new()), Some(ErrKind::Permission)),
            (EngineError::Raised(v), None),
            (EngineError::Budget, None),
            (EngineError::Redo, None),
        ];
        for (e, kind) in kinds { assert_eq!(e.kind(), kind, "{e:?}"); }
    }

    #[test]
    fn a_falsy_mask_holds_each_value_it_names() {
        let f = Falsy::NIL.or(Falsy::ZERO);
        assert!(f.has(Falsy::NIL) && f.has(Falsy::ZERO) && f.has(f) && !f.has(Falsy::FALSE) && !f.has(Falsy::NIL.or(Falsy::EMPTY)));
        // an empty text and an empty container are flags apart which `EMPTY` joins
        let text = Falsy::NIL.or(Falsy::FALSE).or(Falsy::EMPTY_TEXT);
        assert!(text.has(Falsy::EMPTY_TEXT) && !text.has(Falsy::EMPTY_CONTAINER) && !text.has(Falsy::EMPTY) && Falsy::EMPTY.has(Falsy::EMPTY_TEXT.or(Falsy::EMPTY_CONTAINER)));
        assert!(text.text_truth(false) && !text.text_truth(true) && text.container_truth(true) && !Falsy::EMPTY.container_truth(true));
        assert_eq!(Mini::POLICY.falsy.bits(), Falsy::NIL.or(Falsy::FALSE).or(Falsy::ZERO).or(Falsy::NAN).or(Falsy::EMPTY_TEXT).bits());
        assert_eq!(Mini::POLICY.falsy, Policy::ENGINE.falsy);
    }

    // a rule the loop does not read yet builds only as the engine's own
    #[test]
    fn a_policy_refuses_a_rule_the_loop_does_not_read() {
        use edge::lang::{IntDiv, IntMod, MapIter, NegPow, NumEq, StrIter, ZeroDiv};
        let m = Mini::POLICY;
        let unread = [
            (Policy { numeric_eq: NumEq::ByKind, ..m }, "numeric_eq"),
            (Policy { identity_first: false, ..m }, "identity_first"),
            (Policy { str_iter: StrIter::Bytes, ..m }, "str_iter"),
            (Policy { map_iter: MapIter::Pairs, ..m }, "map_iter"),
            (Policy { builtins_scope: false, ..m }, "builtins_scope"),
            (Policy { receiver_slot: false, ..m }, "receiver_slot"),
        ];
        for (policy, field) in unread {
            let said = match std::panic::catch_unwind(|| policy.refuse_unread()) {
                Ok(_) => String::new(),
                Err(e) => e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_default(),
            };
            assert!(said.contains(&format!("reads no {field} yet")), "{field} built with {said:?}");
        }
        // the fields the engine reads take any value
        Policy { map_fields: true, bool_is_int: false, neg_index: false, recursion_limit: 10, max_int_digits: None, ..m }.refuse_unread();
        Policy { int_div: IntDiv::Floor, int_mod: IntMod::Truncated, zero_div: ZeroDiv::Ieee, neg_pow: NegPow::Error, falsy: Falsy::NIL.or(Falsy::FALSE), ..m }.refuse_unread();
        // nil and false read as false under every policy
        let said = std::panic::catch_unwind(|| Policy { falsy: Falsy::NIL.or(Falsy::ZERO), ..m }.refuse_unread()).err().and_then(|e| e.downcast_ref::<&str>().map(|s| s.to_string()));
        assert_eq!(said.as_deref(), Some("a policy reads nil and false as false"));
    }
}
