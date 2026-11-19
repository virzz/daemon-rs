//! Clap-compatible daemon management via systemd on Linux and launchd on macOS.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

#[cfg(target_os = "macos")]
mod launchd;
mod systemd;

#[cfg(target_os = "macos")]
use launchd as platform;
#[cfg(target_os = "linux")]
use systemd as platform;

pub use systemd::{SystemdInstallSection, SystemdServiceSection, SystemdUnit, SystemdUnitSection};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub name: String,
    pub description: String,
    pub program: PathBuf,
    pub workdir: PathBuf,
    pub config_dir: PathBuf,
    pub user: Option<String>,
    pub group: Option<String>,
}

impl Config {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        program: impl Into<PathBuf>,
        workdir: impl Into<PathBuf>,
    ) -> Self {
        let workdir = workdir.into();
        Self {
            name: name.into(),
            description: description.into(),
            program: program.into(),
            config_dir: workdir.clone(),
            workdir,
            user: None,
            group: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let name = self.service_name();
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.@-".contains(&byte))
        {
            bail!("invalid daemon name {:?}", self.name)
        }
        if self.description.trim().is_empty() || self.description.contains(['\n', '\r']) {
            bail!("daemon description must be non-empty and single-line")
        }
        if self.program.as_os_str().is_empty() {
            bail!("program must not be empty")
        }
        if self.workdir.as_os_str().is_empty() {
            bail!("working directory must not be empty")
        }
        if self.config_dir.as_os_str().is_empty() {
            bail!("config directory must not be empty")
        }
        for (field, value) in [("user", &self.user), ("group", &self.group)] {
            if value
                .as_deref()
                .is_some_and(|value| value.is_empty() || value.contains(['\n', '\r']))
            {
                bail!("daemon {field} must be non-empty and single-line")
            }
        }
        Ok(())
    }

    fn service_name(&self) -> &str {
        self.name.trim_end_matches(".service")
    }

    #[cfg(target_os = "linux")]
    fn unit_name(&self) -> String {
        format!("{}.service", self.service_name())
    }
}

#[derive(Clone, Debug)]
pub struct Daemon {
    config: Config,
}

#[derive(Clone, Debug, Args)]
pub struct DaemonCommand {
    #[command(subcommand)]
    pub command: Command,
}

/// Standalone clap arguments for applications that want daemon options and commands together.
#[derive(Clone, Debug, Args)]
pub struct DaemonCli {
    #[arg(long)]
    pub name: String,
    #[arg(long, alias = "desc")]
    pub description: String,
    #[arg(long)]
    pub program: PathBuf,
    #[arg(long)]
    pub workdir: PathBuf,
    #[arg(long)]
    pub config_dir: Option<PathBuf>,
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long)]
    pub group: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    #[command(alias = "i")]
    Install {
        #[arg(last = true, value_name = "ARGS")]
        args: Vec<OsString>,
    },
    #[command(alias = "rm", alias = "remove", alias = "uni", alias = "un")]
    Uninstall,
    #[command(alias = "run")]
    Start,
    Stop,
    #[command(alias = "r", alias = "re")]
    Restart,
    #[command(alias = "info", alias = "if")]
    Status,
}

impl Daemon {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        program: impl Into<PathBuf>,
        workdir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            config: Config::new(name, description, program, workdir),
        }
    }

    pub fn config_dir(mut self, config_dir: impl Into<PathBuf>) -> Self {
        self.config.config_dir = config_dir.into();
        self
    }

    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.config.user = Some(user.into());
        self
    }

    pub fn group(mut self, group: impl Into<String>) -> Self {
        self.config.group = Some(group.into());
        self
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn execute(&self, command: &Command) -> Result<()> {
        execute(&self.config, command)
    }

    pub fn systemd_unit(&self) -> Result<SystemdUnit> {
        self.systemd_unit_with_args(&[])
    }

    pub fn systemd_unit_with_args(&self, args: &[OsString]) -> Result<SystemdUnit> {
        self.systemd_unit_with_args_for(args, !is_root())
    }

    pub fn systemd_unit_for(&self, user_manager: bool) -> Result<SystemdUnit> {
        self.systemd_unit_with_args_for(&[], user_manager)
    }

    pub fn systemd_unit_with_args_for(
        &self,
        args: &[OsString],
        user_manager: bool,
    ) -> Result<SystemdUnit> {
        SystemdUnit::from_config(&self.config, args, user_manager)
    }
}

impl DaemonCli {
    pub fn daemon(&self) -> Daemon {
        let mut daemon = Daemon::new(
            self.name.clone(),
            self.description.clone(),
            self.program.clone(),
            self.workdir.clone(),
        );
        if let Some(path) = &self.config_dir {
            daemon.config.config_dir = path.clone();
        }
        daemon.config.user.clone_from(&self.user);
        daemon.config.group.clone_from(&self.group);
        daemon
    }

    pub fn execute(&self) -> Result<()> {
        self.daemon().execute(&self.command)
    }
}

pub fn execute(config: &Config, command: &Command) -> Result<()> {
    let result = execute_inner(config, command);
    if let Err(error) = &result {
        eprintln!(
            "daemon '{}' command '{command}' failed: {error:#}",
            config.name
        );
    }
    result
}

fn execute_inner(config: &Config, command: &Command) -> Result<()> {
    config.validate()?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        platform::execute(config, command)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = command;
        bail!("daemon management is supported on Linux and macOS")
    }
}

impl std::fmt::Display for Command {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Install { .. } => "install",
            Self::Uninstall => "uninstall",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Status => "status",
        })
    }
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        effective_uid() == 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_definition(path: &std::path::Path, body: &str, mode: u32) -> Result<()> {
    use std::{fs, io::Write, os::unix::fs::PermissionsExt};

    let parent = path
        .parent()
        .expect("service definition has a parent directory");
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(mode))
        .tempfile_in(parent)?;
    temp.write_all(body.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn defaults_and_unit() {
        let c = Config::new(
            "demo",
            "Demo service",
            "/usr/local/bin/demo",
            "/var/lib/demo",
        );
        assert_eq!(c.config_dir, c.workdir);
        let unit = SystemdUnit::from_config(&c, &[], true).unwrap().render();
        assert!(unit.contains("Description=Demo service"));
        assert!(unit.contains("WorkingDirectory=/var/lib/demo"));
        assert!(unit.contains("EnvironmentFile=-/var/lib/demo/demo.env"));
    }

    #[test]
    fn user_unit_uses_user_runtime_and_systemctl() {
        let c = Config::new("demo", "Demo service", "/tmp/demo", "/tmp");
        let unit = SystemdUnit::from_config(&c, &[], true).unwrap().render();
        assert!(unit.contains("PIDFile=%t/demo.pid"));
        assert!(unit.contains("/bin/systemctl --user show"));
        assert!(!unit.contains("User="));
        assert!(!unit.contains("Group="));
    }

    #[test]
    fn command_accepts_remove_alias() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            daemon: DaemonCommand,
        }
        let cli = Cli::try_parse_from(["demo", "remove"]).expect("remove is a CLI alias");
        assert!(matches!(cli.daemon.command, Command::Uninstall));
    }
}
