# Daemon

- [x] SystemD

`daemon` provides a clap-compatible daemon command backed directly by the
systemd D-Bus API. Configuration-file loading is intentionally outside this
crate.

## Features

- `install`, `uninstall`/`remove`, `start`, `stop`, `restart`, and `status`
- `install -- [args...]` forwards arguments to `ExecStart`
- Structured `SystemdUnit` models `[Unit]`, `[Service]`, and `[Install]`
- Root users use the system systemd manager
- Non-root users use `systemd --user` and install to
  `~/.config/systemd/user`
- Detailed state transitions and error context are printed for every action

## Embed In A Clap CLI

Use `DaemonCli` when service metadata is supplied by command-line options:

```rust
use anyhow::Result;
use clap::Parser;
use daemon_rs::DaemonCli;

#[derive(Debug, Parser)]
struct Cli {
    #[command(flatten)]
    daemon: DaemonCli,
}

fn main() -> Result<()> {
    Cli::parse().daemon.execute()
}
```

Example invocation:

```text
my-app \
  --name my-app \
  --description "My application" \
  --program /usr/local/bin/my-app \
  --workdir /var/lib/my-app \
  install -- --config /etc/my-app/config.toml
```

`config_dir` defaults to `workdir`. Optional `--user` and `--group` are
available for system services; user-level services run as the current user and
must not set these fields.

## Embed With Programmatic Metadata

Use `Daemon` and `Command` when metadata is already available in the host
application:

```rust
use daemon_rs::{Command, Daemon};
use std::ffi::OsString;

let daemon = Daemon::new(
    "my-app",
    "My application",
    "/usr/local/bin/my-app",
    "/var/lib/my-app",
)
.config_dir("/etc/my-app");

daemon.execute(&Command::Install {
    args: vec![OsString::from("--config"), OsString::from("/etc/my-app/config.toml")],
})?;
```

The generated model can be inspected before installation:

```rust
let unit = daemon.systemd_unit()?;
println!("{}", unit.render());
```

## Commands

| Command     | Aliases                     |
| ----------- | --------------------------- |
| `install`   | `i`                         |
| `uninstall` | `rm`, `remove`, `uni`, `un` |
| `start`     | `run`                       |
| `stop`      |                             |
| `restart`   | `r`, `re`                   |
| `status`    | `info`, `if`                |

The D-Bus session/system bus must be available. The lifecycle integration test
requires an active user systemd session:

```text
cargo test
cargo test --test systemd systemd_lifecycle -- --ignored
```
