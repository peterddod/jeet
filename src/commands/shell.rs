use std::io::Write;

use anyhow::Result;

use crate::commands::completions::CompletionShell;

pub fn run_init_shell() -> Result<()> {
    let zsh_comp = crate::commands::completions::generate_script(CompletionShell::Zsh)?;
    let bash_comp = crate::commands::completions::generate_script(CompletionShell::Bash)?;

    let mut out = String::from(
        r#"# jeet shell integration — add one line to ~/.zshrc or ~/.bashrc:
#   eval "$(jeet init-shell)"
# Capture binary path at eval time (before function definition)
_JEET_BIN="$(which jeet 2>/dev/null || echo jeet)"
jeet() {
  if [[ "$1" == "cd" ]]; then
    shift
    builtin cd -- "$("$_JEET_BIN" path "$@")"
  elif [[ "$1" == "checkout" ]]; then
    shift
    _jeet_path=$("$_JEET_BIN" checkout "$@" 2>/dev/null)
    _jeet_path="${_jeet_path%%$'\n'*}"
    if [[ -d "$_jeet_path" ]]; then
      builtin cd -- "$_jeet_path"
    fi
  else
    "$_JEET_BIN" "$@"
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
  # checkout: first arg is branch, second is repo filter
  if (( CURRENT == 3 )) && [[ ${words[2]} == checkout ]]; then
    compadd -- $(command jeet complete branches 2>/dev/null)
    return
  fi
  if (( CURRENT == 4 )) && [[ ${words[2]} == checkout ]]; then
    compadd -- $(command jeet complete repos 2>/dev/null)
    return
  fi
  _clap_dynamic_completer_jeet "$@"
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
  # checkout: first arg is branch, second is repo filter
  if [[ ${COMP_WORDS[1]} == "checkout" && $COMP_CWORD -eq 2 ]]; then
    mapfile -t COMPREPLY < <(compgen -W "$(command jeet complete branches 2>/dev/null)" -- "$cur")
    return
  fi
  if [[ ${COMP_WORDS[1]} == "checkout" && $COMP_CWORD -eq 3 ]]; then
    mapfile -t COMPREPLY < <(compgen -W "$(command jeet complete repos 2>/dev/null)" -- "$cur")
    return
  fi
    _clap_complete_jeet "$@"
}
complete -o default -o nospace -F _jeet_wrapper_bash jeet
fi
"#,
    );

    std::io::stdout().write_all(out.as_bytes())?;
    Ok(())
}
