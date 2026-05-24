use crate::cli::{InitShellArgs, SupportedShell};

pub async fn run(args: InitShellArgs) -> anyhow::Result<()> {
    let name = &args.name;
    let script = match args.shell {
        SupportedShell::Powershell => format!(
            "function {name} {{\n    $dest = shoka cd @args\n    if ($LASTEXITCODE -eq 0 -and $dest) {{ Set-Location -LiteralPath $dest }}\n}}\n"
        ),
        SupportedShell::Bash | SupportedShell::Zsh => format!(
            "{name}() {{\n    local dest\n    dest=$(shoka cd \"$@\") && [ -n \"$dest\" ] && cd -- \"$dest\"\n}}\n"
        ),
        SupportedShell::Fish => format!(
            "function {name}\n    set dest (shoka cd $argv)\n    if test $status -eq 0; and test -n \"$dest\"\n        cd -- \"$dest\"\n    end\nend\n"
        ),
    };
    print!("{script}");
    Ok(())
}
