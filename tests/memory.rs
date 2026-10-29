mod test {
    use edge::code::SSAChunk;
    use edge::value::meter::{Meter, set_budget};
    use edge::embed::Limits;

    fn compiled(src: &str) -> SSAChunk {
        let (chunk, errs) = crate::common::parse(src);
        assert!(errs.is_empty(), "{src}");
        chunk
    }

    /* bytes `src` holds after its run and at its peak with the meter it used */
    fn ran(src: &str) -> (usize, usize, &'static Meter) {
        let chunk = compiled(src);
        let mut vm = crate::common::boot(&chunk, Limits::sandbox());
        vm.run().unwrap();
        (vm.memory(), vm.memory_peak(), vm.heap().meter())
    }

    const MIXED: &str = "class P:\n    fn __init__(self, x):\n        self.x = x\nfn squares(n):\n    for i in range(n):\n        yield i * i\nd = {str(i): [i, (i, i + 1.5), {i}] for i in range(300)}\ns = ''.join(f'{k}:{v[0]!r}' for k, v in d.items())\nps = sorted([P(i) for i in range(200)], key=lambda p: -p.x)\nt = list(squares(500))\ntry:\n    raise ValueError('x' * 100)\nexcept ValueError as e:\n    msg = str(e)\nb = bytes(range(256)) * 10\nprint(len(s), ps[0].x, sum(t), len(msg), len(b.hex()))\n";

    // a dropped VM gives back every byte its run took so nothing drifts
    #[test]
    fn a_run_gives_back_every_byte_once_its_vm_drops() {
        for _ in 0..2 {
            let (held, peak, meter) = ran(MIXED);
            assert!(held > 0 && peak >= held, "held {held} peak {peak}");
            assert_eq!(meter.held(), 0);
        }
    }

    #[test]
    fn a_list_of_numbers_holds_eight_bytes_each() {
        let (held, _, _) = ran("xs = [0] * 1000000\n");
        assert!((8_000_000..8_500_000).contains(&held), "{held}");
    }

    // the list goes before the loop starts collecting so only the peak remembers it
    #[test]
    fn the_peak_keeps_what_a_program_held_after_a_collection_let_it_go() {
        let (held, peak, _) = ran("xs = [0] * 1000000\nxs = nil\nn = 0\nfor i in range(100000):\n    n += len([i])\n");
        assert!(peak >= 8_000_000 && held < 4_000_000, "peak {peak} held {held}");
    }

    // an in-place intersection shrinks the table so only the peak keeps the big one
    #[test]
    fn the_peak_keeps_what_a_set_held_before_it_shrank() {
        let (held, peak, _) = ran("s = set(range(100000))\ns &= {0}\n");
        assert!(peak >= 2_000_000 && held < peak / 2, "peak {peak} held {held}");
    }

    // few large dropped objects trigger a collection by their bytes so the peak stays low
    #[test]
    fn large_garbage_collects_by_its_bytes() {
        for src in [
            "a = set(range(0, 200000, 2))\nb = set(range(0, 200000, 3))\nt = 0\nfor r in range(40):\n    t += len(a & b) + len(a | b)\n",
            "x = 1\nfor i in range(1, 3000):\n    x = x * i\n",
            "t = 0\nfor i in range(300):\n    t += len(list(range(20000)))\n",
            "s = 'y' * 100000\nt = 0\nfor i in range(300):\n    t += len(s + 'z')\n",
        ] {
            let (_, peak, _) = ran(src);
            assert!(peak < 48 << 20, "{src}: peak {peak}");
        }
    }

    // a result too large for the memory left is refused before its parts pass it
    #[test]
    fn builders_refuse_before_they_peak_past_the_limit() {
        for (src, most) in [
            ("xs = list(range(2 ** 60, 2 ** 60 + 20000000))\n", 64),
            ("xs = set(range(2 ** 60, 2 ** 60 + 20000000))\n", 64),
            ("x = [1] * 28000000\nd = dict.fromkeys(x)\n", 480),
            ("x = [1] * 14000000\ns = set(x)\n", 160),
            ("s = 'a' * 70000000\nb = s.encode('utf-32')\n", 160),
        ] {
            let chunk = compiled(src);
            let mut vm = crate::common::boot(&chunk, Limits::sandbox());
            let _ = vm.run();
            assert!(vm.memory_peak() >> 20 < most, "{src:?} peaked at {} MiB", vm.memory_peak() >> 20);
        }
    }

    // a stepped cut and a join hold what they build and no list of places
    #[test]
    fn cuts_and_joins_peak_at_what_they_build() {
        for (src, most) in [
            ("s = 'a' * 100000000\nt = s[::-1]\n", 240),
            ("b = bytes(100000000)\nt = b[::2]\n", 192),
            ("s = 'é' * 50000000\nt = s[::-1]\n", 240),
            ("x = [1] * 26000000\ny = x + x\n", 240),
            ("x = (1,) * 26000000\ny = x + x\n", 240),
        ] {
            let chunk = compiled(src);
            let mut vm = crate::common::boot(&chunk, Limits::sandbox());
            let _ = vm.run();
            assert!(vm.memory_peak() >> 20 < most, "{src:?} peaked at {} MiB", vm.memory_peak() >> 20);
        }
    }

    // set builds and copies refuse before they hold twice the limit
    #[test]
    fn set_builds_refuse_before_twice_the_limit() {
        for src in [
            "s = {*range(30000000)}\n",
            "x = list(range(16000000))\ns = {*x}\n",
            "s = set(range(9000000))\nt = s.copy()\nu = s.copy()\n",
            "s = set(range(9000000))\nt = s.copy()\nu = set(s)\n",
        ] {
            let chunk = compiled(src);
            let mut vm = crate::common::boot(&chunk, Limits::sandbox());
            let _ = vm.run();
            assert!(vm.memory_peak() >> 20 < 450, "{src:?} peaked at {} MiB", vm.memory_peak() >> 20);
        }
    }

    // a small result of a large set or map takes room for itself alone
    #[test]
    fn a_small_result_of_a_large_set_or_map_fits() {
        for (src, out) in [
            ("a = set(range(9000000))\nprint(len(a & {1}), len({1} & a), len(a - a), len(a | {1}))\n", "1 1 0 9000000"),
            ("d = dict.fromkeys(range(6000000))\nprint(len(d | {1: 2}))\n", "6000000"),
        ] {
            let chunk = compiled(src);
            let mut vm = crate::common::boot(&chunk, Limits::sandbox());
            vm.run().unwrap_or_else(|e| panic!("{src:?}: {e}"));
            assert_eq!(vm.output, [out]);
        }
    }

    // a table that shrank to a few entries gives back the room it grew to
    #[test]
    fn a_table_that_shrank_gives_back_its_room() {
        for src in [
            "d = dict.fromkeys(range(2000000))\nfor i in range(1999960):\n    del d[i]\nfor r in range(2000):\n    d[-1] = r\n    del d[-1]\n",
            "s = set(range(2000000))\nfor i in range(1999960):\n    s.discard(i)\nfor r in range(2000):\n    s.add(-1)\n    s.discard(-1)\n",
        ] {
            let (held, _, _) = ran(src);
            assert!(held < 8 << 20, "{src:?} holds {held}");
        }
    }

    // a set build sizes its table by its count and limit never the memory held
    #[test]
    fn a_set_build_sizes_by_the_limit_and_not_the_memory_held() {
        use edge::value::meter::Paying;
        use edge::value::{HeapPool, Val, ValSet};
        const K: u64 = 0x517c_c1b7_2722_0a95;
        let inv = (0..6).fold(K, |x, _| x.wrapping_mul(2u64.wrapping_sub(K.wrapping_mul(x))));
        // ints sharing their low ten hash bits probe a sized and a grown table apart
        let keys: Vec<i64> = (0..3000u64).map(|j| ((5 << 38 | j.wrapping_mul(0x2545_f491) & ((1 << 38) - 1)).wrapping_mul(inv) & ((1 << 48) - 1)) as i64).map(|x| if x >= 1 << 47 { x - (1 << 48) } else { x }).collect();
        let items: Vec<Val> = keys.iter().cycle().take(21000).map(|&k| Val::int(k)).collect();
        let heap = HeapPool::new(1 << 20);
        let _pay = Paying::to(heap.meter());
        let mut seen = Vec::new();
        for held in [0usize, 1200 << 10, 1600 << 10] {
            let ballast = vec![1u8; held];
            let built = ValSet::build(&items, &heap).map(|s| s.len()).ok();
            seen.push((built, heap.take_work()));
            drop(ballast);
        }
        assert!(seen[0].0 == Some(3000) && seen[0].1 > 0 && seen[1] == seen[0] && seen[2].0.is_none(), "{seen:?}");
    }

    // a collection due at the floor lands before the slot table doubles
    #[test]
    fn a_loop_collects_before_its_slot_table_doubles() {
        let src = |n: usize| format!("class P:\n    pass\nt = 0\nfor i in range({n}):\n    p = P()\n    setattr(p, 'x' + str(i % 3), i)\n    t += getattr(p, 'x' + str(i % 3))\nprint(t)\n");
        let (_, below, _) = ran(&src(150));
        let (_, past, _) = ran(&src(300));
        assert!(past < below + (16 << 10), "below {below} past {past}");
    }

    // reducers take a generator one item at a time so its items never pile up
    #[test]
    fn reducing_a_generator_holds_one_item_at_a_time() {
        for src in ["print(sum(i * 2 for i in range(2000000)))\n", "print(max(i % 1000 for i in range(2000000)))\n", "print(min(-i for i in range(2000000)))\n"] {
            let (_, peak, _) = ran(src);
            assert!(peak < 4 << 20, "{src}: peak {peak}");
        }
    }

    /* gives the thread back its whole budget however a test ends */
    struct Restore;

    impl Drop for Restore {
        fn drop(&mut self) { set_budget(usize::MAX); }
    }

    // programs in one instance share its budget and only the one passing it raises
    #[test]
    fn the_program_that_passes_the_instance_budget_raises_alone() {
        let _restore = Restore;
        set_budget(64 << 20);
        // host memory stays outside the budget but a received event counts against the program
        drop(std::hint::black_box(vec![0u8; 32 << 20]));
        let holder = compiled("xs = [0] * 5000000\nfn total():\n    return len(xs)\n");
        let mut a = crate::common::boot(&holder, Limits::sandbox());
        a.run().unwrap();
        a.push_event(&"e".repeat(8 << 20)).unwrap();
        assert!(a.memory() >= 40_000_000 + (8 << 20), "{}", a.memory());
        // each stops far below its own 256 MB limit so the instance budget refused it
        for src in ["ys = [0] * 5000000\n", "ys = []\nwhile true:\n    ys.append(0)\n"] {
            let chunk = compiled(src);
            let mut b = crate::common::boot(&chunk, Limits::sandbox());
            let err = b.run().map(|_| ()).unwrap_err();
            assert!(err.to_string().contains("MemoryError") && b.memory_peak() < 64 << 20, "{src:?}: {err}, peak {}", b.memory_peak());
        }
        let total = a.call_export("total", &[]).unwrap();
        assert_eq!(a.display(total), "5000000");
    }

    // blocks of every class come back and move across classes keeping what they held
    #[test]
    fn pooled_blocks_come_back_with_their_bytes() {
        use core::alloc::{GlobalAlloc, Layout};
        let (pool, of) = (edge::value::pool::Pooled::new(std::alloc::System), |n| Layout::from_size_align(n, 8).unwrap());
        // SAFETY each block is written within its layout and freed once with it
        unsafe {
            for round in 0..3u8 {
                let live: Vec<_> = (1..=300).step_by(7).map(|n| { let p = pool.alloc(of(n)); assert!(!p.is_null()); p.write_bytes(round, n); (p, n) }).collect();
                for (p, n) in live.into_iter().rev() { let q = pool.realloc(p, of(n), 2 * n); assert!((0..n).all(|i| *q.add(i) == round)); pool.dealloc(q, of(2 * n)); }
            }
            let z = pool.alloc_zeroed(of(40));
            assert!((0..40).all(|i| *z.add(i) == 0));
            pool.dealloc(z, of(40));
        }
    }
}
