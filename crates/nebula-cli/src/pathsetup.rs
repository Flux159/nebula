//! `nebula setup path` — make the bundled docker/kubectl/helm available to
//! users who don't have their own.
//!
//! Mechanism: symlinks in ~/.nebula/bin pointing at the app bundle, plus one
//! guarded line APPENDED to the shell profile (`$PATH:$HOME/.nebula/bin`) —
//! appended so the user's own installs always shadow the bundled copies.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context};

use crate::client;

pub const TOOLS: &[&str] = &["docker", "kubectl", "helm"];
const PROFILE_MARKER: &str = "# nebula: bundled CLI tools (docker/kubectl/helm fill-ins)";

pub struct Status {
    pub missing_from_path: Vec<&'static str>,
    pub bundle_dir: Option<PathBuf>,
    pub profile_configured: bool,
}

pub fn status() -> Status {
    let missing_from_path = TOOLS.iter().copied().filter(|t| !on_path(t)).collect();
    Status {
        missing_from_path,
        bundle_dir: bundled_bin_dir(),
        profile_configured: profile_has_marker().unwrap_or(false),
    }
}

fn on_path(tool: &str) -> bool {
    std::process::Command::new("/usr/bin/which")
        .arg(tool)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The bundled CLI dir (app Resources), if this nebula runs near one.
fn bundled_bin_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for candidate in [
        dir.join("../Resources/resources/bin"), // Nebula.app sidecar layout
        dir.join("resources/bin"),              // future flat layouts
    ] {
        if candidate.join("docker").is_file() {
            return candidate.canonicalize().ok();
        }
    }
    // Dev tree: staged by scripts/fetch-host-clis.sh.
    for anc in exe.ancestors() {
        let candidate = anc.join("ui/src-tauri/resources/bin");
        if candidate.join("docker").is_file() {
            return Some(candidate);
        }
    }
    None
}

fn profile_path() -> anyhow::Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME not set")?);
    // zsh is the macOS default; fall back to .bash_profile when zsh is absent.
    let shell = std::env::var("SHELL").unwrap_or_default();
    Ok(if shell.contains("bash") {
        home.join(".bash_profile")
    } else {
        home.join(".zshrc")
    })
}

fn profile_has_marker() -> anyhow::Result<bool> {
    let path = profile_path()?;
    Ok(path.is_file() && std::fs::read_to_string(path)?.contains(PROFILE_MARKER))
}

/// Install symlinks + profile line. `assume_yes` skips the prompt (used by
/// the app and --yes). Prints what happened either way.
pub fn install(assume_yes: bool) -> anyhow::Result<()> {
    let st = status();
    let Some(bundle) = st.bundle_dir else {
        bail!(
            "no bundled CLI tools found near this nebula binary — install the Nebula.app \
             (or run scripts/fetch-host-clis.sh in a dev checkout)"
        );
    };

    if st.missing_from_path.is_empty() && st.profile_configured {
        println!("✓ docker, kubectl, and helm are all available; nothing to do");
        return Ok(());
    }
    if st.missing_from_path.is_empty() {
        println!("✓ docker, kubectl, and helm are already on your PATH — not changing anything");
        return Ok(());
    }

    let profile = profile_path()?;
    println!("missing from PATH: {}", st.missing_from_path.join(", "));
    println!("this will:");
    println!(
        "  1. symlink {} -> {}",
        client::nebula_home()?.join("bin").display(),
        bundle.display()
    );
    println!(
        "  2. append to {}: export PATH=\"$PATH:$HOME/.nebula/bin\"",
        profile.display()
    );
    println!("     (appended LAST — your own installs always take precedence)");

    if !assume_yes {
        print!("proceed? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("not changing anything (run `nebula setup path` anytime)");
            return Ok(());
        }
    }

    let bin = client::nebula_home()?.join("bin");
    std::fs::create_dir_all(&bin)?;
    for tool in TOOLS {
        let src = bundle.join(tool);
        if !src.is_file() {
            continue;
        }
        let dst = bin.join(tool);
        let _ = std::fs::remove_file(&dst); // refresh stale links (moved app)
        std::os::unix::fs::symlink(&src, &dst)?;
    }

    if !profile_has_marker()? {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&profile)?;
        writeln!(
            f,
            "\n{PROFILE_MARKER}\nexport PATH=\"$PATH:$HOME/.nebula/bin\""
        )?;
        println!(
            "✓ updated {} — restart your terminal (or `source` it)",
            profile.display()
        );
    } else {
        println!("✓ profile already configured");
    }
    println!("✓ bundled tools linked: {}", TOOLS.join(", "));
    Ok(())
}
