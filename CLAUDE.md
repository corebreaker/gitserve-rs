# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`gitserve` is a minimal HTTP server exposing local bare Git repositories over the smart HTTP protocol,
so they can be cloned/pushed with `git clone http://localhost:8085/<repo>` without a public forge.
It is the Rust translation of the Go project [gitserve](https://github.com/corebreaker/gitserve):
its behavior, command line and archive format are the reference, and the Go ecosystem is replaced by the Rust one.
The repositories live in a store: a directory, or a zip archive loaded in memory.

The goal is decentralized collaboration: bare repositories carried on a USB key or any other medium,
served with zero setup so that collaborators can clone and push without a forge. Simplicity is the point.
Security is deliberately out of scope: no authentication, no access control,
and listening on all interfaces is intended. Do not add them, and do not flag their absence in reviews.

The crate holds a library (`src/lib.rs`) and the binary (`src/main.rs`), which only calls the library.
The translation is in progress: the Git library that implements the protocol is not chosen yet,
and the Architecture section below will describe it once it is.

## Language

Everything written into the project is in English: code, comments, documentation, commit messages.
Conversation with the user happens in French.

## Commands

```bash
cargo build                                       # build the debug binary in target/debug
cargo fmt --all                                   # format the sources
cargo clippy --all-targets -- -D warnings         # static checks, warnings included
cargo test                                        # all tests
cargo test <filter>                               # run only the tests whose name contains <filter>
```

Sources follow standard Rust formatting: run `cargo fmt --all` before committing, the CI rejects unformatted code,
as well as any clippy warning. Lines stay within 120 columns, code included.

The server runs natively on this Windows machine.
In Git Bash, which also runs the commands that the user types with `!`, prefix commands with `MSYS_NO_PATHCONV=1`,
otherwise an argument such as `/git` becomes a Windows path. Keep the commands given to the user short:
a long one may break into several lines when pasted.
Run a job of the CI in its image to reproduce a failure
(`~/project` already exists in the image, hence the copy to `~/work`):

```bash
MSYS_NO_PATHCONV=1 docker run --rm -e CI=true -v "$(pwd -W):/src:ro" cimg/rust:1.100 \
  bash -c 'cp -r /src ~/work && cd ~/work && rm -rf target && cargo test --locked'
```

Lint the GitHub workflows with actionlint, which also runs shellcheck on their scripts:

```bash
MSYS_NO_PATHCONV=1 docker run --rm -v "$(pwd -W):/repo" --workdir /repo rhysd/actionlint:latest
```

## CI

- CircleCI (`.circleci/config.yml`) runs three jobs on `cimg/rust`: `lint` (`cargo fmt --check`, `cargo clippy`),
  `build` and `test`, all with `--locked`. Images are pulled with `$DOCKERHUB_ACCESS_TOKEN`.
- Branch protection on `main` should require the `all-checks` check from the `circleci-checks` app (id 18001),
  named after the CircleCI workflow. Renaming that workflow blocks every merge
  until the required check is updated in the protection of `main`.
- CircleCI logs need a CircleCI token, and `gh pr checks` only gives the outcome:
  reproduce a failing job in the image of the CI (see Commands).
- `.github/workflows/claude-review.yml` runs a Claude review when the repository owner starts a PR comment with
  `/claude review`, which posts a review only, or `/claude review and approve`, which also approves when no Critical
  or Important point is found. It needs the `BOT_APP_ID`, `BOT_APP_KEY` and `CLAUDE_CODE_OAUTH_TOKEN` secrets.
  GitHub runs it from `main` only: changes to the workflow take effect once merged,
  and the PR that carries them is reviewed by the previous version.
- To check a change of the review workflow once merged, open a draft PR holding an empty commit
  (`git commit --allow-empty`), comment each command, and close the PR without merging once its run is over;
  a comment review still lands on a PR closed during its run.
- Auto mode may refuse to validate, commit or push a change of the review workflow, which governs the approval
  of `main`: prepare the change, and give the user the commands to run.
- claude-code-action forbids Claude to submit reviews: Claude only writes `review.json`, and a step of the workflow
  posts it, pinned to the reviewed commit, with an event that the job allows. That step stays inline in the workflow,
  and a `review.json` shipped by the PR is removed beforehand: the jobs check out the head of the PR,
  whose files must never decide an approval.

## Pull requests

- A change goes through an issue, a branch linked to it, and a PR that closes the issue:
  `gh issue develop <n> --base main --name <branch> --checkout` creates and checks out the linked branch.
- Write each paragraph and bullet of an issue, PR or comment on one line: GitHub renders their line breaks.
- Record lasting lessons in CLAUDE.md within the PR of the work that taught them, never in a PR of their own:
  each PR costs a full review cycle.
- The approval of the review bot satisfies the protection of `main`, but any push dismisses it:
  after the last push, comment again `MSYS_NO_PATHCONV=1 gh pr comment <pr> --body "/claude review and approve"`.
  Without the variable, Git Bash turns the command into `C:/env/git/claude review and approve`.
- To follow a review, take the first run of `gh run list --workflow claude-review.yml --event issue_comment`
  created after the command, since each comment of the bot starts a skipped run too.
  Poll `gh run view <id> --json status` until it completes: `gh run watch` may return before the end of the run.
- Answer each inline review comment in its thread with `Fixed in <sha>`, which the next review counts as resolved:
  `gh api -X POST repos/corebreaker/gitserve-rs/pulls/<pr>/comments/<id>/replies -F body=@<file>`.
  The merge also needs every thread resolved, through the `resolveReviewThread` GraphQL mutation
  on the thread ids that the `reviewThreads` of the PR give.
- Merge with `gh pr merge <pr> --merge`, a merge commit, or with `--squash` when the user asks to combine commits,
  then delete the branch, which GitHub keeps after a merge. After a squash, `git branch -d` refuses the branch:
  delete it with `-D` once `git diff --quiet origin/main <branch>` confirms that `main` holds the same tree.

## Architecture

To be written as the translation progresses. The Git protocol itself must come from a library:
gitserve only routes the smart HTTP endpoints (`info/refs`, `git-upload-pack`, `git-receive-pack`) to it.
