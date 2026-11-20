mod test {
    use interpreter::lang;
    use interpreter::lang::{Act, Body, EngineError, ErrKind, Falsy, Gen, Language, Names, Policy, Runtime};
    use interpreter::Val;
    use rig::Rig;

    // a language stating a rule the loop does not read yet
    struct Loose;

    impl Language for Loose {
        const VERSION: u32 = 1;
        const POLICY: Policy = Rig::POLICY.identity_first(false);
        const NATIVES: &'static lang::Natives = &rig::NATIVES;
        const NAMES: &'static Names = &rig::NAMES;
        const FRONT: lang::Front = rig::FRONT;
        const DESCRIPTOR: &'static lang::Descriptor = &rig::DESCRIPTOR;
        const WIDE: Option<&'static interpreter::value::foreign::Kind> = Some(&rig::lang::WIDE);
        const BUILT: interpreter::value::foreign::Built = <rig::Rig as Language>::BUILT;
        type Ops = ();
    }

    #[test]
    fn a_runtime_carries_the_rules_its_language_states() {
        let rt = Runtime::of::<Rig>();
        assert!(core::ptr::eq(rt.natives(), &rig::NATIVES) && core::ptr::eq(rt.names(), &rig::NAMES));
        assert_eq!(*rt.policy(), Rig::POLICY);
        assert_eq!(Rig::POLICY.max_int_digits(None).digits(), u32::MAX);
    }

    #[test]
    #[should_panic(expected = "reads no identity_first yet")]
    fn a_runtime_refuses_a_rule_the_loop_does_not_read() { let _ = Runtime::of::<Loose>(); }

    // a language reading zero as true over natives built where zero reads false
    struct Mixed;

    impl Language for Mixed {
        const VERSION: u32 = 1;
        const POLICY: Policy = Rig::POLICY.falsy(Falsy::NIL.or(Falsy::FALSE));
        const NATIVES: &'static lang::Natives = &rig::NATIVES;
        const NAMES: &'static Names = &rig::NAMES;
        const FRONT: lang::Front = rig::FRONT;
        const DESCRIPTOR: &'static lang::Descriptor = &rig::DESCRIPTOR;
        const WIDE: Option<&'static interpreter::value::foreign::Kind> = Some(&rig::lang::WIDE);
        const BUILT: interpreter::value::foreign::Built = <rig::Rig as Language>::BUILT;
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
            (EngineError::ReadOnly { recv: v, name: String::from("x"), on_type: true }, Some(ErrKind::Type)),
            (EngineError::ReadOnly { recv: v, name: String::from("x"), on_type: false }, Some(ErrKind::Attribute)),
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
        assert_eq!(Rig::POLICY.falsy_of().bits(), Falsy::NIL.or(Falsy::FALSE).or(Falsy::ZERO).or(Falsy::NAN).or(Falsy::EMPTY_TEXT).bits());
        assert_eq!(Rig::POLICY.falsy_of(), Policy::PLAIN.falsy_of());
    }

    // a rule the loop does not read yet builds only as the engine's own
    #[test]
    fn a_policy_refuses_a_rule_the_loop_does_not_read() {
        use interpreter::lang::{IntDiv, IntMod, MapIter, NegPow, NumEq, StrIter, ZeroDiv};
        let m = Rig::POLICY;
        let unread = [
            (m.numeric_eq(NumEq::ByKind), "numeric_eq"),
            (m.identity_first(false), "identity_first"),
            (m.str_iter(StrIter::Bytes), "str_iter"),
            (m.map_iter(MapIter::Pairs), "map_iter"),
            (m.builtins_scope(false), "builtins_scope"),
            (m.receiver_slot(false), "receiver_slot"),
        ];
        for (policy, field) in unread {
            let said = match std::panic::catch_unwind(|| policy.refuse_unread()) {
                Ok(_) => String::new(),
                Err(e) => e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_default(),
            };
            assert!(said.contains(&format!("reads no {field} yet")), "{field} built with {said:?}");
        }
        // the fields the engine reads take any value
        m.map_fields(true).bool_is_int(false).neg_index(false).recursion_limit(10).max_int_digits(None).refuse_unread();
        m.int_div(IntDiv::Floor).int_mod(IntMod::Truncated).zero_div(ZeroDiv::Ieee).neg_pow(NegPow::Error).falsy(Falsy::NIL.or(Falsy::FALSE)).refuse_unread();
        // nil and false read as false under every policy
        let said = std::panic::catch_unwind(|| m.falsy(Falsy::NIL.or(Falsy::ZERO)).refuse_unread()).err().and_then(|e| e.downcast_ref::<&str>().map(|s| s.to_string()));
        assert_eq!(said.as_deref(), Some("a policy reads nil and false as false"));
    }
}
