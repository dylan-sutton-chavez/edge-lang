#[cfg(all(debug_assertions, not(feature = "coverage")))]
// every vm case runs with no generic frame pushed and no Rust loop nested
#[test]
fn old_path_ceilings() {
    use interpreter::embed::Limits;
    let cases = crate::cases::vm_cases(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    let mut total = 0;
    for case in &cases {
        // cases a host drives with events or input run as vm.rs runs them
        if case.get("events").is_some() || case.get("input").is_some() { continue; }
        let src = case["src"].as_str().unwrap();
        let (mut chunk, _) = crate::common::parse(src);
        interpreter::code::constant_fold(&mut chunk, rig::FRONT.policy);
        let mut vm = crate::common::boot(&chunk, Limits::sandbox());
        let _ = vm.run();
        let [generic] = vm.old_path_counts();
        total += generic;
    }
    assert_eq!(total, 0, "generic frames opened {total} times over the vm cases: a native's call into user code is a section, a job or a frame of the running loop");
}
