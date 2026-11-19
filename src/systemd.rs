use std::{
    ffi::{OsStr, OsString},
    fmt::Write as _,
    path::PathBuf,
};

use anyhow::Result;

use crate::Config;

#[cfg(target_os = "linux")]
pub(crate) use manager::execute;

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

#[cfg(target_os = "linux")]
mod manager {
    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
        sync::mpsc,
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result, bail, ensure};
    use dbus::{
        Path as DbusPath,
        arg::Variant,
        blocking::{Connection, Proxy},
        message::MatchRule,
    };

    use super::SystemdUnit;
    use crate::{Command, Config, is_root, write_definition};

    const SYSTEMD_BUS: &str = "org.freedesktop.systemd1";
    const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
    const SYSTEMD_MANAGER: &str = "org.freedesktop.systemd1.Manager";
    const NO_SUCH_UNIT: &str = "org.freedesktop.systemd1.NoSuchUnit";
    const DBUS_TIMEOUT: Duration = Duration::from_secs(10);
    const JOB_TIMEOUT: Duration = Duration::from_secs(90);

    pub(crate) fn execute(config: &Config, command: &Command) -> Result<()> {
        if !is_root() && (config.user.is_some() || config.group.is_some()) {
            bail!("--user/--group require system service mode; unset them for systemd --user")
        }
        let systemd = Systemd::connect()?;
        let label = config.unit_name();
        match command {
            Command::Install { args } => systemd.install(config, args),
            Command::Uninstall => systemd.uninstall(&label),
            Command::Start => systemd.change_state(&label, "start", "StartUnit"),
            Command::Stop => systemd.change_state(&label, "stop", "StopUnit"),
            Command::Restart => {
                systemd.reload()?;
                systemd.change_state(&label, "restart", "RestartUnit")
            }
            Command::Status => systemd.status(&label, &config.name),
        }
    }

    struct Systemd {
        connection: Connection,
        user: bool,
    }

    impl Systemd {
        fn connect() -> Result<Self> {
            let user = !is_root();
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
                .method_call(SYSTEMD_MANAGER, "LoadUnit", (unit,))
            {
                Ok((path,)) => Ok(Some(path)),
                Err(error) if error.name() == Some(NO_SUCH_UNIT) => Ok(None),
                Err(error) => Err(error).with_context(|| format!("load systemd unit {unit}")),
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
            println!("{display_name}: {}", self.status_name(unit)?);
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
            fs::create_dir_all(&config.workdir).with_context(|| {
                format!("create working directory {}", config.workdir.display())
            })?;
            fs::create_dir_all(&config.config_dir).with_context(|| {
                format!("create config directory {}", config.config_dir.display())
            })?;
            let unit = SystemdUnit::from_config(config, args, self.user)?;
            let name = config.unit_name();
            let unit_file = self.unit_file(&name)?;
            println!("state before install: {}", self.status_name(&name)?);
            write_definition(&unit_file, &unit.render(), 0o644)
                .with_context(|| format!("write unit file {}", unit_file.display()))?;
            self.reload()?;
            self.enable(&name)?;
            let after = self.status_name(&name)?;
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
            let unit_file = self.unit_file(unit)?;
            if unit_file.try_exists()? {
                if before != "not-installed" {
                    self.call_unit("StopUnit", unit)?;
                }
                self.disable(unit)?;
                fs::remove_file(&unit_file)
                    .with_context(|| format!("remove unit file {}", unit_file.display()))?;
            }
            self.reload()?;
            println!("state after uninstall: {}", self.status_name(unit)?);
            Ok(())
        }

        fn change_state(&self, unit: &str, action: &str, method: &str) -> Result<()> {
            println!("state before {action}: {}", self.status_name(unit)?);
            self.call_unit(method, unit)?;
            println!("state after {action}: {}", self.status_name(unit)?);
            Ok(())
        }

        fn call_unit(&self, method: &str, unit: &str) -> Result<()> {
            let manager = self.manager();
            let (sender, receiver) = mpsc::channel();
            let rule = MatchRule::new_signal(SYSTEMD_MANAGER, "JobRemoved")
                .with_sender(SYSTEMD_BUS)
                .with_path(SYSTEMD_PATH);
            self.connection
                .add_match(
                    rule,
                    move |(_, path, _, result): (u32, DbusPath<'static>, String, String), _, _| {
                        let _ = sender.send((path, result));
                        true
                    },
                )
                .context("listen for completed systemd jobs")?;
            let _: () = manager
                .method_call(SYSTEMD_MANAGER, "Subscribe", ())
                .context("subscribe to systemd jobs")?;
            let (job,): (DbusPath<'static>,) = manager
                .method_call(SYSTEMD_MANAGER, method, (unit, "replace"))
                .with_context(|| format!("systemd {method} {unit}"))?;
            let deadline = Instant::now() + JOB_TIMEOUT;
            loop {
                if let Some((_, result)) = receiver.try_iter().find(|(path, _)| *path == job) {
                    ensure!(result == "done", "systemd {method} {unit}: {result}");
                    return Ok(());
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                ensure!(!remaining.is_zero(), "systemd {method} {unit} timed out");
                self.connection
                    .process(remaining)
                    .with_context(|| format!("wait for systemd {method} {unit}"))?;
            }
        }

        fn reload(&self) -> Result<()> {
            let _: () = self
                .manager()
                .method_call(SYSTEMD_MANAGER, "Reload", ())
                .context("reload systemd manager")?;
            Ok(())
        }

        fn enable(&self, unit: &str) -> Result<()> {
            let _: (bool, Vec<(String, String, String)>) = self
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
            let _: (Vec<(String, String, String)>,) = self
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
}
