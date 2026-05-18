use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{CommandFactory, Parser, Subcommand};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use inquire::{Confirm, Select};
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct WtConfig {
    #[serde(default)]
    symlinks: Vec<String>,
}

#[derive(Deserialize, Default)]
struct GlobalConfig {
    #[serde(default)]
    repos: Vec<String>,
}

fn global_config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/wt/config.toml"))
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if p == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(p)
}

fn read_global_config() -> GlobalConfig {
    let Some(path) = global_config_path() else {
        return GlobalConfig::default();
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return GlobalConfig::default();
    };
    match toml::from_str::<GlobalConfig>(&contents) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Warning: failed to parse {}: {e}", path.display());
            GlobalConfig::default()
        }
    }
}

fn read_config(main_repo: &Path) -> WtConfig {
    let path = main_repo.join(".wt.toml");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return WtConfig::default();
    };
    match toml::from_str::<WtConfig>(&contents) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Warning: failed to parse .wt.toml: {e}");
            WtConfig::default()
        }
    }
}

fn apply_symlinks(main_repo: &Path, worktree: &Path, entries: &[String]) {
    for entry in entries {
        let src = main_repo.join(entry);
        let dest = worktree.join(entry);

        if !src.exists() {
            eprintln!("Warning: symlink source missing in main repo: {entry}");
            continue;
        }
        if dest.exists() || dest.is_symlink() {
            eprintln!("Warning: skipping symlink, destination already exists: {entry}");
            continue;
        }
        if let Some(parent) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("Warning: could not create parent for {entry}: {e}");
                continue;
            }
        }
        if let Err(e) = std::os::unix::fs::symlink(&src, &dest) {
            eprintln!("Warning: failed to symlink {entry}: {e}");
        }
    }
}

#[derive(Parser)]
#[command(name = "wt", about = "Git worktree manager", disable_help_flag = true)]
struct Cli {
    /// Branch name to switch to, create, or remove
    branch: Option<String>,

    /// Create a new worktree for the branch
    #[arg(short = 'c', long = "create", conflicts_with = "remove")]
    create: bool,

    /// Remove a worktree
    #[arg(short = 'r', long = "remove", conflicts_with = "create")]
    remove: bool,

    /// Print the shell integration function and exit
    #[arg(long = "shell-init")]
    shell_init: bool,

    /// Base branch for new worktrees (defaults to the origin's default branch)
    #[arg(short = 'b', long = "base")]
    base: Option<String>,

    /// Do not create or switch tmux sessions
    #[arg(short = 'n', long = "no-session")]
    no_session: bool,

    /// Show worktrees from all repos in ~/.config/wt/config.toml
    #[arg(short = 'g', long = "global")]
    global: bool,

    /// Print help
    #[arg(short = 'h', long = "help")]
    help: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Rename a branch, its worktree directory, and tmux session
    Rename {
        /// New branch name
        new_branch: String,
        /// Branch to rename (defaults to current branch)
        #[arg(long)]
        from: Option<String>,
    },
    /// Remove a branch, its worktree, tmux session, and remote branch
    Destroy {
        /// Branch to destroy (defaults to current branch)
        branch: Option<String>,
    },
    /// Create tmux sessions for every worktree in every configured repo
    Sessions,
}

struct Worktree {
    branch: String,
    path: PathBuf,
}

fn get_main_repo_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if msg.is_empty() {
            "Not in a git repository".to_string()
        } else {
            msg
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // The first "worktree <path>" line is always the main repo.
    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            return Ok(PathBuf::from(path));
        }
    }

    Err("Could not determine main repo root".to_string())
}

fn get_worktrees_dir(main_repo: &Path) -> PathBuf {
    let repo_name = main_repo
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    let parent = main_repo.parent().unwrap_or(Path::new("/"));
    parent.join(format!("{repo_name}-worktrees"))
}

fn branch_to_dir_name(branch: &str) -> String {
    branch.replace('/', "-")
}

fn list_worktrees(main_repo: &Path) -> Result<Vec<Worktree>, String> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(main_repo)
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let worktrees_dir = get_worktrees_dir(main_repo);
    let mut result = vec![];
    let mut first_block = true;

    // Porcelain output separates worktrees with blank lines.
    for block in stdout.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }

        if first_block {
            first_block = false;
            continue; // skip the main repo entry
        }

        let mut path: Option<PathBuf> = None;
        let mut branch: Option<String> = None;

        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            }
        }

        if let (Some(p), Some(b)) = (path, branch) {
            if p.starts_with(&worktrees_dir) {
                result.push(Worktree { branch: b, path: p });
            }
        }
    }

    Ok(result)
}

fn has_uncommitted_changes(path: &Path) -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(path)
        .output()
        .map(|out| !out.stdout.is_empty())
        .unwrap_or(false)
}

fn fetch_origin(main_repo: &Path) -> Result<(), String> {
    eprintln!("Fetching origin...");
    let out = Command::new("git")
        .args(["fetch", "origin"])
        .current_dir(main_repo)
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "Failed to fetch origin:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Returns the remote tracking branch to use as a base, e.g. `origin/main`.
/// Prefers the symbolic HEAD of origin; falls back to probing origin/main and origin/master.
fn get_default_base_branch(main_repo: &Path) -> String {
    let out = Command::new("git")
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .current_dir(main_repo)
        .output();

    if let Ok(o) = out {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            // "refs/remotes/origin/main" -> "origin/main"
            if let Some(stripped) = s.strip_prefix("refs/remotes/") {
                return stripped.to_string();
            }
        }
    }

    for candidate in ["origin/main", "origin/master"] {
        let exists = Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/remotes/{candidate}"),
            ])
            .current_dir(main_repo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if exists {
            return candidate.to_string();
        }
    }

    "origin/main".to_string()
}

fn create_worktree(main_repo: &Path, branch: &str, base: &str) -> Result<PathBuf, String> {
    let worktrees_dir = get_worktrees_dir(main_repo);
    let worktree_path = worktrees_dir.join(branch_to_dir_name(branch));

    if worktree_path.exists() {
        return Err(format!(
            "Directory already exists: {}",
            worktree_path.display()
        ));
    }

    std::fs::create_dir_all(&worktrees_dir)
        .map_err(|e| format!("Failed to create worktrees directory: {e}"))?;

    // Check if the branch already exists locally.
    let branch_exists = Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(main_repo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let output = if branch_exists {
        Command::new("git")
            .args([
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                branch,
            ])
            .current_dir(main_repo)
            .output()
    } else {
        Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                branch,
                worktree_path.to_str().unwrap(),
                base,
            ])
            .current_dir(main_repo)
            .output()
    }
    .map_err(|e| format!("Failed to run git: {e}"))?;

    if output.status.success() {
        let config = read_config(main_repo);
        apply_symlinks(main_repo, &worktree_path, &config.symlinks);
        Ok(worktree_path)
    } else {
        Err(format!(
            "Failed to create worktree:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn remove_worktree(main_repo: &Path, worktree: &Worktree, force: bool) -> Result<(), String> {
    let path_str = worktree.path.to_str().unwrap();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path_str);

    let output = Command::new("git")
        .args(&args)
        .current_dir(main_repo)
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Failed to remove worktree:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn tmux_session_name(repo_name: &str, branch: &str) -> String {
    // Replace characters that have special meaning in tmux target syntax.
    let safe = branch.replace(['/', '.', ':'], "-");
    format!("{repo_name}-{safe}")
}

fn is_in_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

fn tmux_session_exists(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create the tmux session (detached) if it doesn't exist.
/// Returns true if a new session was created, false if it already existed.
fn ensure_tmux_session(name: &str, path: &Path) -> Result<bool, String> {
    if tmux_session_exists(name) {
        return Ok(false);
    }
    let out = Command::new("tmux")
        .args(["new-session", "-d", "-s", name, "-c", path.to_str().unwrap()])
        .output()
        .map_err(|e| format!("Failed to run tmux: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "Failed to create tmux session:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(true)
}

/// Create the tmux session (detached) if it doesn't already exist, then switch to it.
/// No-ops silently if not inside tmux or if tmux is not available.
fn ensure_and_switch_tmux_session(name: &str, path: &Path) -> Result<(), String> {
    if !is_in_tmux() {
        return Ok(());
    }

    ensure_tmux_session(name, path)?;

    let out = Command::new("tmux")
        .args(["switch-client", "-t", name])
        .output()
        .map_err(|e| format!("Failed to run tmux: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "Failed to switch tmux session:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    Ok(())
}

/// Returns the name of the currently active tmux session, if inside tmux.
fn current_tmux_session() -> Option<String> {
    if !is_in_tmux() {
        return None;
    }
    let out = Command::new("tmux")
        .args(["display-message", "-p", "#S"])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Kill the tmux session for a worktree if it exists.
/// If it is the currently active session, switches to another session first
/// to avoid being detached from tmux.
fn kill_tmux_session(name: &str) {
    if !tmux_session_exists(name) {
        return;
    }

    // If we're sitting in the session we're about to kill, move away first.
    if current_tmux_session().as_deref() == Some(name) {
        // Pick the first session that isn't this one.
        let other = Command::new("tmux")
            .args(["list-sessions", "-F", "#S"])
            .output()
            .ok()
            .and_then(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .find(|s| *s != name)
                    .map(str::to_string)
            });

        if let Some(other_session) = other {
            let _ = Command::new("tmux")
                .args(["switch-client", "-t", &other_session])
                .status();
        }
    }

    let _ = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .status();
}

fn destroy_worktree(
    main_repo: &Path,
    repo_name: &str,
    branch: &str,
    no_session: bool,
) -> Result<(), String> {
    let worktree_path = get_worktrees_dir(main_repo).join(branch_to_dir_name(branch));

    // Confirm if there are uncommitted changes.
    let force = if worktree_path.exists() && has_uncommitted_changes(&worktree_path) {
        let confirmed = Confirm::new(&format!(
            "Worktree '{branch}' has uncommitted changes. Destroy anyway?"
        ))
        .with_default(false)
        .prompt()
        .unwrap_or(false);

        if !confirmed {
            return Err("Aborted.".to_string());
        }
        true
    } else {
        false
    };

    // 1. Remove the worktree directory.
    if worktree_path.exists() {
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(worktree_path.to_str().unwrap());

        let out = Command::new("git")
            .args(&args)
            .current_dir(main_repo)
            .output()
            .map_err(|e| format!("Failed to run git: {e}"))?;

        if !out.status.success() {
            return Err(format!(
                "Failed to remove worktree:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }

    // 2. Delete the local branch.
    let out = Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(main_repo)
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if !out.status.success() {
        eprintln!(
            "Warning: could not delete local branch: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    // 3. Delete the remote branch if a tracking ref exists.
    let remote_exists = Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{branch}"),
        ])
        .current_dir(main_repo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if remote_exists {
        let out = Command::new("git")
            .args(["push", "origin", "--delete", branch])
            .current_dir(main_repo)
            .output()
            .map_err(|e| format!("Failed to run git: {e}"))?;

        if !out.status.success() {
            eprintln!(
                "Warning: could not delete remote branch: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }

    // 4. Kill the tmux session.
    if !no_session {
        kill_tmux_session(&tmux_session_name(repo_name, branch));
    }

    Ok(())
}

fn get_current_branch() -> Result<String, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn rename_worktree(
    main_repo: &Path,
    repo_name: &str,
    old_branch: &str,
    new_branch: &str,
    no_session: bool,
) -> Result<(), String> {
    let worktrees_dir = get_worktrees_dir(main_repo);
    let old_path = worktrees_dir.join(branch_to_dir_name(old_branch));
    let new_path = worktrees_dir.join(branch_to_dir_name(new_branch));

    if !old_path.exists() {
        return Err(format!(
            "No worktree directory found for branch '{old_branch}'"
        ));
    }
    if new_path.exists() {
        return Err(format!("Directory already exists: {}", new_path.display()));
    }

    // 1. Rename the git branch.
    let out = Command::new("git")
        .args(["branch", "-m", old_branch, new_branch])
        .current_dir(main_repo)
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "Failed to rename branch:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    // 2. Move the worktree directory (updates git's internal tracking).
    let out = Command::new("git")
        .args([
            "worktree",
            "move",
            old_path.to_str().unwrap(),
            new_path.to_str().unwrap(),
        ])
        .current_dir(main_repo)
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if !out.status.success() {
        // Roll back the branch rename so we don't leave things half-done.
        let _ = Command::new("git")
            .args(["branch", "-m", new_branch, old_branch])
            .current_dir(main_repo)
            .status();
        return Err(format!(
            "Failed to move worktree (branch rename rolled back):\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    // 3. Rename the tmux session if it exists.
    if !no_session {
        let old_session = tmux_session_name(repo_name, old_branch);
        let new_session = tmux_session_name(repo_name, new_branch);
        if tmux_session_exists(&old_session) {
            let _ = Command::new("tmux")
                .args(["rename-session", "-t", &old_session, &new_session])
                .status();
        }
    }

    Ok(())
}

/// Scorer for inquire's Select: returns Some(score) to include+rank, None to exclude.
/// Supports fzf-style space-separated terms — all terms must match.
fn fuzzy_scorer(input: &str, _option: &String, string_value: &str, _idx: usize) -> Option<i64> {
    if input.is_empty() {
        return Some(0);
    }
    let matcher = SkimMatcherV2::default();
    // All space-separated terms must match; sum their scores.
    let mut total: i64 = 0;
    for term in input.split_whitespace() {
        let score = matcher.fuzzy_match(string_value, term)?;
        total += score;
    }
    Some(total)
}

fn die(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    std::process::exit(1);
}

fn handle_tmux_switch(repo_name: &str, branch: &str, path: &Path, no_session: bool) -> bool {
    if no_session || !is_in_tmux() {
        return false;
    }
    let session = tmux_session_name(repo_name, branch);
    let already_here = current_tmux_session().as_deref() == Some(session.as_str());
    match ensure_and_switch_tmux_session(&session, path) {
        Ok(()) => !already_here,
        Err(e) => {
            eprintln!("Warning: {e}");
            false
        }
    }
}

fn run_sessions() {
    let cfg = read_global_config();
    if cfg.repos.is_empty() {
        die("No repos configured in ~/.config/wt/config.toml");
    }

    let mut created = 0usize;
    let mut existed = 0usize;
    let mut total = 0usize;

    for repo_path_str in &cfg.repos {
        let repo_path = expand_tilde(repo_path_str);
        let repo_name = repo_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo")
            .to_string();

        let worktrees = match list_worktrees(&repo_path) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("Warning: {}: {e}", repo_path.display());
                continue;
            }
        };

        for w in worktrees {
            total += 1;
            let session = tmux_session_name(&repo_name, &w.branch);
            match ensure_tmux_session(&session, &w.path) {
                Ok(true) => {
                    eprintln!("Created: {session}");
                    created += 1;
                }
                Ok(false) => {
                    existed += 1;
                }
                Err(e) => eprintln!("Warning: {session}: {e}"),
            }
        }
    }

    eprintln!("Done. {total} worktrees, {created} created, {existed} already existed.");
}

fn run_global_mode(no_session: bool) {
    let cfg = read_global_config();
    if cfg.repos.is_empty() {
        die("Not in a git repo and no repos configured in ~/.config/wt/config.toml");
    }

    struct Entry {
        label: String,
        repo_name: String,
        branch: String,
        path: PathBuf,
    }

    let mut entries: Vec<Entry> = vec![];
    for repo_path_str in &cfg.repos {
        let repo_path = expand_tilde(repo_path_str);
        let repo_name = repo_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo")
            .to_string();

        match list_worktrees(&repo_path) {
            Ok(wts) => {
                for w in wts {
                    entries.push(Entry {
                        label: format!("{repo_name}: {}", w.branch),
                        repo_name: repo_name.clone(),
                        branch: w.branch,
                        path: w.path,
                    });
                }
            }
            Err(e) => eprintln!("Warning: {}: {e}", repo_path.display()),
        }
    }

    if entries.is_empty() {
        die("No worktrees found in any configured repo.");
    }

    let labels: Vec<String> = entries.iter().map(|e| e.label.clone()).collect();
    let selected = Select::new("Select a worktree:", labels)
        .with_scorer(&fuzzy_scorer)
        .prompt();

    match selected {
        Ok(label) => {
            if let Some(e) = entries.iter().find(|e| e.label == label) {
                if !handle_tmux_switch(&e.repo_name, &e.branch, &e.path, no_session) {
                    println!("{}", e.path.display());
                }
            }
        }
        Err(_) => std::process::exit(1),
    }
}

fn main() {
    let cli = Cli::parse();

    if cli.help {
        let mut cmd = Cli::command();
        let help = cmd.render_help();
        eprint!("{help}");
        return;
    }

    if cli.shell_init {
        print!(
            r#"# Add this function to your shell config (~/.bashrc, ~/.zshrc, etc.)
wt() {{
    local target
    target=$(command wt "$@")
    local code=$?
    if [ $code -eq 0 ] && [ -n "$target" ]; then
        if [ -d "$target" ]; then
            cd "$target"
        else
            echo "$target"
        fi
    fi
    return $code
}}
"#
        );
        return;
    }

    if cli.global {
        run_global_mode(cli.no_session);
        return;
    }

    if matches!(cli.command, Some(Commands::Sessions)) {
        run_sessions();
        return;
    }

    let main_repo = match get_main_repo_root() {
        Ok(p) => p,
        Err(e) => {
            let interactive = cli.branch.is_none()
                && !cli.create
                && !cli.remove
                && cli.command.is_none();
            if interactive {
                run_global_mode(cli.no_session);
                return;
            }
            die(&e);
        }
    };
    let repo_name = main_repo
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string();

    match cli.command {
        Some(Commands::Rename { new_branch, from }) => {
            let old_branch = match from {
                Some(b) => b,
                None => get_current_branch().unwrap_or_else(|e| die(&e)),
            };
            match rename_worktree(&main_repo, &repo_name, &old_branch, &new_branch, cli.no_session) {
                Ok(()) => eprintln!("Renamed '{old_branch}' to '{new_branch}'."),
                Err(e) => die(&e),
            }
            return;
        }
        Some(Commands::Destroy { branch }) => {
            let branch = match branch {
                Some(b) => b,
                None => get_current_branch().unwrap_or_else(|e| die(&e)),
            };
            match destroy_worktree(&main_repo, &repo_name, &branch, cli.no_session) {
                Ok(()) => eprintln!("Destroyed '{branch}'."),
                Err(e) => die(&e),
            }
            return;
        }
        Some(Commands::Sessions) => unreachable!("handled before main_repo lookup"),
        None => {}
    }

    // Fetch origin and resolve the base branch for new worktree creation.
    let resolve_base = || -> String {
        fetch_origin(&main_repo).unwrap_or_else(|e| eprintln!("Warning: {e}"));
        cli.base
            .clone()
            .unwrap_or_else(|| get_default_base_branch(&main_repo))
    };

    // Helper: handle tmux after resolving a worktree path.
    // Returns true if we switched to a different tmux session (so the caller should skip cd).
    let handle_tmux = |branch: &str, path: &Path| -> bool {
        if cli.no_session || !is_in_tmux() {
            return false;
        }
        let session = tmux_session_name(&repo_name, branch);
        let already_here = current_tmux_session().as_deref() == Some(session.as_str());
        match ensure_and_switch_tmux_session(&session, path) {
            Ok(()) => !already_here,
            Err(e) => {
                eprintln!("Warning: {e}");
                false
            }
        }
    };

    if cli.create {
        let branch = cli
            .branch
            .as_deref()
            .unwrap_or_else(|| die("branch name is required with --create"));

        let base = resolve_base();
        match create_worktree(&main_repo, branch, &base) {
            Ok(path) => {
                if !handle_tmux(branch, &path) {
                    println!("{}", path.display());
                }
            }
            Err(e) => die(&e),
        }
    } else if cli.remove {
        let branch = cli
            .branch
            .as_deref()
            .unwrap_or_else(|| die("branch name is required with --remove"));

        let worktrees = list_worktrees(&main_repo).unwrap_or_else(|e| die(&e));
        let wt = worktrees
            .iter()
            .find(|w| w.branch == branch)
            .unwrap_or_else(|| die(&format!("no worktree found for branch '{branch}'")));

        let force = if has_uncommitted_changes(&wt.path) {
            let confirmed = Confirm::new(&format!(
                "Worktree '{branch}' has uncommitted changes. Remove anyway?"
            ))
            .with_default(false)
            .prompt()
            .unwrap_or(false);

            if !confirmed {
                eprintln!("Aborted.");
                std::process::exit(1);
            }
            true
        } else {
            false
        };

        match remove_worktree(&main_repo, wt, force) {
            Ok(()) => {
                if !cli.no_session {
                    kill_tmux_session(&tmux_session_name(&repo_name, branch));
                }
                eprintln!("Removed worktree for branch '{branch}'.");
            }
            Err(e) => die(&e),
        }
    } else if let Some(branch) = &cli.branch {
        // Switch to the worktree for the given branch.
        let worktrees = list_worktrees(&main_repo).unwrap_or_else(|e| die(&e));

        match worktrees.iter().find(|w| &w.branch == branch) {
            Some(wt) => {
                if !handle_tmux(branch, &wt.path) {
                    println!("{}", wt.path.display());
                }
            }
            None => {
                eprintln!("No worktree found for branch '{branch}'.");
                let confirmed = Confirm::new("Create a new worktree for this branch?")
                    .with_default(true)
                    .prompt()
                    .unwrap_or(false);

                if !confirmed {
                    std::process::exit(1);
                }

                let base = resolve_base();
                match create_worktree(&main_repo, branch, &base) {
                    Ok(path) => {
                        if !handle_tmux(branch, &path) {
                            println!("{}", path.display());
                        }
                    }
                    Err(e) => die(&e),
                }
            }
        }
    } else {
        // No args: interactive list with fuzzy search.
        let worktrees = list_worktrees(&main_repo).unwrap_or_else(|e| die(&e));

        if worktrees.is_empty() {
            die("No worktrees found. Use -c <branch> to create one.");
        }

        let options: Vec<String> = worktrees.iter().map(|w| w.branch.clone()).collect();

        let selected = Select::new("Select a worktree:", options)
            .with_scorer(&fuzzy_scorer)
            .prompt();

        match selected {
            Ok(branch) => {
                if let Some(wt) = worktrees.iter().find(|w| w.branch == branch) {
                    if !handle_tmux(&branch, &wt.path) {
                        println!("{}", wt.path.display());
                    }
                }
            }
            Err(_) => std::process::exit(1),
        }
    }
}
