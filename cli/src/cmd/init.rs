use anyhow::{bail, Context, Result};
use clap::{ArgMatches, Args, FromArgMatches};
use std::fs;
use std::path::Path;

use crate::kit::{Command, Descriptor, Kit};

// a new project records its engine so an earlier engine says why it cannot run
const EDGE_JSON: &str = concat!("{\n  \"edge\": \"", env!("CARGO_PKG_VERSION"), "\"\n}\n");
const INDEX_HTML: &str = include_str!("../templates/scaffold.html");

#[derive(Args)]
struct InitArgs {
    /// Project directory to create, the current one when omitted.
    name: Option<String>,
    #[arg(long)]
    bare: bool,
}

/* `init` scaffolding the runtime's entry beside a manifest */
pub struct Init;

impl Command for Init {
    fn spec(&self, kit: &Kit) -> clap::Command {
        let entry = kit.descriptor().entry();
        InitArgs::augment_args(clap::Command::new("init").about("Scaffold a new project"))
            .mut_arg("bare", |arg| arg.help(format!("Skip the browser index.html, scaffold only {entry} and edge.json")))
    }

    fn summary(&self, _kit: &Kit) -> (String, String) {
        ("init <name>".to_string(), "Scaffold a new project".to_string())
    }

    fn run(&self, kit: &Kit, m: &ArgMatches) -> Result<i32> {
        let args = InitArgs::from_arg_matches(m)?;
        run(kit, args.name.as_deref(), args.bare)?;
        Ok(0)
    }
}

fn index_html(d: &Descriptor, title: &str) -> String {
    INDEX_HTML.replace("__EDGE_ENTRY__", d.entry()).replace("__EDGE_TITLE__", title)
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
    let mut files = vec![(d.entry(), d.scaffold.to_string()), ("edge.json", EDGE_JSON.to_string())];
    // the page runs the program in a browser so only the web host writes it
    let page = !bare && cfg!(feature = "web");
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
    let step = if page && kit.wires("serve") { "serve".to_string() } else { format!("run {}", d.entry()) };
    let next = if dir == "." { format!("{} {step}", kit.name()) } else { format!("cd {dir} && {} {step}", kit.name()) };
    crate::ui::scaffolded(dir, &items, &next);
    Ok(())
}
