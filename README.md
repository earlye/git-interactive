# git-interactive

Extensible interactive Git TUI, organized as a monorepo.

## Plugin Architecture

Follows Git's subcommand discovery pattern:

```
git interactive signing-key
     │
     └─► git-interactive signing-key
              │
              └─► git-interactive-signing-key
```

Any `git-interactive-*` executable in `$PATH` becomes a subcommand.

## Project Structure

```
git-interactive/
├── dist/                             # Distribution root
│   ├── bin/                          # Executables
│   └── conf/                         # Default configs
├── src/
│   ├── git-interactive/              # Main dispatcher
│   └── git-interactive-signing-key/  # GPG signing key selector
```

## Installation

```bash
# Add to PATH directly
export PATH="$PATH:/path/to/git-interactive/dist/bin"

# Or symlink and add
ln -s /path/to/git-interactive/dist ~/.git-interactive
export PATH="$PATH:$HOME/.git-interactive/bin"
```

## Creating Plugins

Create an executable named `git-interactive-<name>` and place it in `$PATH`. It will be invoked when running `git interactive <name>`.

## Current plugins

* [git interactive signing-key](src/git-interactive-signing-key/README.md) - interactively change signing key for current repo (default) or globally (--global)
