use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "shoka",
    about = "shoka — your repository bookshelf",
    long_about = "Repository workspace manager: jj-aware successor to ghq / rhq with a TUI dashboard.",
    version,
    propagate_version = true
)]
pub struct Cli {
    /// Active profile (overrides $SHOKA_PROFILE and config default_profile).
    #[arg(long, global = true, env = "SHOKA_PROFILE")]
    pub profile: Option<String>,

    /// Path to config.toml (defaults to XDG config dir).
    #[arg(long, global = true, env = "SHOKA_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Subcommand. Omitted ⇒ launches the TUI dashboard.
    #[command(subcommand)]
    pub cmd: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Clone a repository (no arg ⇒ fuzzy-select from your gh repos).
    Clone(CloneArgs),

    /// List repos on the shelf.
    List(ListArgs),

    /// cd into a repo (no arg ⇒ fuzzy-select).
    ///
    /// Emits the chosen path to stdout. A shell wrapper installed via
    /// `shoka init-shell <shell>` consumes that to perform the cd.
    Cd(CdArgs),

    /// Run a command across the shelf in parallel.
    Exec(ExecArgs),

    /// Propose stale / merged repo candidates for cleanup.
    Prune(PruneArgs),

    /// Adopt an existing ghq tree (or any directory hierarchy in
    /// `<root>/<host>/<owner>/<name>` shape).
    Import(ImportArgs),

    /// Export the shelf state.toml (portable ledger).
    Export(ExportArgs),

    /// Tag management.
    #[command(subcommand)]
    Tag(TagCommand),

    /// Set per-repo metadata (vcs override etc.).
    Set(SetArgs),

    /// Set or read a per-repo note.
    Note(NoteArgs),

    /// Diagnose environment (gh, git, jj presence; config validity).
    /// The first command run on a fresh machine also auto-creates a
    /// starter `config.toml` at the resolved config path.
    Doctor,

    /// Launch the TUI dashboard explicitly.
    Tui(TuiArgs),

    /// Print shell completion script.
    Completion(CompletionArgs),

    /// Print the shell integration wrapper for `cd` (PowerShell / bash / zsh / fish).
    InitShell(InitShellArgs),

    /// Per-repo volatile cache management (refresh / show / clear).
    #[command(subcommand)]
    Cache(CacheCommand),
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Refresh cache entries for the shelf. Skips repos whose
    /// `last_refreshed` is within `[global.cache].refresh_threshold_secs`
    /// of now, unless `--force` is passed.
    Refresh {
        /// Bypass the staleness threshold; refresh every entry.
        #[arg(long)]
        force: bool,
        /// Restrict to repos carrying every listed tag (AND).
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
    },
    /// Dump the raw cache.toml to stdout.
    Show,
    /// Drop every cache entry. Equivalent to `rm cache.toml`, but
    /// goes through the atomic-save path so we don't leave half-
    /// written state behind.
    Clear,
}

#[derive(Debug, Args)]
pub struct CloneArgs {
    /// Repository URL or `owner/name`. Omitted ⇒ fuzzy-select.
    pub url: Option<String>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Filter by tag (repeatable; AND semantics).
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Show only repos that carry an AGENTS.md.
    #[arg(long)]
    pub has_agents: bool,
}

#[derive(Debug, Args)]
pub struct CdArgs {
    /// Repository hint (URL or owner/name fragment). Omitted ⇒ fuzzy-select.
    pub repo: Option<String>,

    /// Restrict fuzzy candidates by tag.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// Restrict by tag.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Status filter (e.g. `dirty`, `behind`, `ahead`).
    #[arg(long)]
    pub filter: Option<String>,

    /// Command + args (everything after `--`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

#[derive(Debug, Args)]
pub struct PruneArgs {
    /// Don't actually remove anything; just print candidates.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Source directory (e.g. `~/ghq`). Omitted ⇒ pick from common candidates.
    pub path: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Output path. `-` writes to stdout (default).
    #[arg(long, value_name = "PATH")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum TagCommand {
    /// Add tags to a repo (no arg ⇒ fuzzy-select repo + interactive tag prompt).
    Add {
        /// Repository identifier (owner/name). Omitted ⇒ fuzzy-select.
        repo: Option<String>,
        /// Tags to add (one or more). Omitted ⇒ interactive prompt.
        tags: Vec<String>,
    },
    /// Remove tags from a repo (no arg ⇒ fuzzy-select repo + interactive tag prompt).
    Rm {
        /// Repository identifier (owner/name). Omitted ⇒ fuzzy-select.
        repo: Option<String>,
        /// Tags to remove (one or more). Omitted ⇒ interactive prompt.
        tags: Vec<String>,
    },
    /// List tags carried by a repo (no arg ⇒ fuzzy-select repo).
    Ls { repo: Option<String> },
    /// List repos that carry a tag (no arg ⇒ fuzzy-select tag).
    Who { tag: Option<String> },
}

#[derive(Debug, Args)]
pub struct SetArgs {
    /// Repository identifier (no arg ⇒ fuzzy-select).
    pub repo: Option<String>,

    /// Force VCS for this repo (overrides auto-detection).
    #[arg(long, value_name = "VCS")]
    pub vcs: Option<VcsKind>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum VcsKind {
    Auto,
    Git,
    Jj,
}

#[derive(Debug, Args)]
pub struct NoteArgs {
    /// Repository identifier (no arg ⇒ fuzzy-select).
    pub repo: Option<String>,
    /// Note text. Omitted with `--clear` clears the note; omitted otherwise prints current.
    pub text: Option<String>,
    /// Clear the note instead of setting it.
    #[arg(long, conflicts_with = "text")]
    pub clear: bool,
}

#[derive(Debug, Args)]
pub struct TuiArgs {
    /// Pre-apply tag filter on launch.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,
}

#[derive(Debug, Args)]
pub struct CompletionArgs {
    /// Target shell.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

#[derive(Debug, Args)]
pub struct InitShellArgs {
    /// Shell to emit a `cd` wrapper for.
    #[arg(value_enum)]
    pub shell: SupportedShell,

    /// Wrapper function name (default: `s`).
    #[arg(long, default_value = "s")]
    pub name: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SupportedShell {
    Powershell,
    Bash,
    Zsh,
    Fish,
}
