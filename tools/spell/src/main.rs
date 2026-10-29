#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};

use spell::corpus::{self, Back, Status};
use spell::{line_of, literals, rekey, respell, split, Spell};

// a run meters what it allocates as edge.wasm does
edge::allocator!();

const USAGE: &str = "usage
  spell mini|braces|indent FILE prints FILE or stdin given as - in that spelling from braces or else from mini
  spell corpus [--from S] [--to S] [--first N] [--root DIR] [--dry DIR] [--report DIR] [FILE...]
                                rewrites every program of each FILE or of the whole tree from braces into mini in place
                                or into DIR where --first N keeps the first N cases of a case file as they are
  spell rust FILE...            rewrites in place each literal of the rust files that holds a braces program
  spell rekey [--root DIR] --corpus DIR [--moved TSV] [--back TSV] [--out DIR] SNAPSHOT...
                                maps each old bench key to the key of the rewritten case
  spell keys [--root DIR]       prints the bench key of every vm case with its source
  spell run FILE                runs a mini program and prints what it printed and how it ended
  spell split [--spelling mini|indent] [--root DIR] [--dry DIR] [--report DIR]
                                moves every native and syntax vm case to its own files in place or into DIR";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let run = match args.first().map(String::as_str) {
        Some("corpus") => corpus_cmd(&args[1..]),
        Some("rust") => rust_cmd(&args[1..]),
        Some("rekey") => rekey_cmd(&args[1..]),
        Some("keys") => keys_cmd(&args[1..]),
        Some("run") if args.len() == 2 => run_cmd(&args[1]),
        Some("split") => split_cmd(&args[1..]),
        Some(name) if args.len() == 2 && Spell::named(name).is_some() => file(name, &args[1]),
        _ => Err(USAGE.to_string()),
    };
    match run {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
}

// the flags a command takes with their values and the words left over
fn options(args: &[String], flags: &[&str]) -> Result<(HashMap<String, String>, Vec<String>), String> {
    let (mut opts, mut rest) = (HashMap::new(), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if flags.contains(&a.as_str()) {
            let v = it.next().ok_or_else(|| format!("{a} wants a value\n{USAGE}"))?;
            opts.insert(a.clone(), v.clone());
        } else if a.starts_with("--") {
            return Err(format!("unknown flag {a}\n{USAGE}"));
        } else {
            rest.push(a.clone());
        }
    }
    Ok((opts, rest))
}

fn spelling(opts: &HashMap<String, String>, flag: &str, default: &str) -> Result<Spell, String> {
    let name = opts.get(flag).map_or(default, String::as_str);
    Spell::named(name).ok_or_else(|| format!("{flag} names mini braces or indent, not {name}"))
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir).map_err(|e| format!("{} {e}", dir.display()))?; }
    std::fs::write(path, text).map_err(|e| format!("{} {e}", path.display()))
}

// `path` made absolute through its longest ancestor that exists
fn resolved(path: &Path) -> Result<PathBuf, String> {
    let abs = std::path::absolute(path).map_err(|e| format!("{} {e}", path.display()))?;
    let (mut base, mut rest) = (abs.as_path(), Vec::new());
    while !base.exists() {
        rest.extend(base.file_name());
        base = base.parent().ok_or_else(|| format!("{} has no existing ancestor", path.display()))?;
    }
    let mut out = base.canonicalize().map_err(|e| format!("{} {e}", base.display()))?;
    out.extend(rest.iter().rev());
    Ok(out)
}

// the output tree never overlaps the tree it reads
fn apart(root: &Path, out: &Path) -> Result<(), String> {
    let (r, o) = (resolved(root)?, resolved(out)?);
    if r == o || o.starts_with(r.join("tests")) || r.starts_with(&o) { return Err(format!("{} would write over {}", out.display(), root.display())); }
    std::fs::create_dir_all(out).map_err(|e| format!("{} {e}", out.display()))
}

fn file(to: &str, path: &str) -> Result<i32, String> {
    let to = Spell::named(to).ok_or(USAGE)?;
    let mut src = String::new();
    let read = if path == "-" { std::io::stdin().read_to_string(&mut src).map(drop) } else { std::fs::read_to_string(path).map(|s| src = s) };
    read.map_err(|e| format!("{path} {e}"))?;
    match respell(to.other(), to, &src) {
        Ok(text) => {
            print!("{text}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("{path}:{}: {}", line_of(&src, e.at), e.why);
            Ok(1)
        }
    }
}

fn tsv(rows: &[Vec<String>]) -> String {
    rows.iter().map(|r| r.join("\t") + "\n").collect()
}

// the files named on the command line or else every source of the tree
fn chosen(root: &Path, files: &[String]) -> Result<Vec<(PathBuf, corpus::Kind)>, String> {
    if files.is_empty() { return Ok(corpus::sources(root)); }
    let base = resolved(root)?;
    files.iter().map(|f| {
        let rel = resolved(Path::new(f))?.strip_prefix(&base).map(Path::to_path_buf).map_err(|_| format!("{f} is not under {}", root.display()))?;
        let kind = corpus::kind_of(root, &rel).ok_or_else(|| format!("{f} is no file the walk reads"))?;
        Ok((rel, kind))
    }).collect()
}

fn corpus_cmd(args: &[String]) -> Result<i32, String> {
    let (opts, files) = options(args, &["--from", "--to", "--first", "--root", "--dry", "--report"])?;
    let to = spelling(&opts, "--to", "mini")?;
    let from = spelling(&opts, "--from", to.other().name())?;
    let first: usize = opts.get("--first").map_or(Ok(0), |n| n.parse().map_err(|_| format!("--first wants a case count, not {n}")))?;
    let root = PathBuf::from(opts.get("--root").map_or(".", String::as_str));
    let dry = opts.get("--dry").map(PathBuf::from);
    if let Some(dir) = &dry { apart(&root, dir)?; }
    let report = opts.get("--report").map(PathBuf::from).or_else(|| dry.clone());
    if let Some(dir) = &report { apart(&root, dir)?; }
    let files = corpus::translate_tree(&root, chosen(&root, &files)?, from, to, first)?;
    let head = |cols: &[&str]| vec![cols.iter().map(|c| c.to_string()).collect::<Vec<_>>()];
    let mut refused = head(&["file", "line", "program", "program_line", "why"]);
    let mut back = head(&["file", "line", "program", "back", "detail"]);
    let mut moved = head(&["file", "line", "program", "from_line", "to_line"]);
    let mut worded = head(&["file", "line", "program", "errors_before", "errors_after"]);
    let mut counts = head(&["file", "programs", "same", "translated", "untranslatable", "differs", "invalid_both", "back_exact", "back_inexact", "back_fails", "error"]);
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = [0usize; 9];
    let mut broken = 0;
    for f in &files {
        match &dry {
            Some(dir) => write(&dir.join(&f.to), &f.text)?,
            // a file is written only when it changed and leaves its old name once renamed
            None => {
                if f.error.is_none() && (f.to != f.rel || std::fs::read_to_string(root.join(&f.rel)).ok().as_deref() != Some(f.text.as_str())) {
                    write(&root.join(&f.to), &f.text)?;
                    if f.to != f.rel { std::fs::remove_file(root.join(&f.rel)).map_err(|e| format!("{} {e}", f.rel.display()))?; }
                }
            }
        }
        let file = f.rel.display().to_string();
        let mut n = [0usize; 9];
        for (site, c) in &f.outcomes {
            n[0] += 1;
            match &c.status {
                Status::Same => n[1] += 1,
                Status::Translated => n[2] += 1,
                Status::Untranslatable { line, why } => {
                    n[3] += 1;
                    let at = corpus::file_line(site, *line);
                    eprintln!("{file}:{at}: {} line {line}: {why}", site.label);
                    *reasons.entry(why.clone()).or_default() += 1;
                    refused.push(vec![file.clone(), at.to_string(), site.label.clone(), line.to_string(), why.clone()]);
                }
                Status::Differs(why) => {
                    n[4] += 1;
                    broken += 1;
                    eprintln!("{file}:{}: {} compiles to another program in {}, {why}", site.line, site.label, to.name());
                    refused.push(vec![file.clone(), site.line.to_string(), site.label.clone(), String::new(), format!("compiles differently {why}")]);
                }
            }
            if let Some((a, b)) = &c.invalid {
                n[5] += 1;
                worded.push(vec![file.clone(), site.line.to_string(), site.label.clone(), json(&a.join(" | ")), json(&b.join(" | "))]);
            }
            match &c.back {
                Some(Back::Exact) => n[6] += 1,
                Some(Back::Inexact { line, was, now }) => {
                    n[7] += 1;
                    let detail = format!("line {line} {} came back as {}", json(was), json(now));
                    back.push(vec![file.clone(), corpus::file_line(site, *line).to_string(), site.label.clone(), "inexact".into(), detail]);
                }
                Some(Back::Fails(why)) => {
                    n[8] += 1;
                    back.push(vec![file.clone(), site.line.to_string(), site.label.clone(), "fails".into(), why.clone()]);
                }
                None => {}
            }
            if let Some((a, b)) = c.moved { moved.push(vec![file.clone(), site.line.to_string(), site.label.clone(), a.to_string(), b.to_string()]); }
        }
        if let Some(e) = &f.error {
            broken += 1;
            eprintln!("{file}: copied unchanged, {e}");
        }
        let mut row = vec![file];
        row.extend(n.iter().map(usize::to_string));
        row.push(f.error.clone().unwrap_or_default());
        counts.push(row);
        for (t, v) in total.iter_mut().zip(n) { *t += v; }
    }
    let mut row = vec!["total".to_string()];
    row.extend(total.iter().map(usize::to_string));
    row.push(String::new());
    counts.push(row);
    let mut hand = head(&["file", "line", "what", "status"]);
    for h in literals::by_hand(&root, from, to) {
        hand.push(vec![h.file.display().to_string(), h.line.to_string(), h.what, h.status]);
    }
    if let Some(dir) = &report {
        for (name, rows) in [("untranslatable.tsv", &refused), ("roundtrip.tsv", &back), ("moved.tsv", &moved), ("worded.tsv", &worded), ("counts.tsv", &counts), ("by-hand.tsv", &hand)] {
            write(&dir.join(name), &tsv(rows))?;
        }
    }
    let mut summary = String::new();
    let _ = writeln!(summary, "{} files, {} programs from {} to {}", files.len(), total[0], from.name(), to.name());
    let _ = writeln!(summary, "  {} translated, {} already alike, {} untranslatable, {} compile differently", total[2], total[1], total[3], total[4]);
    let _ = writeln!(summary, "  back to {} {} exact, {} inexact, {} fail, {} statements moved line", from.name(), total[6], total[7], total[8], moved.len() - 1);
    let _ = writeln!(summary, "  {} programs both spellings reject in other words", worded.len() - 1);
    let _ = writeln!(summary, "  {} literals and files to respell by hand", hand.len() - 1);
    for (why, n) in &reasons { let _ = writeln!(summary, "  {n:>5} {why}"); }
    print!("{summary}");
    Ok(if broken > 0 { 1 } else { 0 })
}

fn json(s: &str) -> String { serde_json::to_string(s).unwrap_or_default() }

fn rust_cmd(files: &[String]) -> Result<i32, String> {
    let mut left = 0;
    for f in files {
        let src = std::fs::read_to_string(f).map_err(|e| format!("{f} {e}"))?;
        let (out, done, skipped) = literals::respell_rust(&src, Spell::Braces, Spell::Mini);
        if out != src { write(Path::new(f), &out)?; }
        println!("{f}: {done} literals respelled, {} left", skipped.len());
        for s in &skipped { eprintln!("{f}:{}: {} {}", s.line, s.why, json(&s.text)); }
        left += skipped.len();
    }
    Ok(i32::from(left > 0))
}

fn rekey_cmd(args: &[String]) -> Result<i32, String> {
    let (opts, snapshots) = options(args, &["--root", "--corpus", "--moved", "--back", "--out"])?;
    let root = PathBuf::from(opts.get("--root").map_or(".", String::as_str));
    let corpus = PathBuf::from(opts.get("--corpus").ok_or(USAGE)?);
    let out = opts.get("--out").map_or(corpus.clone(), PathBuf::from);
    apart(&root, &out)?;
    // the cases of the old tree that left the vm files and so the bench
    let moved = match opts.get("--moved") {
        Some(path) => rekey::moved(&std::fs::read_to_string(path).map_err(|e| format!("{path} {e}"))?)?,
        None => Vec::new(),
    };
    // returned cases of the new tree that pair with nothing in the old
    let back = match opts.get("--back") {
        Some(path) => rekey::moved(&std::fs::read_to_string(path).map_err(|e| format!("{path} {e}"))?)?,
        None => Vec::new(),
    };
    let (pairs, gone) = rekey::pairs(&root, &corpus, &moved, &back)?;
    let mut rows = vec![vec!["old".to_string(), "new".into(), "file".into(), "index".into()]];
    rows.extend(pairs.iter().map(|p| vec![p.old.clone(), p.new.clone(), p.file.clone(), p.index.to_string()]));
    write(&out.join("rekey.tsv"), &tsv(&rows))?;
    let keys: String = spell::cases::vm_cases(&corpus).iter().map(|c| format!("{}\t{}\n", spell::cases::key(c), c["src"])).collect();
    write(&out.join("keys.tsv"), &keys)?;
    let changed = pairs.iter().filter(|p| p.old != p.new).count();
    println!("{} cases paired by file and index, {changed} under a new key, {} keep theirs, {} left the bench", pairs.len(), pairs.len() - changed, gone.len());
    let mut unpaired = 0;
    for snap in &snapshots {
        let path = Path::new(snap);
        let text = std::fs::read_to_string(path).map_err(|e| format!("{snap} {e}"))?;
        let applied = rekey::apply(&text, &pairs, &gone).map_err(|e| format!("{snap} {e}"))?;
        let rel = path.canonicalize().ok().and_then(|p| root.canonicalize().ok().and_then(|r| p.strip_prefix(r).ok().map(Path::to_path_buf)));
        let dest = out.join(rel.unwrap_or_else(|| PathBuf::from(path.file_name().unwrap_or_default())));
        write(&dest, &applied.text)?;
        println!("{snap} to {}, {} entries re-keyed, {} dropped with their case, {} entries with no case, {} cases with no entry", dest.display(), applied.mapped, applied.dropped, applied.stale.len(), applied.missing.len());
        for k in &applied.stale { println!("  entry {k} has no case"); }
        for p in &applied.missing { println!("  {} case {} has no entry, old key {} new key {}", p.file, p.index, p.old, p.new); }
        unpaired += applied.stale.len();
    }
    Ok(if unpaired > 0 { 1 } else { 0 })
}

fn keys_cmd(args: &[String]) -> Result<i32, String> {
    let (opts, _) = options(args, &["--root"])?;
    let root = PathBuf::from(opts.get("--root").map_or(".", String::as_str));
    for c in spell::cases::vm_cases(&root) { println!("{}\t{}", spell::cases::key(&c), c["src"]); }
    Ok(0)
}

fn run_cmd(path: &str) -> Result<i32, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{path} {e}"))?;
    let (chunk, errors) = edge::code::compile_as::<spell::Mini>(&src);
    if let Some(e) = errors.first() {
        eprintln!("{path}:{}: {}", line_of(&src, e.start), e.msg);
        return Ok(1);
    }
    let mut vm = edge::embed::VM::with_runtime(&mini::RUNTIME, &chunk, edge::embed::Limits::sandbox());
    let ran = vm.run();
    print!("{}", vm.output_text());
    let failed = ran.as_ref().err().map(|e| vm.render_error(e, &src, Some(path)));
    eprintln!("{}memory peak {} bytes", failed.map(|f| f + "\n").unwrap_or_default(), vm.heap_mut().peak());
    Ok(i32::from(ran.is_err()))
}

fn split_cmd(args: &[String]) -> Result<i32, String> {
    let (opts, _) = options(args, &["--spelling", "--root", "--dry", "--report"])?;
    let spell = spelling(&opts, "--spelling", "mini")?;
    let root = PathBuf::from(opts.get("--root").map_or(".", String::as_str));
    let dry = opts.get("--dry").map(PathBuf::from);
    let report = opts.get("--report").map(PathBuf::from).or_else(|| dry.clone());
    for dir in dry.iter().chain(&report) { apart(&root, dir)?; }
    let rows = split::classify_tree(&root, spell)?;
    let moved = split::moved(&root, &rows)?;
    // a dry run writes the tree a move would leave beside its reports
    let into = dry.as_deref().unwrap_or(&root);
    for (rel, text) in &moved.files { write(&into.join(rel), text)?; }
    let mut list = vec![["file", "index", "line", "key", "kind", "natives", "kept"].map(String::from).to_vec()];
    let mut per_file: BTreeMap<String, [usize; 6]> = BTreeMap::new();
    let mut modules: BTreeMap<&str, usize> = BTreeMap::new();
    let mut reasons: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for r in &rows {
        let c = per_file.entry(r.file.clone()).or_default();
        c[0] += 1;
        let (names, kept) = match &r.fate {
            split::Fate::Native(cases) => {
                c[2] += 1;
                c[3] += cases.len();
                for n in cases { *modules.entry(n.call.module).or_default() += 1; }
                (cases.iter().map(|n| format!("{}@{}", n.call.native, n.call.module)).collect::<Vec<_>>().join(" "), String::new())
            }
            split::Fate::Syntax(_) => { c[4] += 1; (String::new(), String::new()) }
            split::Fate::Kept(kind, why) => {
                c[1] += 1;
                c[5] += 1;
                *reasons.entry((kind.name(), why)).or_default() += 1;
                (String::new(), format!("{} {why}", kind.name()))
            }
            split::Fate::Engine => { c[1] += 1; (String::new(), String::new()) }
        };
        list.push(vec![r.file.clone(), r.index.to_string(), r.line.to_string(), r.key.clone(), r.fate.kind().name().into(), names, kept]);
    }
    let mut counts = vec![["file", "cases", "engine", "native", "native_cases", "syntax", "kept"].map(String::from).to_vec()];
    let mut total = [0usize; 6];
    for (file, c) in &per_file {
        counts.push(std::iter::once(file.clone()).chain(c.iter().map(usize::to_string)).collect());
        for (t, v) in total.iter_mut().zip(c) { *t += v; }
    }
    counts.push(std::iter::once("total".to_string()).chain(total.iter().map(usize::to_string)).collect());
    counts.push(Vec::new());
    counts.push(vec!["module".into(), "native_cases".into()]);
    counts.extend(modules.iter().map(|(m, n)| vec![m.to_string(), n.to_string()]));
    if let Some(dir) = &report {
        write(&dir.join("split.tsv"), &tsv(&list))?;
        write(&dir.join("split-counts.tsv"), &tsv(&counts))?;
    }
    let verb = if dry.is_some() { "would move" } else { "moved" };
    println!("{} vm cases read in {}, {} stay engine cases, {verb} {} native giving {} native cases and {} syntax", rows.len(), spell.name(), total[1], total[2], total[3], total[4]);
    println!("  native cases by module {}", modules.iter().map(|(m, n)| format!("{m} {n}")).collect::<Vec<_>>().join(", "));
    for ((kind, why), n) in &reasons { println!("  {n:>5} {kind} cases stay as {why}"); }
    for line in &moved.sampled { println!("  miri.tsv drops {line}"); }
    Ok(0)
}
