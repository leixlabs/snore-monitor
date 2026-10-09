---
name: land
description: >-
  Lands the current thread's changes into this repository by committing
  outstanding work, rebasing onto origin/main, verifying with cargo test and
  cargo clippy, and pushing main to origin. Use only when the user has
  explicitly requested landing, merging, or shipping the current changes — not
  for reviewing, preparing, or merely running checks.

disable-model-invocation: true
metadata:
  delta-action: land
---

# Land changes

Land the work currently in this worktree into `origin/main` of
`leixlabs/snore-monitor`.

## When this applies

The user has already asked to land these changes (for example by choosing Land
Changes). That request is the merge intent: proceed with the workflow below
do not stop to ask whether they want to merge, and do not ask for permission
again. Stop only for a genuine blocker described under "Stop and report" below.

## Repository facts (verified)

- Remote: `origin` = `git@github.com:leixlabs/snore-monitor.git` (public).
- Default and only branch: `main`, tracking `origin/main`.
- **No CI workflows** (`.github/` does not exist), **no branch protection**
  (`gh api repos/leixlabs/snore-monitor/branches/main/protection` returns 404),
  and **no CONTRIBUTING/policy docs**. Verification is entirely local.
- No pull-request process is in use; landing means pushing `main` directly.
- `gh` is authenticated for `0xkamalei` if a remote check is ever needed.

## Verification commands

Run these from the repository root. They are the project's only gates, and they
are the ones documented in `README.md` under "Development".

```sh
cargo test
cargo clippy --all-targets
```

- `cargo test` must report `ok` for every test binary.
- `cargo clippy --all-targets` must print no `error` or `warning` lines.
- **Do not gate on `cargo fmt --check`.** The repository is not `rustfmt`-clean
  today; formatting differences exist in pre-existing files such as
  `src/audio_capture.rs` and `src/config.rs`. Requiring it would fail for
  reasons unrelated to the change being landed. Formatting is optional and, if
  you do format, restrict it to the files this change touched.

## Workflow

### 1. Inspect the change set

```sh
git --no-optional-locks status --short --branch
git diff --stat
git diff --cached --stat
```

Confirm what is actually outstanding. Do not assume work was already committed.

### 2. Verify before landing

Run the two gates above. If either fails, **do not land**. Fix the failure if
the user's request clearly covers it; otherwise stop and report (see below).

### 3. Stage the work

Stage tracked modifications and the new source files that belong to the change:

```sh
git add -A
```

Then check what is staged and remove anything that should not be committed:

```sh
git status --short
```

Never commit build output or runtime artifacts. These are already ignored by
`.gitignore`, but confirm none appear staged: `target/`, a real `config.toml`,
`*.db`, `*.db-wal`, `*.db-shm`, `*.wav.partial`, `.env*`. If any are staged,
unstage them rather than committing them.

### 4. Commit

Write a message that describes the change. Pass it with `-m` so no editor opens:

```sh
GIT_EDITOR=true git commit -m "<subject>" -m "<optional body>"
```

If nothing is staged and the working tree is clean, the work is already
committed: skip to the next step.

### 5. Rebase onto origin/main

**Always rebase onto `origin/main` before pushing.** This is the user's standing
preference for this repository.

```sh
git fetch origin main
git rebase origin/main
```

Conflicts: resolve them when the intended result is clear, preserving unrelated
work. Automatic resolution must still pause when intent is ambiguous or when a
conflict touches work you do not understand — in that case stop and report
because of your own preference to be asked in unclear cases.

If a conflict cannot be resolved confidently, run `git rebase --abort` to return
to the pre-rebase state, then stop and report. Never force-push and never
discard commits to get past a conflict.

### 6. Push

```sh
git push origin main
```

Do not force-push under any circumstances. If the push is rejected as
non-fast-forward, another push landed since the rebase; fetch and rebase again,
then retry. If it still fails, stop and report.

### 7. Verify the destination

Confirm the commit actually reached the remote:

```sh
git fetch origin main
git rev-parse origin/main
git rev-parse HEAD
```

The two SHAs must match. Report success only when they do.

## Stop and report

Stop and tell the user the changes have **not** landed, with the specific reason,
when:

- `cargo test` or `cargo clippy --all-targets` fails;
- the rebase conflicts in a way you cannot resolve confidently;
- the push is rejected and a re-fetch plus rebase does not clear it;
- the destination SHA does not match after pushing.

Do not report partial progress as a completed landing. Preparing commits,
staging files, or passing local checks is not landing; only a verified push to
`origin/main` is.
