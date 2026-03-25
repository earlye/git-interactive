//! `git interactive repo` — branch and status UI for a single git repository.

use std::io::{self, stdout, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
use unicode_width::UnicodeWidthChar;

#[derive(Parser)]
#[command(name = "git-interactive-repo")]
#[command(about = "Interactive branch and status for a single git repository")]
struct Args {
    /// Path to the repository (default: current directory)
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
enum Row {
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
}

impl FocusField {
    fn next(self) -> Self {
        match self {
            FocusField::Branch => FocusField::Status,
            FocusField::Status => FocusField::Branch,
        }
    }

    fn prev(self) -> Self {
        self.next()
    }
}

enum AppMode {
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
    /// Confirm `git restore --source=HEAD` for a single path (destructive).
    ConfirmFileReset {
        repo_idx: usize,
        rel_path: String,
    },
    /// Confirm `git commit` when the current branch is `main`.
    ConfirmCommitMain {
        repo_idx: usize,
    },
    /// Nothing in the index to commit (`git diff --cached` empty).
    NothingStagedWarning {
        repo_idx: usize,
    },
    /// Append a line to `.gitignore` (buffer is the pattern text).
    GitignoreEdit {
        repo_idx: usize,
        buffer: String,
        /// Character index of the caret in `buffer`.
        cursor_char: usize,
        /// macOS: Option+i / nearby Option chords can enqueue stray symbols (`ˆ`, `¨`, …); skip until real input.
        suppress_macos_option_i_ghost: bool,
    },
    /// Status-column hotkey help (Esc dismisses).
    StatusHelp {
        repo_idx: usize,
    },
    /// Create branch from current HEAD (`git switch -c`); opened from branch panel **Alt+b**.
    BranchCreateEdit {
        repo_idx: usize,
        buffer: String,
        cursor_char: usize,
        /// macOS: Option+b can emit a stray symbol after the chord.
        suppress_macos_option_b_ghost: bool,
    },
}

struct App {
    rows: Vec<Row>,
    mode: AppMode,
    /// Last `frame.area()` from [`Terminal::draw`], for status scroll viewport math.
    last_area: Rect,
    /// Deferred `AppMode` switch (avoids borrowing `mode` while `RepoDetail` fields are borrowed).
    pending_mode: Option<AppMode>,
}

#[derive(Clone)]
enum RunOutcome {
    Quit,
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let raw_path = args.path.unwrap_or_else(|| PathBuf::from("."));
    let path = std::fs::canonicalize(&raw_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("{}: {}", raw_path.display(), e),
        )
    })?;

    if !is_git_repo(&path) {
        eprintln!("Not a git repository: {}", path.display());
        std::process::exit(1);
    }

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let row = probe_repo(name, path);

    let (tw, _) = crossterm::terminal::size()?;
    // inner list width ≈ tw − border; content column ≈ inner − 1 for `>` highlight prefix
    let min_content = tw.saturating_sub(2).saturating_sub(1);
    if min_content < 1 {
        eprintln!("Terminal isn't wide enough to display.");
        return Ok(());
    }

    let rows = vec![row];
    let mode = load_repo_detail(0, FocusField::Branch, rows.as_slice());
    let mut app = App {
        rows,
        mode,
        last_area: Rect::default(),
        pending_mode: None,
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

fn status_scroll_viewport_lines_disjoint(last_area: Rect) -> usize {
    let h = if last_area.height > 0 {
        last_area.height
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

                let App {
                    mode,
                    rows,
                    last_area,
                    pending_mode,
                } = app;
                let status_viewport = status_scroll_viewport_lines_disjoint(*last_area);

                match mode {
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

                    let mut alt_status_handled = false;
                    if *focus == FocusField::Status && key.modifiers.contains(KeyModifiers::ALT) {
                        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                        match key.code {
                            KeyCode::Char('?') => {
                                *pending_mode = Some(AppMode::StatusHelp {
                                    repo_idx: *repo_idx,
                                });
                                alt_status_handled = true;
                            }
                            KeyCode::Char('r') if !shift => {
                                if let Some(rel) = rows
                                    .get(*repo_idx)
                                    .and_then(|r| r.path())
                                    .and_then(|p| {
                                        selected_porcelain_rel_path(p, *status_selected_line)
                                    })
                                {
                                    *pending_mode = Some(AppMode::ConfirmFileReset {
                                        repo_idx: *repo_idx,
                                        rel_path: rel,
                                    });
                                }
                                alt_status_handled = true;
                            }
                            KeyCode::Char('r') | KeyCode::Char('R') if shift => {
                                if let Some(Row::Git {
                                    dirty: true, ..
                                }) = rows.get(*repo_idx)
                                {
                                    *pending_mode = Some(AppMode::ConfirmReset {
                                        repo_idx: *repo_idx,
                                    });
                                }
                                alt_status_handled = true;
                            }
                            KeyCode::Char('a') if !shift => {
                                if let (Some(p), Some(rel)) = (
                                    rows.get(*repo_idx).and_then(|r| r.path()),
                                    rows
                                        .get(*repo_idx)
                                        .and_then(|r| r.path())
                                        .and_then(|p| {
                                            selected_porcelain_rel_path(p, *status_selected_line)
                                        }),
                                ) {
                                    let _ = git_add_path(p, &rel);
                                    if let Some(r) = rows.get_mut(*repo_idx) {
                                        if let Ok(new_row) = refresh_git_row(r) {
                                            *r = new_row;
                                        }
                                    }
                                }
                                alt_status_handled = true;
                            }
                            KeyCode::Char('u') if !shift => {
                                if let (Some(p), Some(rel)) = (
                                    rows.get(*repo_idx).and_then(|r| r.path()),
                                    rows
                                        .get(*repo_idx)
                                        .and_then(|r| r.path())
                                        .and_then(|p| {
                                            selected_porcelain_rel_path(p, *status_selected_line)
                                        }),
                                ) {
                                    let _ = git_unstage_path(p, &rel);
                                    if let Some(r) = rows.get_mut(*repo_idx) {
                                        if let Ok(new_row) = refresh_git_row(r) {
                                            *r = new_row;
                                        }
                                    }
                                }
                                alt_status_handled = true;
                            }
                            KeyCode::Char('d') if !shift => {
                                if let (Some(p), Some(rel)) = (
                                    rows.get(*repo_idx).and_then(|r| r.path()),
                                    rows
                                        .get(*repo_idx)
                                        .and_then(|r| r.path())
                                        .and_then(|p| {
                                            selected_porcelain_rel_path(p, *status_selected_line)
                                        }),
                                ) {
                                    let p = p.to_path_buf();
                                    let rel = rel.clone();
                                    let _ = suspend_tui_run_external(terminal, move || {
                                        git_diff_head_file_pager(&p, &rel)
                                    });
                                    if let Some(r) = rows.get_mut(*repo_idx) {
                                        if let Ok(new_row) = refresh_git_row(r) {
                                            *r = new_row;
                                        }
                                    }
                                }
                                alt_status_handled = true;
                            }
                            KeyCode::Char('i') if !shift => {
                                if let Some(rel) = rows
                                    .get(*repo_idx)
                                    .and_then(|r| r.path())
                                    .and_then(|p| {
                                        selected_porcelain_rel_path(p, *status_selected_line)
                                    })
                                {
                                    let cc = rel.chars().count();
                                    *pending_mode = Some(AppMode::GitignoreEdit {
                                        repo_idx: *repo_idx,
                                        buffer: rel,
                                        cursor_char: cc,
                                        suppress_macos_option_i_ghost: true,
                                    });
                                }
                                alt_status_handled = true;
                            }
                            KeyCode::Char('s') if !shift => {
                                if let Some(p) = rows.get(*repo_idx).and_then(|r| r.path()) {
                                    let _ = git_stash(p);
                                    if let Some(r) = rows.get_mut(*repo_idx) {
                                        if let Ok(new_row) = refresh_git_row(r) {
                                            *r = new_row;
                                        }
                                    }
                                }
                                alt_status_handled = true;
                            }
                            KeyCode::Char('p') if !shift => {
                                if let Some(p) = rows.get(*repo_idx).and_then(|r| r.path()) {
                                    let _ = git_stash_pop(p);
                                    if let Some(r) = rows.get_mut(*repo_idx) {
                                        if let Ok(new_row) = refresh_git_row(r) {
                                            *r = new_row;
                                        }
                                    }
                                }
                                alt_status_handled = true;
                            }
                            KeyCode::Char('c') if !shift => {
                                if let Some(p) = rows.get(*repo_idx).and_then(|r| r.path()) {
                                    match has_staged_changes(p) {
                                        Ok(false) | Err(_) => {
                                            *pending_mode = Some(AppMode::NothingStagedWarning {
                                                repo_idx: *repo_idx,
                                            });
                                        }
                                        Ok(true) => {
                                            let on_main = rows.get(*repo_idx).map_or(false, |r| {
                                                let Row::Git { branch_label, .. } = r;
                                                branch_label == "main"
                                            });
                                            if on_main {
                                                *pending_mode = Some(AppMode::ConfirmCommitMain {
                                                    repo_idx: *repo_idx,
                                                });
                                            } else {
                                                let _ = git_commit_with_message_editor(terminal, p);
                                                if let Some(r) = rows.get_mut(*repo_idx) {
                                                    if let Ok(new_row) = refresh_git_row(r) {
                                                        *r = new_row;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                alt_status_handled = true;
                            }
                            _ => {}
                        }
                    }

                    let mut alt_branch_handled = false;
                    if *focus == FocusField::Branch && key.modifiers.contains(KeyModifiers::ALT) {
                        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                        match key.code {
                            KeyCode::Char('b') if !shift => {
                                *pending_mode = Some(AppMode::BranchCreateEdit {
                                    repo_idx: *repo_idx,
                                    buffer: String::new(),
                                    cursor_char: 0,
                                    suppress_macos_option_b_ghost: true,
                                });
                                alt_branch_handled = true;
                            }
                            _ => {}
                        }
                    }

                    if !alt_status_handled && !alt_branch_handled {
                        match key.code {
                        KeyCode::Esc => {
                            return Ok(RunOutcome::Quit);
                        }
                        KeyCode::Left => {
                            let prev_focus = *focus;
                            *focus = focus.prev();
                            if prev_focus != FocusField::Branch && *focus == FocusField::Branch {
                                if let Some(r) = rows.get(*repo_idx) {
                                    let Row::Git { branch_label, .. } = r;
                                    sync_filtered_selection_from_head(
                                        branch_label,
                                        branches,
                                        filter,
                                        branch_list_state,
                                    );
                                }
                            }
                        }
                        KeyCode::Right => {
                            let prev_focus = *focus;
                            *focus = focus.next();
                            if prev_focus != FocusField::Branch && *focus == FocusField::Branch {
                                if let Some(r) = rows.get(*repo_idx) {
                                    let Row::Git { branch_label, .. } = r;
                                    sync_filtered_selection_from_head(
                                        branch_label,
                                        branches,
                                        filter,
                                        branch_list_state,
                                    );
                                }
                            }
                        }
                        KeyCode::Enter => {
                            let idx = *repo_idx;
                            let row = rows.get(idx).cloned();
                            let Some(row) = row else { continue };
                            let Row::Git { path, .. } = row;
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
                                    if let Some(r) = rows.get_mut(idx) {
                                        if let Ok(new_row) = refresh_git_row(r) {
                                            *r = new_row;
                                        }
                                    }
                                    *branches = list_branches(&path).unwrap_or_default();
                                    filter.clear();
                                    if let Some(r) = rows.get(idx) {
                                        let Row::Git { branch_label, .. } = r;
                                        sync_filtered_selection_from_head(
                                            branch_label,
                                            branches,
                                            filter,
                                            branch_list_state,
                                        );
                                    }
                                }
                                FocusField::Status => {}
                            }
                        }
                        KeyCode::Up => {
                            if *focus == FocusField::Branch && nf > 0 {
                                let sel = branch_list_state
                                    .selected()
                                    .unwrap_or(0)
                                    .min(nf - 1);
                                let new_sel = if sel == 0 { nf - 1 } else { sel - 1 };
                                branch_list_state.select(Some(new_sel));
                            } else if *focus == FocusField::Status {
                                if let Some(path) = rows.get(*repo_idx).and_then(|r| r.path()) {
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
                        KeyCode::Down => {
                            if *focus == FocusField::Branch && nf > 0 {
                                let sel = branch_list_state
                                    .selected()
                                    .unwrap_or(0)
                                    .min(nf - 1);
                                let new_sel = if sel >= nf - 1 { 0 } else { sel + 1 };
                                branch_list_state.select(Some(new_sel));
                            } else if *focus == FocusField::Status {
                                if let Some(path) = rows.get(*repo_idx).and_then(|r| r.path()) {
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
                }
                AppMode::ConfirmReset { repo_idx } => {
                    match key.code {
                        KeyCode::Esc => {
                            *mode = load_repo_detail(*repo_idx, FocusField::Status, rows.as_slice());
                        }
                        KeyCode::Enter => {
                            let path = rows[*repo_idx]
                                .path()
                                .expect("git row")
                                .to_path_buf();
                            let _ = git_reset_hard_and_clean(&path);
                            if let Some(r) = rows.get_mut(*repo_idx) {
                                if let Ok(new_row) = refresh_git_row(r) {
                                    *r = new_row;
                                }
                            }
                            *mode = load_repo_detail(*repo_idx, FocusField::Status, rows.as_slice());
                        }
                        _ => {}
                    }
                }
                AppMode::ConfirmFileReset { repo_idx, rel_path } => {
                    match key.code {
                        KeyCode::Esc => {
                            *mode = load_repo_detail(*repo_idx, FocusField::Status, rows.as_slice());
                        }
                        KeyCode::Enter => {
                            let path = rows[*repo_idx]
                                .path()
                                .expect("git row")
                                .to_path_buf();
                            let rel = rel_path.clone();
                            let _ = git_restore_worktree_from_head(&path, &rel);
                            if let Some(r) = rows.get_mut(*repo_idx) {
                                if let Ok(new_row) = refresh_git_row(r) {
                                    *r = new_row;
                                }
                            }
                            *mode = load_repo_detail(*repo_idx, FocusField::Status, rows.as_slice());
                        }
                        _ => {}
                    }
                }
                AppMode::ConfirmCommitMain { repo_idx } => {
                    match key.code {
                        KeyCode::Esc => {
                            *mode = load_repo_detail(*repo_idx, FocusField::Status, rows.as_slice());
                        }
                        KeyCode::Enter => {
                            let path = rows[*repo_idx]
                                .path()
                                .expect("git row")
                                .to_path_buf();
                            match has_staged_changes(&path) {
                                Ok(false) | Err(_) => {
                                    *mode = AppMode::NothingStagedWarning {
                                        repo_idx: *repo_idx,
                                    };
                                }
                                Ok(true) => {
                                    let _ = git_commit_with_message_editor(terminal, &path);
                                    if let Some(r) = rows.get_mut(*repo_idx) {
                                        if let Ok(new_row) = refresh_git_row(r) {
                                            *r = new_row;
                                        }
                                    }
                                    *mode = load_repo_detail(
                                        *repo_idx,
                                        FocusField::Status,
                                        rows.as_slice(),
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                }
                AppMode::NothingStagedWarning { repo_idx } => {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                            *mode = load_repo_detail(*repo_idx, FocusField::Status, rows.as_slice());
                        }
                        _ => {}
                    }
                }
                AppMode::GitignoreEdit {
                    repo_idx,
                    buffer,
                    cursor_char,
                    suppress_macos_option_i_ghost,
                } => {
                    match key.code {
                        KeyCode::Esc => {
                            *mode = load_repo_detail(*repo_idx, FocusField::Status, rows.as_slice());
                        }
                        KeyCode::Enter => {
                            let path = rows[*repo_idx]
                                .path()
                                .expect("git row")
                                .to_path_buf();
                            let _ = append_gitignore_line(
                                &path,
                                trim_gitignore_trailing_ghosts(buffer),
                            );
                            if let Some(r) = rows.get_mut(*repo_idx) {
                                if let Ok(new_row) = refresh_git_row(r) {
                                    *r = new_row;
                                }
                            }
                            *mode = load_repo_detail(*repo_idx, FocusField::Status, rows.as_slice());
                        }
                        KeyCode::Left => {
                            if *cursor_char > 0 {
                                *cursor_char -= 1;
                            }
                        }
                        KeyCode::Right => {
                            let n = buffer.chars().count();
                            if *cursor_char < n {
                                *cursor_char += 1;
                            }
                        }
                        KeyCode::Backspace => {
                            gitignore_delete_before(buffer, cursor_char);
                        }
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            if *suppress_macos_option_i_ghost {
                                if gitignore_macos_option_ghost_char(c) {
                                    // Drop queued stray keys; keep suppress until we see real input.
                                } else {
                                    *suppress_macos_option_i_ghost = false;
                                    gitignore_insert_char(buffer, cursor_char, c);
                                }
                            } else {
                                gitignore_insert_char(buffer, cursor_char, c);
                            }
                        }
                        _ => {}
                    }
                }
                AppMode::StatusHelp { repo_idx } => {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => {
                            *mode = load_repo_detail(*repo_idx, FocusField::Status, rows.as_slice());
                        }
                        _ => {}
                    }
                }
                AppMode::BranchCreateEdit {
                    repo_idx,
                    buffer,
                    cursor_char,
                    suppress_macos_option_b_ghost,
                } => {
                    match key.code {
                        KeyCode::Esc => {
                            *mode = load_repo_detail(*repo_idx, FocusField::Branch, rows.as_slice());
                        }
                        KeyCode::Enter => {
                            let path = rows[*repo_idx]
                                .path()
                                .expect("git row")
                                .to_path_buf();
                            let name = buffer.trim();
                            if !name.is_empty() {
                                let _ = git_switch_new_branch(&path, name);
                            }
                            if let Some(r) = rows.get_mut(*repo_idx) {
                                if let Ok(new_row) = refresh_git_row(r) {
                                    *r = new_row;
                                }
                            }
                            *mode = load_repo_detail(*repo_idx, FocusField::Branch, rows.as_slice());
                        }
                        KeyCode::Left => {
                            if *cursor_char > 0 {
                                *cursor_char -= 1;
                            }
                        }
                        KeyCode::Right => {
                            let n = buffer.chars().count();
                            if *cursor_char < n {
                                *cursor_char += 1;
                            }
                        }
                        KeyCode::Backspace => {
                            gitignore_delete_before(buffer, cursor_char);
                        }
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            if *suppress_macos_option_b_ghost {
                                *suppress_macos_option_b_ghost = false;
                                // macOS: Option+b can emit U+222B (∫) as a follow-up key.
                                if c == '\u{222b}' {
                                    // skip insert
                                } else {
                                    gitignore_insert_char(buffer, cursor_char, c);
                                }
                            } else {
                                gitignore_insert_char(buffer, cursor_char, c);
                            }
                        }
                        _ => {}
                    }
                }
                }
                if let Some(m) = pending_mode.take() {
                    *mode = m;
                }
                needs_draw = true;
            }
            _ => {}
        }
    }
}

impl Row {
    fn path(&self) -> Option<&Path> {
        match self {
            Row::Git { path, .. } => Some(path.as_path()),
        }
    }
}

fn probe_repo(name: String, path: PathBuf) -> Row {
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
    let Row::Git {
        name,
        path,
        branch_label: _,
        dirty: _,
    } = row;
    let branch_label = branch_display(path).unwrap_or_else(|_| "<error>".to_string());
    let dirty = is_dirty(path).unwrap_or(false);
    Ok(Row::Git {
        name: name.clone(),
        path: path.clone(),
        branch_label,
        dirty,
    })
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

/// True when the index column (first porcelain character) shows a staged change.
/// Workspace-only changes (` M`), untracked (`?…`), and ignored lines use false.
fn porcelain_line_has_staged_index(line: &str) -> bool {
    let x = line.chars().next().unwrap_or(' ');
    if x == '?' {
        return false;
    }
    x != ' '
}

/// Path segment from a v1 `git status --porcelain` line (two status chars, space, then path or rename).
fn parse_porcelain_path(line: &str) -> Option<String> {
    let line = line.trim_end();
    let mut chars = line.chars();
    let _ = chars.next()?;
    let _ = chars.next()?;
    let sp = chars.next()?;
    if sp != ' ' {
        return None;
    }
    let rest: String = chars.collect();
    let rest = rest.trim_start();
    if rest.is_empty() {
        return None;
    }
    if rest.contains(" -> ") {
        rest.split(" -> ").last().map(|s| s.trim().to_string())
    } else {
        Some(rest.to_string())
    }
}

fn porcelain_line_at(path: &Path, line_index: usize) -> io::Result<Option<String>> {
    let s = git_status_porcelain(path)?;
    Ok(s.lines().nth(line_index).map(str::to_string))
}

fn selected_porcelain_rel_path(repo_root: &Path, line_index: usize) -> Option<String> {
    let line = porcelain_line_at(repo_root, line_index).ok().flatten()?;
    parse_porcelain_path(&line)
}

/// Characters macOS may emit after Option+i / adjacent Option chords (dead keys, accents).
fn gitignore_macos_option_ghost_char(c: char) -> bool {
    matches!(
        c,
        '\u{02c6}' // ˆ circumflex (Option+i)
            | '\u{00a8}' // ¨ diaeresis (Option+u)
            | '^'
            | '\u{00b4}' // ´ acute
            | '\u{02dc}' // ˜ tilde
            | '\u{0060}' // ` grave
    )
}

/// Strip trailing stray symbols so a bad paste/queue cannot write `yarn.lockˆ` into `.gitignore`.
fn trim_gitignore_trailing_ghosts(s: &str) -> &str {
    let s = s.trim();
    let mut t = s;
    loop {
        let Some(last) = t.chars().rev().next() else {
            break;
        };
        if gitignore_macos_option_ghost_char(last) {
            t = t.trim_end_matches(last);
        } else {
            break;
        }
    }
    t
}

fn append_gitignore_line(repo_root: &Path, line: &str) -> io::Result<()> {
    let p = repo_root.join(".gitignore");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)?;
    let t = line.trim();
    if t.is_empty() {
        return Ok(());
    }
    writeln!(f, "{t}")?;
    Ok(())
}

fn gitignore_insert_char(buffer: &mut String, cursor_char: &mut usize, c: char) {
    let mut v: Vec<char> = buffer.chars().collect();
    let i = (*cursor_char).min(v.len());
    v.insert(i, c);
    *buffer = v.iter().collect();
    *cursor_char += 1;
}

fn gitignore_delete_before(buffer: &mut String, cursor_char: &mut usize) {
    if *cursor_char == 0 {
        return;
    }
    let mut v: Vec<char> = buffer.chars().collect();
    v.remove(*cursor_char - 1);
    *buffer = v.iter().collect();
    *cursor_char -= 1;
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

/// `* ` marks HEAD; two spaces keep branch names aligned with the highlight column.
fn format_branch_list_line(branch_name: &str, branch_label: &str, content_width: u16) -> String {
    let is_head = !branch_label.starts_with("<detached") && branch_name == branch_label;
    let prefix = if is_head { "* " } else { "  " };
    let line = format!("{prefix}{branch_name}");
    truncate_to_width(&line, content_width)
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
        let Row::Git { branch_label, .. } = r;
        sync_filtered_selection_from_head(branch_label, &branches, &filter, &mut branch_list_state);
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

/// Create and switch to a new branch from the current HEAD (`git switch -c`).
fn git_switch_new_branch(path: &Path, new_branch: &str) -> io::Result<()> {
    let status = git_cmd_status(path, &["switch", "-c", new_branch])?;
    if !status.success() {
        return Err(io::Error::other("git switch -c failed"));
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

/// `true` if something is staged for commit (`git diff --cached` is non-empty).
fn has_staged_changes(repo: &Path) -> io::Result<bool> {
    let s = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--cached", "--quiet"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    match s.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(io::Error::other("git diff --cached")),
    }
}

/// Standard Git comment block (we set `commit.status=false` so Git does not add its own).
const COMMIT_EDITOR_HELP: &str = r#"# Please enter the commit message for your changes. Lines starting
# with '#' will be ignored, and an empty message aborts the commit.
#
"#;

/// Prefix each line of `git diff` output with `# ` for the commit message buffer.
fn comment_diff_lines(diff: &str) -> String {
    diff.lines()
        .map(|line| format!("# {line}\n"))
        .collect()
}

/// Builds the initial `git commit -e -m` text: help block, `# <branch>`, then commented `git diff` output.
fn build_commit_initial_message(repo: &Path) -> io::Result<String> {
    let branch_o = git_c(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if !branch_o.status.success() {
        return Err(io::Error::other("git rev-parse --abbrev-ref HEAD failed"));
    }
    let branch = String::from_utf8_lossy(&branch_o.stdout).trim().to_string();
    if branch.is_empty() {
        return Err(io::Error::other("empty branch ref"));
    }

    let diff_o = git_c(repo, &["--no-pager", "diff", "--cached"])?;
    if !diff_o.status.success() {
        return Err(io::Error::other("git diff failed"));
    }
    let diff_raw = String::from_utf8_lossy(&diff_o.stdout);
    let diff_commented = comment_diff_lines(&diff_raw);

    Ok(format!(
        "{COMMIT_EDITOR_HELP}# {branch}\n{diff_commented}"
    ))
}

/// `git commit -c commit.status=false -e -m <msg>` with `<msg>` from [`build_commit_initial_message`].
fn git_commit_with_message_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    repo: &Path,
) -> io::Result<()> {
    let msg = build_commit_initial_message(repo)?;
    let repo = repo.to_path_buf();
    suspend_tui_run_external(terminal, move || {
        let s = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("-c")
            .arg("commit.status=false")
            .args(["commit", "-e", "-m", &msg])
            .status()?;
        if !s.success() {
            return Err(io::Error::other("git commit failed"));
        }
        Ok(())
    })
}

fn git_stash_pop(path: &Path) -> io::Result<()> {
    let status = git_cmd_status(path, &["stash", "pop"])?;
    if !status.success() {
        return Err(io::Error::other("git stash pop failed"));
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

fn git_add_path(repo: &Path, rel: &str) -> io::Result<()> {
    let s = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["add", "--"])
        .arg(rel)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !s.success() {
        return Err(io::Error::other("git add failed"));
    }
    Ok(())
}

fn git_restore_worktree_from_head(repo: &Path, rel: &str) -> io::Result<()> {
    let s = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["restore", "--source=HEAD", "--"])
        .arg(rel)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !s.success() {
        return Err(io::Error::other("git restore failed"));
    }
    Ok(())
}

fn git_unstage_path(repo: &Path, rel: &str) -> io::Result<()> {
    let s = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["restore", "--staged", "--"])
        .arg(rel)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !s.success() {
        return Err(io::Error::other("git restore --staged failed"));
    }
    Ok(())
}

/// Run `git --no-pager diff HEAD -- <rel>` with stdout piped into `less`.
///
/// `--no-pager` is a top-level `git` option (not `git diff --no-pager`, which is invalid).
/// Piping through `less` with `LESS` cleared keeps even one-screen diffs visible until the user
/// exits the pager.
fn git_diff_head_file_pager(repo: &Path, rel: &str) -> io::Result<()> {
    let mut git = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("--no-pager")
        .args(["diff", "HEAD", "--"])
        .arg(rel)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let stdout = git.stdout.take().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "git diff: missing stdout pipe")
    })?;

    let _less_status = Command::new("less")
        .arg("-R")
        .env_remove("LESS")
        .stdin(stdout)
        .status()?;

    let git_status = git.wait()?;
    if !git_status.success() {
        return Err(io::Error::other("git diff failed"));
    }
    Ok(())
}

fn suspend_tui_run_external(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    f: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    execute!(stdout(), LeaveAlternateScreen)?;
    disable_raw_mode()?;
    let r = f();
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    terminal.clear()?;
    r
}

fn draw(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    match &mut app.mode {
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
        AppMode::ConfirmFileReset { repo_idx, rel_path } => {
            draw_confirm_file_reset(frame, area, &app.rows, *repo_idx, rel_path);
        }
        AppMode::ConfirmCommitMain { repo_idx } => {
            draw_confirm_commit_main(frame, area, &app.rows, *repo_idx);
        }
        AppMode::NothingStagedWarning { repo_idx } => {
            draw_nothing_staged_warning(frame, area, &app.rows, *repo_idx);
        }
        AppMode::GitignoreEdit {
            repo_idx,
            buffer,
            cursor_char,
            suppress_macos_option_i_ghost: _,
        } => draw_gitignore_edit(frame, area, &app.rows, *repo_idx, buffer, *cursor_char),
        AppMode::StatusHelp { repo_idx } => draw_status_help(frame, area, &app.rows, *repo_idx),
        AppMode::BranchCreateEdit {
            repo_idx,
            buffer,
            cursor_char,
            suppress_macos_option_b_ghost: _,
        } => draw_branch_create_edit(frame, area, &app.rows, *repo_idx, buffer, *cursor_char),
    }
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
    } = row;

    let status_porcelain = git_status_porcelain(path).unwrap_or_else(|_| "<error>".to_string());

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
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
            .map(|b| {
                ListItem::new(format_branch_list_line(b.as_str(), branch_label, text_w))
            })
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
            .map(|b| {
                ListItem::new(format_branch_list_line(b.as_str(), branch_label, text_w))
            })
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
        .map(|ln| {
            let prefix = if porcelain_line_has_staged_index(ln) {
                'º'
            } else {
                ' '
            };
            let display = format!("{prefix}{ln}");
            ListItem::new(truncate_to_width(&display, tw1))
        })
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

    frame.render_stateful_widget(status_list, chunks[1], &mut status_list_state);
}

fn draw_confirm_reset(frame: &mut Frame<'_>, area: Rect, rows: &[Row], repo_idx: usize) {
    let name = rows
        .get(repo_idx)
        .map(|r| {
            let Row::Git { name, .. } = r;
            name.as_str()
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

fn draw_confirm_file_reset(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[Row],
    repo_idx: usize,
    rel_path: &str,
) {
    let name = rows
        .get(repo_idx)
        .map(|r| {
            let Row::Git { name, .. } = r;
            name.as_str()
        })
        .unwrap_or("?");
    let text = format!(
        "Discard local changes to this file (git restore --source=HEAD)?\n\nRepo: {}\nFile: {}\n\nEnter = confirm   Esc = cancel",
        name, rel_path
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title("confirm file reset");
    let tw = block.inner(area).width.max(1);
    let p = Paragraph::new(truncate_lines(&text, tw)).block(block);
    frame.render_widget(p, area);
}

fn draw_confirm_commit_main(frame: &mut Frame<'_>, area: Rect, rows: &[Row], repo_idx: usize) {
    let name = rows
        .get(repo_idx)
        .map(|r| {
            let Row::Git { name, .. } = r;
            name.as_str()
        })
        .unwrap_or("?");
    let text = format!(
        "You are on branch main. Committing directly to main is often discouraged.\n\n\
         Proceed? The editor opens with a template: Git's default status block is off\n\
         (commit.status=false); you get a short help comment, `# main`, then `git diff` with each line commented.\n\
         Add your real message above or among those lines; all-comment aborts.\n\n\
         Repo: {}\n\n\
         Enter = continue   Esc = cancel",
        name
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title("confirm commit on main");
    let tw = block.inner(area).width.max(1);
    let p = Paragraph::new(truncate_lines(&text, tw)).block(block);
    frame.render_widget(p, area);
}

fn draw_nothing_staged_warning(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[Row],
    repo_idx: usize,
) {
    let name = rows
        .get(repo_idx)
        .map(|r| {
            let Row::Git { name, .. } = r;
            name.as_str()
        })
        .unwrap_or("?");
    let text = format!(
        "Nothing staged to commit.\n\n\
         Stage changes first (e.g. Alt+a on a line in status), then try Alt+c again.\n\n\
         Repo: {}\n\n\
         Enter / Esc / q  dismiss",
        name
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title("nothing staged");
    let tw = block.inner(area).width.max(1);
    let p = Paragraph::new(truncate_lines(&text, tw)).block(block);
    frame.render_widget(p, area);
}

fn gitignore_buffer_display(buf: &str, cursor_char: usize) -> String {
    let chars: Vec<char> = buf.chars().collect();
    let n = chars.len();
    let c = cursor_char.min(n);
    let mut out = String::new();
    for (i, ch) in chars.iter().enumerate() {
        if i == c {
            out.push('▏');
        }
        out.push(*ch);
    }
    if c >= n {
        out.push('▏');
    }
    out
}

fn draw_gitignore_edit(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[Row],
    repo_idx: usize,
    buffer: &str,
    cursor_char: usize,
) {
    let name = rows
        .get(repo_idx)
        .map(|r| {
            let Row::Git { name, .. } = r;
            name.as_str()
        })
        .unwrap_or("?");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("add to .gitignore ({})", name));
    let tw = block.inner(area).width.max(1);
    let body = format!(
        "Edit pattern (▏ = caret). Example: narrow a path to *.log\n\n{}",
        truncate_lines(&gitignore_buffer_display(buffer, cursor_char), tw)
    );
    let p = Paragraph::new(truncate_lines(&body, tw)).block(block);
    frame.render_widget(p, area);
}

fn draw_branch_create_edit(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[Row],
    repo_idx: usize,
    buffer: &str,
    cursor_char: usize,
) {
    let name = rows
        .get(repo_idx)
        .map(|r| {
            let Row::Git { name, .. } = r;
            name.as_str()
        })
        .unwrap_or("?");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("new branch ({})", name));
    let tw = block.inner(area).width.max(1);
    let body = format!(
        "git switch -c <name> from current HEAD (▏ = caret)\n\n{}",
        truncate_lines(&gitignore_buffer_display(buffer, cursor_char), tw)
    );
    let p = Paragraph::new(truncate_lines(&body, tw)).block(block);
    frame.render_widget(p, area);
}

fn draw_status_help(frame: &mut Frame<'_>, area: Rect, rows: &[Row], repo_idx: usize) {
    let name = rows
        .get(repo_idx)
        .map(|r| {
            let Row::Git { name, .. } = r;
            name.as_str()
        })
        .unwrap_or("?");
    let text = format!(
        "Status column hotkeys ({})\n\n\
         Alt+?     this help\n\
         Alt+r     reset file to HEAD (confirm)\n\
         Alt+Shift+r  reset whole repo: reset --hard && clean -fd (confirm)\n\
         Alt+a     git add (stage) selected path\n\
         Alt+u     unstage (restore --staged)\n\
         Alt+d     git --no-pager diff | less (file)\n\
         Alt+i     add pattern to .gitignore\n\
         Alt+s     git stash push\n\
         Alt+p     git stash pop\n\
         Alt+c     commit (staged; template + commented diff; commit.status=false)\n\n\
         Esc or q  dismiss",
        name
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title("status help");
    let tw = block.inner(area).width.max(1);
    let p = Paragraph::new(truncate_lines(&text, tw)).block(block);
    frame.render_widget(p, area);
}
