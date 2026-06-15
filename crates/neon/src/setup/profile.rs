/// `neon setup wire-profile` — wire the shell profile to the NeonOS-managed profile.
///
/// Reads `~/.config/neon/setup.toml` for `[shell].preferred`, determines the
/// correct rc / profile file to modify, and inserts a guarded `# neon:start` /
/// `# neon:end` marker block that sources the NeonOS-managed profile stub.
/// Idempotent: re-running prints "(already wired)" and exits cleanly.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

// --- Arg struct ---

/// Arguments for `neon setup wire-profile`.
#[derive(clap::Args, Debug)]
pub struct WireProfileArgs {
    /// Path to the NeonOS-managed profile file to source (default: auto-detect from setup.toml [shell])
    #[arg(long, value_name = "PATH")]
    pub profile_path: Option<PathBuf>,

    /// Print what would happen without making changes
    #[arg(long)]
    pub dry_run: bool,
}

// --- Constants ---

const MARKER_START: &str = "# neon:start";
const MARKER_END: &str = "# neon:end";

const STUB_PS1: &str = "\
# NeonOS managed profile — add your customizations below
# This file is sourced by your PowerShell profile automatically.
";

const STUB_SH: &str = "\
# NeonOS managed profile — add your customizations below
# This file is sourced by your shell rc file automatically.
";

// --- Pure helpers (testable without touching real filesystem) ---

/// Return `true` if the content already contains a `# neon:start` marker.
pub(crate) fn has_marker(content: &str) -> bool {
    content.contains(MARKER_START)
}

/// Resolve the shell profile (rc) file to modify for a given shell string and home dir.
///
/// Returns an error if the shell is unrecognized or if USERPROFILE is unavailable
/// on Windows.
pub(crate) fn profile_path_for(shell: &str, home: &Path) -> Result<PathBuf> {
    match shell {
        "powershell7" => {
            // On Windows, the canonical profile lives under Documents\PowerShell\.
            // USERPROFILE is the Windows home (e.g. C:\Users\alice).
            // We derive it from the home path we already have rather than re-reading env.
            let path = home
                .join("Documents")
                .join("PowerShell")
                .join("Microsoft.PowerShell_profile.ps1");
            Ok(path)
        }
        "zsh" => Ok(home.join(".zshrc")),
        "bash" => Ok(home.join(".bashrc")),
        "wsl" => Ok(home.join(".bashrc")),
        other => anyhow::bail!(
            "unrecognized shell '{}' in setup.toml — run `neon setup pick-shell` first",
            other
        ),
    }
}

/// Build the marker block to insert into the profile / rc file.
///
/// The literal env-var references (`$env:USERPROFILE`, `$HOME`) are written
/// verbatim so they are expanded by the shell at startup time, not by neon.
pub(crate) fn marker_block(shell: &str) -> String {
    let source_line = match shell {
        "powershell7" => r#". "$env:USERPROFILE\.config\neon\profile.ps1""#.to_string(),
        _ => {
            // zsh / bash / wsl
            r#"source "$HOME/.config/neon/profile.sh""#.to_string()
        }
    };
    format!("{MARKER_START}\n{source_line}\n{MARKER_END}\n")
}

/// Decide which stub file (path + content) should accompany a given shell.
pub(crate) fn stub_for(shell: &str, home: &Path) -> (PathBuf, &'static str) {
    match shell {
        "powershell7" => (
            home.join(".config").join("neon").join("profile.ps1"),
            STUB_PS1,
        ),
        _ => (
            home.join(".config").join("neon").join("profile.sh"),
            STUB_SH,
        ),
    }
}

// --- Public entry point ---

pub fn run(args: WireProfileArgs) -> Result<()> {
    let dry_run = args.dry_run;

    // Resolve home directory (use setup.rs's helper via super::).
    let home = super::home_dir()?;

    // Determine the shell: --profile-path implies we skip auto-detect; otherwise
    // read setup.toml to find [shell].preferred.
    let (shell_str, rc_path) = match args.profile_path {
        Some(explicit) => {
            // User supplied the rc path directly; we still need the shell for
            // choosing the stub and marker block.  Read setup.toml if possible;
            // fall back to "zsh" as a safe generic (produces a POSIX source line).
            let cfg_path = super::config_path()?;
            let cfg = super::load_config(&cfg_path)?;
            let shell = cfg
                .shell
                .map(|s| s.preferred)
                .unwrap_or_else(|| "zsh".to_string());
            (shell, explicit)
        }
        None => {
            let cfg_path = super::config_path()?;
            let cfg = super::load_config(&cfg_path)?;
            let shell = cfg.shell.map(|s| s.preferred).ok_or_else(|| {
                anyhow::anyhow!("no preferred shell configured — run `neon setup pick-shell` first")
            })?;
            let rc = profile_path_for(&shell, &home)?;
            (shell, rc)
        }
    };

    // --- Step 1: ensure the managed stub file exists ---
    let (stub_path, stub_content) = stub_for(&shell_str, &home);
    if dry_run {
        if stub_path.exists() {
            println!(
                "[dry-run] [~] managed profile stub: {} (already exists)",
                stub_path.display()
            );
        } else {
            println!(
                "[dry-run] [✓] managed profile stub: {} (would create)",
                stub_path.display()
            );
        }
    } else if !stub_path.exists() {
        if let Some(parent) = stub_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        std::fs::write(&stub_path, stub_content)
            .with_context(|| format!("writing stub to {}", stub_path.display()))?;
        println!(
            "[✓] managed profile stub: {} (created)",
            stub_path.display()
        );
    } else {
        println!(
            "[~] managed profile stub: {} (already exists)",
            stub_path.display()
        );
    }

    // --- Step 2: check / append marker block in the rc/profile file ---
    // Treat a missing file as an empty string (the file may not exist yet, e.g.
    // a fresh PS profile dir).
    let current_content = match std::fs::read_to_string(&rc_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", rc_path.display()));
        }
    };

    if has_marker(&current_content) {
        println!("[~] {} (already wired)", rc_path.display());
        return Ok(());
    }

    let block = marker_block(&shell_str);

    if dry_run {
        println!(
            "[dry-run] [✓] {} (would append neon:start / neon:end block)",
            rc_path.display()
        );
        println!("[dry-run]     block content:");
        for line in block.lines() {
            println!("[dry-run]       {line}");
        }
        return Ok(());
    }

    // Create parent directories if needed (e.g. Documents\PowerShell on a fresh Windows).
    if let Some(parent) = rc_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }

    // Append with a leading newline for separation when the file is non-empty.
    let mut new_content = current_content;
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    if !new_content.is_empty() {
        new_content.push('\n');
    }
    new_content.push_str(&block);

    std::fs::write(&rc_path, &new_content)
        .with_context(|| format!("writing {}", rc_path.display()))?;

    println!("[✓] {} (wired)", rc_path.display());
    Ok(())
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // --- has_marker ---

    #[test]
    fn marker_absent_returns_false() {
        assert!(!has_marker("# some comment\necho hello\n"));
    }

    #[test]
    fn marker_present_returns_true() {
        let content = "# other stuff\n# neon:start\n. profile.ps1\n# neon:end\n";
        assert!(has_marker(content));
    }

    #[test]
    fn empty_string_returns_false() {
        assert!(!has_marker(""));
    }

    // --- profile_path_for ---

    #[test]
    fn powershell7_path_uses_documents_powershell() {
        let home = Path::new("/fake/home");
        let path = profile_path_for("powershell7", home).unwrap();
        assert!(
            path.to_string_lossy()
                .contains("Microsoft.PowerShell_profile.ps1"),
            "expected PS profile file, got: {}",
            path.display()
        );
        assert!(
            path.to_string_lossy().contains("PowerShell"),
            "expected PowerShell directory, got: {}",
            path.display()
        );
    }

    #[test]
    fn zsh_path_is_zshrc() {
        let home = Path::new("/fake/home");
        let path = profile_path_for("zsh", home).unwrap();
        assert_eq!(path, Path::new("/fake/home/.zshrc"));
    }

    #[test]
    fn bash_path_is_bashrc() {
        let home = Path::new("/fake/home");
        let path = profile_path_for("bash", home).unwrap();
        assert_eq!(path, Path::new("/fake/home/.bashrc"));
    }

    #[test]
    fn wsl_path_is_bashrc() {
        let home = Path::new("/fake/home");
        let path = profile_path_for("wsl", home).unwrap();
        assert_eq!(path, Path::new("/fake/home/.bashrc"));
    }

    #[test]
    fn unknown_shell_returns_error() {
        let home = Path::new("/fake/home");
        let result = profile_path_for("fish", home);
        assert!(result.is_err(), "expected error for unknown shell");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("fish"),
            "error should mention the bad shell name"
        );
    }

    // --- stub content ---

    #[test]
    fn ps1_stub_contains_expected_comment() {
        assert!(
            STUB_PS1.contains("NeonOS managed profile"),
            "PS1 stub must contain the NeonOS header comment"
        );
        assert!(
            STUB_PS1.contains("PowerShell profile"),
            "PS1 stub must mention PowerShell profile"
        );
    }

    #[test]
    fn sh_stub_contains_expected_comment() {
        assert!(
            STUB_SH.contains("NeonOS managed profile"),
            "SH stub must contain the NeonOS header comment"
        );
        assert!(
            STUB_SH.contains("shell rc file"),
            "SH stub must mention shell rc file"
        );
    }

    // --- marker_block ---

    #[test]
    fn powershell_marker_block_has_correct_source_line() {
        let block = marker_block("powershell7");
        assert!(block.contains(MARKER_START));
        assert!(block.contains(MARKER_END));
        assert!(
            block.contains(r#"$env:USERPROFILE"#),
            "PS block must contain literal $env:USERPROFILE"
        );
        assert!(
            block.contains("profile.ps1"),
            "PS block must reference profile.ps1"
        );
    }

    #[test]
    fn zsh_marker_block_has_correct_source_line() {
        let block = marker_block("zsh");
        assert!(block.contains(MARKER_START));
        assert!(block.contains(MARKER_END));
        assert!(
            block.contains(r#"$HOME"#),
            "zsh block must contain literal $HOME"
        );
        assert!(
            block.contains("profile.sh"),
            "zsh block must reference profile.sh"
        );
    }

    #[test]
    fn bash_marker_block_matches_zsh() {
        assert_eq!(
            marker_block("bash"),
            marker_block("zsh"),
            "bash and zsh share the same source line"
        );
    }

    // --- stub_for ---

    #[test]
    fn stub_for_powershell_returns_ps1_path() {
        let home = Path::new("C:/Users/test");
        let (path, content) = stub_for("powershell7", home);
        assert!(
            path.to_string_lossy().ends_with("profile.ps1"),
            "PS stub path must end with profile.ps1"
        );
        assert_eq!(content, STUB_PS1);
    }

    #[test]
    fn stub_for_zsh_returns_sh_path() {
        let home = Path::new("/home/test");
        let (path, content) = stub_for("zsh", home);
        assert!(
            path.to_string_lossy().ends_with("profile.sh"),
            "zsh stub path must end with profile.sh"
        );
        assert_eq!(content, STUB_SH);
    }
}
