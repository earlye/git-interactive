# git-interactive-repos

Browse immediate subdirectories of the current directory, show git branch and dirty state, and open a single-repo UI for any listed git workspace.

## Usage

```bash
git interactive repos
```

Run from a directory whose child folders you want to inspect (for example a parent folder that contains several clones).

The UI uses the terminal’s **alternate screen** (full-screen TUI). Your shell session is restored after you quit.

On the top-level list, the first column after the selection marker is a **status character**: space (clean git), `*` (dirty), `%` (still scanning), `!` (not a git repo). The branch column shows the current branch (or `<scanning>` / `<not-git>`). Long names are elided to fit the terminal (60% width for the directory name, remainder for the branch column; middle elision with `…` when there is room).

If the terminal is too narrow to show at least one content column, the program exits with an error before starting the UI.

## Opening a repository

**Enter** on a **git** row runs [`git interactive repo`](../git-interactive-repo/README.md) with that directory’s path (the `git-interactive-repo` binary must be on `PATH`, as with the rest of this toolkit). The parent list suspends its TUI while the child runs; when you quit the repo view (**Esc** or **q** as documented there), you return to the repo list with an updated row.

Rows still **scanning** ignore **Enter**. Non-git directories ignore **Enter** on the top-level list.

## Keys (top level)

| Keys | Action |
|------|--------|
| **Arrow Up/Down** | Move selection |
| **Enter** | Open `git interactive repo` for the selected git directory |
| **q** / **Esc** | Quit |
| **Ctrl+C** | Quit |

## Requirements

- `git` on `PATH`
- `git-interactive-repo` on `PATH` (installed with the same distribution as this binary)
