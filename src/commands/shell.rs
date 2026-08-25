use std::io::Write;

use anyhow::Result;

use crate::commands::completions::CompletionShell;

pub fn run_init_shell() -> Result<()> {
    let zsh_comp = crate::commands::completions::generate_script(CompletionShell::Zsh)?;
    let bash_comp = crate::commands::completions::generate_script(CompletionShell::Bash)?;

    let mut out = String::from(
        r#"# jeet shell integration — add one line to ~/.zshrc or ~/.bashrc:
#   eval "$(jeet init-shell)"
# Resolve the binary, not the function: zsh's `which` prints function bodies and
# a bare-word fallback would make the wrapper call itself.
if [ -n "${ZSH_VERSION:-}" ]; then
  _JEET_BIN="$(whence -p jeet 2>/dev/null)"
else
  _JEET_BIN="$(type -P jeet 2>/dev/null)"
fi
jeet() {
  local _jeet_bin _jeet_status _jeet_target _jeet_out
  # Re-resolve if the rc file was eval'd before PATH knew about jeet.
  _jeet_bin="${_JEET_BIN:-}"
  if [ -z "$_jeet_bin" ]; then
    if [ -n "${ZSH_VERSION:-}" ]; then
      _jeet_bin="$(whence -p jeet 2>/dev/null)"
    else
      _jeet_bin="$(type -P jeet 2>/dev/null)"
    fi
  fi
  if [ -z "$_jeet_bin" ]; then
    echo "jeet: could not find the jeet binary on PATH" >&2
    return 127
  fi

  if [[ "${1:-}" == "cd" ]]; then
    shift
    _jeet_target="$("$_jeet_bin" path "$@")" || return $?
    [ -n "$_jeet_target" ] || return 1
    builtin cd -- "$_jeet_target"
    return $?
  fi
  if [[ "${1:-}" == "checkout" ]]; then
    shift
    _jeet_out="$("$_jeet_bin" checkout "$@")"
    _jeet_status=$?
    _jeet_out="${_jeet_out%%$'\n'*}"
    if [[ -d "$_jeet_out" ]]; then
      builtin cd -- "$_jeet_out"
    fi
    return $_jeet_status
  fi

  # Everything else runs the binary, which may ask us to cd by writing a path
  # to $JEET_CD_FILE (the explorer and `jeet worktree` both do). One mktemp file
  # per shell, reused, so an interrupted run cannot leave a trail of them.
  if [[ -z "${_JEET_CD_FILE:-}" || ! -f "${_JEET_CD_FILE:-}" ]]; then
    _JEET_CD_FILE="$(mktemp "${TMPDIR:-/tmp}/jeet-cd.XXXXXX" 2>/dev/null)" || _JEET_CD_FILE=""
  fi
  if [[ -n "$_JEET_CD_FILE" ]]; then
    : >"$_JEET_CD_FILE"
  fi
  JEET_CD_FILE="$_JEET_CD_FILE" "$_jeet_bin" "$@"
  _jeet_status=$?
  if [[ -n "$_JEET_CD_FILE" && -s "$_JEET_CD_FILE" ]]; then
    _jeet_target="$(cat "$_JEET_CD_FILE")"
    : >"$_JEET_CD_FILE"
    if [[ -d "$_jeet_target" ]]; then
      builtin cd -- "$_jeet_target"
    fi
  fi
  return $_jeet_status
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
(( $+functions[compdef] )) && compdef _jeet_wrapper jeet
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
