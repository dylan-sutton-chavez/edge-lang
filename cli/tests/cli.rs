mod suite;

use suite::{net, suite, Bin};

const EDGE: Bin = Bin { path: env!("CARGO_BIN_EXE_edge"), env: &[], plugins: true };

/// JSON-driven CLI suite, each case in `cli.json` is one tempdir plus one spawn of the `edge` binary.
#[test]
fn cli_suite() {
    suite(EDGE, include_str!("cli.json"), &[&[]]);
}

/// third party wasm plugins built from `plugin/example` run the way a project declares them
#[test]
fn plugin_suite() {
    suite(EDGE, include_str!("plugins.json"), &[&[]]);
}

/// Programs both hosts must run alike, each case natively and again in the browser, which needs a Chrome.
#[test]
fn hosts_suite() {
    suite(EDGE, include_str!("hosts.json"), &[&[], &["--web"]]);
}

/// the net cases on both hosts against one loopback fixture which needs a Chrome
#[test]
fn net_suite() {
    net(EDGE, &[&[], &["--web"]]);
}

/// every transcript and program of docs/ runs on this binary, some fetch from GitHub
#[test]
fn docs_net_suite() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs");
    let mut pages: Vec<_> = std::fs::read_dir(&docs).expect("docs/").flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "mdx")).collect();
    pages.sort();
    let cache = tempfile::tempdir().expect("cache dir");
    let mut failed = Vec::new();
    for page in &pages {
        let text = std::fs::read_to_string(page).expect("a page");
        let name = page.file_name().unwrap().to_string_lossy().into_owned();
        if let Err(e) = docs_page(&text, cache.path()) {
            failed.push(format!("{name}: {e}"));
        }
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
    commands_match_help(&std::fs::read_to_string(docs.join("06-cli.mdx")).expect("the cli page"));
}

/* the fences of a page as their info words and body */
fn fences(text: &str) -> Vec<(Vec<String>, String)> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(info) = line.trim().strip_prefix("```") else { continue };
        let body: Vec<&str> = lines.by_ref().take_while(|l| l.trim() != "```").collect();
        out.push((info.split_whitespace().map(String::from).collect(), body.join("\n") + "\n"));
    }
    out
}

/* runs one page in a fresh project dir, files and transcripts in page order */
fn docs_page(text: &str, cache: &std::path::Path) -> Result<(), String> {
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut cwd = root.path().to_path_buf();
    let fences = fences(text);
    for (at, (info, body)) in fences.iter().enumerate() {
        match info.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            // a fence naming a path holds that file of the example project
            ["mini", path] => std::fs::write(cwd.join(path), body).map_err(|e| e.to_string())?,
            ["text"] if body.starts_with("$ ") => transcript(body, &mut cwd, cache)?,
            ["edge"] => {
                let manifest = at.checked_sub(1).filter(|&i| fences[i].0 == ["edge-manifest"]).map(|i| fences[i].1.clone());
                let output = fences.get(at + 1).filter(|f| f.0 == ["output"]).map(|f| f.1.clone());
                program(body, manifest, output, cache)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/* each `$ edge ...` line runs and must print every line shown below it */
fn transcript(body: &str, cwd: &mut std::path::PathBuf, cache: &std::path::Path) -> Result<(), String> {
    let mut steps: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in body.lines() {
        match line.strip_prefix("$ ") {
            Some(cmd) => steps.push((cmd, Vec::new())),
            None => steps.last_mut().ok_or("a transcript opens with a command")?.1.push(line.trim()),
        }
    }
    for (cmd, shown) in steps {
        let args: Vec<&str> = cmd.split_whitespace().collect();
        match args[..] {
            ["cd", dir] => *cwd = cwd.join(dir),
            ["edge", ..] => {
                let out = std::process::Command::new(EDGE.path).args(&args[1..]).current_dir(&*cwd).env("XDG_CACHE_HOME", cache)
                    .stdin(std::process::Stdio::null()).output().map_err(|e| e.to_string())?;
                let printed = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                // a transcript that shows an error expects the command to fail
                let fails = shown.iter().any(|l| l.starts_with("error"));
                if out.status.success() == fails {
                    return Err(format!("`{cmd}` exited {:?}:\n{printed}", out.status.code()));
                }
                let lines: Vec<&str> = printed.lines().map(str::trim).collect();
                if let Some(missing) = shown.iter().filter(|l| !l.is_empty()).find(|l| !lines.contains(l)) {
                    return Err(format!("`{cmd}` never printed {missing:?}:\n{printed}"));
                }
            }
            _ => return Err(format!("`{cmd}` is no edge command")),
        }
    }
    Ok(())
}

/* an edge block runs as main.mini under its manifest and prints its output block */
fn program(src: &str, manifest: Option<String>, output: Option<String>, cache: &std::path::Path) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    std::fs::write(dir.path().join("main.mini"), src).map_err(|e| e.to_string())?;
    std::fs::write(dir.path().join("edge.json"), manifest.unwrap_or_else(|| "{}\n".into())).map_err(|e| e.to_string())?;
    let out = std::process::Command::new(EDGE.path).args(["run", "main.mini"]).current_dir(dir.path()).env("XDG_CACHE_HOME", cache)
        .stdin(std::process::Stdio::null()).output().map_err(|e| e.to_string())?;
    let printed = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return Err(format!("the program\n{src}failed: {}", String::from_utf8_lossy(&out.stderr)));
    }
    match output {
        Some(want) if printed != want => Err(format!("the program\n{src}printed {printed:?}, the page shows {want:?}")),
        _ => Ok(()),
    }
}

/* the command table of the cli page names exactly the commands the help lists */
fn commands_match_help(page: &str) {
    let documented: std::collections::BTreeSet<&str> = page.lines().filter_map(|l| l.strip_prefix("| `edge ")).filter_map(|l| l.split([' ', '`']).next()).collect();
    let help = std::process::Command::new(EDGE.path).arg("--help").output().expect("edge --help");
    let help = String::from_utf8_lossy(&help.stdout).into_owned();
    let listed: std::collections::BTreeSet<&str> = help.lines().skip_while(|l| *l != "Commands").skip(1).take_while(|l| !l.is_empty()).filter_map(|l| l.split_whitespace().next()).collect();
    assert_eq!(documented, listed, "docs/06-cli.mdx lists other commands than edge --help");
}
