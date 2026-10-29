use spell::corpus::{self, Kind, Seg, Style};
use spell::{respell, translate, translate_renamed, Braces, Indent, Mini, Renames, Spell, BRACES, INDENT};

// a line indented with blanks then a tab is refused instead of panicking
#[test]
fn malformed_indentation_is_refused() {
    assert!(translate::<Indent, Braces>("x\n  \ty\n").is_err());
    assert!(translate::<Indent, Braces>("if x:\n  \ty\n").is_err());
}

// token soups from both tables translate or refuse in either direction but never panic
#[test]
fn soups_never_panic() {
    let mut pieces: Vec<&str> = INDENT.words.iter().chain(BRACES.words).chain(INDENT.symbols).chain(BRACES.symbols).map(|(w, _)| *w).collect();
    pieces.extend(["x", "y", "f", "0", "2.5", "'a'", "\"b\"", "f'{x}'", "f\"{y=}\"", "f'{x!r:>4}'", "b'z'", "# c", "// c"]);
    pieces.extend(["\n", "\n", "\n    ", "\n        ", "\n\t", "\n  \t", " ", " ", "  ", "\\\n", ":", "{", "}", "(", ")"]);
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    for _ in 0..20_000 {
        let len = 1 + next(24);
        let soup: String = (0..len).map(|_| { let p = pieces[next(pieces.len())]; if next(3) == 0 { format!("{p} ") } else { p.to_string() } }).collect();
        let _ = translate::<Indent, Braces>(&soup);
        let _ = translate::<Braces, Indent>(&soup);
        let _ = translate::<Braces, Mini>(&soup);
    }
}

#[test]
fn blocks_take_braces_on_the_lines_they_had() {
    let src = "def f(x):\n    if x and not y:\n        return None\n    elif x // 2:\n        pass\n    else:\n        return True  # done\nprint(f(1))\n";
    let braces = translate::<Indent, Braces>(src).expect("a plain program translates");
    assert_eq!(braces, "fn f(x) {\n    if x && !y {\n        return nil\n    } else if x ~/ 2 {\n        pass\n    } else {\n        return true } }  // done\nprint(f(1))\n");
    assert_eq!(translate::<Braces, Indent>(&braces).as_deref(), Ok(src));
    assert_eq!(src.matches('\n').count(), braces.matches('\n').count());
    // mini reads the words of braces under a header colon and indentation
    let mini = translate::<Braces, Mini>(&braces).expect("braces flip to mini");
    assert_eq!(mini, "fn f(x):\n    if x && !y:\n        return nil\n    else if x ~/ 2:\n        pass\n    else:\n        return true  // done\nprint(f(1))\n");
    assert_eq!(translate::<Mini, Braces>(&mini).as_deref(), Ok(braces.as_str()));
}

#[test]
fn what_braces_cannot_spell_is_refused_with_its_reason() {
    let why = |src: &str| translate::<Indent, Braces>(src).expect_err(src).why;
    assert!(why("if True: if True: print(1)\n").contains("compound statement after a colon"));
    assert!(why("if True:\nprint(1)\n").contains("expects an indented block"));
    assert!(why("x = 1\nprint(f\"{x or 5=}\")\n").contains("`=` field"));
    assert!(why("nil = 1\n").contains("reserved word of braces"));
    assert!(translate::<Indent, Braces>("fn = 1\n").is_ok());
}

#[test]
fn a_spelling_respells_to_itself_unchanged() {
    let src = "x = [1, 2]\nprint(x)\n";
    assert_eq!(respell(Spell::Indent, Spell::Indent, src).as_deref(), Ok(src));
    assert_eq!(Spell::named("braces"), Some(Spell::Braces));
    assert_eq!((Spell::Braces.other(), Spell::Mini.other()), (Spell::Mini, Spell::Braces));
}

const CASES: &str = "- case: |-\n    if x:\n        y = 1\n\n    print(y)\n  expected: |-\n    1\n\n- case: \"x=\\\"\\\"\\\"a\\rb\\\"\\\"\\\"\\nprint(x == None)\"\n  expected: |-\n    False\n\n- case: |\n    print(True)\n  error: boom\n";

#[test]
fn case_files_read_each_scalar_as_the_yaml_parser_does() {
    let sites = corpus::sites(Kind::VmCases, CASES).expect("three cases");
    let texts: Vec<&str> = sites.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, ["if x:\n    y = 1\n\nprint(y)", "x=\"\"\"a\rb\"\"\"\nprint(x == None)", "print(True)\n"]);
    assert_eq!(sites.iter().map(|s| s.line).collect::<Vec<_>>(), [2, 9, 14]);
    assert_eq!((sites[0].style, sites[1].style), (Style::Block { indent: 4 }, Style::Quoted));
}

#[test]
fn a_rewrite_changes_only_the_programs() {
    let sites = corpus::sites(Kind::VmCases, CASES).expect("three cases");
    let programs: Vec<String> = sites.iter().map(|s| respell(Spell::Indent, Spell::Braces, &s.text).expect("translates")).collect();
    let new = corpus::rewrite(CASES, &sites, &programs);
    corpus::verify(Kind::VmCases, CASES, &new, &programs).expect("reads back");
    assert_eq!(new, "- case: |-\n    if x {\n        y = 1 }\n\n    print(y)\n  expected: |-\n    1\n\n- case: \"x=\\\"\\\"\\\"a\\rb\\\"\\\"\\\"\\nprint(x == nil)\"\n  expected: |-\n    False\n\n- case: |\n    print(true)\n  error: boom\n");
    let back: Vec<String> = programs.iter().map(|p| respell(Spell::Braces, Spell::Indent, p).expect("comes back")).collect();
    assert_eq!(corpus::rewrite(&new, &corpus::sites(Kind::VmCases, &new).expect("reads"), &back), CASES);
}

#[test]
fn json_strings_carry_their_paths() {
    let text = "[\n  {\"src\": \"print(True)\", \"modules\": {\"m\": {\"code\": \"x = None\"}}, \"output\": [\"True\"]}\n]\n";
    let paths: Vec<Vec<Seg>> = corpus::json_strings(text).expect("json").into_iter().map(|(p, _)| p).collect();
    assert_eq!(paths[0], [Seg::At(0), Seg::Key("src".into())]);
    let sites = corpus::sites(Kind::CaseJson, text).expect("json cases");
    assert_eq!(sites.iter().map(|s| (s.label.as_str(), s.text.as_str())).collect::<Vec<_>>(), [("case 0", "print(True)"), ("case 0 module m", "x = None")]);
}

#[test]
fn eval_programs_of_an_actor_file_are_found_by_their_role() {
    let text = "groups:\n  runners:\n    eval: true\n  plain:\n    code: |\n      x = receive()\n      print(x is None)\n    seed: [\"go\"]\npublish:\n  - \"runners print(True)\"\n  - \"plain hello\"\npost:\n  - path: \"/eval/runners\"\n    body: \"print(None)\"\n";
    let sites = corpus::sites(Kind::ActorYml, text).expect("actor file");
    let found: Vec<(&str, &str)> = sites.iter().map(|s| (s.prefix.as_str(), s.text.as_str())).collect();
    assert_eq!(found, [("", "x = receive()\nprint(x is None)\n"), ("runners ", "print(True)"), ("", "print(None)")]);
}

// a table that renames one builtin and one error the way a later respell would
const RENAMES: Renames = Renames { names: &[("print", "say"), ("ValueError", "ValueFault")], texts: &[("ValueError", "ValueFault")] };

#[test]
fn a_table_renames_names_in_programs_and_words_in_texts() {
    let src = "try:\n    raise ValueError(print)\nexcept ValueError as e:\n    print(e, 'print')\n";
    let braces = translate_renamed::<Indent, Braces>(src, RENAMES, true).expect("renames");
    assert_eq!(braces, "try {\n    raise ValueFault(say)\n} except ValueFault as e {\n    say(e, 'print') }\n");
    assert_eq!(translate_renamed::<Braces, Indent>(&braces, RENAMES, false).as_deref(), Ok(src));
    assert_eq!(RENAMES.text("ValueError: x, ValueErrors and print", true), "ValueFault: x, ValueErrors and print");
    assert_eq!((RENAMES.chunk_name("print_3", true), RENAMES.chunk_name("printer_3", true)), ("say_3".to_string(), "printer_3".to_string()));
    let cases = "- case: |-\n    raise ValueError('x')\n  error: ValueError\n\n- case: |-\n    print(1)\n  expected: |-\n    ValueError ok\n";
    let renamed = corpus::rename_texts(Kind::VmCases, cases, &RENAMES, true).expect("texts");
    assert_eq!(renamed, "- case: |-\n    raise ValueError('x')\n  error: ValueFault\n\n- case: |-\n    print(1)\n  expected: |-\n    ValueFault ok\n");
    // the tables of the repo name every builtin alike so a flip renames nothing
    assert!(Spell::Braces.renames(Spell::Mini).0.is_empty());
}
