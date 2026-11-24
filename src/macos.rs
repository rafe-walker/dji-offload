//! The macOS glue: notifications, ejecting, and the launchd agent that runs
//! the offload every time a drive mounts.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub const LABEL: &str = "io.github.rafe-walker.dji-offload";

pub fn notify(title: &str, message: &str) {
    println!("{title}: {message}");
    if cfg!(target_os = "macos") {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\" sound name \"Glass\"",
            esc(message),
            esc(title)
        );
        let _ = Command::new("osascript").args(["-e", &script]).status();
    }
}

pub fn eject(mount: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let out = Command::new("diskutil").arg("eject").arg(mount).output()?;
    if !out.status.success() {
        bail!("diskutil eject: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn agent_plist() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home")?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "501".into())
}

/// Copy this binary somewhere stable and register a launchd agent with
/// `StartOnMount`, so macOS runs `dji-offload run --auto` on every mount.
pub fn install(state: &Path) -> Result<PathBuf> {
    let bin_dir = state.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let bin = bin_dir.join("dji-offload");
    let me = std::env::current_exe()?;
    if me != bin {
        fs::copy(&me, &bin).context("copying the binary into place")?;
    }
    let log = dirs::home_dir().context("no home")?.join("Library/Logs/dji-offload.log");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin}</string>
    <string>run</string>
    <string>--auto</string>
  </array>
  <key>StartOnMount</key><true/>
  <key>RunAtLoad</key><false/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        bin = bin.display(),
        log = log.display()
    );
    let path = agent_plist()?;
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, plist)?;
    if cfg!(target_os = "macos") {
        let domain = format!("gui/{}", uid());
        let _ = Command::new("launchctl").args(["bootout", &domain]).arg(&path).output();
        let out = Command::new("launchctl").args(["bootstrap", &domain]).arg(&path).output()?;
        if !out.status.success() {
            bail!("launchctl bootstrap: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    Ok(path)
}

pub fn uninstall() -> Result<()> {
    let path = agent_plist()?;
    if cfg!(target_os = "macos") {
        let domain = format!("gui/{}", uid());
        let _ = Command::new("launchctl").args(["bootout", &domain]).arg(&path).output();
    }
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

pub fn installed() -> bool {
    agent_plist().map(|p| p.exists()).unwrap_or(false)
}

/// `~/.local/bin/dji-offload` -> the installed binary, so `dji-offload` works
/// in Terminal. Adds `~/.local/bin` to PATH in ~/.zprofile if no shell
/// startup file mentions it yet. Returns the shortcut path.
pub fn add_shortcut(bin: &Path) -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home")?;
    let dir = home.join(".local/bin");
    fs::create_dir_all(&dir)?;
    let link = dir.join("dji-offload");
    if link.symlink_metadata().is_ok() {
        fs::remove_file(&link)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(bin, &link).context("creating the shortcut")?;
    let mentioned = [".zshrc", ".zprofile", ".bash_profile", ".bashrc", ".profile"]
        .iter()
        .filter_map(|f| fs::read_to_string(home.join(f)).ok())
        .any(|t| t.lines().any(|l| !l.trim_start().starts_with('#') && l.contains(".local/bin")));
    if !mentioned {
        let mut text = fs::read_to_string(home.join(".zprofile")).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("# added by dji-offload: lets you type `dji-offload` in Terminal\nexport PATH=\"$HOME/.local/bin:$PATH\"\n");
        fs::write(home.join(".zprofile"), text)?;
    }
    Ok(link)
}

/// Remove the shortcut if it points at our binary.
pub fn remove_shortcut() -> Result<()> {
    let link = dirs::home_dir().context("no home")?.join(".local/bin/dji-offload");
    if let Ok(target) = fs::read_link(&link)
        && target.to_string_lossy().contains("dji-offload/bin/dji-offload") {
            fs::remove_file(&link)?;
        }
    Ok(())
}
