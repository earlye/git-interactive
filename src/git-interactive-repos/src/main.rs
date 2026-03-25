//! `git interactive repos` — browse sibling directories as git workspaces.

use std::io::{self, stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Parser)]
#[command(name = "git-interactive-repos")]
#[command(about = "Interactive overview of git repos in the current directory")]
struct Args {}

#[derive(Clone, Debug)]
enum Row {
    Scanning {
        name: String,
        path: PathBuf,
    },
    NotGit {
        name: String,
        path: PathBuf,
    },
    Git {
        name: String,
        path: PathBuf,
        branch_label: String,
        dirty: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FocusField {
    Branch,
    Status,
    Stash,
}

impl FocusField {
    fn next(self) -> Self {
        match self {
            FocusField::Branch => FocusField::Status,
            FocusField::Status => FocusField::Stash,
            FocusField::Stash => FocusField::Branch,
        }
    }

    fn prev(self) -> Self {
        match self {
            FocusField::Branch => FocusField::Stash,
            FocusField::Status => FocusField::Branch,
            FocusField::Stash => FocusField::Status,
        }
    }
}

enum AppMode {
    TopLevel {
        list_state: ListState,
    },
    RepoDetail {
        repo_idx: usize,
        focus: FocusField,
    },
    BranchSelect {
        repo_idx: usize,
        filter: String,
        branches: Vec<String>,
        list_state: ListState,
    },
    ConfirmReset {
        repo_idx: usize,
    },
}

struct App {
    rows: Vec<Row>,
    mode: AppMode,
    /// Updates from background probes: (index, new row).
    rx: mpsc::Receiver<(usize, Row)>,
}

#[derive(Clone)]
enum RunOutcome {
    Quit,
}

fn main() -> io::Result<()> {
    let _args = Args::parse();

    let cwd = std::env::current_dir()?;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&cwd)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect();
    entries.sort();

    if entries.is_empty() {
        println!("No subdirectories in {}.", cwd.display());
        return Ok(());
    }

    let (tw, _) = crossterm::terminal::size()?;
    // inner list width ≈ tw − border; content column ≈ inner − 1 for `>` highlight prefix
    let min_content = tw.saturating_sub(2).saturating_sub(1);
    if min_content < 1 {
        eprintln!("Terminal isn't wide enough to display.");
        return Ok(());
    }

    let (tx, rx) = mpsc::channel::<(usize, Row)>();
    for (idx, path) in entries.iter().enumerate() {
        let tx = tx.clone();
        let path = path.clone();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        std::thread::spawn(move || {
            let row = probe_repo(name, path);
            let _ = tx.send((idx, row));
        });
    }
    drop(tx);

    let rows: Vec<Row> = entries
        .iter()
        .map(|p| {
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            Row::Scanning {
                name,
                path: p.clone(),
            }
        })
        .collect();

    let mut list_state = ListState::default();
    if !rows.is_empty() {
        list_state.select(Some(0));
    }

    let mut app = App {
        rows,
        mode: AppMode::TopLevel { list_state },
        rx,
    };

    enable_raw_mode()?;
    // Fullscreen on the alternate screen avoids `Viewport::Inline` injecting newlines into the
    // shell scrollback (which looked like duplicate/garbled output below the bordered area).
    execute!(stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let result = run_app(&mut terminal, &mut app);

    terminal.clear()?;
    execute!(stdout(), LeaveAlternateScreen)?;
    disable_raw_mode()?;

    match result {
        Ok(RunOutcome::Quit) => {}
        Err(e) => return Err(e),
    }

    Ok(())
}

/// Minimum display width for middle elision (`a…b`); below this, use prefix-only [`truncate_to_width`].
const MIN_MIDDLE_ELISION_WIDTH: u16 = 5;

fn display_width_str(s: &str) -> usize {
    s.width()
}

/// One visual line, truncated to `max_width` display columns (prefix + U+2026 when cut).
fn truncate_to_width(s: &str, max_width: u16) -> String {
    let max = max_width as usize;
    if max == 0 {
        return String::new();
    }
    let mut width = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + cw > max {
            if width < max {
                out.push('…');
            }
            break;
        }
        width += cw;
        out.push(ch);
    }
    out
}

/// Pad on the right with ASCII spaces until display width is `target_width` (for fixed columns).
fn pad_right_to_width(s: &str, target_width: u16) -> String {
    let tw = target_width as usize;
    let mut out = s.to_string();
    let mut w = display_width_str(&out);
    while w < tw {
        out.push(' ');
        w += 1;
    }
    out
}

/// Fit `s` into `max_width` display columns: middle elision (`…`) when wide enough, else prefix truncate.
fn fit_width(s: &str, max_width: u16) -> String {
    if max_width == 0 {
        return String::new();
    }
    if display_width_str(s) <= max_width as usize {
        return s.to_string();
    }
    if max_width < MIN_MIDDLE_ELISION_WIDTH {
        return truncate_to_width(s, max_width);
    }
    elide_middle(s, max_width)
}

/// `left…right` by display width; falls back to [`truncate_to_width`] if pieces would overlap.
fn elide_middle(s: &str, max_width: u16) -> String {
    let w = max_width as usize;
    let sw = display_width_str(s);
    if sw <= w {
        return s.to_string();
    }
    const ELL: char = '…';
    let ell_w = UnicodeWidthChar::width(ELL).unwrap_or(1);
    if w <= ell_w {
        return truncate_to_width(s, max_width);
    }
    let inner = w - ell_w;
    let left_w = inner / 2;
    let right_w = inner - left_w;

    let chars: Vec<char> = s.chars().collect();
    let mut acc = 0usize;
    let mut left_end = 0usize;
    for (i, ch) in chars.iter().enumerate() {
        let cw = UnicodeWidthChar::width(*ch).unwrap_or(0);
        if acc + cw > left_w {
            break;
        }
        acc += cw;
        left_end = i + 1;
    }
    let mut acc = 0usize;
    let mut right_start = chars.len();
    for i in (0..chars.len()).rev() {
        let ch = chars[i];
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if acc + cw > right_w {
            break;
        }
        acc += cw;
        right_start = i;
    }
    if left_end >= right_start {
        return truncate_to_width(s, max_width);
    }
    let left: String = chars[..left_end].iter().collect();
    let right: String = chars[right_start..].iter().collect();
    format!("{}{}{}", left, ELL, right)
}

/// Two-char status prefix, then elided name, space, elided branch/status text.
fn format_top_level_row(row: &Row, content_w: u16) -> String {
    let status = match row {
        Row::Scanning { .. } => "% ",
        Row::NotGit { .. } => "! ",
        Row::Git { dirty, .. } => {
            if *dirty {
                "* "
            } else {
                "  "
            }
        }
    };
    let (name_src, branch_src) = match row {
        Row::Scanning { name, .. } => (name.as_str(), "<scanning>"),
        Row::NotGit { name, .. } => (name.as_str(), "<not-git>"),
        Row::Git {
            name,
            branch_label,
            ..
        } => (name.as_str(), branch_label.as_str()),
    };

    // After 2-char status: name + one space + branch; total display width = content_w.
    let usable = content_w.saturating_sub(2);
    if usable < 1 {
        return truncate_to_width(
            &format!("{}{}", status, name_src),
            content_w,
        );
    }
    // `inner` = display width for name + branch (one space gap is inside `usable`).
    let inner = usable.saturating_sub(1);
    if inner == 0 {
        return truncate_to_width(
            &format!("{}{}", status, name_src),
            content_w,
        );
    }
    let name_max = ((inner as u32 * 60 / 100) as u16)
        .max(1)
        .min(inner.saturating_sub(1).max(1));
    let branch_max = inner.saturating_sub(name_max);
    let name_fit = fit_width(name_src, name_max);
    let name_padded = pad_right_to_width(&name_fit, name_max);
    let branch_fit = if branch_max == 0 {
        String::new()
    } else {
        fit_width(branch_src, branch_max)
    };
    if branch_fit.is_empty() {
        format!("{}{}", status, name_padded)
    } else {
        format!("{}{} {}", status, name_padded, branch_fit)
    }
}

fn truncate_lines(s: &str, max_width: u16) -> String {
    s.lines()
        .map(|ln| truncate_to_width(ln, max_width))
        .collect::<Vec<_>>()
        .join("\n")
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<RunOutcome> {
    let mut needs_draw = true;
    loop {
        while let Ok((idx, row)) = app.rx.try_recv() {
            if idx < app.rows.len() {
                app.rows[idx] = row;
                needs_draw = true;
            }
        }

        if needs_draw {
            terminal.draw(|frame| {
                let area = frame.area();
                draw(frame, area, app);
            })?;
            needs_draw = false;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        match event::read()? {
            Event::Resize(_, _) => {
                needs_draw = true;
                continue;
            }
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(RunOutcome::Quit);
                }

                match &mut app.mode {
                AppMode::TopLevel { list_state } => {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(RunOutcome::Quit),
                        KeyCode::Up | KeyCode::Char('k') => {
                            move_selection(list_state, app.rows.len(), -1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            move_selection(list_state, app.rows.len(), 1);
                        }
                        KeyCode::Enter => {
                            let Some(i) = list_state.selected() else {
                                continue;
                            };
                            match &app.rows[i] {
                                Row::Scanning { .. } | Row::NotGit { .. } => {}
                                Row::Git { .. } => {
                                    app.mode = AppMode::RepoDetail {
                                        repo_idx: i,
                                        focus: FocusField::Branch,
                                    };
                                }
                            }
                        }
                        _ => {}
                    }
                }
                AppMode::RepoDetail { repo_idx, focus } => {
                    match key.code {
                        KeyCode::Esc => {
                            app.mode = AppMode::TopLevel {
                                list_state: top_level_state_from_idx(*repo_idx),
                            };
                        }
                        KeyCode::Left | KeyCode::Char('h') => {
                            *focus = focus.prev();
                        }
                        KeyCode::Right | KeyCode::Char('l') => {
                            *focus = focus.next();
                        }
                        KeyCode::Enter => {
                            let idx = *repo_idx;
                            let row = app.rows.get(idx).cloned();
                            let Some(row) = row else { continue };
                            let Row::Git { path, dirty, .. } = row else {
                                continue;
                            };
                            match *focus {
                                FocusField::Branch => {
                                    match list_branches(&path) {
                                        Ok(branches) if branches.is_empty() => {}
                                        Ok(branches) => {
                                            let mut list_state = ListState::default();
                                            list_state.select(Some(0));
                                            app.mode = AppMode::BranchSelect {
                                                repo_idx: idx,
                                                filter: String::new(),
                                                branches,
                                                list_state,
                                            };
                                        }
                                        Err(_) => {}
                                    }
                                }
                                FocusField::Status => {
                                    if dirty {
                                        app.mode = AppMode::ConfirmReset { repo_idx: idx };
                                    }
                                }
                                FocusField::Stash => {
                                    let _ = git_stash(&path);
                                    if let Some(r) = app.rows.get_mut(idx) {
                                        if let Ok(new_row) = refresh_git_row(&*r) {
                                            *r = new_row;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                AppMode::BranchSelect {
                    repo_idx,
                    filter,
                    branches,
                    list_state,
                } => {
                    let filtered: Vec<usize> = branches
                        .iter()
                        .enumerate()
                        .filter(|(_, b)| branch_matches_filter(b, filter))
                        .map(|(i, _)| i)
                        .collect();

                    match key.code {
                        KeyCode::Esc => {
                            app.mode = AppMode::RepoDetail {
                                repo_idx: *repo_idx,
                                focus: FocusField::Branch,
                            };
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            let nf = filtered.len();
                            if nf > 0 {
                                let sel = list_state.selected().unwrap_or(0).min(nf - 1);
                                let new_sel = if sel == 0 { nf - 1 } else { sel - 1 };
                                list_state.select(Some(new_sel));
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            let nf = filtered.len();
                            if nf > 0 {
                                let sel = list_state.selected().unwrap_or(0).min(nf - 1);
                                let new_sel = if sel >= nf - 1 { 0 } else { sel + 1 };
                                list_state.select(Some(new_sel));
                            }
                        }
                        KeyCode::Enter => {
                            let nf = filtered.len();
                            if nf == 0 {
                                continue;
                            }
                            let sel = list_state.selected().unwrap_or(0).min(nf - 1);
                            let branch_idx = filtered[sel];
                            let branch_name = branches[branch_idx].clone();
                            let path = app.rows[*repo_idx]
                                .path()
                                .expect("repo row")
                                .to_path_buf();
                            let _ = git_switch(&path, &branch_name);
                            if let Some(r) = app.rows.get_mut(*repo_idx) {
                                if let Ok(new_row) = refresh_git_row(r) {
                                    *r = new_row;
                                }
                            }
                            app.mode = AppMode::RepoDetail {
                                repo_idx: *repo_idx,
                                focus: FocusField::Branch,
                            };
                        }
                        KeyCode::Backspace => {
                            filter.pop();
                            list_state.select(Some(0));
                        }
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            filter.push(c);
                            list_state.select(Some(0));
                        }
                        _ => {}
                    }
                }
                AppMode::ConfirmReset { repo_idx } => {
                    match key.code {
                        KeyCode::Esc => {
                            app.mode = AppMode::RepoDetail {
                                repo_idx: *repo_idx,
                                focus: FocusField::Status,
                            };
                        }
                        KeyCode::Enter => {
                            let path = app.rows[*repo_idx]
                                .path()
                                .expect("git row")
                                .to_path_buf();
                            let _ = git_reset_hard_and_clean(&path);
                            if let Some(r) = app.rows.get_mut(*repo_idx) {
                                if let Ok(new_row) = refresh_git_row(r) {
                                    *r = new_row;
                                }
                            }
                            app.mode = AppMode::RepoDetail {
                                repo_idx: *repo_idx,
                                focus: FocusField::Status,
                            };
                        }
                        _ => {}
                    }
                }
                }
                needs_draw = true;
            }
            _ => {}
        }
    }
}

fn top_level_state_from_idx(idx: usize) -> ListState {
    let mut s = ListState::default();
    s.select(Some(idx));
    s
}

fn move_selection(list_state: &mut ListState, len: usize, delta: i32) {
    if len == 0 {
        return;
    }
    let i = list_state.selected().unwrap_or(0);
    let new_i = if delta < 0 {
        if i == 0 {
            len - 1
        } else {
            i - 1
        }
    } else if i >= len - 1 {
        0
    } else {
        i + 1
    };
    list_state.select(Some(new_i));
}

impl Row {
    fn path(&self) -> Option<&Path> {
        match self {
            Row::Scanning { path, .. } | Row::NotGit { path, .. } | Row::Git { path, .. } => {
                Some(path.as_path())
            }
        }
    }
}

fn probe_repo(name: String, path: PathBuf) -> Row {
    if !is_git_repo(&path) {
        return Row::NotGit { name, path };
    }
    let branch_label = match branch_display(&path) {
        Ok(s) => s,
        Err(_) => "<error>".to_string(),
    };
    let dirty = is_dirty(&path).unwrap_or(false);
    Row::Git {
        name,
        path,
        branch_label,
        dirty,
    }
}

fn refresh_git_row(row: &Row) -> io::Result<Row> {
    match row {
        Row::Git {
            name,
            path,
            branch_label: _,
            dirty: _,
        } => {
            let branch_label = branch_display(path).unwrap_or_else(|_| "<error>".to_string());
            let dirty = is_dirty(path).unwrap_or(false);
            Ok(Row::Git {
                name: name.clone(),
                path: path.clone(),
                branch_label,
                dirty,
            })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a git row",
        )),
    }
}

fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).trim().eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

fn is_detached(path: &Path) -> bool {
    // `Command::status()` inherits stderr by default; git prints errors (e.g. detached HEAD) to the TUI.
    !git_cmd_status(path, &["symbolic-ref", "-q", "HEAD"])
        .map(|s| s.success())
        .unwrap_or(false)
}

fn branch_display(path: &Path) -> io::Result<String> {
    if is_detached(path) {
        let o = git_c(path, &["rev-parse", "--short", "HEAD"])?;
        if !o.status.success() {
            return Err(io::Error::other(
                "git rev-parse failed",
            ));
        }
        let short = String::from_utf8_lossy(&o.stdout).trim().to_string();
        return Ok(format!("<detached-{}>", short));
    }
    let o = git_c(path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if !o.status.success() {
        return Err(io::Error::other(
            "git rev-parse failed",
        ));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Run `git -C path ...`, capturing stdout/stderr (nothing printed to the terminal).
fn git_c(path: &Path, args: &[&str]) -> io::Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Exit status only; discard all git output so nothing leaks onto the alternate screen.
fn git_cmd_status(path: &Path, args: &[&str]) -> io::Result<std::process::ExitStatus> {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

fn is_dirty(path: &Path) -> io::Result<bool> {
    let o = git_c(path, &["status", "--porcelain"])?;
    if !o.status.success() {
        return Err(io::Error::other(
            "git status failed",
        ));
    }
    Ok(!o.stdout.is_empty())
}

fn list_branches(path: &Path) -> io::Result<Vec<String>> {
    let o = git_c(
        path,
        &[
            "for-each-ref",
            "--sort=refname",
            "--format=%(refname:short)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    if !o.status.success() {
        return Err(io::Error::other(
            "git for-each-ref failed",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let s = line.trim();
        if s.is_empty() || s == "HEAD" {
            continue;
        }
        seen.insert(s.to_string());
    }
    Ok(seen.into_iter().collect())
}

fn branch_matches_filter(branch: &str, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let f = filter.to_lowercase();
    branch.to_lowercase().contains(f.as_str())
}

fn git_switch(path: &Path, branch: &str) -> io::Result<()> {
    let status = git_cmd_status(path, &["switch", branch])?;
    if !status.success() {
        return Err(io::Error::other(
            "git switch failed",
        ));
    }
    Ok(())
}

fn git_stash(path: &Path) -> io::Result<()> {
    let status = git_cmd_status(path, &["stash", "push"])?;
    if !status.success() {
        return Err(io::Error::other("git stash failed"));
    }
    Ok(())
}

fn git_reset_hard_and_clean(path: &Path) -> io::Result<()> {
    let s1 = git_cmd_status(path, &["reset", "--hard"])?;
    if !s1.success() {
        return Err(io::Error::other(
            "git reset --hard failed",
        ));
    }
    let s2 = git_cmd_status(path, &["clean", "-fd"])?;
    if !s2.success() {
        return Err(io::Error::other(
            "git clean failed",
        ));
    }
    Ok(())
}

fn draw(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    match &mut app.mode {
        AppMode::TopLevel { list_state } => draw_top_level(frame, area, &app.rows, list_state),
        AppMode::RepoDetail { repo_idx, focus } => {
            draw_repo_detail(frame, area, &app.rows, *repo_idx, *focus)
        }
        AppMode::BranchSelect {
            repo_idx,
            filter,
            branches,
            list_state,
        } => draw_branch_select(frame, area, &app.rows, *repo_idx, filter, branches, list_state),
        AppMode::ConfirmReset { repo_idx } => draw_confirm_reset(frame, area, &app.rows, *repo_idx),
    }
}

fn draw_top_level(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[Row],
    list_state: &mut ListState,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("git interactive repos");
    let inner = block.inner(area);
    // One column for `>` highlight prefix; content holds status + name + branch.
    let content_w = inner.width.saturating_sub(1).max(1);

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let line = format_top_level_row(row, content_w);
            ListItem::new(truncate_to_width(&line, content_w))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol(">");

    frame.render_stateful_widget(list, area, list_state);
}

fn draw_repo_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[Row],
    repo_idx: usize,
    focus: FocusField,
) {
    let Some(row) = rows.get(repo_idx) else {
        return;
    };
    let Row::Git {
        name,
        branch_label,
        dirty,
        ..
    } = row
    else {
        return;
    };

    let status_text = if *dirty { "<dirty>" } else { "" };
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(area);

    let b_style = |f: FocusField| {
        if f == focus {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        }
    };

    let b_branch = Block::default()
        .borders(Borders::ALL)
        .title("branch")
        .border_style(b_style(FocusField::Branch));
    let tw0 = b_branch.inner(chunks[0]).width.max(1);
    let p0 = Paragraph::new(format!(
        "{}\n{}",
        truncate_to_width(name.as_str(), tw0),
        truncate_to_width(branch_label.as_str(), tw0)
    ))
    .block(b_branch);

    let b_status = Block::default()
        .borders(Borders::ALL)
        .title("status")
        .border_style(b_style(FocusField::Status));
    let tw1 = b_status.inner(chunks[1]).width.max(1);
    let p1 = Paragraph::new(truncate_to_width(status_text, tw1)).block(b_status);

    let p2 = Paragraph::new("stash").block(
        Block::default()
            .borders(Borders::ALL)
            .title("stash")
            .border_style(b_style(FocusField::Stash)),
    );

    frame.render_widget(p0, chunks[0]);
    frame.render_widget(p1, chunks[1]);
    frame.render_widget(p2, chunks[2]);
}

fn draw_branch_select(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[Row],
    repo_idx: usize,
    filter: &str,
    branches: &[String],
    list_state: &mut ListState,
) {
    let filtered: Vec<&String> = branches
        .iter()
        .filter(|b| branch_matches_filter(b, filter))
        .collect();

    let nf = filtered.len();
    if nf == 0 {
        list_state.select(None);
    } else {
        let sel = list_state.selected().unwrap_or(0).min(nf - 1);
        list_state.select(Some(sel));
    }

    let title_raw = format!(
        "branches [{}] — filter: {}",
        rows
            .get(repo_idx)
            .and_then(|r| match r {
                Row::Git { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .unwrap_or(""),
        if filter.is_empty() {
            "(type to filter)".to_string()
        } else {
            filter.to_string()
        }
    );
    let title = truncate_to_width(&title_raw, area.width.saturating_sub(4).max(1));

    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    let text_w = inner.width.saturating_sub(2).max(1);

    let items: Vec<ListItem> = filtered
        .iter()
        .map(|b| ListItem::new(truncate_to_width(b.as_str(), text_w)))
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, list_state);
}

fn draw_confirm_reset(frame: &mut Frame<'_>, area: Rect, rows: &[Row], repo_idx: usize) {
    let name = rows
        .get(repo_idx)
        .map(|r| match r {
            Row::Git { name, .. } => name.as_str(),
            _ => "?",
        })
        .unwrap_or("?");
    let text = format!(
        "Reset --hard and clean -fd in {}?\n\nEnter = confirm   Esc = cancel",
        name
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title("confirm destructive reset");
    let tw = block.inner(area).width.max(1);
    let p = Paragraph::new(truncate_lines(&text, tw)).block(block);
    frame.render_widget(p, area);
}
