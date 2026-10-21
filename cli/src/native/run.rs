use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::tools::Sysroot;
use super::{Opt, Opts};

/* Runs one fetched tool, its own stderr standing for the failure. */
fn tool(sys: &Sysroot, name: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(sys.tool(name)).args(args).output().with_context(|| format!("running {}", sys.tool(name).display()))?;
    if !out.status.success() {
        bail!("{name} failed\n{}", String::from_utf8_lossy(&out.stderr).trim_end());
    }
    Ok(())
}

/* The modules linked into one, optimized, counted when asked, compiled and linked for `target`; `out` ending in `.o` stops at the object. */
pub fn compile(sys: &Sysroot, modules: &[PathBuf], target: &str, opts: &Opts, out: &Path) -> Result<()> {
    let dir = tempfile::tempdir().context("creating a build dir")?;
    let at = |name: &str| dir.path().join(name).to_string_lossy().into_owned();
    let mut link: Vec<&str> = modules.iter().filter_map(|m| m.to_str()).collect();
    let linked = at("linked.bc");
    link.extend(["-o", &linked]);
    tool(sys, "llvm-link", &link)?;
    if let Some(keep) = &opts.emit_glue {
        std::fs::copy(&linked, keep).with_context(|| format!("writing {}", keep.display()))?;
    }
    let level = match opts.opt { Opt::Speed => "O3", Opt::Size => "Oz" };
    let optimized = at("opt.ll");
    // The program is whole, so every symbol but its entry is internal and LLVM may inline, drop and specialize across it.
    let keep = format!("-internalize-public-api-list={}", super::entry(target));
    tool(sys, "opt", &[&format!("-passes=internalize,default<{level}>"), &keep, "-S", &linked, "-o", &optimized])?;
    if let Some(keep) = &opts.emit_llvm {
        std::fs::copy(&optimized, keep).with_context(|| format!("writing {}", keep.display()))?;
    }
    // The counter's own module joins after the count, so its instructions are never counted.
    let optimized = match opts.count {
        false => optimized,
        true => {
            std::fs::write(&optimized, super::count::instrument(&std::fs::read_to_string(&optimized)?))?;
            std::fs::write(at("count.ll"), super::count::runtime(target))?;
            tool(sys, "llvm-link", &[&optimized, &at("count.ll"), "-o", &at("counted.bc")])?;
            at("counted.bc")
        }
    };
    let object = match out.extension().is_some_and(|e| e == "o") { true => out.to_string_lossy().into_owned(), false => at("main.o") };
    // A static musl binary has no loader to relocate it, the other targets keep LLVM's default.
    let model = if target.contains("musl") { "-relocation-model=static" } else { "-relocation-model=pic" };
    tool(sys, "llc", &[if opts.opt == Opt::Speed { "-O3" } else { "-O2" }, "-filetype=obj", model, &optimized, "-o", &object])?;
    if object == out.to_string_lossy() {
        return Ok(());
    }
    lld(sys, target, &object, opts.strip, out)
}

/* rust-lld in the flavor `target` links with, the object first and the static libc and compiler builtins after it. */
fn lld(sys: &Sysroot, target: &str, object: &str, strip: bool, out: &Path) -> Result<()> {
    let crt = sys.crt(target);
    let lib = crt.parent().unwrap_or(&crt).to_path_buf();
    // Rust's own builtins (128-bit division, float conversions) are kept out of LTO, so they link from their rlib.
    let builtins = std::fs::read_dir(&lib).with_context(|| format!("reading {}", lib.display()))?.flatten().map(|e| e.path())
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("libcompiler_builtins-") && n.to_string_lossy().ends_with(".rlib")))
        .with_context(|| format!("no compiler builtins under {}", lib.display()))?;
    let c = |name: &str| crt.join(name).to_string_lossy().into_owned();
    let mut args: Vec<String> = Vec::new();
    let tail = [object.to_string(), builtins.to_string_lossy().into_owned()];
    if target.contains("musl") {
        args.extend(["-flavor", "gnu", "-static", "--gc-sections"].map(String::from));
        args.extend([c("crt1.o"), c("crti.o")]);
        args.extend(tail);
        args.extend([c("libc.a"), c("crtn.o")]);
        args.extend(strip.then(|| "--strip-all".to_string()));
    } else if target.starts_with("wasm32") {
        args.extend(["-flavor", "wasm", "--gc-sections"].map(String::from));
        args.push(c("crt1-command.o"));
        args.extend(tail);
        args.push(c("libc.a"));
        args.extend(strip.then(|| "--strip-all".to_string()));
    } else if target.ends_with("apple-darwin") {
        let sdk = Command::new("xcrun").args(["--show-sdk-path"]).output().ok().filter(|o| o.status.success())
            .context("an apple-darwin target links against the macOS SDK, which only a Mac with Xcode's command line tools has")?;
        let arch = if target.starts_with("aarch64") { "arm64" } else { "x86_64" };
        args.extend(["-flavor", "darwin", "-arch", arch, "-platform_version", "macos", "11.0", "11.0", "-dead_strip", "-lSystem", "-syslibroot"].map(String::from));
        args.push(String::from_utf8_lossy(&sdk.stdout).trim().to_string());
        args.extend(tail);
        args.extend(strip.then(|| ["-x".to_string(), "-S".to_string()]).into_iter().flatten());
    } else {
        bail!("edge build has no linker setup for {target}");
    }
    args.extend(["-o".to_string(), out.to_string_lossy().into_owned()]);
    tool(sys, "rust-lld", &args.iter().map(String::as_str).collect::<Vec<_>>())
}
