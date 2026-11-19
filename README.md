[![Crates.io]](https://crates.io/crates/gitserve)
[![CircleCI]](https://circleci.com/gh/corebreaker/gitserve-rs/tree/main)
[![Coverage Status]](https://coveralls.io/github/corebreaker/gitserve-rs?branch=main)
[![License]](https://github.com/corebreaker/gitserve-rs/blob/main/LICENSE)

# gitserve
_A little http server for Git, in Rust_

gitserve serves local bare Git repositories over the smart HTTP protocol of Git,
so that they can be cloned and pushed to without registering them on a public forge like GitHub or GitLab.
The repositories live in a directory, or in a single zip archive that is easy to carry on a USB key:
collaborators clone and push with zero setup.

gitserve is the Rust translation of the Go project [gitserve](https://github.com/corebreaker/gitserve),
whose command line and archive format it keeps. It does not run the `git` binary:
the protocol comes from [gunnar-wire](https://crates.io/crates/gunnar-wire),
and the storage from [gitoxide](https://github.com/GitoxideLabs/gitoxide),
so the serving machine needs no Git installation.

gitserve is meant for a trusted network: it has no authentication and no access control,
and it listens on all network interfaces.


## Installation

```bash
cargo install gitserve
```


## Start server

```bash
gitserve create gitroot my-repo   # create the bare repository gitroot/my-repo
gitserve serve gitroot            # serve the repositories of gitroot on port 8085
```

When started, you can clone the repository with `git clone http://localhost:8085/my-repo`, and push to it.

To run the server in the background:

```bash
nohup gitserve serve gitroot >gitserve.log 2>&1 &
```

The server stops on Ctrl+C or SIGTERM: it stops accepting connections,
gives the requests in progress 5 seconds to complete, then exits.
A second signal exits at once.
The log level follows the `RUST_LOG` environment variable, `info` by default, which logs each request.


## Stores

The repositories live in a store: a directory, or a zip archive when its name ends with `.zip`.
Both have the same layout, one bare repository per directory, possibly in subdirectories:
the repository `team/tools` is served at `http://localhost:8085/team/tools`.
A repository whose directory name ends with `.git` is also served without that suffix,
and repositories created with `git init --bare` are served as well, with SHA-1 or SHA-256 objects.

A zip archive acts as the image of a filesystem: the server extracts it to a temporary directory,
and writes it back after each push.
The new archive replaces the previous one only once fully written, so that an interrupted write never corrupts it.
When the archive cannot be written, for instance on a write-protected key, the push fails and the server forgets it.
The archive holds the repositories at its root, next to a `.gitserve` file that records the version of its format;
a zip made from a directory store is a valid archive too, and archives made by the Go project are read as they are.

Since the whole archive is rewritten at each push, it suits small and medium repositories.
An archive must be served by a single server at a time,
but the other commands can change it while it is served: the server reloads it.

Whatever the store, the server reads each request whole before accessing the store,
and sends its response once that access is over, so that a slow client never holds back the others.
Requests and responses are held in memory.


## Commands

| Command                                                   | Description                                 |
|-----------------------------------------------------------|---------------------------------------------|
| `gitserve serve [--port 8085] [--prefix /] <store>`       | Serve the repositories of a store over HTTP |
| `gitserve init <store>`                                   | Create an empty store                       |
| `gitserve create [--branch main] <store> <repository>...` | Create bare repositories                    |
| `gitserve delete <store> <repository>...`                 | Delete repositories                         |
| `gitserve list <store>`                                   | List the repositories of a store            |

- `serve` and `create` initialize the store when it does not exist yet.
- `--port` (`-p`) sets the port on which the server listens, on all network interfaces.
- `--prefix` sets the URL path under which the repositories are served:
  with `--prefix /git`, the repository `my-repo` is served at `http://localhost:8085/git/my-repo`.
- `--branch` (`-b`) sets the branch that HEAD points to in the new repositories.
- `create` and `delete` go on with the other repositories when one fails, then exit with an error.

Run `gitserve help <command>` for the details of a command.


## Development

```bash
cargo build                                 # build the debug binary in target/debug
cargo test                                  # run the tests
cargo fmt --all                             # format the sources
cargo clippy --all-targets -- -D warnings   # static checks
cargo run -- serve gitroot                  # serve ./gitroot on port 8085
```

The end-to-end tests drive the real `git` client against the server: they are skipped when `git` is missing.
CircleCI checks the formatting, runs clippy, builds and runs the tests, and reports the coverage to Coveralls.


## License

gitserve is licensed under the [GNU General Public License v3.0](LICENSE) or any later version.
It uses gunnar-wire, licensed under the AGPL-3.0:
the source of a server built from it must be offered to its network users, which this repository does.

[Crates.io]: https://img.shields.io/crates/v/gitserve?style=for-the-badge
[CircleCI]: https://img.shields.io/circleci/build/github/corebreaker/gitserve-rs/main?style=for-the-badge
[Coverage Status]: https://img.shields.io/coveralls/github/corebreaker/gitserve-rs/main?style=for-the-badge
[License]: https://img.shields.io/github/license/corebreaker/gitserve-rs?style=for-the-badge
