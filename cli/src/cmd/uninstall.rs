use anyhow::{anyhow, bail, Result};
use std::io::{IsTerminal, Write};
use std::process::Command;

use crate::host::browser;

// Bundled at compile time so the binary is self-contained, no network needed to clean up.
const UNINSTALL_SH: &str = include_str!("../../setup/uninstall.sh");

/// Drop the bundled uninstall.sh to a temp file and run it via bash.
pub fn run() -> Result<()> {
    println!("This removes the edge binary, its cache and the PATH entry from your shell rc files.");
    drop_browser()?;

    // Spawning bash with the script as a file lets read prompts (none, in this path) work normally.
    let temp = stage_script()?;

    let mut cmd = Command::new("bash");
    cmd.arg(temp.path());
    // Point at the install dir derived from where this binary lives, so non-default installs still clean up.
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

/* A browser edge downloaded is a hundred megabytes it put there, so it goes only when asked, and stays when nobody is there to answer. */
fn drop_browser() -> Result<()> {
    let Ok(dir) = browser::chrome_dir() else { return Ok(()) };
    if !dir.exists() {
        return Ok(());
    }

    if !std::io::stdin().is_terminal() {
        println!("kept the headless Chromium at {}, remove it by hand", dir.display());
        return Ok(());
    }

    if !crate::ui::confirm(&format!("Remove the headless Chromium edge downloaded to {}?", dir.display()))? {
        println!("kept {}", dir.display());
        return Ok(());
    }

    std::fs::remove_dir_all(&dir).map_err(|e| anyhow!("removing {}: {e}", dir.display()))?;
    println!("removed {}", dir.display());
    Ok(())
}

// A unique temp file, a predictable path in the shared temp dir invites symlink squatting.
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
