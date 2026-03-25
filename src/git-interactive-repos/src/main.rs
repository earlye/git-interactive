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
        branches: Vec<String>,
        /// Substring filter (case-insensitive); only applied while `focus == Branch`.
        filter: String,
        branch_list_state: ListState,
        /// First visible line of porcelain in the status panel (viewport offset).
        status_scroll: usize,
        /// Selected line index in full `git status --porcelain` output (like branch list).
        status_selected_line: usize,
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
    /// Last `frame.area()` from [`Terminal::draw`], for status scroll viewport math.
    last_area: Rect,
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
        last_area: Rect::default(),
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

/// Inner line count for the status panel: horizontal layout chunk height equals `area.height`; block removes two border rows.
fn status_viewport_lines(area_height: u16) -> usize {
    (area_height as usize).saturating_sub(2).max(1)
}

fn status_scroll_viewport_lines(app: &App) -> usize {
    let h = if app.last_area.height > 0 {
        app.last_area.height
    } else {
        crossterm::terminal::size()
            .map(|(_, h)| h)
            .unwrap_or(24)
    };
    status_viewport_lines(h)
}

fn status_max_scroll(total_lines: usize, viewport_lines: usize) -> usize {
    total_lines.saturating_sub(viewport_lines)
}

fn status_selection_move_up(sel: usize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    if sel == 0 {
        total - 1
    } else {
        sel - 1
    }
}

fn status_selection_move_down(sel: usize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    if sel >= total - 1 {
        0
    } else {
        sel + 1
    }
}

fn ensure_status_scroll_visible(
    scroll: &mut usize,
    selected: usize,
    viewport: usize,
    total: usize,
) {
    if total == 0 || viewport == 0 {
        *scroll = 0;
        return;
    }
    let max_scroll = status_max_scroll(total, viewport);
    if selected < *scroll {
        *scroll = selected;
    } else if selected >= (*scroll).saturating_add(viewport) {
        *scroll = selected + 1 - viewport;
    }
    *scroll = (*scroll).min(max_scroll);
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
                app.last_area = area;
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

                let status_viewport = status_scroll_viewport_lines(app);

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
                                    app.mode = load_repo_detail(i, FocusField::Branch, &app.rows);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                AppMode::RepoDetail {
                    repo_idx,
                    focus,
                    branches,
                    filter,
                    branch_list_state,
                    status_scroll,
                    status_selected_line,
                } => {
                    let filtered_indices: Vec<usize> = branches
                        .iter()
                        .enumerate()
                        .filter(|(_, b)| branch_matches_filter(b, filter))
                        .map(|(i, _)| i)
                        .collect();
                    let nf = filtered_indices.len();

                    match key.code {
                        KeyCode::Esc => {
                            app.mode = AppMode::TopLevel {
                                list_state: top_level_state_from_idx(*repo_idx),
                            };
                        }
                        KeyCode::Left | KeyCode::Char('h') => {
                            let prev_focus = *focus;
                            *focus = focus.prev();
                            if prev_focus != FocusField::Branch && *focus == FocusField::Branch {
                                if let Some(r) = app.rows.get(*repo_idx) {
                                    if let Row::Git { branch_label, .. } = r {
                                        sync_filtered_selection_from_head(
                                            branch_label,
                                            branches,
                                            filter,
                                            branch_list_state,
                                        );
                                    }
                                }
                            }
                        }
                        KeyCode::Right | KeyCode::Char('l') => {
                            let prev_focus = *focus;
                            *focus = focus.next();
                            if prev_focus != FocusField::Branch && *focus == FocusField::Branch {
                                if let Some(r) = app.rows.get(*repo_idx) {
                                    if let Row::Git { branch_label, .. } = r {
                                        sync_filtered_selection_from_head(
                                            branch_label,
                                            branches,
                                            filter,
                                            branch_list_state,
                                        );
                                    }
                                }
                            }
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
                                    if nf == 0 {
                                        continue;
                                    }
                                    let sel = branch_list_state
                                        .selected()
                                        .unwrap_or(0)
                                        .min(nf - 1);
                                    let branch_idx = filtered_indices[sel];
                                    let branch_name = branches[branch_idx].clone();
                                    let _ = git_switch(&path, &branch_name);
                                    if let Some(r) = app.rows.get_mut(idx) {
                                        if let Ok(new_row) = refresh_git_row(r) {
                                            *r = new_row;
                                        }
                                    }
                                    *branches = list_branches(&path).unwrap_or_default();
                                    filter.clear();
                                    if let Some(r) = app.rows.get(idx) {
                                        if let Row::Git { branch_label, .. } = r {
                                            sync_filtered_selection_from_head(
                                                branch_label,
                                                branches,
                                                filter,
                                                branch_list_state,
                                            );
                                        }
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
                        KeyCode::Up | KeyCode::Char('k') => {
                            if *focus == FocusField::Branch && nf > 0 {
                                let sel = branch_list_state
                                    .selected()
                                    .unwrap_or(0)
                                    .min(nf - 1);
                                let new_sel = if sel == 0 { nf - 1 } else { sel - 1 };
                                branch_list_state.select(Some(new_sel));
                            } else if *focus == FocusField::Status {
                                if let Some(path) = app.rows.get(*repo_idx).and_then(|r| r.path()) {
                                    let total = git_status_porcelain(path)
                                        .map(|s| s.lines().count())
                                        .unwrap_or(0);
                                    if total > 0 {
                                        *status_selected_line =
                                            status_selection_move_up(*status_selected_line, total);
                                        ensure_status_scroll_visible(
                                            status_scroll,
                                            *status_selected_line,
                                            status_viewport,
                                            total,
                                        );
                                    }
                                }
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if *focus == FocusField::Branch && nf > 0 {
                                let sel = branch_list_state
                                    .selected()
                                    .unwrap_or(0)
                                    .min(nf - 1);
                                let new_sel = if sel >= nf - 1 { 0 } else { sel + 1 };
                                branch_list_state.select(Some(new_sel));
                            } else if *focus == FocusField::Status {
                                if let Some(path) = app.rows.get(*repo_idx).and_then(|r| r.path()) {
                                    let total = git_status_porcelain(path)
                                        .map(|s| s.lines().count())
                                        .unwrap_or(0);
                                    if total > 0 {
                                        *status_selected_line =
                                            status_selection_move_down(*status_selected_line, total);
                                        ensure_status_scroll_visible(
                                            status_scroll,
                                            *status_selected_line,
                                            status_viewport,
                                            total,
                                        );
                                    }
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            if *focus == FocusField::Branch {
                                filter.pop();
                                branch_list_state.select(Some(0));
                            }
                        }
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            if *focus == FocusField::Branch {
                                filter.push(c);
                                branch_list_state.select(Some(0));
                            }
                        }
                        _ => {}
                    }
                }
                AppMode::ConfirmReset { repo_idx } => {
                    match key.code {
                        KeyCode::Esc => {
                            app.mode = load_repo_detail(*repo_idx, FocusField::Status, &app.rows);
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
                            app.mode = load_repo_detail(*repo_idx, FocusField::Status, &app.rows);
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

/// Raw `git status --porcelain` output (may be empty when clean).
fn git_status_porcelain(path: &Path) -> io::Result<String> {
    let o = git_c(path, &["status", "--porcelain"])?;
    if !o.status.success() {
        return Err(io::Error::other(
            "git status failed",
        ));
    }
    Ok(String::from_utf8_lossy(&o.stdout).to_string())
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

fn index_of_current_branch(branch_label: &str, branches: &[String]) -> Option<usize> {
    if branch_label.starts_with("<detached") {
        return None;
    }
    branches.iter().position(|b| b == branch_label)
}

fn sync_branch_list_to_head(
    branch_label: &str,
    branches: &[String],
    branch_list_state: &mut ListState,
) {
    if branches.is_empty() {
        branch_list_state.select(None);
        return;
    }
    match index_of_current_branch(branch_label, branches) {
        Some(i) => branch_list_state.select(Some(i)),
        None => branch_list_state.select(None),
    }
}

fn branch_matches_filter(branch: &str, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let f = filter.to_lowercase();
    branch.to_lowercase().contains(f.as_str())
}

fn filtered_branch_indices(branches: &[String], filter: &str) -> Vec<usize> {
    branches
        .iter()
        .enumerate()
        .filter(|(_, b)| branch_matches_filter(b, filter))
        .map(|(i, _)| i)
        .collect()
}

/// Sets `branch_list_state` to the **filtered** row index for the current HEAD (or `0` / none).
fn sync_filtered_selection_from_head(
    branch_label: &str,
    branches: &[String],
    filter: &str,
    branch_list_state: &mut ListState,
) {
    let filtered = filtered_branch_indices(branches, filter);
    let nf = filtered.len();
    if nf == 0 {
        branch_list_state.select(None);
        return;
    }
    if let Some(head_i) = index_of_current_branch(branch_label, branches) {
        if let Some(pos) = filtered.iter().position(|&bi| bi == head_i) {
            branch_list_state.select(Some(pos));
            return;
        }
    }
    branch_list_state.select(Some(0));
}

fn load_repo_detail(repo_idx: usize, focus: FocusField, rows: &[Row]) -> AppMode {
    let branches = rows
        .get(repo_idx)
        .and_then(|r| r.path())
        .map(|p| list_branches(p).unwrap_or_default())
        .unwrap_or_default();
    let filter = String::new();
    let mut branch_list_state = ListState::default();
    if let Some(r) = rows.get(repo_idx) {
        if let Row::Git { branch_label, .. } = r {
            sync_filtered_selection_from_head(branch_label, &branches, &filter, &mut branch_list_state);
        }
    }
    AppMode::RepoDetail {
        repo_idx,
        focus,
        branches,
        filter,
        branch_list_state,
        status_scroll: 0,
        status_selected_line: 0,
    }
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
        AppMode::RepoDetail {
            repo_idx,
            focus,
            branches,
            filter,
            branch_list_state,
            status_scroll,
            status_selected_line,
        } => draw_repo_detail(
            frame,
            area,
            &app.rows,
            *repo_idx,
            *focus,
            branches,
            filter,
            branch_list_state,
            status_scroll,
            status_selected_line,
        ),
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
    branches: &[String],
    filter: &str,
    branch_list_state: &mut ListState,
    status_scroll: &mut usize,
    status_selected_line: &mut usize,
) {
    let Some(row) = rows.get(repo_idx) else {
        return;
    };
    let Row::Git {
        name,
        path,
        branch_label,
        ..
    } = row
    else {
        return;
    };

    let status_porcelain = git_status_porcelain(path).unwrap_or_else(|_| "<error>".to_string());

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

    let chunk_w = chunks[0].width.max(1);

    let title_raw = if focus == FocusField::Branch {
        format!(
            "branch [{}] — filter: {}",
            name,
            if filter.is_empty() {
                "(type to filter)".to_string()
            } else {
                filter.to_string()
            }
        )
    } else {
        format!("branch [{}]", name)
    };
    let title = truncate_to_width(&title_raw, chunk_w.saturating_sub(4).max(1));

    let b_branch = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(b_style(FocusField::Branch));
    let inner_branch = b_branch.inner(chunks[0]);
    let text_w = inner_branch.width.saturating_sub(2).max(1);

    let items: Vec<ListItem> = if focus == FocusField::Branch {
        let filtered: Vec<&String> = branches
            .iter()
            .filter(|b| branch_matches_filter(b, filter))
            .collect();
        let nf = filtered.len();
        if nf == 0 {
            branch_list_state.select(None);
        } else {
            let sel = branch_list_state.selected().unwrap_or(0).min(nf - 1);
            branch_list_state.select(Some(sel));
        }
        filtered
            .iter()
            .map(|b| ListItem::new(truncate_to_width(b.as_str(), text_w)))
            .collect()
    } else {
        sync_branch_list_to_head(branch_label, branches, branch_list_state);
        let n = branches.len();
        if n > 0 {
            let sel = branch_list_state.selected().unwrap_or(0).min(n - 1);
            branch_list_state.select(Some(sel));
        } else {
            branch_list_state.select(None);
        }
        branches
            .iter()
            .map(|b| ListItem::new(truncate_to_width(b.as_str(), text_w)))
            .collect()
    };

    let list = List::new(items)
        .block(b_branch)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, chunks[0], branch_list_state);

    let b_status = Block::default()
        .borders(Borders::ALL)
        .title("status")
        .border_style(b_style(FocusField::Status));
    let inner_status = b_status.inner(chunks[1]);
    let tw1 = inner_status.width.max(1);
    let viewport = inner_status.height.max(1) as usize;
    let status_lines_vec: Vec<&str> = status_porcelain.lines().collect();
    let total_lines = status_lines_vec.len();
    if total_lines == 0 {
        *status_selected_line = 0;
        *status_scroll = 0;
    } else {
        *status_selected_line = (*status_selected_line).min(total_lines - 1);
        ensure_status_scroll_visible(
            status_scroll,
            *status_selected_line,
            viewport,
            total_lines,
        );
    }

    let status_items: Vec<ListItem> = status_lines_vec
        .iter()
        .skip(*status_scroll)
        .take(viewport)
        .map(|ln| ListItem::new(truncate_to_width(ln, tw1)))
        .collect();

    let mut status_list_state = ListState::default();
    if total_lines > 0 {
        let rel = (*status_selected_line).saturating_sub(*status_scroll);
        if rel < status_items.len() {
            status_list_state.select(Some(rel));
        }
    }

    let status_list = List::new(status_items)
        .block(b_status)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");

    let p2 = Paragraph::new("stash").block(
        Block::default()
            .borders(Borders::ALL)
            .title("stash")
            .border_style(b_style(FocusField::Stash)),
    );

    frame.render_stateful_widget(status_list, chunks[1], &mut status_list_state);
    frame.render_widget(p2, chunks[2]);
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
