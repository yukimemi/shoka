//! Emit a shell wrapper that turns `shoka cd` into an actual cd.
//!
//! The wrapper has to work around a constraint of `shoka cd`'s
//! interactive picker: `inquire` 0.9 writes its prompt UI to stdout
//! and exposes no public switch to stderr. If the shell captured
//! stdout naively (`dest=$(shoka cd ...)`), the prompt would be
//! swallowed into the variable and the user would see nothing.
//!
//! Each wrapper therefore:
//!
//! 1. Creates a tmp file and sets [`SHOKA_CD_OUT`] to point at it.
//!    `shoka cd` writes the resolved path to that file (see
//!    [`crate::commands::cd::emit_path`]).
//! 2. Lets `shoka cd`'s stdout flow to the user's terminal (so the
//!    `inquire` prompt UI renders normally) and reads the path back
//!    from the tmp file.
//! 3. Cleans up the tmp file and does the actual `cd` if a non-empty
//!    path came back.
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
    format!(
        r#"{name}() {{
    local tmp dest status
    tmp=$(mktemp) || return 1
    {env}="$tmp" shoka cd "$@"
    status=$?
    if [ "$status" -eq 0 ]; then
        dest=$(cat "$tmp")
    fi
    rm -f "$tmp"
    [ "$status" -eq 0 ] && [ -n "$dest" ] && cd -- "$dest"
    return $status
}}
"#,
        env = CD_OUT_ENV,
    )
}

fn fish_wrapper(name: &str) -> String {
    format!(
        r#"function {name}
    set -l tmp (mktemp); or return 1
    {env}=$tmp shoka cd $argv
    set -l status $status
    set -l dest ""
    if test $status -eq 0
        set dest (cat $tmp)
    end
    rm -f $tmp
    if test $status -eq 0; and test -n "$dest"
        cd -- $dest
    end
    return $status
end
"#,
        env = CD_OUT_ENV,
    )
}

fn powershell_wrapper(name: &str) -> String {
    // `New-TemporaryFile` returns a `FileInfo`; `.FullName` is the
    // path. We deliberately don't use try/finally with `throw` so a
    // missing tmp file (rare) doesn't mask the underlying `shoka cd`
    // exit code.
    format!(
        r#"function {name} {{
    $tmp = New-TemporaryFile
    try {{
        $env:{env} = $tmp.FullName
        shoka cd @args
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
    fn posix_wrapper_uses_sidechannel_env_var() {
        let body = rendered(SupportedShell::Bash, "s");
        assert!(body.contains("s()"), "function name should be `s`: {body}");
        assert!(
            body.contains("SHOKA_CD_OUT"),
            "wrapper must set the sidechannel env var: {body}"
        );
        // The previous wrapper version captured `shoka cd`'s stdout
        // into a variable. With the sidechannel, stdout is left alone
        // (so the prompt UI renders) — guard against the old shape
        // creeping back in.
        assert!(
            !body.contains("$(shoka cd"),
            "wrapper must not capture stdout via command substitution: {body}"
        );
    }

    #[test]
    fn fish_wrapper_uses_sidechannel_env_var() {
        let body = rendered(SupportedShell::Fish, "s");
        assert!(body.contains("function s"));
        assert!(body.contains("SHOKA_CD_OUT"));
        // fish wrappers also must not capture stdout via subshell.
        assert!(
            !body.contains("(shoka cd"),
            "fish wrapper must not capture stdout via subshell: {body}"
        );
    }

    #[test]
    fn powershell_wrapper_uses_sidechannel_env_var() {
        let body = rendered(SupportedShell::Powershell, "s");
        assert!(body.contains("function s"));
        assert!(body.contains("SHOKA_CD_OUT"));
        // Verify try/finally is present so the tmp file + env var
        // cleanup happens even if shoka cd panics.
        assert!(body.contains("finally"), "wrapper missing cleanup: {body}");
    }

    #[test]
    fn run_emits_wrapper_for_each_supported_shell() {
        // Doesn't capture stdout (would need IO redirection), but
        // confirms the dispatcher actually picks a wrapper without
        // panicking for every supported shell. The wrapper body itself
        // is verified by the per-shell tests above.
        let rt = tokio::runtime::Runtime::new().unwrap();
        for shell in [
            SupportedShell::Bash,
            SupportedShell::Zsh,
            SupportedShell::Fish,
            SupportedShell::Powershell,
        ] {
            rt.block_on(run(args(shell, "s"))).unwrap();
        }
    }
}
