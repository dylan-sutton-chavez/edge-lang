use anyhow::{bail, Context, Result};
use clap::{ArgMatches, Args, FromArgMatches};
use std::fs;
use std::path::Path;

use crate::engine::cli::kit::{Command, Descriptor, Kit};

const INDEX_HTML: &str = include_str!("../templates/scaffold.html");

#[derive(Args)]
struct InitArgs {
    #[arg(help = "Project directory to create, the current one when omitted")]
    name: Option<String>,
    #[arg(long)]
    bare: bool,
}

/* `init` scaffolding the runtime's entry beside a manifest */
#[expect(clippy::exhaustive_structs, reason = "a marker a user names to pick what it stands for")]
pub struct Init;

impl Command for Init {
    fn spec(&self, _kit: &Kit) -> clap::Command {
        InitArgs::augment_args(clap::Command::new("init").about("Scaffold a new project on the runtime --runtime names"))
            .mut_arg("bare", |arg| arg.help("Skip the browser index.html, scaffold only the entry and interpreter.json"))
    }

    fn runs(&self) -> bool {
        true
    }

    fn summary(&self, _kit: &Kit) -> (String, String) {
        ("init [name]".to_string(), "Scaffold a new project".to_string())
    }

    fn run(&self, kit: &Kit, m: &ArgMatches) -> Result<i32> {
        let args = InitArgs::from_arg_matches(m)?;
        run(kit, args.name.as_deref(), args.bare)?;
        Ok(0)
    }
}

/* the manifest of a new project with its engine and the runtime it was scaffolded on */
fn manifest(kit: &Kit, root: &Path) -> Result<String> {
    let runtime = match kit.declaration() {
        Some((spec, _, false)) if spec.contains("://") => serde_json::json!(spec),
        Some((spec, base, false)) => serde_json::json!(relative(&base.join(spec), root)?),
        Some((spec, base, true)) => serde_json::json!({ "native": relative(&base.join(spec), root)? }),
        None => bail!("init scaffolds a project on a runtime, name its .wasm with --runtime <file>"),
    };
    let value = serde_json::json!({ "interpreter": env!("CARGO_PKG_VERSION"), "runtime": runtime });
    Ok(format!("{}\n", serde_json::to_string_pretty(&value)?))
}

/* `target` as a path from the directory `from` so a project moves with its runtime */
fn relative(target: &Path, from: &Path) -> Result<String> {
    let target = fs::canonicalize(target).with_context(|| format!("reading the runtime {}", target.display()))?;
    let from = fs::canonicalize(from).with_context(|| format!("reading {}", from.display()))?;
    let shared = target.components().zip(from.components()).take_while(|(a, b)| a == b).count();
    let mut path = std::path::PathBuf::new();
    for _ in from.components().skip(shared) {
        path.push("..");
    }
    for part in target.components().skip(shared) {
        path.push(part);
    }
    Ok(path.to_string_lossy().replace('\\', "/"))
}

fn index_html(d: &Descriptor, title: &str) -> String {
    INDEX_HTML.replace("__INTERPRETER_ENTRY__", d.entry()).replace("__INTERPRETER_TITLE__", title)
}

// scaffolds a ready-to-run project
fn run(kit: &Kit, name: Option<&str>, bare: bool) -> Result<()> {
    let d = kit.descriptor();
    let dir = name.unwrap_or(".");
    let root = Path::new(dir);

    if dir != "." {
        if root.exists() {
            bail!("'{dir}' already exists");
        }
        fs::create_dir_all(root).with_context(|| format!("creating {dir}"))?;
    }

    let title = if dir == "." { format!("{} app", kit.name()) } else { dir.to_string() };
    let mut files = vec![(d.entry(), d.scaffold.to_string()), ("interpreter.json", manifest(kit, root)?)];
    // the page runs the program in a browser so only the web host writes it
    let page = !bare && cfg!(feature = "cli-web");
    if page {
        files.insert(0, ("index.html", index_html(d, &title)));
    }
    // a file already there is the project's own so init refuses before writing any
    if let Some((name, _)) = files.iter().find(|(name, _)| fs::symlink_metadata(root.join(name)).is_ok()) {
        bail!("'{}' already exists, init writes only into a directory without it", root.join(name).display());
    }
    for (name, text) in &files {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(root.join(name)).with_context(|| format!("creating {name}"))?;
        std::io::Write::write_all(&mut file, text.as_bytes()).with_context(|| format!("writing {name}"))?;
    }
    let items: Vec<&str> = files.iter().map(|(name, _)| *name).collect();

    // the next step names a command this composition wires
    let name = kit.name();
    // a page runs only a wasm runtime so a native project starts from its entry
    let native = kit.declaration().is_some_and(|(_, _, native)| *native);
    let step = match page && !native && kit.wires("serve") && kit.wires("build") {
        true => format!("{name} build --web && {name} serve"),
        false => format!("{name} run {}", d.entry()),
    };
    let next = if dir == "." { step } else { format!("cd {dir} && {step}") };
    crate::engine::cli::ui::scaffolded(dir, &items, &next);
    Ok(())
}
