use std::io::Write;

use anyhow::Result;

use crate::commands::completions::CompletionShell;

pub fn run_init_shell() -> Result<()> {
    let zsh_comp = crate::commands::completions::generate_script(CompletionShell::Zsh)?;
    let bash_comp = crate::commands::completions::generate_script(CompletionShell::Bash)?;

    let mut out = String::from(
        r#"# plete shell integration — add one line to ~/.zshrc or ~/.bashrc:
#   eval "$(plete init-shell)"
# Resolve the binary, not the function: zsh's `which` prints function bodies and
# a bare-word fallback would make the wrapper call itself.
if [ -n "${ZSH_VERSION:-}" ]; then
  _PLETE_BIN="$(whence -p plete 2>/dev/null)"
else
  _PLETE_BIN="$(type -P plete 2>/dev/null)"
fi
plete() {
  local _plete_bin _plete_status _plete_target
  # Re-resolve if the rc file was eval'd before PATH knew about plete.
  _plete_bin="${_PLETE_BIN:-}"
  if [ -z "$_plete_bin" ]; then
    if [ -n "${ZSH_VERSION:-}" ]; then
      _plete_bin="$(whence -p plete 2>/dev/null)"
    else
      _plete_bin="$(type -P plete 2>/dev/null)"
    fi
  fi
  if [ -z "$_plete_bin" ]; then
    echo "plete: could not find the plete binary on PATH" >&2
    return 127
  fi

  if [[ "${1:-}" == "cd" ]]; then
    shift
    _plete_target="$("$_plete_bin" path "$@")" || return $?
    [ -n "$_plete_target" ] || return 1
    builtin cd -- "$_plete_target"
    return $?
  fi

  # Everything else runs the binary, which may ask us to cd by writing a path
  # to $PLETE_CD_FILE (the explorer and `plete worktree` both do). One mktemp file
  # per shell, reused, so an interrupted run cannot leave a trail of them.
  if [[ -z "${_PLETE_CD_FILE:-}" || ! -f "${_PLETE_CD_FILE:-}" ]]; then
    _PLETE_CD_FILE="$(mktemp "${TMPDIR:-/tmp}/plete-cd.XXXXXX" 2>/dev/null)" || _PLETE_CD_FILE=""
  fi
  if [[ -n "$_PLETE_CD_FILE" ]]; then
    : >"$_PLETE_CD_FILE"
  fi
  JEET_CD_FILE="$_PLETE_CD_FILE" PLETE_CD_FILE="$_PLETE_CD_FILE" "$_plete_bin" "$@"
  _plete_status=$?
  if [[ -n "$_PLETE_CD_FILE" && -s "$_PLETE_CD_FILE" ]]; then
    _plete_target="$(cat "$_PLETE_CD_FILE")"
    : >"$_PLETE_CD_FILE"
    if [[ -d "$_plete_target" ]]; then
      builtin cd -- "$_plete_target"
    fi
  fi
  return $_plete_status
}

# `jeet` while a jeet binary is on PATH (the packages keep one), and `pt` when
# nothing else is called that (or it is a link to plete): a function named for
# a different program would shadow it just as a binary would.
_plete_names=(plete)
if [ -n "${ZSH_VERSION:-}" ]; then
  _plete_old="$(whence -p jeet 2>/dev/null)"
else
  _plete_old="$(type -P jeet 2>/dev/null)"
fi
if [ -n "$_plete_old" ]; then
  jeet() { plete "$@"; }
  _plete_names+=(jeet)
fi
_plete_pt="$(command -v pt 2>/dev/null)"
# (Read again, `pt` is our own function, which the marker tells from another.)
if [ -z "$_plete_pt" ] || { [ -n "${_PLETE_PT_FN:-}" ] && [ "$_plete_pt" = pt ]; } \
  || { [ "${_plete_pt#/}" != "$_plete_pt" ] && [ "$_plete_pt" -ef "${_PLETE_BIN:-/nonexistent}" ]; }; then
  pt() { plete "$@"; }
  _PLETE_PT_FN=1
  _plete_names+=(pt)
fi
unset _plete_old _plete_pt

"#,
    );

    out.push_str("# zsh: wrapper `plete cd` + subcommand completion\n");
    out.push_str("if [ -n \"${ZSH_VERSION:-}\" ]; then\n");
    out.push_str(&zsh_comp);
    out.push_str(
        r#"_plete_wrapper() {
  if (( CURRENT == 3 )) && [[ ${words[2]} == cd ]]; then
    compadd -- $(command plete complete repos 2>/dev/null)
    return
  fi
  _clap_dynamic_completer_plete "$@"
}
if (( $+functions[compdef] )); then
  for _plete_n in "${_plete_names[@]}"; do compdef _plete_wrapper "$_plete_n"; done
  unset _plete_n
fi
fi

"#,
    );

    out.push_str("# bash: wrapper `plete cd` + subcommand completion\n");
    out.push_str("if [ -n \"${BASH_VERSION:-}\" ]; then\n");
    out.push_str(&bash_comp);
    out.push_str(
        r#"_plete_wrapper_bash() {
  local cur
  cur="${COMP_WORDS[COMP_CWORD]}"
  if [[ ${COMP_WORDS[1]} == "cd" && $COMP_CWORD -eq 2 ]]; then
    mapfile -t COMPREPLY < <(compgen -W "$(command plete complete repos 2>/dev/null)" -- "$cur")
    return
  fi
    _clap_complete_plete "$@"
}
for _plete_n in "${_plete_names[@]}"; do
  complete -o default -o nospace -F _plete_wrapper_bash "$_plete_n"
done
unset _plete_n
fi
"#,
    );

    std::io::stdout().write_all(out.as_bytes())?;
    Ok(())
}
