//! Emit a shell wrapper that lets `shoka cd` / `shoka tui` actually
//! change the parent shell's working directory.
//!
//! A child process can't chdir its parent on any OS shoka cares
//! about — kernel-enforced. The standard workaround is a shell
//! function that captures shoka's resolved path and runs the
//! parent's own `cd` builtin on it. The function we emit goes one
//! step further: it claims the `shoka` *name itself* (instead of
//! the older `s` alias), intercepts the `cd` / `tui` subcommands,
//! and transparently passes every other shoka subcommand through to
//! the binary via `command shoka` / `& shoka.exe`. The user sees a
//! single `shoka` they can run any subcommand against; the wrapper
//! is invisible until they touch `cd` or `tui`.
//!
//! Each subcommand has a small twist:
//!
//! - **`shoka cd`** uses an interactive picker. `inquire` 0.9 writes
//!   its prompt UI to stdout with no public switch to stderr, so the
//!   wrapper can't capture stdout (would swallow the prompt).
//!   Instead it sets [`SHOKA_CD_OUT`] to a tmp file, lets stdout
//!   flow to the terminal, and reads the resolved path back from
//!   that file.
//! - **`shoka tui`** owns stdin/stdout for the ratatui dashboard;
//!   the path emission goes through the same sidechannel so the
//!   wrapper's logic is identical.
//! - **Everything else** is a plain pass-through to the binary.
//!
//! [`SHOKA_CD_OUT`]: crate::commands::cd::CD_OUT_ENV

use crate::cli::{InitShellArgs, SupportedShell};
use crate::commands::cd::CD_OUT_ENV;

pub async fn run(args: InitShellArgs) -> anyhow::Result<()> {
    let name = &args.name;
    let script = match args.shell {
        SupportedShell::Powershell => powershell_wrapper(name),
        SupportedShell::Bash | SupportedShell::Zsh => posix_wrapper(name),
        SupportedShell::Fish => fish_wrapper(name),
    };
    print!("{script}");
    Ok(())
}

fn posix_wrapper(name: &str) -> String {
    // `command shoka` bypasses the function lookup so the same name
    // can shadow the binary without infinite recursion.
    //
    // Bare `shoka` with no arguments defaults to `tui` so the most
    // common action (drop into the dashboard) is a single keystroke.
    // `set -- tui` rewrites the positional args; the `cd|tui` case
    // arm below then routes correctly.
    //
    // The exit code is held in `rc`, not `status`: this wrapper is
    // shared with zsh, where `status` is a readonly special parameter
    // (an alias for `$?`), so `local ... status` / `status=$?` would
    // abort with `read-only variable: status` (#146). `rc` is plain
    // in both bash and zsh.
    format!(
        r#"{name}() {{
    if [ $# -eq 0 ]; then
        set -- tui
    fi
    case "$1" in
        cd|tui)
            local tmp dest rc
            tmp=$(mktemp) || return 1
            {env}="$tmp" command shoka "$@"
            rc=$?
            if [ "$rc" -eq 0 ]; then
                dest=$(cat "$tmp")
            fi
            rm -f "$tmp"
            [ "$rc" -eq 0 ] && [ -n "$dest" ] && cd -- "$dest"
            return $rc
            ;;
        *)
            command shoka "$@"
            ;;
    esac
}}
"#,
        env = CD_OUT_ENV,
    )
}

fn fish_wrapper(name: &str) -> String {
    // Bare `shoka` defaults to `tui` (see posix_wrapper comment for
    // the rationale). fish's `set argv tui` rewrites the function's
    // arg list so the `case cd tui` arm picks it up cleanly.
    format!(
        r#"function {name}
    if test (count $argv) -eq 0
        set argv tui
    end
    switch "$argv[1]"
        case cd tui
            set -l tmp (mktemp); or return 1
            set -lx {env} $tmp
            command shoka $argv
            set -l rc $status
            set -l dest ""
            if test $rc -eq 0
                set dest (cat $tmp)
            end
            rm -f $tmp
            if test $rc -eq 0; and test -n "$dest"
                cd -- $dest
            end
            return $rc
        case '*'
            command shoka $argv
    end
end
"#,
        env = CD_OUT_ENV,
    )
}

fn powershell_wrapper(name: &str) -> String {
    // PowerShell needs the binary resolved explicitly because the
    // function name shadows the executable in command lookup. We
    // cache the resolved path in a `$script:` variable so the
    // `Get-Command` lookup (50-200 ms on Windows depending on PATH
    // length) runs once per session rather than on every wrapper
    // invocation — meaningful because the wrapper is now on the hot
    // path for every shoka subcommand. The cache invalidates
    // automatically on shell restart, which is also when a
    // `cargo install --force` would land a new binary.
    //
    // `Get-Command -CommandType Application` works on both Windows
    // (`shoka.exe`) and pwsh on Linux/macOS without a separate
    // code path.
    //
    // Bare `shoka` defaults to `tui`. **Do not** build the args via
    // `$effectiveArgs = if (...) { @('tui') } else { $args }` and
    // splat it: PowerShell silently unwraps single-element arrays on
    // scalar assignment, the variable becomes the string `'tui'`,
    // and `@$str` then iterates *characters*, invoking the binary
    // as `shoka.exe t u i` (errors with "unrecognized subcommand
    // 't'"). The explicit `& $exe tui` arm below avoids the trap.
    //
    // No try/finally around the pass-through branch — there's no
    // tmp file to clean up there, and a missing executable should
    // surface as PowerShell's standard error rather than be
    // swallowed.
    format!(
        r#"function {name} {{
    if (-not $script:ShokaExe) {{
        $script:ShokaExe = (Get-Command -Name shoka -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1).Source
    }}
    if (-not $script:ShokaExe) {{
        Write-Error 'shoka binary not found on PATH'
        return
    }}
    $first = if ($args.Count -gt 0) {{ $args[0] }} else {{ 'tui' }}
    if ($first -eq 'cd' -or $first -eq 'tui') {{
        $tmp = New-TemporaryFile
        try {{
            $env:{env} = $tmp.FullName
            if ($args.Count -eq 0) {{
                & $script:ShokaExe tui
            }} else {{
                & $script:ShokaExe @args
            }}
            $code = $LASTEXITCODE
            if ($code -eq 0) {{
                $dest = Get-Content -LiteralPath $tmp.FullName -Raw
                if ($dest) {{ Set-Location -LiteralPath $dest.TrimEnd() }}
            }}
            $global:LASTEXITCODE = $code
        }} finally {{
            Remove-Item -LiteralPath $tmp.FullName -Force -ErrorAction SilentlyContinue
            Remove-Item Env:{env} -ErrorAction SilentlyContinue
        }}
    }} else {{
        & $script:ShokaExe @args
    }}
}}
"#,
        env = CD_OUT_ENV,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::SupportedShell;

    fn args(shell: SupportedShell, name: &str) -> InitShellArgs {
        InitShellArgs {
            shell,
            name: name.into(),
        }
    }

    fn rendered(shell: SupportedShell, name: &str) -> String {
        // Bypass the async run() — the wrapper string is pure, the
        // print! is just a side effect we don't want in tests.
        match shell {
            SupportedShell::Powershell => powershell_wrapper(name),
            SupportedShell::Bash | SupportedShell::Zsh => posix_wrapper(name),
            SupportedShell::Fish => fish_wrapper(name),
        }
    }

    #[test]
    fn posix_wrapper_dispatches_cd_and_tui_through_sidechannel() {
        let body = rendered(SupportedShell::Bash, "shoka");
        assert!(
            body.contains("shoka()"),
            "function name should be `shoka`: {body}"
        );
        assert!(
            body.contains("SHOKA_CD_OUT"),
            "wrapper must set the sidechannel env var: {body}"
        );
        assert!(
            body.contains("cd|tui)"),
            "wrapper must intercept both `cd` and `tui`: {body}"
        );
        assert!(
            body.contains("command shoka"),
            "wrapper must use `command shoka` to bypass the function: {body}"
        );
        // The previous wrapper version captured `shoka cd`'s stdout
        // into a variable. With the sidechannel, stdout is left
        // alone (so the prompt UI renders) — guard against the old
        // shape creeping back in.
        assert!(
            !body.contains("$(command shoka cd"),
            "wrapper must not capture stdout via command substitution: {body}"
        );
    }

    #[test]
    fn posix_wrapper_avoids_zsh_readonly_status_variable() {
        // The posix wrapper is shared with zsh, where `status` is a
        // readonly special parameter (an alias for `$?`). Declaring or
        // assigning to it aborts the function with `read-only
        // variable: status`, breaking `shoka cd` under zsh (#146).
        // Guard against the reserved name creeping back in.
        let body = rendered(SupportedShell::Zsh, "shoka");
        assert!(
            !body.contains("status"),
            "posix wrapper must not use the zsh-readonly `status` variable: {body}"
        );
        assert!(
            body.contains("local tmp dest rc"),
            "posix wrapper should hold the exit code in `rc`: {body}"
        );
    }

    #[test]
    fn posix_wrapper_passes_other_subcommands_through_to_binary() {
        // A non-cd, non-tui subcommand must reach the binary
        // unchanged so users can run e.g. `shoka clone` with the
        // function shadow still present. The pass-through arm is
        // the wildcard branch of the case statement.
        let body = rendered(SupportedShell::Bash, "shoka");
        let wildcard_pos = body
            .find("*)")
            .expect("wildcard pass-through branch must exist");
        let after = &body[wildcard_pos..];
        assert!(
            after.contains(r#"command shoka "$@""#),
            "pass-through arm must invoke the binary without sidechannel setup: {body}"
        );
    }

    #[test]
    fn fish_wrapper_dispatches_cd_and_tui_through_sidechannel() {
        let body = rendered(SupportedShell::Fish, "shoka");
        assert!(body.contains("function shoka"));
        assert!(body.contains("SHOKA_CD_OUT"));
        assert!(
            body.contains("case cd tui"),
            "fish wrapper must intercept both `cd` and `tui`: {body}"
        );
        assert!(
            body.contains("command shoka"),
            "fish wrapper must invoke the binary via `command`: {body}"
        );
        // fish wrappers also must not capture stdout via subshell.
        assert!(
            !body.contains("(command shoka cd"),
            "fish wrapper must not capture stdout via subshell: {body}"
        );
    }

    #[test]
    fn powershell_wrapper_dispatches_cd_and_tui_through_sidechannel() {
        let body = rendered(SupportedShell::Powershell, "shoka");
        assert!(body.contains("function shoka"));
        assert!(body.contains("SHOKA_CD_OUT"));
        // Verify the binary is resolved via `Get-Command`, not
        // re-invoked by name (which would recurse into the function).
        assert!(
            body.contains("Get-Command -Name shoka -CommandType Application"),
            "PowerShell wrapper must resolve the binary explicitly to avoid recursion: {body}"
        );
        // Verify the resolved path is cached in a script-scope
        // variable so the 50-200 ms `Get-Command` cost runs once
        // per session, not on every wrapper invocation.
        assert!(
            body.contains("$script:ShokaExe"),
            "PowerShell wrapper must cache the resolved binary in $script:ShokaExe: {body}"
        );
        assert!(
            body.contains("$first -eq 'cd' -or $first -eq 'tui'"),
            "PowerShell wrapper must dispatch on the first arg: {body}"
        );
        // try/finally is present so the tmp file + env var cleanup
        // happens even if shoka cd panics.
        assert!(body.contains("finally"), "wrapper missing cleanup: {body}");
        // PowerShell-specific: avoid the array-unwrap trap. The
        // bare-`shoka` branch must invoke `& $exe tui` *literally*
        // (not splat a single-element array, which PowerShell
        // silently unwraps to a string then iterates as characters).
        assert!(
            body.contains("& $script:ShokaExe tui"),
            "PowerShell wrapper must invoke tui literally to avoid the array-unwrap trap: {body}"
        );
    }

    #[test]
    fn posix_wrapper_defaults_to_tui_when_called_without_args() {
        // Bare `shoka` is the most common action users want — drop
        // into the dashboard. `set -- tui` rewrites the positional
        // args inside the function so the existing `cd|tui` case
        // arm picks it up without duplicating the sidechannel
        // teardown logic.
        let body = rendered(SupportedShell::Bash, "shoka");
        assert!(
            body.contains("if [ $# -eq 0 ]; then\n        set -- tui\n    fi"),
            "bash wrapper must default to `tui` when called without args: {body}"
        );
    }

    #[test]
    fn fish_wrapper_defaults_to_tui_when_called_without_args() {
        let body = rendered(SupportedShell::Fish, "shoka");
        assert!(
            body.contains("if test (count $argv) -eq 0\n        set argv tui\n    end"),
            "fish wrapper must default to `tui` when called without args: {body}"
        );
    }

    #[test]
    fn powershell_wrapper_defaults_to_tui_when_called_without_args() {
        // Two-part assertion: the `$first` default is `'tui'`, and
        // the actual subprocess invocation passes `tui` literally
        // (the array-unwrap-trap guard above covers the literal
        // form; this test catches the regression of dropping the
        // default-fallback altogether).
        let body = rendered(SupportedShell::Powershell, "shoka");
        assert!(
            body.contains("if ($args.Count -gt 0) { $args[0] } else { 'tui' }"),
            "PowerShell wrapper must compute first-arg default as `tui`: {body}"
        );
        assert!(
            body.contains("if ($args.Count -eq 0) {\n                & $script:ShokaExe tui"),
            "PowerShell wrapper must invoke shoka.exe tui when called without args: {body}"
        );
    }

    #[test]
    fn custom_name_substitutes_throughout() {
        // Users overriding `--name s` for a shorter alias must get
        // the same dispatch behavior at the new name, not a half-
        // renamed wrapper.
        let body = rendered(SupportedShell::Bash, "s");
        assert!(body.contains("s()"));
        assert!(body.contains("cd|tui)"));
        assert!(body.contains("command shoka"));
        assert!(
            !body.contains("shoka()"),
            "custom name must not also emit a default `shoka()` definition: {body}"
        );
    }

    #[test]
    fn run_emits_wrapper_for_each_supported_shell() {
        // Doesn't capture stdout (would need IO redirection), but
        // confirms the dispatcher actually picks a wrapper without
        // panicking for every supported shell. The wrapper body
        // itself is verified by the per-shell tests above.
        let rt = tokio::runtime::Runtime::new().unwrap();
        for shell in [
            SupportedShell::Bash,
            SupportedShell::Zsh,
            SupportedShell::Fish,
            SupportedShell::Powershell,
        ] {
            rt.block_on(run(args(shell, "shoka"))).unwrap();
        }
    }
}
