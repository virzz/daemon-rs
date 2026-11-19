use std::{
    ffi::{OsStr, OsString},
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::{Command as Process, Output},
};

use anyhow::{Context, Result, bail, ensure};

use crate::{Command, Config, effective_uid, write_definition};

const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const USER_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

pub(crate) fn execute(config: &Config, command: &Command) -> Result<()> {
    let uid = effective_uid();
    let library = if uid == 0 {
        PathBuf::from("/Library")
    } else {
        dirs::home_dir()
            .context("find home directory for the LaunchAgent")?
            .join("Library")
    };
    let service = Launchd::new(config, uid, &library)?;
    match command {
        Command::Install { args } => service.install(config, args),
        Command::Uninstall => service.uninstall(),
        Command::Start => service.start(),
        Command::Stop => service.stop(),
        Command::Restart => service.restart(),
        Command::Status => service.status(),
    }
}

struct Launchd<'a> {
    label: &'a str,
    domain: String,
    target: String,
    plist: PathBuf,
    log_dir: Option<PathBuf>,
    path: &'static str,
    mode: u32,
}

impl<'a> Launchd<'a> {
    fn new(config: &'a Config, uid: u32, library: &Path) -> Result<Self> {
        let label = config.service_name();
        ensure!(
            !matches!(label, "." | ".."),
            "invalid launchd label {label:?}"
        );
        let (domain, definition_dir, mode, path) = if uid == 0 {
            ("system".to_owned(), "LaunchDaemons", 0o644, SYSTEM_PATH)
        } else {
            ensure!(
                config.user.is_none() && config.group.is_none(),
                "--user/--group require system service mode; unset them for a LaunchAgent"
            );
            (format!("gui/{uid}"), "LaunchAgents", 0o600, USER_PATH)
        };
        Ok(Self {
            label,
            target: format!("{domain}/{label}"),
            domain,
            plist: library.join(definition_dir).join(format!("{label}.plist")),
            // launchd opens stdout and stderr after switching to UserName.
            log_dir: config
                .user
                .is_none()
                .then(|| library.join("Logs").join(label)),
            path,
            mode,
        })
    }

    fn install(&self, config: &Config, args: &[OsString]) -> Result<()> {
        let body = self.render(config, args)?;
        for path in [&config.workdir, &config.config_dir]
            .into_iter()
            .chain(&self.log_dir)
        {
            fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
        }
        write_definition(&self.plist, &body, self.mode)
            .with_context(|| format!("install launchd plist {}", self.plist.display()))?;
        println!("{}: installed {}", self.label, self.plist.display());
        Ok(())
    }

    fn uninstall(&self) -> Result<()> {
        self.stop()?;
        match fs::remove_file(&self.plist) {
            Ok(()) => {
                println!("{}: removed {}", self.label, self.plist.display());
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("remove launchd plist {}", self.plist.display()))
            }
        }
    }

    fn require_installed(&self) -> Result<()> {
        ensure!(
            self.plist.try_exists()?,
            "launchd service is not installed: {}",
            self.plist.display()
        );
        Ok(())
    }

    fn bootstrap(&self) -> Result<()> {
        run_launchctl(&[
            OsStr::new("bootstrap"),
            self.domain.as_ref(),
            self.plist.as_os_str(),
        ])
    }

    fn start(&self) -> Result<()> {
        self.require_installed()?;
        if self.inspect()?.is_some() {
            run_launchctl(&["kickstart".as_ref(), self.target.as_ref()])
        } else {
            self.bootstrap()
        }
    }

    fn restart(&self) -> Result<()> {
        self.require_installed()?;
        self.stop()?;
        self.bootstrap()
    }

    fn stop(&self) -> Result<()> {
        if self.inspect()?.is_some() {
            run_launchctl(&["bootout".as_ref(), self.target.as_ref()])?;
        }
        Ok(())
    }

    fn status(&self) -> Result<()> {
        if let Some(output) = self.inspect()? {
            print!("{}", String::from_utf8_lossy(&output.stdout));
        } else {
            let state = if self.plist.try_exists()? {
                "not loaded"
            } else {
                "not installed"
            };
            println!("{}: {state}", self.label);
        }
        Ok(())
    }

    fn inspect(&self) -> Result<Option<Output>> {
        let args = ["print".as_ref(), self.target.as_ref()];
        let output = launchctl(&args)?;
        if output.status.success() {
            Ok(Some(output))
        } else if String::from_utf8_lossy(&output.stderr).contains(&format!(
            "Could not find service \"{}\" in domain",
            self.label
        )) {
            Ok(None)
        } else {
            Err(launchctl_error(&args, &output))
        }
    }

    fn render(&self, config: &Config, args: &[OsString]) -> Result<String> {
        let mut arguments = String::new();
        for argument in
            std::iter::once(config.program.as_os_str()).chain(args.iter().map(OsString::as_os_str))
        {
            let _ = writeln!(arguments, "        <string>{}</string>", xml(argument)?);
        }
        let workdir = std::path::absolute(&config.workdir).context("resolve working directory")?;
        let mut credentials = String::new();
        for (key, value) in [("UserName", &config.user), ("GroupName", &config.group)] {
            if let Some(value) = value {
                let _ = writeln!(
                    credentials,
                    "<key>{key}</key>\n<string>{}</string>",
                    xml(value.as_ref())?
                );
            }
        }
        let logs = if let Some(log_dir) = &self.log_dir {
            format!(
                "<key>StandardOutPath</key>\n<string>{}</string>\n\
<key>StandardErrorPath</key>\n<string>{}</string>\n",
                xml(log_dir.join("out.log").as_os_str())?,
                xml(log_dir.join("err.log").as_os_str())?,
            )
        } else {
            String::new()
        };
        Ok(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
<key>Label</key>\n<string>{label}</string>\n\
<key>ProgramArguments</key>\n<array>\n{arguments}</array>\n\
<key>WorkingDirectory</key>\n<string>{workdir}</string>\n\
<key>EnvironmentVariables</key>\n<dict>\n<key>PATH</key>\n<string>{path}</string>\n</dict>\n\
<key>RunAtLoad</key>\n<true/>\n\
<key>KeepAlive</key>\n<true/>\n\
{logs}{credentials}</dict>\n</plist>\n",
            label = self.label,
            workdir = xml(workdir.as_os_str())?,
            path = self.path,
        ))
    }
}

fn xml(value: &OsStr) -> Result<String> {
    let value = value
        .to_str()
        .context("launchd plist value is not valid UTF-8")?;
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            '\r' => escaped.push_str("&#13;"),
            '\0'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}' => {
                bail!("launchd plist value contains an invalid XML character")
            }
            _ => escaped.push(character),
        }
    }
    Ok(escaped)
}

fn launchctl(args: &[&OsStr]) -> Result<Output> {
    Process::new("/bin/launchctl")
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("run launchctl {args:?}"))
}

fn run_launchctl(args: &[&OsStr]) -> Result<()> {
    let output = launchctl(args)?;
    if !output.status.success() {
        return Err(launchctl_error(args, &output));
    }
    Ok(())
}

fn launchctl_error(args: &[&OsStr], output: &Output) -> anyhow::Error {
    anyhow::anyhow!(
        "launchctl {args:?} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_preserves_arguments_and_paths() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::new("example.service", "Example", "/tmp/my app", "/tmp/a & <b>");
        let service = Launchd::new(&config, 501, directory.path()).unwrap();
        let body = service
            .render(&config, &["".into(), "--config".into(), "a&b\"\r".into()])
            .unwrap();
        assert!(body.contains("<key>Label</key>\n<string>example</string>"));
        assert!(body.contains("<string>/tmp/my app</string>\n        <string></string>"));
        assert!(body.contains("<string>a&amp;b&quot;&#13;</string>"));
        assert!(body.contains("<string>/tmp/a &amp; &lt;b&gt;</string>"));

        write_definition(&service.plist, &body, service.mode).unwrap();
        let output = Process::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&service.plist)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[test]
    #[ignore = "requires an active user launchd GUI domain"]
    fn launchd_lifecycle() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let name = format!("com.example.daemon-rs-test-{}", std::process::id());
        let config = Config::new(name, "Lifecycle test", "/bin/sleep", directory.path());
        let uid = effective_uid();
        ensure!(uid != 0, "run as the logged-in user");
        let service = Launchd::new(&config, uid, directory.path())?;
        service.install(&config, &["300".into()])?;
        let lifecycle = (|| {
            ensure!(
                service.inspect()?.is_none(),
                "install should only write the plist"
            );
            service.start()?;
            ensure!(service.inspect()?.is_some(), "start should load the agent");
            service.restart()?;
            ensure!(
                service.inspect()?.is_some(),
                "restart should reload the agent"
            );
            service.stop()?;
            ensure!(service.inspect()?.is_none(), "stop should unload the agent");
            service.start()?;
            Ok(())
        })();
        let cleanup = service.uninstall();
        lifecycle?;
        cleanup?;
        ensure!(
            service.inspect()?.is_none(),
            "uninstall should unload the agent"
        );
        ensure!(!service.plist.exists(), "uninstall should remove the plist");
        Ok(())
    }

    #[test]
    fn selects_launchd_domain_and_credentials() {
        let library = Path::new("/Users/example/Library");
        let config = Config::new("com.example.service", "Example", "/bin/true", "/tmp");
        let agent = Launchd::new(&config, 501, library).unwrap();
        assert_eq!(agent.target, "gui/501/com.example");
        assert_eq!(agent.plist, library.join("LaunchAgents/com.example.plist"));
        assert_eq!(agent.mode, 0o600);
        let body = agent.render(&config, &[]).unwrap();
        assert!(body.contains("/Users/example/Library/Logs/com.example/out.log"));

        let config = Config {
            user: Some("example".into()),
            group: Some("staff".into()),
            ..config
        };
        assert!(Launchd::new(&config, 501, library).is_err());
        let daemon = Launchd::new(&config, 0, Path::new("/Library")).unwrap();
        assert_eq!(daemon.target, "system/com.example");
        assert_eq!(
            daemon.plist,
            Path::new("/Library/LaunchDaemons/com.example.plist")
        );
        let body = daemon.render(&config, &[]).unwrap();
        assert!(body.contains("<key>UserName</key>\n<string>example</string>"));
        assert!(body.contains("<key>GroupName</key>\n<string>staff</string>"));
        assert!(!body.contains("<key>StandardOutPath</key>"));
    }
}
