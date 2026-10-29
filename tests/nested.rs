#[cfg(all(debug_assertions, not(feature = "coverage")))]
// every vm case runs with no generic frame pushed and no Rust loop nested
#[test]
fn old_path_ceilings() {
    use edge::embed::Limits;
    const CEILINGS: [u64; 1] = [0];
    const NAMES: [&str; 1] = ["generic frames"];
    let cases = crate::cases::vm_cases(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    let mut total = [0u64; 1];
    for case in &cases {
        // cases a host drives with events or input run as vm.rs runs them
        if case.get("events").is_some() || case.get("input").is_some() { continue; }
        let src = case["src"].as_str().unwrap();
        let (mut chunk, _) = crate::common::parse(src);
        edge::code::constant_fold(&mut chunk, mini::FRONT.policy);
        let mut vm = crate::common::boot(&chunk, Limits::sandbox());
        let _ = vm.run();
        for (t, n) in total.iter_mut().zip(vm.old_path_counts()) { *t += n; }
    }
    eprintln!("old path over the vm cases: {:?} (ceilings {:?})", total, CEILINGS);
    for k in 0..1 {
        assert!(total[k] <= CEILINGS[k], "{} opened {} times over the vm cases, past its ceiling of {}: a native's call into user code is a section, a job or a frame of the running loop", NAMES[k], total[k], CEILINGS[k]);
    }
}
