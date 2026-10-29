use anyhow::{anyhow, bail, Result};
use clap::ArgMatches;
#[cfg(feature = "web")]
use std::io::IsTerminal;
use std::io::Write;
use std::process::Command;

#[cfg(feature = "web")]
use crate::host::browser;
use crate::kit::Kit;

// bundled at compile time so cleaning up needs no network
const UNINSTALL_SH: &str = include_str!("../../setup/uninstall.sh");

/* `uninstall` removing the binary the install script placed */
pub struct Uninstall;

impl crate::kit::Command for Uninstall {
    fn spec(&self, kit: &Kit) -> clap::Command {
        clap::Command::new("uninstall").about(format!("Remove the {} binary and its PATH entry", kit.name()))
    }

    fn summary(&self, kit: &Kit) -> (String, String) {
        ("uninstall".to_string(), format!("Remove the {} binary and PATH entry", kit.name()))
    }

    fn run(&self, _kit: &Kit, _m: &ArgMatches) -> anyhow::Result<i32> {
        run()?;
        Ok(0)
    }
}

// runs the bundled uninstall.sh with bash from a temp file
fn run() -> Result<()> {
    println!("This removes the edge binary, its cache and the PATH entry from your shell rc files.");
    drop_browser()?;

    // a script file rather than stdin keeps read prompts working
    let temp = stage_script()?;

    let mut cmd = Command::new("bash");
    cmd.arg(temp.path());
    // the install dir comes from where this binary lives so custom installs clean up too
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        cmd.env("EDGE_INSTALL_DIR", dir);
    }

    let status = cmd.status().map_err(|e| anyhow!("running bash: {e}"))?;
    if !status.success() {
        bail!("uninstall script exited with code {:?}", status.code());
    }
    Ok(())
}

/* the downloaded browser goes only on a yes and stays when no terminal can answer */
#[cfg(feature = "web")]
fn drop_browser() -> Result<()> {
    let Ok(dir) = browser::chrome_dir() else { return Ok(()) };
    if !dir.exists() {
        return Ok(());
    }

    if !std::io::stdin().is_terminal() {
        println!("kept the headless Chromium at {}, remove it by hand", dir.display());
        return Ok(());
    }

    print!("Remove the headless Chromium edge downloaded to {}? [y/N] ", dir.display());
    std::io::stdout().flush().ok();

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(|e| anyhow!("reading the answer: {e}"))?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES") {
        println!("kept {}", dir.display());
        return Ok(());
    }

    std::fs::remove_dir_all(&dir).map_err(|e| anyhow!("removing {}: {e}", dir.display()))?;
    println!("removed {}", dir.display());
    Ok(())
}

// a composition with no browser host downloaded none
#[cfg(not(feature = "web"))]
fn drop_browser() -> Result<()> {
    Ok(())
}

// a unique temp file because a predictable shared path invites symlink squatting
fn stage_script() -> Result<tempfile::NamedTempFile> {
    let mut f = tempfile::NamedTempFile::new().map_err(|e| anyhow!("staging uninstall script: {e}"))?;
    f.write_all(UNINSTALL_SH.as_bytes()).map_err(|e| anyhow!("staging uninstall script: {e}"))?;
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_names_are_unique() {
        let a = stage_script().unwrap();
        let b = stage_script().unwrap();
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn staged_file_carries_the_bundled_script() {
        let f = stage_script().unwrap();
        assert_eq!(std::fs::read_to_string(f.path()).unwrap(), UNINSTALL_SH);
    }

    #[test]
    fn staged_file_lands_in_the_temp_dir() {
        let f = stage_script().unwrap();
        assert_eq!(f.path().parent(), Some(std::env::temp_dir().as_path()));
    }

    #[test]
    fn staged_file_is_removed_on_drop() {
        let path = {
            let f = stage_script().unwrap();
            assert!(f.path().exists());
            f.path().to_path_buf()
        };
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_preplanted_symlink_at_the_old_path_is_not_followed() {
        let target = std::env::temp_dir().join(format!("edge-uninstall-target-{}", std::process::id()));
        let decoy = std::env::temp_dir().join("edge-uninstall.sh");
        std::fs::write(&target, "keep me").unwrap();
        let _ = std::fs::remove_file(&decoy);
        std::os::unix::fs::symlink(&target, &decoy).unwrap();
        let staged = stage_script().unwrap();
        assert_ne!(staged.path(), decoy);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");
        drop(staged);
        let _ = std::fs::remove_file(&decoy);
        let _ = std::fs::remove_file(&target);
    }
}
