use anyhow::{anyhow, bail, Context, Result};
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

// The Rust release this CLI was built with, whose LLVM is the one the VM's bitcode was written by.
const RUSTC: &str = env!("EDGE_RUSTC");

/* A Rust sysroot, the layout rustup installs and the one a fetch writes, so `rustc --print sysroot` with llvm-tools works as it is. */
pub struct Sysroot(PathBuf);

impl Sysroot {
    /// The LLVM tools and the crt `target` links with, fetched into the cache the first time, asking first.
    pub fn open(target: &str) -> Result<Sysroot> {
        if let Some(root) = std::env::var_os("EDGE_LLVM_ROOT") {
            return Ok(Sysroot(root.into()));
        }
        let root = Sysroot(crate::host::cache_root().map_err(|e| anyhow!("{e}"))?.join("llvm").join(RUSTC));
        let host = host();
        // What each component brings: LLVM itself, the linker beside it, and the target's libc and compiler builtins.
        let std = format!("lib/rustlib/{target}/lib/");
        let wanted = [
            ("llvm-tools-preview", host.clone(), vec![format!("lib/rustlib/{host}/")]),
            ("rustc", host.clone(), vec![format!("lib/rustlib/{host}/bin/rust-lld")]),
            ("rust-std", target.to_string(), vec![format!("{std}self-contained/"), format!("{std}libcompiler_builtins-")]),
        ];
        // Each component leaves a mark once it is whole, so an interrupted fetch starts that one over.
        let mark = |pkg: &str, triple: &str| root.0.join(format!("{pkg}-{triple}.fetched"));
        let missing: Vec<_> = wanted.into_iter().filter(|(pkg, triple, _)| !mark(pkg, triple).exists()).collect();
        if missing.is_empty() {
            return Ok(root);
        }
        if !std::io::stdin().is_terminal() {
            bail!("edge build needs LLVM {RUSTC}'s tools and found none\nhelp: run it once in a terminal to be offered a download, or set EDGE_LLVM_ROOT to a Rust sysroot with llvm-tools");
        }
        if !crate::ui::confirm(&format!("edge build needs LLVM's tools from Rust {RUSTC} (about 200 MB, once). Download them?"))? {
            bail!("edge build needs LLVM's tools, set EDGE_LLVM_ROOT to a Rust sysroot with llvm-tools instead");
        }
        let channel = read(&format!("https://static.rust-lang.org/dist/channel-rust-{RUSTC}.toml"))?;
        let channel = String::from_utf8_lossy(&channel);
        for (pkg, triple, keep) in missing {
            let (url, hash) = component(&channel, pkg, &triple).ok_or_else(|| anyhow!("Rust {RUSTC} ships no {pkg} for {triple}"))?;
            let sp = crate::ui::spinner(&format!("downloading {pkg} for {triple} into {}", root.0.display()));
            let bytes = read(&url)?;
            if compiler::util::sha256::hex_encode(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref()) != hash {
                sp.fail(&format!("{url} does not match its published sha256"));
                bail!("refusing {url}, its sha256 is not the one the channel publishes");
            }
            unpack(&bytes, &keep, &root.0).with_context(|| format!("unpacking {url}"))?;
            std::fs::write(mark(pkg, &triple), &hash)?;
            sp.done(&format!("{pkg} for {triple}"));
        }
        Ok(root)
    }

    /// A tool the host runs, from llvm-tools or `rust-lld`.
    pub fn tool(&self, name: &str) -> PathBuf {
        self.0.join(format!("lib/rustlib/{}/bin/{name}{}", host(), std::env::consts::EXE_SUFFIX))
    }

    /// The crt objects and static libc a musl target links with.
    pub fn crt(&self, target: &str) -> PathBuf {
        self.0.join(format!("lib/rustlib/{target}/lib/self-contained"))
    }
}

/* The triple Rust publishes this host's tools under, glibc builds run on every Linux. */
fn host() -> String {
    match std::env::consts::OS {
        "macos" => format!("{}-apple-darwin", std::env::consts::ARCH),
        "windows" => format!("{}-pc-windows-msvc", std::env::consts::ARCH),
        _ => format!("{}-unknown-linux-gnu", std::env::consts::ARCH),
    }
}

/* The url and sha256 of one component in a channel manifest, read off its `[pkg.<name>.target.<triple>]` table. */
fn component(channel: &str, pkg: &str, triple: &str) -> Option<(String, String)> {
    let table = channel.split(&format!("\n[pkg.{pkg}.target.{triple}]\n")).nth(1)?.split("\n[").next()?;
    let field = |key: &str| table.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(" = \"")?.strip_suffix('"').map(str::to_string));
    Some((field("url")?, field("hash")?))
}

fn read(url: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    crate::host::get(url).map_err(|e| anyhow!("fetching {url}: {e}"))?.into_body().into_reader().read_to_end(&mut bytes)?;
    Ok(bytes)
}

/* Writes the entries under one of the `keep` prefixes with the archive's `<name>/<component>/` prefix taken off, so every component lands in one sysroot. */
fn unpack(gz: &[u8], keep: &[String], root: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(gz));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let inner: PathBuf = path.components().skip(2).collect();
        if !keep.iter().any(|k| inner.to_string_lossy().starts_with(k.as_str())) || !entry.header().entry_type().is_file() {
            continue;
        }
        let to = root.join(&inner);
        std::fs::create_dir_all(to.parent().unwrap_or(root))?;
        // A half-written tool never passes for a fetched one, the rename lands it whole.
        let part = to.with_extension("part");
        entry.unpack(&part)?;
        std::fs::rename(&part, &to)?;
    }
    Ok(())
}
