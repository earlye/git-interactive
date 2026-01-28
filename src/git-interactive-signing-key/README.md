# git-interactive-signing-key

Interactively select a GPG signing key for git commits.

## Usage

```bash
git interactive signing-key [--local|--global]
```

## Options

| Flag | Description |
|------|-------------|
| `--local` | Set `user.signingkey` in local repo config (default) |
| `--global` | Set `user.signingkey` in global git config |

## Behavior

1. Lists available GPG secret keys (from `gpg --list-secret-keys`)
2. Highlights the currently configured signing key (if any)
3. Navigate with `↑`/`↓` arrow keys or `j`/`k` (vi-style)
4. Press `Enter` to select and update git config
5. Press `Ctrl+C` to cancel without changes

## Example

```
GPG Signing Keys (--local)

  ABC123... John Doe <john@example.com> [expires: 2025-12-01]
> DEF456... John Doe <john@work.com> [expires: 2026-03-15]  ← current
  GHI789... Alt Identity <alt@example.com> [expires: 2024-06-01]

[↑↓] Navigate  [Enter] Select  [Ctrl+C] Cancel
```
