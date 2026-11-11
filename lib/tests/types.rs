use edge::value::{DictMap, HeapObj, HeapPool, Val};
use lib::lang::Tree;
use mini::Mini;
use mini::types::NAMES;

const TREE: &Tree = &mini::types::TREE;

// the builtin types a load names besides the exceptions
const PLAIN: [&str; 12] = ["int", "float", "text", "bytes", "bytearray", "bool", "list", "tuple", "record", "set", "range", "type"];

// the roots: the errors and the three signals no handler of errors catches
const ROOTS: [&str; 4] = ["Error", "Stop", "Close", "Exit"];

fn put(heap: &mut HeapPool, obj: HeapObj) -> Val { heap.alloc(obj).unwrap_or_else(|e| panic!("{e}")) }

fn parent(name: &str) -> Option<&'static str> { TREE.parent(name) }

fn place(name: &str) -> Option<usize> { TREE.place(name) }

fn derives(name: &str, ancestor: &str) -> bool { TREE.derives(name, ancestor) }

fn match_exc(heap: &HeapPool, exc: Val, ty: Val) -> Result<bool, edge::value::VmErr> { lib::types::match_exc::<Mini>(heap, exc, ty) }

#[test]
fn every_exception_reaches_a_root_without_a_cycle() {
    for &(name, _) in TREE.classes() {
        let (mut at, mut steps) = (name, 0);
        while let Some(p) = parent(at) {
            at = p;
            steps += 1;
            assert!(steps <= TREE.classes().len(), "{name} loops");
        }
        assert!(ROOTS.contains(&at), "{name} stops short of a root at {at}");
    }
    for (i, e) in TREE.classes().iter().enumerate() { assert_eq!(place(e.0), Some(i), "{}", e.0); }
    assert!(place("exception").is_none() && place("ExceptioN").is_none() && place("Exceptio").is_none());
    assert_eq!((parent("KeyError"), parent("PermissionError")), (Some("Error"), Some("IOError")));
    assert!(ROOTS.iter().all(|r| parent(r).is_none()) && parent("int").is_none());
    assert!(derives("DivisionError", "Error") && derives("PermissionError", "Error") && derives("int", "int"));
    assert!(!derives("Exit", "Error") && !derives("Stop", "Error") && !derives("Close", "Error") && !derives("Error", "KeyError"));
}

#[test]
fn a_load_names_each_type_once_and_the_tree_drops_what_nothing_raises() {
    for name in NAMES { assert_eq!(NAMES.iter().filter(|n| *n == name).count(), 1, "{name}"); }
    assert!(TREE.classes().iter().all(|e| NAMES.contains(&e.0)) && PLAIN.iter().all(|p| NAMES.contains(p)));
    assert!(!NAMES.contains(&"StopAsyncIteration") && !TREE.has("StopAsyncIteration"));
}

#[test]
fn an_except_clause_matches_along_the_tree_and_the_bases() {
    let mut heap = HeapPool::new(1 << 20);
    let ty = |heap: &mut HeapPool, n: &str| heap.type_object(n).unwrap_or_else(|e| panic!("{e}"));
    let (key, lookup, value, base) = (ty(&mut heap, "KeyError"), ty(&mut heap, "Error"), ty(&mut heap, "ValueError"), ty(&mut heap, "Error"));
    let raised = heap.new_error("KeyError", vec![]).unwrap_or_else(|e| panic!("{e}"));
    let either = put(&mut heap, HeapObj::Tuple(vec![value, lookup]));
    let matches = |heap: &HeapPool, exc: Val, t: Val| match_exc(heap, exc, t).unwrap_or_else(|e| panic!("{e}"));
    assert!(matches(&heap, raised, key) && matches(&heap, raised, lookup) && matches(&heap, raised, base) && matches(&heap, raised, either));
    assert!(!matches(&heap, raised, value));
    let class = heap.new_class("Mine", vec![value], vec![]).unwrap_or_else(|e| panic!("{e}"));
    let child = heap.new_class("Child", vec![class], vec![]).unwrap_or_else(|e| panic!("{e}"));
    let mine = heap.new_instance(child, DictMap::new()).unwrap_or_else(|e| panic!("{e}"));
    assert!(matches(&heap, mine, class) && matches(&heap, mine, child) && matches(&heap, mine, value) && matches(&heap, mine, base));
    assert!(!matches(&heap, mine, key) && !matches(&heap, raised, class));
    let text = put(&mut heap, HeapObj::Str("KeyError".into()));
    assert!(match_exc(&heap, raised, text).is_err_and(|e| e.to_string().contains("a handler must name a class or a group of classes")));
}
