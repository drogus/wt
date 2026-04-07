# wt

A git worktree manager with tmux integration and fuzzy search.

`wt` organizes worktrees in a sibling directory next to your main repo
(e.g. `myproject-worktrees/`) and optionally manages a tmux session per
worktree so each branch gets its own terminal workspace.

## Install

```
make install
```

This builds the binary and installs it to `~/bin/`. Make sure `~/bin` is in
your `PATH`.

## Shell integration

`wt` prints worktree paths to stdout so a shell wrapper can `cd` into them.
Add the following to your `~/.bashrc` or `~/.zshrc`:

```sh
eval "$(wt --shell-init)"
```

Run `wt --shell-init` to see what the function looks like.

## Usage

### Interactive selection

Run `wt` with no arguments to get a fuzzy-searchable list of existing
worktrees:

```
wt
```

### Switch to a worktree

```
wt my-feature
```

If no worktree exists for the branch, `wt` will offer to create one.

### Create a new worktree

```
wt -c my-feature
```

This fetches origin, creates a worktree branched from the default branch
(usually `origin/main`), and `cd`s into it.

Use `-b` to specify a different base branch:

```
wt -c my-feature -b origin/develop
```

### Remove a worktree

```
wt -r my-feature
```

Removes the worktree directory and kills the associated tmux session. Prompts
for confirmation if there are uncommitted changes.

### Rename a branch and its worktree

```
wt rename new-name
wt rename new-name --from old-name
```

Renames the git branch, moves the worktree directory, and renames the tmux
session. Defaults to the current branch if `--from` is not specified.

### Destroy a branch completely

```
wt destroy my-feature
```

Removes the worktree, deletes the local branch, deletes the remote branch
(if it exists), and kills the tmux session. Defaults to the current branch
if no argument is given.

Run `wt --help` for the full list of options.

## How it works

- Worktrees are stored in `<repo>-worktrees/` next to your main repo directory.
  For example, if your repo is at `~/code/myproject`, worktrees go in
  `~/code/myproject-worktrees/`.
- Branch names with `/` are converted to `-` for directory names
  (e.g. `feature/foo` becomes `feature-foo`).
- When inside tmux, each worktree gets a dedicated session named
  `<repo>-<branch>`. Switching worktrees switches tmux sessions.
- The base branch for new worktrees is auto-detected from `origin/HEAD`,
  falling back to `origin/main` or `origin/master`.
