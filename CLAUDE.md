# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`gitserve` is a minimal HTTP server exposing local bare Git repositories over the smart HTTP protocol,
so they can be cloned/pushed with `git clone http://localhost:8085/<repo>` without a public forge.
It is the Rust translation of the Go project [gitserve](https://github.com/corebreaker/gitserve):
its behavior, command line and archive format are the reference, and the Go ecosystem is replaced by the Rust one.
The repositories live in a store: a directory, or a zip archive, extracted to a temporary directory.

The goal is decentralized collaboration: bare repositories carried on a USB key or any other medium,
served with zero setup so that collaborators can clone and push without a forge. Simplicity is the point.
Security is deliberately out of scope: no authentication, no access control,
and listening on all interfaces is intended. Do not add them, and do not flag their absence in reviews.

The crate holds a library (`src/lib.rs`) and the binary (`src/main.rs`, command line), one module per concern,
each with its unit tests: `server.rs` (HTTP), `store.rs` (stores), `archive.rs` (zip archives),
`repository.rs` (repositories) and `ingest.rs` (pushed packs). `tests/e2e.rs` drives the real `git` client
against the server; `tests/fixtures/go-store.zip` is an archive made by the Go project (see `GO_FIXTURE_HEAD`).
It covers the whole Go project: stores in directories and zip archives, the commands, and the graceful shutdown.

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
cargo run -- serve gitroot                        # serve ./gitroot on port 8085 (foreground)
cargo run -- serve -p 9000 --prefix git gitroot   # serve it on port 9000, under the URL path /git/
cargo run -- init repos                           # create an empty store
cargo run -- create repos.zip team/tools          # create a repository in a zip archive store, created if needed
cargo run -- create -b master gitroot team/tools  # create a bare repository, HEAD on master (main by default)
cargo run -- delete gitroot team/tools            # delete a repository, also found with a .git suffix
cargo run -- list gitroot                         # list the repositories of a store
```

The end-to-end tests need `git`: they are skipped without it, except on CI (when `CI` is set).
The log level follows `RUST_LOG`, `info` by default.

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

- Git itself is not implemented here. gitoxide (`gix`) has no server side, so the protocol comes from `gunnar-wire`,
  the server-side state machines of `upload-pack` (protocol v2, with a v0 fallback) and `receive-pack` (v0),
  run in stateless-rpc mode (`Session::StatelessRpc`), and the storage from `storage-git-gix`,
  whose `GixStore` reads and writes an ordinary bare repository with `gix-odb` and `gix-ref`.
  `gunnar-wire` and `gunnar-store` are licensed under AGPL-3.0-only, compatible with the GPL-3.0 of gitserve:
  the combined program must offer its source to its network users, which the public repository does.
- `server.rs`: `route` maps the smart HTTP endpoints (`info/refs`, `git-upload-pack`, `git-receive-pack`)
  found under the URL prefix, which it strips, to a `GitRequest` whose name it percent-decodes;
  the dumb protocol is refused with a 403. `handle` reads the whole request before accessing the store,
  then gunzips it when needed and runs the service in `spawn_blocking`, since both block,
  and sends the response once the access is over:
  no network transfer ever happens while a repository is locked, so a slow client cannot stall the others.
  When a fetch or a push fails after writing a response, that response is still sent, since it reports the failure
  to the client; a failed advertisement always answers a 500.
  The `# service=` announcement of an advertisement is written by gitserve, for protocol v0 only.
- `store.rs`: a `Store` holds the repositories, in a directory or in a zip archive (any path ending with `.zip`).
  `view` gives shared access, `change` exclusive access; a push runs under `change`,
  since `storage-git-gix` takes no writer lock. A directory store has one `RwLock` per repository, keyed by
  the canonical path, so that every spelling of a repository (case, symbolic link) shares one.
  A `GixStore` is opened for each request, so that it sees what other processes wrote.
- An archive store extracts the archive into a temporary directory, its image, under a single `RwLock`:
  writing the archive covers every repository. `change` writes the archive back to its file afterwards,
  and returns whether it did (`Changed::saved`); `create_repositories` and `delete_repositories` run all their names
  in one change and write once. The image is reloaded when the stamp of the file changes (size, modification time,
  and the inode and change time on Unix or the creation time on Windows, since FAT has a 2-second modification time),
  as when another gitserve command writes it. When a write fails, or a change panics, the stamp is cleared:
  the next access reloads the file, which discards the change.
  When the write of a push fails, the server replaces its response with an error on the error band of the side-band,
  when the client asked for one (an HTTP 500 otherwise): the push fails on the client,
  which displays the reason as a `remote:` message.
  Another process writing the archive between a reload and a write of the server is not detected: its change is lost.
  `Store::close` lets a change in progress complete, and refuses the later accesses with `AccessError::Closed`
  (a 503) rather than holding them back: their tasks then end and release the store, whose drop removes the image.
  A closed store refuses before looking a repository up or reloading an archive.
- `archive.rs` converts between zip and a directory, with the format of the Go project, in both directions:
  the repositories at the root, a `.gitserve` marker recording the format version (`{"version":1}`, added to an
  archive without marker on its next write), files under the `objects` directory of a repository stored without
  compression (zip64 for an entry of 4 GiB or more). An archive is written to a temporary file next to it,
  then renamed over it, keeping its permissions (0644 for a new archive on Unix);
  entries escaping the image are refused, and `.gitserve-*` directories are left out.
- `repository.rs`: a repository is a directory holding `HEAD` and `config` files. A name is a slash-separated path
  checked by `check_name`; `find_repository` also tries it with a `.git` suffix. The hash kind (SHA-1 or SHA-256)
  comes from `extensions.objectFormat` in `config`. `GixStore` reads that entry again, with a plain scan that
  ignores sections and comments: a hand-written entry that only `hash_kind` understands still fails to open.
  `create_repository` builds a repository with `GixStore::open_or_init`, without the `git` binary,
  in a hidden `.gitserve-*` directory next to it, then renames it into place; `delete_repository` renames it away
  before deleting it: a server running on the same store never sees a repository half made or half deleted.
  A repository is never created inside another one, and `.gitserve-*` names are reserved;
  `list_repositories` skips nested repositories and `.gitserve-*` directories.
  The lock of a deletion only coordinates with the requests of the same `Store`: against a `serve` of another
  process, the rename is the only guard. On Unix, a request then fails if it opens another file of the repository;
  on Windows, the rename, hence the deletion, fails while a file of the repository is open.
- `ingest.rs`: `StoreIngest`, the `PackIngest` that takes a pushed pack into the store through `GitOps::put_pack`,
  which completes a thin pack. Requests and packs are held in memory, unlike the spools of the Go project.
- `store.rs` also holds `init_store`, `ensure_store` (`serve` and `create` initialize a missing store)
  and `Store::open`.
- The tests of a failed archive write make its directory read-only, which only works on Unix and not as root:
  they skip themselves when the directory is still writable.
- `main.rs`: the command line uses clap: `serve`, `init`, `create`, `delete` and `list`.
  A usage error exits with 2 (an invalid branch name included), a failed command with 1;
  `create` and `delete` go on with the other names when one fails, then exit with 1.
  The server listens on `0.0.0.0:<port>`, 8085 by default, and logs each request with its status code.
- Graceful shutdown: `watch_signals` (`main.rs`) watches SIGINT and SIGTERM (Ctrl+C and the closing of the console
  on Windows) in a thread of its own, with a runtime of its own, installed before serving: the first signal starts
  the shutdown, a second one exits at once with 1, whatever the shutdown waits for (the main runtime, waiting for
  its blocking tasks, could not handle it). `server::serve` takes the shutdown as a future, so that tests drive it
  (`tests/shutdown.rs`, whose routes tell the test when they start rather than relying on delays):
  it stops accepting connections, gives the requests in flight `SHUTDOWN_GRACE` (5 s), then returns;
  dropping its future before the end stops the server too.
  axum runs each connection in a task of its own, so the connections left open are closed when the runtime is dropped. `run_server` then calls `Store::close`, drops the runtime, which waits for its blocking
  tasks (a push in progress completes, even if its response can no longer be sent), and drops the store last.
  A signal exits with 0; serving that fails on its own, such as a port already taken, exits with 1.
