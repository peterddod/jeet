//! `jeet todo …`: the assistant's todo list, scriptable. Items and `--json`
//! go to stdout; what jeet has to say about them goes to stderr.

use anyhow::Result;

use crate::assistant;
use crate::commands::task::printable;
use crate::context::App;
use crate::tasks;
use crate::todo::{self, ItemView};

pub fn ls(app: &App, json: bool) -> Result<()> {
    let dir = assistant::home_dir(&app.home);
    let items = todo::read(&dir, assistant::TODO)?.views();
    if json {
        println!("{}", serde_json::to_string(&items)?);
        return Ok(());
    }
    if items.is_empty() {
        eprintln!("jeet: nothing to do; `jeet todo add <text>` adds an item");
        return Ok(());
    }
    for item in &items {
        println!("{}", line(item));
    }
    Ok(())
}

pub fn add(
    app: &App,
    text: &[String],
    priority: Option<u8>,
    links: &[String],
    json: bool,
) -> Result<()> {
    let mut text = todo::item_text(&text.join(" "))?;
    if let Some(p) = priority {
        text = format!("!p{p} {text}");
    }
    for key in links {
        let task = tasks::find(app, key)?;
        text.push_str(&format!(" @{}", task.slug));
    }
    let dir = assistant::home_dir(&app.home);
    assistant::ensure_files(&dir)?;
    let added = todo::update(&dir, assistant::TODO, |list| Ok(list.add(&text)))?;
    report(&[added], "added", json)
}

pub fn done(app: &App, ids: &[usize], undo: bool, json: bool) -> Result<()> {
    let dir = assistant::home_dir(&app.home);
    assistant::ensure_files(&dir)?;
    let changed = todo::update(&dir, assistant::TODO, |list| list.set_done(ids, !undo))?;
    report(&changed, if undo { "not done" } else { "done" }, json)
}

pub fn rm(app: &App, ids: &[usize], json: bool) -> Result<()> {
    let dir = assistant::home_dir(&app.home);
    assistant::ensure_files(&dir)?;
    let removed = todo::update(&dir, assistant::TODO, |list| list.remove(ids))?;
    report(&removed, "removed", json)
}

fn report(items: &[ItemView], what: &str, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(items)?);
        return Ok(());
    }
    for item in items {
        eprintln!("jeet: {what} {}", line(item));
    }
    Ok(())
}

/// ` 3  [x] !p1 Fix the login cookie @fix-login`. The file may have been
/// edited by hand, so nothing in it is played on the terminal.
fn line(item: &ItemView) -> String {
    let mark = if item.done { 'x' } else { ' ' };
    format!("{:>2}  [{mark}] {}", item.id, printable(&item.text))
}
