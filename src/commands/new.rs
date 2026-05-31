//! `shoka new` — scaffold a brand-new repo end to end.
//!
//! Pipeline:
//!
//! 1. Resolve `owner/name` — from the CLI spec (`owner/name` or bare
//!    `name`), or an interactive name prompt. A bare name / prompt
//!    defaults the owner to `[global.ui].own_owners[0]` (the same
//!    "self" `shoka clone` already uses), falling back to the
//!    authenticated gh login.
//! 2. `gh repo create <owner>/<name> --public|--private
//!    [--description …] --add-readme` — the roadmap's chosen create
//!    path. It reuses the user's existing `gh auth`, and `--add-readme`
//!    seeds an initial commit + default branch so the clone in step 3
//!    is deterministic (an empty repo has no HEAD for gix / jj to
//!    check out).
//! 3. Delegate to [`clone::clone_and_record`] so the
//!    path-layout / routing / shelf behaviour is byte-for-byte what
//!    `shoka clone` does — `new` is "clone, but I make the remote
//!    first".
//! 4. `kata init <preset> --at <dest>` when a preset resolves
//!    (`--preset` overrides `[global.new].preset`) and `--no-kata`
//!    isn't set. Best-effort: a kata failure warns but does NOT fail
//!    the command — the repo already exists and is cloned, so the user
//!    can re-run kata by hand rather than being left with a
//!    half-created repo and a non-zero exit.

use std::path::Path;

use anyhow::{Context, Result, bail};
use inquire::Text;
use owo_colors::OwoColorize;

use crate::cli::NewArgs;
use crate::commands::ShokaContext;
use crate::commands::clone::clone_and_record;
use crate::config::ShokaConfig;
use crate::gh;

pub async fn run(ctx: &ShokaContext, args: NewArgs) -> Result<()> {
    let cfg = ShokaConfig::load(&ctx.paths)?.resolve(ctx.profile_override.as_deref())?;

    let (owner, name) = resolve_owner_name(args.spec.as_deref(), &cfg.ui.own_owners).await?;
    let slug = format!("{owner}/{name}");

    // 1. Create the remote first — if this fails (name taken, no auth,
    //    no `gh`), we bail before touching the filesystem.
    gh_repo_create(&slug, args.private, args.description.as_deref()).await?;

    // 2. Clone + record through the shared core so `new` and `clone`
    //    can never drift on where things land or how they're shelved.
    let repo = clone_and_record(ctx, &cfg, &slug).await?;
    let dest = cfg.clone_path_for_one(&repo)?;

    // 3. kata scaffolding — `--preset` wins over the configured
    //    default; `--no-kata` opts out entirely.
    let preset = if args.no_kata {
        None
    } else {
        // `args` is owned and unused past here, so move `preset` out
        // rather than cloning it.
        args.preset.or_else(|| cfg.raw.global.new.preset.clone())
    };
    match preset {
        Some(p) => {
            // Best-effort: the repo is already created + cloned, so a
            // kata stumble is a warning, not a command failure.
            if let Err(e) = kata_init(&p, &dest).await {
                eprintln!(
                    "{} kata init failed ({e:#}) — repo is created and cloned; \
                     re-run `kata init {p} --at {}` by hand",
                    "new:".bold(),
                    dest.display()
                );
            }
        }
        None => {
            println!(
                "{} no kata preset (set [global.new].preset or pass --preset) — \
                 skipped scaffolding",
                "new:".bold()
            );
        }
    }

    println!("{} created {} at {}", "new:".bold(), slug, dest.display());
    Ok(())
}

/// Owner/name split of a `shoka new` spec.
#[derive(Debug, PartialEq, Eq)]
enum Spec {
    /// `owner/name` — owner pinned by the user.
    Full { owner: String, name: String },
    /// Bare `name` — owner is defaulted by the caller.
    NameOnly { name: String },
}

/// Parse a spec string into [`Spec`]. A single `/` splits owner from
/// name; anything else is a bare name. Both segments are validated so
/// a typo like `owner/` or `a/b/c` fails here with a clear message
/// rather than surfacing later as a confusing `gh` / clone error.
fn parse_spec(spec: &str) -> Result<Spec> {
    match spec.split_once('/') {
        Some((owner, name)) => {
            let owner = owner.trim();
            let name = name.trim();
            validate_segment(owner, "owner")?;
            validate_segment(name, "name")?;
            Ok(Spec::Full {
                owner: owner.to_string(),
                name: name.to_string(),
            })
        }
        None => {
            let name = spec.trim();
            validate_segment(name, "name")?;
            Ok(Spec::NameOnly {
                name: name.to_string(),
            })
        }
    }
}

/// Reject empty segments and ones carrying a slash or whitespace —
/// `gh repo create` and the clone-path layout both assume a single
/// clean path component.
fn validate_segment(seg: &str, what: &str) -> Result<()> {
    if seg.is_empty() {
        bail!("{what} must not be empty");
    }
    if seg.contains('/') || seg.chars().any(char::is_whitespace) {
        bail!("{what} `{seg}` has an invalid character (no slashes or spaces)");
    }
    Ok(())
}

/// Resolve `(owner, name)` from the optional CLI spec, prompting for
/// the name when no spec is given.
async fn resolve_owner_name(spec: Option<&str>, own_owners: &[String]) -> Result<(String, String)> {
    match spec {
        Some(s) => match parse_spec(s)? {
            Spec::Full { owner, name } => Ok((owner, name)),
            Spec::NameOnly { name } => Ok((default_owner(own_owners).await?, name)),
        },
        None => {
            let owner = default_owner(own_owners).await?;
            let help = format!("created under {owner}/");
            let name = Text::new("new repo name:")
                .with_help_message(&help)
                .prompt()
                .context("repo name prompt cancelled")?;
            let name = name.trim();
            validate_segment(name, "name")?;
            Ok((owner, name.to_string()))
        }
    }
}

/// Default owner when the spec carries none: first `own_owners` entry
/// (already declared as "self" for the TUI / clone), else the
/// authenticated gh login. Errors when neither is available so the
/// repo is never created under a surprising owner.
async fn default_owner(own_owners: &[String]) -> Result<String> {
    if let Some(first) = own_owners.first() {
        return Ok(first.clone());
    }
    // A resolved token means the user expects gh auth to fill the
    // owner in — so a client-build / whoami failure (expired token,
    // network) is worth surfacing with context rather than silently
    // collapsing into the generic "can't determine the owner" below.
    // The whoami context still points at the `owner/name` escape hatch.
    if let Some(token) = gh::resolve_token().await {
        let client = gh::build_client(&token)
            .context("building a GitHub client to resolve the default owner")?;
        let login = gh::whoami(&client).await.context(
            "looking up your GitHub login to default the owner — \
             pass `owner/name` explicitly to skip this lookup",
        )?;
        return Ok(login);
    }
    bail!(
        "can't determine the owner — pass `owner/name` explicitly or set \
         [global.ui].own_owners in config"
    )
}

/// `gh repo create <slug> --public|--private [--description …]
/// --add-readme`. stdout / stderr inherit the terminal so gh's own
/// "✓ Created repository …" line shows through.
async fn gh_repo_create(slug: &str, private: bool, description: Option<&str>) -> Result<()> {
    let gh = which::which("gh").context("`gh` not found on PATH (required for `shoka new`)")?;
    let mut cmd = tokio::process::Command::new(&gh);
    cmd.arg("repo").arg("create").arg(slug);
    // gh requires an explicit visibility; supply it from the flag.
    cmd.arg(if private { "--private" } else { "--public" });
    // Seed an initial commit so the subsequent clone has a default
    // branch — see module docs.
    cmd.arg("--add-readme");
    if let Some(d) = description {
        cmd.arg("--description").arg(d);
    }
    // Consistent with the other subprocess spawns; a no-op for this
    // foreground call (the parent has a console) — see
    // `crate::silent_creation_flags`.
    #[cfg(windows)]
    cmd.creation_flags(crate::silent_creation_flags());
    let status = cmd
        .status()
        .await
        .with_context(|| format!("spawning `{} repo create`", gh.display()))?;
    if !status.success() {
        bail!("`gh repo create {slug}` exited with status {status}");
    }
    Ok(())
}

/// `kata init <preset> --at <dest>`. Inherits stdio so kata's
/// interactive scaffolding dialog (and any AI handoff) works. No
/// Windows console-suppression flag here: this is an interactive
/// foreground tool and must stay fully attached to the terminal.
async fn kata_init(preset: &str, dest: &Path) -> Result<()> {
    let kata =
        which::which("kata").context("`kata` not found on PATH (skip kata with --no-kata)")?;
    println!(
        "{} kata init {} (at {})",
        "new:".bold(),
        preset,
        dest.display()
    );
    let status = tokio::process::Command::new(&kata)
        .arg("init")
        .arg(preset)
        .arg("--at")
        .arg(dest)
        .status()
        .await
        .with_context(|| format!("spawning `{} init`", kata.display()))?;
    if !status.success() {
        bail!("`kata init {preset}` exited with status {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spec_splits_owner_and_name() {
        assert_eq!(
            parse_spec("yukimemi/shoka").unwrap(),
            Spec::Full {
                owner: "yukimemi".into(),
                name: "shoka".into()
            }
        );
    }

    #[test]
    fn parse_spec_bare_name_defers_owner() {
        assert_eq!(
            parse_spec("shoka").unwrap(),
            Spec::NameOnly {
                name: "shoka".into()
            }
        );
    }

    #[test]
    fn parse_spec_trims_surrounding_whitespace() {
        assert_eq!(
            parse_spec("  yukimemi / shoka ").unwrap(),
            Spec::Full {
                owner: "yukimemi".into(),
                name: "shoka".into()
            }
        );
    }

    #[test]
    fn parse_spec_rejects_empty_segments() {
        // Trailing slash → empty name.
        assert!(parse_spec("owner/").is_err());
        // Leading slash → empty owner.
        assert!(parse_spec("/name").is_err());
        // Bare empty / whitespace-only.
        assert!(parse_spec("   ").is_err());
    }

    #[test]
    fn parse_spec_rejects_host_owner_name_triple() {
        // `shoka new` only creates on the gh default host, so a
        // three-segment spec is a user error, not a host override.
        let err = parse_spec("github.com/yukimemi/shoka").unwrap_err();
        assert!(
            err.to_string().contains("invalid character"),
            "expected invalid-character error, got: {err}"
        );
    }

    #[test]
    fn validate_segment_flags_spaces() {
        assert!(validate_segment("my repo", "name").is_err());
        assert!(validate_segment("ok-name_1.2", "name").is_ok());
    }
}
