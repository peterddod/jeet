use std::io::Write;

use anyhow::Result;

use crate::commands::completions::CompletionShell;

pub fn run_init_shell() -> Result<()> {
    let zsh_comp = crate::commands::completions::generate_script(CompletionShell::Zsh)?;
    let bash_comp = crate::commands::completions::generate_script(CompletionShell::Bash)?;

    let mut out = String::from(
        r#"# jeet shell integration — add one line to ~/.zshrc or ~/.bashrc:
#   eval "$(jeet init-shell)"
jeet() {
  if [[ "$1" == "cd" ]]; then
    shift
    builtin cd -- "$(command jeet path "$@")"
  else
    command jeet "$@"
  fi
}

"#,
    );

    out.push_str("# zsh: wrapper `jeet cd` + subcommand completion\n");
    out.push_str("if [ -n \"${ZSH_VERSION:-}\" ]; then\n");
    out.push_str(&zsh_comp);
    out.push_str(
        r#"_jeet_wrapper() {
  if (( CURRENT == 3 )) && [[ ${words[2]} == cd ]]; then
    compadd -- $(command jeet complete repos 2>/dev/null)
    return
  fi
  if (( CURRENT == 5 )) && [[ ${words[2]} == cd && ${words[4]} == --branch ]]; then
    compadd -- $(command jeet complete branches "${words[3]}" 2>/dev/null)
    return
  fi
  _jeet "$@"
}
compdef _jeet_wrapper jeet
fi

"#,
    );

    out.push_str("# bash: wrapper `jeet cd` + subcommand completion\n");
    out.push_str("if [ -n \"${BASH_VERSION:-}\" ]; then\n");
    out.push_str(&bash_comp);
    out.push_str(
        r#"_jeet_wrapper_bash() {
  local cur
  cur="${COMP_WORDS[COMP_CWORD]}"
  if [[ ${COMP_WORDS[1]} == "cd" && $COMP_CWORD -eq 2 ]]; then
    mapfile -t COMPREPLY < <(compgen -W "$(command jeet complete repos 2>/dev/null)" -- "$cur")
    return
  fi
  if [[ ${COMP_WORDS[1]} == "cd" && $COMP_CWORD -eq 4 && ${COMP_WORDS[COMP_CWORD-1]} == --branch ]]; then
    mapfile -t COMPREPLY < <(compgen -W "$(command jeet complete branches "${COMP_WORDS[2]}" 2>/dev/null)" -- "$cur")
    return
  fi
  _jeet "$@"
}
complete -o default -o nospace -F _jeet_wrapper_bash jeet
fi
"#,
    );

    std::io::stdout().write_all(out.as_bytes())?;
    Ok(())
}
