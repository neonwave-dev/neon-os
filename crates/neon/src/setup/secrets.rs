/// `neon setup secrets` — orchestrated credential setup (NPM token, SSH identities, Docker login).
///
/// Walks through up to three steps in sequence:
/// 1. **npm** — prompt for NPM_TOKEN, write to `~/.npmrc` (idempotent).
/// 2. **ssh** — prompt for SSH identity map entries, write to `~/.ssh/config` and
///    `~/.config/neon/ssh-identities.toml` (idempotent, keyed by `Host` alias).
/// 3. **docker** — prompt for registry and invoke `docker login` (docker handles credentials).
///
/// Use `--steps npm,ssh` to run only specific steps.  `--dry-run` prints what would happen
/// without making any disk changes.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::common::{dry_run_print, home_relative};
use super::docker::{run_login as docker_run_login, DockerLoginArgs};
use super::npm_token::{registry_to_key, upsert_npmrc_line};

// --- Clap argument struct ---

#[derive(clap::Args, Debug)]
pub struct SecretsArgs {
    /// Only run specific steps: npm, ssh, docker (comma-separated; default: all)
    #[arg(long, value_delimiter = ',')]
    pub steps: Vec<String>,

    /// Print what would happen without making changes
    #[arg(long)]
    pub dry_run: bool,
}

// --- Step filter ---

/// Returns `true` when the given step should run.
///
/// If `steps` is empty every step runs.  Unknown names are ignored (they may
/// come from future steps); a warning is printed for each unrecognised entry so
/// the user knows a typo won't silently drop a step.
fn should_run(step: &str, steps: &[String]) -> bool {
    if steps.is_empty() {
        return true;
    }
    steps.iter().any(|s| s.eq_ignore_ascii_case(step))
}

/// Warn about any step names that are not recognised.
fn warn_unknown_steps(steps: &[String]) {
    const KNOWN: &[&str] = &["npm", "ssh", "docker"];
    for s in steps {
        if !KNOWN.iter().any(|k| k.eq_ignore_ascii_case(s)) {
            eprintln!("warning: unknown step name '{s}' (known: npm, ssh, docker)");
        }
    }
}

// =============================================================================
// STEP 1 — NPM token
// =============================================================================

fn npmrc_path() -> Option<PathBuf> {
    home_relative(".npmrc")
}

const NPM_REGISTRY: &str = "https://registry.npmjs.org";

/// Check whether the npm auth token line is already present in `~/.npmrc`.
pub(crate) fn npmrc_has_token(content: &str) -> bool {
    let key = registry_to_key(NPM_REGISTRY);
    let prefix = format!("{key}=");
    content.lines().any(|l| l.trim_start().starts_with(&prefix))
}

fn run_npm_step(dry_run: bool) -> Result<()> {
    println!("\n=== Step 1/3: NPM token ===");

    let path = npmrc_path().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;

    // Idempotency check
    let existing = if path.exists() {
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };

    if npmrc_has_token(&existing) {
        println!(
            "  (already configured) NPM token found in {}",
            path.display()
        );
        return Ok(());
    }

    if dry_run {
        dry_run_print!(
            "prompt for NPM_TOKEN then upsert //registry.npmjs.org/:_authToken=**** in {}",
            path.display()
        );
        return Ok(());
    }

    let token = inquire::Password::new("NPM_TOKEN:")
        .without_confirmation()
        .prompt()
        .context("NPM token prompt cancelled")?;

    if token.is_empty() {
        println!("  Skipping NPM token (empty input).");
        return Ok(());
    }

    let updated = upsert_npmrc_line(&existing, NPM_REGISTRY, &token);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    std::fs::write(&path, &updated).with_context(|| format!("writing {}", path.display()))?;

    println!("  \u{2713} Written to {}", path.display());
    Ok(())
}

// =============================================================================
// STEP 2 — SSH identity map
// =============================================================================

// --- TOML model (separate from git_identity.rs's StoredIdentity to avoid schema collision) ---

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SshIdentity {
    /// The `Host` alias used in `~/.ssh/config` (e.g. `github-personal`).
    pub alias: String,
    /// GitHub / GitLab username associated with this key.
    pub username: String,
    /// Path to the SSH private key (tilde-expanded at use time).
    pub key: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct SshIdentityFile {
    #[serde(default)]
    identity: Vec<SshIdentity>,
}

// --- Path resolution ---

fn ssh_config_path() -> Option<PathBuf> {
    home_relative(".ssh/config")
}

fn ssh_identities_path() -> Option<PathBuf> {
    home_relative(".config/neon/ssh-identities.toml")
}

// --- Pure helpers (tested below) ---

/// Build a `~/.ssh/config` `Host` block for one identity.
///
/// ```text
/// Host github-personal
///     HostName github.com
///     User git
///     IdentityFile ~/.ssh/id_ed25519_personal
/// ```
pub(crate) fn format_host_entry(alias: &str, key: &str) -> String {
    format!("Host {alias}\n    HostName github.com\n    User git\n    IdentityFile {key}\n")
}

/// Return `true` if `~/.ssh/config` already contains a `Host <alias>` block.
pub(crate) fn ssh_config_has_host(content: &str, alias: &str) -> bool {
    let needle = format!("Host {alias}");
    content.lines().any(|l| l.trim() == needle.trim())
}

/// Append a host block to the ssh config content, ensuring a blank separator line.
pub(crate) fn append_ssh_host(content: &str, alias: &str, key: &str) -> String {
    let entry = format_host_entry(alias, key);
    if content.is_empty() {
        entry
    } else if content.ends_with('\n') {
        format!("{content}\n{entry}")
    } else {
        format!("{content}\n\n{entry}")
    }
}

// --- File I/O ---

fn load_ssh_identities(path: &PathBuf) -> Result<SshIdentityFile> {
    if !path.exists() {
        return Ok(SshIdentityFile::default());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: SshIdentityFile =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(parsed)
}

fn save_ssh_identities(path: &PathBuf, file: &SshIdentityFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(file).context("serialising ssh-identities.toml")?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn run_one_ssh_identity() -> Result<Option<SshIdentity>> {
    let alias = inquire::Text::new("Host alias (e.g. github-personal):")
        .prompt()
        .context("alias prompt cancelled")?;
    let alias = alias.trim().to_string();
    if alias.is_empty() {
        println!("  Skipping (empty alias).");
        return Ok(None);
    }

    let username = inquire::Text::new("GitHub username:")
        .prompt()
        .context("username prompt cancelled")?;
    let username = username.trim().to_string();

    let key = inquire::Text::new("Path to SSH private key (e.g. ~/.ssh/id_ed25519_personal):")
        .prompt()
        .context("key path prompt cancelled")?;
    let key = key.trim().to_string();

    let ssh_cfg_path =
        ssh_config_path().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;

    // Idempotency check
    let existing_ssh_cfg = if ssh_cfg_path.exists() {
        std::fs::read_to_string(&ssh_cfg_path)
            .with_context(|| format!("reading {}", ssh_cfg_path.display()))?
    } else {
        String::new()
    };

    if ssh_config_has_host(&existing_ssh_cfg, &alias) {
        println!(
            "  (already configured) Host '{alias}' found in {}",
            ssh_cfg_path.display()
        );
        return Ok(Some(SshIdentity {
            alias,
            username,
            key,
        }));
    }

    // Write to ~/.ssh/config
    if let Some(parent) = ssh_cfg_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }

    let updated = append_ssh_host(&existing_ssh_cfg, &alias, &key);
    std::fs::write(&ssh_cfg_path, &updated)
        .with_context(|| format!("writing {}", ssh_cfg_path.display()))?;

    println!(
        "  \u{2713} Appended Host '{alias}' to {}",
        ssh_cfg_path.display()
    );

    Ok(Some(SshIdentity {
        alias,
        username,
        key,
    }))
}

fn run_ssh_step(dry_run: bool) -> Result<()> {
    println!("\n=== Step 2/3: SSH identity map ===");

    if dry_run {
        let ssh_cfg =
            ssh_config_path().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
        let id_path = ssh_identities_path()
            .ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
        dry_run_print!(
            "prompt for identity count + (alias, github username, key path) per identity"
        );
        dry_run_print!(
            "append Host <alias> blocks to {} (idempotent)",
            ssh_cfg.display()
        );
        dry_run_print!(
            "write ssh-identities to {} (idempotent, keyed by alias)",
            id_path.display()
        );
        return Ok(());
    }

    let count_str = inquire::Text::new("How many git identities do you have?")
        .with_default("1")
        .prompt()
        .context("count prompt cancelled")?;

    let count: usize = count_str
        .trim()
        .parse()
        .context("expected a number for identity count")?;

    if count == 0 {
        println!("  Skipping SSH identity setup (count = 0).");
        return Ok(());
    }

    let identities_path =
        ssh_identities_path().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;

    let mut id_file = load_ssh_identities(&identities_path)?;

    for i in 1..=count {
        println!("\n  Identity {i}/{count}:");
        if let Some(identity) = run_one_ssh_identity()? {
            // Upsert into the neon identity store (keyed by alias)
            if let Some(existing) = id_file
                .identity
                .iter_mut()
                .find(|e| e.alias == identity.alias)
            {
                *existing = identity.clone();
            } else {
                id_file.identity.push(identity);
            }
        }
    }

    save_ssh_identities(&identities_path, &id_file)?;
    println!(
        "  \u{2713} SSH identities saved to {}",
        identities_path.display()
    );

    Ok(())
}

// =============================================================================
// STEP 3 — Docker login
// =============================================================================

const DOCKER_HUB_REGISTRY: &str = "docker.io";

fn run_docker_step(dry_run: bool) -> Result<()> {
    println!("\n=== Step 3/3: Docker login ===");

    if dry_run {
        dry_run_print!("prompt for registry (default: {DOCKER_HUB_REGISTRY}) + optional username");
        dry_run_print!("docker login <registry> (docker itself prompts for password)");
        return Ok(());
    }

    let registry = inquire::Text::new("Docker registry (default: docker.io):")
        .with_default(DOCKER_HUB_REGISTRY)
        .prompt()
        .context("registry prompt cancelled")?;
    let registry = registry.trim().to_string();

    let username_str = inquire::Text::new("Username (leave blank to let docker prompt):")
        .prompt()
        .context("username prompt cancelled")?;
    let username_str = username_str.trim().to_string();
    let username = if username_str.is_empty() {
        None
    } else {
        Some(username_str)
    };

    let login_args = DockerLoginArgs {
        registry,
        username,
        dry_run: false,
    };

    docker_run_login(&login_args)
}

// =============================================================================
// Public entry point
// =============================================================================

pub fn run(args: &SecretsArgs) -> Result<()> {
    warn_unknown_steps(&args.steps);

    println!("=== neon setup secrets ===");
    println!("Configuring machine credentials (NPM token, SSH identities, Docker login).");
    if args.dry_run {
        println!("[dry-run mode — no changes will be made]");
    }

    if should_run("npm", &args.steps) {
        run_npm_step(args.dry_run)?;
    }

    if should_run("ssh", &args.steps) {
        run_ssh_step(args.dry_run)?;
    }

    if should_run("docker", &args.steps) {
        run_docker_step(args.dry_run)?;
    }

    println!("\n\u{2713} Done.");
    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- should_run ---

    #[test]
    fn should_run_empty_steps_runs_all() {
        assert!(should_run("npm", &[]));
        assert!(should_run("ssh", &[]));
        assert!(should_run("docker", &[]));
    }

    #[test]
    fn should_run_filters_correctly() {
        let steps = vec!["npm".to_string(), "ssh".to_string()];
        assert!(should_run("npm", &steps));
        assert!(should_run("ssh", &steps));
        assert!(!should_run("docker", &steps));
    }

    #[test]
    fn should_run_is_case_insensitive() {
        let steps = vec!["NPM".to_string()];
        assert!(should_run("npm", &steps));
        assert!(!should_run("ssh", &steps));
    }

    // --- npmrc_has_token ---

    #[test]
    fn npmrc_has_token_detects_present() {
        let content = "//registry.npmjs.org/:_authToken=secret\nother=value\n";
        assert!(npmrc_has_token(content));
    }

    #[test]
    fn npmrc_has_token_false_when_absent() {
        let content = "other=value\nfoo=bar\n";
        assert!(!npmrc_has_token(content));
    }

    #[test]
    fn npmrc_has_token_false_for_empty() {
        assert!(!npmrc_has_token(""));
    }

    // --- format_host_entry ---

    #[test]
    fn format_host_entry_contains_alias() {
        let entry = format_host_entry("github-personal", "~/.ssh/id_ed25519_personal");
        assert!(entry.contains("Host github-personal"));
        assert!(entry.contains("HostName github.com"));
        assert!(entry.contains("User git"));
        assert!(entry.contains("IdentityFile ~/.ssh/id_ed25519_personal"));
    }

    #[test]
    fn format_host_entry_ends_with_newline() {
        let entry = format_host_entry("github-work", "~/.ssh/id_ed25519_work");
        assert!(entry.ends_with('\n'));
    }

    // --- ssh_config_has_host ---

    #[test]
    fn ssh_config_has_host_detects_exact_match() {
        let content = "Host github-personal\n    HostName github.com\n    User git\n";
        assert!(ssh_config_has_host(content, "github-personal"));
    }

    #[test]
    fn ssh_config_has_host_false_for_partial_match() {
        let content = "Host github-personal\n    HostName github.com\n";
        assert!(!ssh_config_has_host(content, "github"));
    }

    #[test]
    fn ssh_config_has_host_false_when_absent() {
        let content = "Host github-work\n    HostName github.com\n";
        assert!(!ssh_config_has_host(content, "github-personal"));
    }

    #[test]
    fn ssh_config_has_host_false_for_empty() {
        assert!(!ssh_config_has_host("", "github-personal"));
    }

    // --- append_ssh_host ---

    #[test]
    fn append_ssh_host_to_empty_content() {
        let result = append_ssh_host("", "github-personal", "~/.ssh/id_ed25519");
        assert!(result.starts_with("Host github-personal"));
        assert!(!result.starts_with('\n'));
    }

    #[test]
    fn append_ssh_host_adds_blank_separator() {
        let existing = "Host github-work\n    HostName github.com\n    User git\n    IdentityFile ~/.ssh/id_ed25519_work\n";
        let result = append_ssh_host(existing, "github-personal", "~/.ssh/id_ed25519_personal");
        // There should be a blank line between the two blocks
        assert!(result.contains("\n\nHost github-personal"));
    }

    #[test]
    fn append_ssh_host_both_blocks_present() {
        let existing = "Host github-work\n    HostName github.com\n    User git\n    IdentityFile ~/.ssh/id_ed25519_work\n";
        let result = append_ssh_host(existing, "github-personal", "~/.ssh/key");
        assert!(result.contains("Host github-work"));
        assert!(result.contains("Host github-personal"));
    }

    // --- SshIdentity TOML round-trip ---

    #[test]
    fn ssh_identity_toml_roundtrip() {
        let file = SshIdentityFile {
            identity: vec![
                SshIdentity {
                    alias: "github-personal".to_string(),
                    username: "chris".to_string(),
                    key: "~/.ssh/id_ed25519_personal".to_string(),
                },
                SshIdentity {
                    alias: "github-work".to_string(),
                    username: "chris-corp".to_string(),
                    key: "~/.ssh/id_ed25519_work".to_string(),
                },
            ],
        };

        let text = toml::to_string_pretty(&file).expect("serialize");
        let loaded: SshIdentityFile = toml::from_str(&text).expect("deserialize");

        assert_eq!(loaded.identity.len(), 2);
        assert_eq!(loaded.identity[0].alias, "github-personal");
        assert_eq!(loaded.identity[1].username, "chris-corp");
    }

    #[test]
    fn ssh_identity_file_empty_roundtrip() {
        let file = SshIdentityFile::default();
        let text = toml::to_string_pretty(&file).expect("serialize");
        let loaded: SshIdentityFile = toml::from_str(&text).expect("deserialize");
        assert!(loaded.identity.is_empty());
    }
}
