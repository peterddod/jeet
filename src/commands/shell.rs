use std::io::Write;

use anyhow::Result;

pub fn run_init_shell() -> Result<()> {
    let snippet = r#"# jeet shell integration — add to ~/.zshrc or ~/.bashrc
jeet() {
  if [[ "$1" == "cd" ]]; then
    shift
    builtin cd -- "$(command jeet path "$@")"
  else
    command jeet "$@"
  fi
}

# Tab completion (zsh) — includes wrapper `jeet cd`
if [ -n "${ZSH_VERSION:-}" ]; then
  source <(command jeet completions zsh)
  _jeet_wrapper() {
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

# Tab completion (bash) — wrapper `jeet cd` + binary subcommands
if [ -n "${BASH_VERSION:-}" ]; then
  source <(command jeet completions bash)
  _jeet_wrapper_bash() {
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
"#;
    std::io::stdout().write_all(snippet.as_bytes())?;
    Ok(())
}
