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
"#;
    std::io::stdout().write_all(snippet.as_bytes())?;
    Ok(())
}
