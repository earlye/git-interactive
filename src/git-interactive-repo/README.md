# git-interactive-repo

Interactive branch list and `git status --porcelain` view for a **single** git repository (the same behavior as the repo detail screen in `git interactive repos`).

## Usage

```bash
git interactive repo
git interactive repo /path/to/repo
```

The optional `PATH` defaults to the current directory. The path is canonicalized; the program exits with an error if it is not inside a git work tree.

The UI uses the terminal’s **alternate screen** (full-screen TUI). **Esc** returns to the shell.

This tool is also run by `git interactive repos` when you open a repository from the list (see [git-interactive-repos README](../git-interactive-repos/README.md)).

A future refactor may deduplicate code with `git-interactive-repos` via a shared library; behavior should stay aligned.
