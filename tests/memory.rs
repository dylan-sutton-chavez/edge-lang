/* What the meter reads for a program, every byte the allocator handed its run and took back. */
mod test {
    use compiler::lexer::lex;
    use compiler::parser::Parser;
    use compiler::parser::SSAChunk;
    use compiler::value::meter::{Meter, set_budget};
    use compiler::vm::{Limits, VM};

    fn compiled(src: &str) -> SSAChunk {
        let (tokens, _) = lex(src);
        let (chunk, errs) = Parser::new(src, tokens.into_iter()).parse();
        assert!(errs.is_empty(), "{src}");
        chunk
    }

    /* What `src` holds once it ran, the most it held at once, and the meter its run paid with. */
    fn ran(src: &str) -> (usize, usize, &'static Meter) {
        let chunk = compiled(src);
        let mut vm = VM::with_limits(&chunk, Limits::sandbox());
        vm.run().unwrap();
        (vm.memory(), vm.memory_peak(), vm.heap().meter())
    }

    const MIXED: &str = "class P:\n    def __init__(self, x):\n        self.x = x\ndef squares(n):\n    for i in range(n):\n        yield i * i\nd = {str(i): [i, (i, i + 1.5), {i}] for i in range(300)}\ns = ''.join(f'{k}:{v[0]!r}' for k, v in d.items())\nps = sorted([P(i) for i in range(200)], key=lambda p: -p.x)\nt = list(squares(500))\ntry:\n    raise ValueError('x' * 100)\nexcept ValueError as e:\n    msg = str(e)\nb = bytes(range(256)) * 10\nprint(len(s), ps[0].x, sum(t), len(msg), len(b.hex()))\n";

    // Once its VM drops a run has given back every byte it took, so the count never drifts.
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

    // The list goes before the loop starts collecting, so only the peak still remembers it.
    #[test]
    fn the_peak_keeps_what_a_program_held_after_a_collection_let_it_go() {
        let (held, peak, _) = ran("xs = [0] * 1000000\nxs = None\nn = 0\nfor i in range(100000):\n    n += len([i])\n");
        assert!(peak >= 8_000_000 && held < 4_000_000, "peak {peak} held {held}");
    }

    // An in-place intersection swaps in a smaller table, so only the peak keeps the big one.
    #[test]
    fn the_peak_keeps_what_a_set_held_before_it_shrank() {
        let (held, peak, _) = ran("s = set(range(100000))\ns &= {0}\n");
        assert!(peak >= 2_000_000 && held < peak / 2, "peak {peak} held {held}");
    }

    /* Gives the thread back its whole budget when a test ends, however it ends. */
    struct Restore;

    impl Drop for Restore {
        fn drop(&mut self) { set_budget(usize::MAX); }
    }

    // Programs in one instance share its budget, the one that passes it raises and the others keep running.
    #[test]
    fn the_program_that_passes_the_instance_budget_raises_alone() {
        let _restore = Restore;
        set_budget(64 << 20);
        // Host memory stays out of the budget, an event handed to a program counts as its own.
        drop(std::hint::black_box(vec![0u8; 32 << 20]));
        let holder = compiled("xs = [0] * 5000000\ndef total():\n    return len(xs)\n");
        let mut a = VM::with_limits(&holder, Limits::sandbox());
        a.run().unwrap();
        a.push_event(&"e".repeat(8 << 20)).unwrap();
        assert!(a.memory() >= 40_000_000 + (8 << 20), "{}", a.memory());
        // Each stops far below its own limit of 256 MB, the instance budget is what refused it.
        for src in ["ys = [0] * 5000000\n", "ys = []\nwhile True:\n    ys.append(0)\n"] {
            let chunk = compiled(src);
            let mut b = VM::with_limits(&chunk, Limits::sandbox());
            let err = b.run().map(|_| ()).unwrap_err();
            assert!(err.to_string().contains("MemoryError") && b.memory_peak() < 64 << 20, "{src:?}: {err}, peak {}", b.memory_peak());
        }
        let total = a.call_export("total", &[]).unwrap();
        assert_eq!(a.display(total), "5000000");
    }
}
