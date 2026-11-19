# Daemon

`daemon` provides a clap-compatible daemon command backed by systemd over
D-Bus on Linux and by `launchctl` on macOS.

| Scope        | Linux                                                                           | macOS                                   |
| ------------ | ------------------------------------------------------------------------------- | --------------------------------------- |
| Current user | `systemd --user`, `~/.config/systemd/user` (or `$XDG_CONFIG_HOME/systemd/user`) | GUI session, `~/Library/LaunchAgents`   |
| Root         | System manager, `/etc/systemd/system`                                           | System domain, `/Library/LaunchDaemons` |

`install -- [args...]` forwards arguments to the service executable. Install
writes the service definition. It also enables the systemd unit for future
boot or login. On macOS the LaunchAgent loads at login and the LaunchDaemon
at boot. Run `start` to start the service immediately, or `restart` to apply
an updated definition to a running service.

## Embed In A Clap CLI

Use `DaemonCli` when service metadata is supplied by command-line options:

```rust
use anyhow::Result;
use clap::Parser;
use daemon::DaemonCli;

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
available for system services. User-level services run as the current user.
systemd reads an optional `<name>.env` file from `config_dir`. launchd writes
standard output and error to `out.log` and `err.log` under
`~/Library/Logs/<name>` for LaunchAgents or `/Library/Logs/<name>` for
LaunchDaemons running as root. LaunchDaemons with an explicit `--user` use
launchd’s default standard output and error handling.

## Embed With Programmatic Metadata

Use `Daemon` and `Command` when metadata is already available in the host
application:

```rust
use daemon::{Command, Daemon};
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

The generated systemd model can be inspected before installation:

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

Use `cargo test` to check CLI parsing and service definitions. The systemd
lifecycle test requires Linux with an active user systemd session. The launchd
lifecycle test requires macOS with an active GUI login.

```text
cargo test --test systemd systemd_lifecycle -- --ignored
cargo test --lib launchd_lifecycle -- --ignored
```
