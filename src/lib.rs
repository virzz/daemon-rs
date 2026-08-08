//! Clap-compatible systemd daemon lifecycle management over D-Bus.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use dbus::Path as DbusPath;
use dbus::arg::Variant;
use dbus::blocking::{Connection, Proxy};

const SYSTEMD_BUS: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER: &str = "org.freedesktop.systemd1.Manager";
const DBUS_TIMEOUT: Duration = Duration::from_secs(10);

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
        self.systemd_unit_with_args_for(args, is_user_manager())
    }

    pub fn systemd_unit_for(&self, user_manager: bool) -> Result<SystemdUnit> {
        self.systemd_unit_with_args_for(&[], user_manager)
    }

    pub fn systemd_unit_with_args_for(
        &self,
        args: &[OsString],
        user_manager: bool,
    ) -> Result<SystemdUnit> {
        self.config.validate()?;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemdUnit {
    pub unit: SystemdUnitSection,
    pub service: SystemdServiceSection,
    pub install: SystemdInstallSection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemdUnitSection {
    pub description: String,
    pub wants: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemdServiceSection {
    pub service_type: String,
    pub exec_start: String,
    pub working_directory: PathBuf,
    pub pid_file: String,
    pub exec_start_pre: String,
    pub exec_start_post: String,
    pub restart: String,
    pub restart_sec: u32,
    pub start_limit_interval_sec: u32,
    pub start_limit_burst: u32,
    pub restart_prevent_exit_status: String,
    pub user: Option<String>,
    pub group: Option<String>,
    pub environment_file: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemdInstallSection {
    pub wanted_by: String,
}

impl SystemdUnit {
    pub fn from_config(config: &Config, args: &[OsString], user_manager: bool) -> Result<Self> {
        config.validate()?;
        let service_name = config.service_name();
        let pid_dir = if user_manager { "%t" } else { "/run" };
        let systemctl = if user_manager {
            "/bin/systemctl --user"
        } else {
            "/bin/systemctl"
        };
        let exec_start = std::iter::once(config.program.as_os_str())
            .chain(args.iter().map(OsString::as_os_str))
            .map(systemd_arg)
            .collect::<Vec<_>>()
            .join(" ");
        let pid_file = format!("{pid_dir}/{service_name}.pid");
        Ok(Self {
            unit: SystemdUnitSection {
                description: config.description.clone(),
                wants: "network.target".to_owned(),
            },
            service: SystemdServiceSection {
                service_type: "exec".to_owned(),
                exec_start,
                working_directory: config.workdir.clone(),
                pid_file: pid_file.clone(),
                exec_start_pre: format!("/bin/rm -f {pid_file}"),
                exec_start_post: format!(
                    "/bin/bash -c '{systemctl} show -p MainPID --value {service_name} > {pid_file}'"
                ),
                restart: "always".to_owned(),
                restart_sec: 0,
                start_limit_interval_sec: 30,
                start_limit_burst: 10,
                restart_prevent_exit_status: "SIGKILL".to_owned(),
                user: (!user_manager).then(|| config.user.clone()).flatten(),
                group: (!user_manager).then(|| config.group.clone()).flatten(),
                environment_file: config.config_dir.join(format!("{service_name}.env")),
            },
            install: SystemdInstallSection {
                wanted_by: if user_manager {
                    "default.target".to_owned()
                } else {
                    "multi-user.target".to_owned()
                },
            },
        })
    }

    pub fn render(&self) -> String {
        let mut unit = String::new();
        let _ = writeln!(unit, "[Unit]");
        let _ = writeln!(unit, "Description={}", self.unit.description);
        let _ = writeln!(unit, "Wants={}", self.unit.wants);
        let _ = writeln!(unit, "\n[Service]");
        let service = &self.service;
        let _ = writeln!(unit, "Type={}", service.service_type);
        let _ = writeln!(unit, "ExecStart={}", service.exec_start);
        let _ = writeln!(
            unit,
            "WorkingDirectory={}",
            service.working_directory.display()
        );
        let _ = writeln!(unit, "PIDFile={}", service.pid_file);
        let _ = writeln!(unit, "ExecStartPre={}", service.exec_start_pre);
        let _ = writeln!(unit, "ExecStartPost={}", service.exec_start_post);
        if let Some(user) = &service.user {
            let _ = writeln!(unit, "User={user}");
        }
        if let Some(group) = &service.group {
            let _ = writeln!(unit, "Group={group}");
        }
        let _ = writeln!(
            unit,
            "EnvironmentFile=-{}",
            service.environment_file.display()
        );
        let _ = writeln!(unit, "Restart={}", service.restart);
        let _ = writeln!(unit, "RestartSec={}", service.restart_sec);
        let _ = writeln!(
            unit,
            "StartLimitIntervalSec={}",
            service.start_limit_interval_sec
        );
        let _ = writeln!(unit, "StartLimitBurst={}", service.start_limit_burst);
        let _ = writeln!(
            unit,
            "RestartPreventExitStatus={}",
            service.restart_prevent_exit_status
        );
        let _ = writeln!(unit, "\n[Install]\nWantedBy={}", self.install.wanted_by);
        unit
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
    if !is_root() && (config.user.is_some() || config.group.is_some()) {
        bail!("--user/--group require system service mode; unset them for systemd --user")
    }
    let systemd = Systemd::connect()?;
    let label = config.unit_name();
    match command {
        Command::Install { args } => systemd.install(config, args),
        Command::Uninstall => systemd.uninstall(&label),
        Command::Start => systemd.change_state(&label, "start"),
        Command::Stop => systemd.change_state(&label, "stop"),
        Command::Restart => systemd.restart(&label),
        Command::Status => systemd.status(&label, &config.name),
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

struct Systemd {
    connection: Connection,
    user: bool,
}

impl Systemd {
    fn connect() -> Result<Self> {
        let user = is_user_manager();
        let connection = if user {
            Connection::new_session()
        } else {
            Connection::new_system()
        }
        .with_context(|| {
            format!(
                "connect to systemd{} D-Bus",
                if user { " --user" } else { "" }
            )
        })?;
        Ok(Self { connection, user })
    }

    fn unit_file(&self, unit: &str) -> Result<PathBuf> {
        unit_file_path(self.user, unit)
    }

    fn manager(&self) -> Proxy<'_, &Connection> {
        self.connection
            .with_proxy(SYSTEMD_BUS, SYSTEMD_PATH, DBUS_TIMEOUT)
    }

    fn unit_path(&self, unit: &str) -> Result<Option<DbusPath<'static>>> {
        match self
            .manager()
            .method_call(SYSTEMD_MANAGER, "GetUnit", (unit,))
        {
            Ok((path,)) => Ok(Some(path)),
            Err(error) if error.name() == Some("org.freedesktop.systemd1.NoSuchUnit") => Ok(None),
            Err(error) => Err(error).context("get systemd unit path"),
        }
    }

    fn property(&self, path: &DbusPath<'static>, name: &str) -> Result<String> {
        let proxy = self.connection.with_proxy(SYSTEMD_BUS, path, DBUS_TIMEOUT);
        let (value,): (Variant<String>,) = proxy
            .method_call(
                "org.freedesktop.DBus.Properties",
                "Get",
                ("org.freedesktop.systemd1.Unit", name),
            )
            .with_context(|| format!("read systemd property {name}"))?;
        Ok(value.0)
    }

    fn status(&self, unit: &str, display_name: &str) -> Result<()> {
        let state = match self.unit_path(unit)? {
            None => "not-installed".to_owned(),
            Some(path) => {
                let load = self.property(&path, "LoadState")?;
                if load == "not-found" {
                    "not-installed".to_owned()
                } else {
                    let active = self.property(&path, "ActiveState")?;
                    let sub = self.property(&path, "SubState")?;
                    format!("{active} ({sub}, load={load})")
                }
            }
        };
        println!("{display_name}: {state}");
        Ok(())
    }

    fn status_name(&self, unit: &str) -> Result<String> {
        let Some(path) = self.unit_path(unit)? else {
            return Ok("not-installed".to_owned());
        };
        let load = self.property(&path, "LoadState")?;
        if load == "not-found" {
            return Ok("not-installed".to_owned());
        }
        Ok(format!(
            "{} ({}, load={load})",
            self.property(&path, "ActiveState")?,
            self.property(&path, "SubState")?
        ))
    }

    fn install(&self, config: &Config, args: &[OsString]) -> Result<()> {
        fs::create_dir_all(&config.workdir)
            .with_context(|| format!("create working directory {}", config.workdir.display()))?;
        fs::create_dir_all(&config.config_dir)
            .with_context(|| format!("create config directory {}", config.config_dir.display()))?;
        let unit = SystemdUnit::from_config(config, args, self.user)?;
        let unit_file = self.unit_file(&config.unit_name())?;
        if let Some(parent) = unit_file.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create systemd unit directory {}", parent.display()))?;
        }
        let before = self.status_name(&config.unit_name())?;
        println!("state before install: {before}");
        fs::write(&unit_file, unit.render())
            .with_context(|| format!("write unit file {}", unit_file.display()))?;
        self.reload()?;
        self.enable(&config.unit_name())?;
        let after = self.status_name(&config.unit_name())?;
        println!(
            "state after install: {after}; program={}, args={args:?}",
            config.program.display()
        );
        Ok(())
    }

    fn uninstall(&self, unit: &str) -> Result<()> {
        println!("uninstalling daemon '{unit}'");
        let before = self.status_name(unit)?;
        println!("state before uninstall: {before}");
        if before != "not-installed" {
            if before.starts_with("active (running") {
                self.call_unit("StopUnit", unit)?;
            }
            self.disable(unit)?;
        }
        let unit_file = self.unit_file(unit)?;
        if unit_file.exists() {
            fs::remove_file(&unit_file)
                .with_context(|| format!("remove unit file {}", unit_file.display()))?;
            self.reload()?;
        }
        println!("state after uninstall: {}", self.status_name(unit)?);
        Ok(())
    }

    fn change_state(&self, unit: &str, action: &str) -> Result<()> {
        let before = self.status_name(unit)?;
        println!("state before {action}: {before}");
        self.call_unit(
            if action == "start" {
                "StartUnit"
            } else {
                "StopUnit"
            },
            unit,
        )?;
        println!("state after {action}: {}", self.status_name(unit)?);
        Ok(())
    }

    fn restart(&self, unit: &str) -> Result<()> {
        let before = self.status_name(unit)?;
        println!("state before restart: {before}");
        self.call_unit("RestartUnit", unit)?;
        println!("state after restart: {}", self.status_name(unit)?);
        Ok(())
    }

    fn call_unit(&self, method: &str, unit: &str) -> Result<()> {
        let _: (DbusPath<'static>,) = self
            .manager()
            .method_call(SYSTEMD_MANAGER, method, (unit, "replace"))
            .with_context(|| format!("systemd {method} {unit}"))?;
        Ok(())
    }

    fn reload(&self) -> Result<()> {
        let _: () = self
            .manager()
            .method_call(SYSTEMD_MANAGER, "Reload", ())
            .context("reload systemd manager")?;
        Ok(())
    }

    fn enable(&self, unit: &str) -> Result<()> {
        let _: (bool, Vec<(String, String)>) = self
            .manager()
            .method_call(
                SYSTEMD_MANAGER,
                "EnableUnitFiles",
                (vec![unit], false, true),
            )
            .with_context(|| format!("enable systemd unit {unit}"))?;
        Ok(())
    }

    fn disable(&self, unit: &str) -> Result<()> {
        let _: (Vec<(String, String)>,) = self
            .manager()
            .method_call(SYSTEMD_MANAGER, "DisableUnitFiles", (vec![unit], false))
            .with_context(|| format!("disable systemd unit {unit}"))?;
        Ok(())
    }
}

fn unit_file_path(user: bool, unit: &str) -> Result<PathBuf> {
    if user {
        Ok(dirs::config_dir()
            .context("resolve user config directory")?
            .join("systemd/user")
            .join(unit))
    } else {
        Ok(Path::new("/etc/systemd/system").join(unit))
    }
}

fn is_user_manager() -> bool {
    !is_root()
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        // geteuid is a side-effect-free libc query with no pointer or lifetime requirements.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn systemd_arg(value: &OsStr) -> String {
    let value = value
        .to_string_lossy()
        .replace('%', "%%")
        .replace('$', "$$");
    if !value.is_empty()
        && !value
            .chars()
            .any(|character| character.is_whitespace() || matches!(character, '"' | '\\'))
    {
        return value;
    }
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
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
