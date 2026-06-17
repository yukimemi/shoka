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

    /// Scaffold a new repo: `gh repo create` + clone + optional kata init.
    New(NewArgs),

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

    /// Remove a repo: delete its working tree and drop it from the shelf
    /// (no arg ⇒ fuzzy-select).
    Rm(RmArgs),

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

    /// Update the shoka binary itself to the latest GitHub release.
    SelfUpdate(SelfUpdateArgs),
}

#[derive(Debug, Args)]
pub struct SelfUpdateArgs {
    /// Skip the confirmation prompt and install immediately.
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// Print availability and exit without installing — useful in
    /// scripts that want to surface "an upgrade is available" without
    /// performing it.
    #[arg(long)]
    pub check: bool,
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
        /// Internal flag — set by the dispatcher when spawning the
        /// detached background refresh. Suppresses the pretty
        /// summary output and downgrades errors to log messages so
        /// nothing leaks back to the parent process.
        #[arg(long, hide = true)]
        background: bool,
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
pub struct NewArgs {
    /// New repo as `owner/name` or just `name` (owner defaults to
    /// `[global.ui].own_owners[0]`, else your gh login). Omitted ⇒
    /// prompt for the name interactively.
    pub spec: Option<String>,

    /// Create the GitHub repo private. Default is public (this repo's
    /// `shoka new` is OSS-first); `gh repo create` itself requires an
    /// explicit visibility, which shoka supplies from this flag.
    #[arg(long)]
    pub private: bool,

    /// Repository description passed to `gh repo create`.
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,

    /// kata preset spec to scaffold with, overriding
    /// `[global.new].preset` for this run (e.g.
    /// `github.com/yukimemi/pj-presets:rust-cli`). Omitted ⇒ use the
    /// configured preset, or skip kata when neither is set.
    #[arg(long, value_name = "SPEC")]
    pub preset: Option<String>,

    /// Skip the kata scaffolding step even if a preset is configured.
    /// Use when you just want the create + clone and will scaffold by
    /// hand later.
    #[arg(long)]
    pub no_kata: bool,
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

    /// Skip the interactive confirmation. Implies you have already
    /// reviewed the candidate list (`--dry-run` first is a good
    /// habit). No-op when combined with `--dry-run`.
    #[arg(long, short)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// Repository hint (URL or owner/name fragment). Omitted ⇒ fuzzy-select.
    pub repo: Option<String>,

    /// Restrict fuzzy candidates by tag (repeatable; AND semantics).
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Don't remove anything; just print what would be removed.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the interactive confirmation. Since `rm` deletes a working
    /// tree, the prompt defaults to "no" — pass this only when you're
    /// sure (a `--dry-run` first is a good habit). A repo with
    /// uncommitted changes still refuses under `--yes` unless `--force`
    /// is also given, so a scripted `rm -y` can't silently lose work.
    #[arg(long, short)]
    pub yes: bool,

    /// Delete even when the working tree has uncommitted git / jj
    /// changes. Bypasses the dirty-tree safety check.
    #[arg(long, short)]
    pub force: bool,

    /// Drop the shelf entry but leave the working tree on disk. The
    /// shelf-only "forget this repo" mode — handy when you've already
    /// moved the clone elsewhere by hand.
    #[arg(long)]
    pub keep_files: bool,
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

    /// Wrapper function name (default: `shoka`). The function
    /// intercepts `cd` / `tui` to chdir the parent shell and
    /// transparently passes every other subcommand through to the
    /// `shoka` binary, so the same name covers both worlds. Override
    /// to `s` (or anything shorter) if you want a separate alias.
    #[arg(long, default_value = "shoka")]
    pub name: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SupportedShell {
    Powershell,
    Bash,
    Zsh,
    Fish,
}
