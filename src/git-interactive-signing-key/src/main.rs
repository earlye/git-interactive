use std::io::{self, stdout};
use std::process::Command;

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    prelude::*,
    widgets::{List, ListItem, ListState},
    TerminalOptions, Viewport, 
};

#[derive(Parser)]
#[command(name = "git-interactive-signing-key")]
#[command(about = "Interactively select a GPG signing key for git commits")]
struct Args {
    /// Set signing key in global git config instead of local
    #[arg(long, default_value_t = false)]
    global: bool,

    /// Set signing key in local git config (default)
    #[arg(long, default_value_t = false)]
    local: bool,
}

#[derive(Debug)]
struct GpgKey {
    key_id: String,
    uid: String,
}

fn get_gpg_keys() -> Vec<GpgKey> {
    let output = Command::new("gpg")
        .args(["--list-secret-keys", "--keyid-format", "long"])
        .output()
        .expect("Failed to execute gpg");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut keys = Vec::new();
    let mut current_key_id: Option<String> = None;

    for line in stdout.lines() {
        // Lines like: "sec   rsa4096/ABCD1234EFGH5678 2023-01-01 [SC]"
        if line.starts_with("sec") {
            if let Some(key_part) = line.split_whitespace().nth(1) {
                if let Some(key_id) = key_part.split('/').nth(1) {
                    current_key_id = Some(key_id.to_string());
                }
            }
        }
        // Lines like: "uid           [ultimate] John Doe <john@example.com>"
        if line.contains("uid") && line.contains("[") {
            if let Some(key_id) = current_key_id.take() {
                // Extract the part after the trust level bracket
                let uid = line
                    .split(']')
                    .nth(1)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                keys.push(GpgKey { key_id, uid });
            }
        }
    }

    keys
}

fn get_current_signing_key(global: bool) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(["config"]);
    if global {
        cmd.arg("--global");
    }
    cmd.arg("user.signingkey");

    let output = cmd.output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn set_signing_key(key_id: &str, global: bool) -> io::Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(["config"]);
    if global {
        cmd.arg("--global");
    }
    cmd.args(["user.signingkey", key_id]);

    let status = cmd.status()?;
    if !status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "Failed to set signing key",
        ));
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    // --local is default, --global overrides
    let global = args.global && !args.local;

    let keys = get_gpg_keys();
    if keys.is_empty() {
        eprintln!("No GPG secret keys found.");
        return Ok(());
    }

    let current_key = get_current_signing_key(global);

    // Find index of current key
    let initial_index = current_key
        .as_ref()
        .and_then(|ck| keys.iter().position(|k| k.key_id == *ck))
        .unwrap_or(0);

    // Setup terminal with inline viewport
    enable_raw_mode()?;
    let backend = CrosstermBackend::new(stdout());
    let options = TerminalOptions {
        viewport: Viewport::Inline(keys.len() as u16),
    };
    let mut terminal = Terminal::with_options(backend, options)?;

    let mut list_state = ListState::default();
    list_state.select(Some(initial_index));

    let result = run_app(&mut terminal, &keys, &mut list_state, &current_key);

    // Clear the inline viewport and restore terminal
    terminal.clear()?;
    disable_raw_mode()?;

    match result {
        Ok(Some(selected_index)) => {
            let selected_key = &keys[selected_index];
            set_signing_key(&selected_key.key_id, global)?;
            println!(
                "Set {} user.signingkey to {}",
                if global { "global" } else { "local" },
                selected_key.key_id
            );
        }
        Ok(None) => {
            println!("Cancelled.");
        }
        Err(e) => return Err(e),
    }

    Ok(())
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    keys: &[GpgKey],
    list_state: &mut ListState,
    current_key: &Option<String>,
) -> io::Result<Option<usize>> {
    loop {
        terminal.draw(|frame| {
            let area = frame.area();

            let items: Vec<ListItem> = keys
                .iter()
                .map(|key| {
                    let is_current = current_key.as_ref().map_or(false, |ck| ck == &key.key_id);
                    let marker = if is_current { " ← current" } else { "" };
                    let content = format!("{} {}{}", key.key_id, key.uid, marker);
                    ListItem::new(content)
                })
                .collect();

            let list = List::new(items)
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("> ");

            frame.render_stateful_widget(list, area, list_state);
        })?;

        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('q') => return Ok(None),
                    KeyCode::Char('c')
                        if key.modifiers.contains(event::KeyModifiers::CONTROL) =>
                    {
                        return Ok(None)
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let i = list_state.selected().unwrap_or(0);
                        let new_i = if i == 0 { keys.len() - 1 } else { i - 1 };
                        list_state.select(Some(new_i));
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let i = list_state.selected().unwrap_or(0);
                        let new_i = if i >= keys.len() - 1 { 0 } else { i + 1 };
                        list_state.select(Some(new_i));
                    }
                    KeyCode::Enter => {
                        return Ok(list_state.selected());
                    }
                    _ => {}
                }
            }
        }
    }
}
