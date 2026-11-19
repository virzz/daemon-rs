use clap::Parser;
use daemon::{Command, Daemon, DaemonCommand};
#[cfg(target_os = "linux")]
use dbus::blocking::Connection;
use std::ffi::{OsStr, OsString};

#[derive(Debug, Parser)]
struct Cli {
    #[command(flatten)]
    daemon: DaemonCommand,
}

#[test]
fn systemd_unit_contains_go_compatible_defaults() {
    let daemon = Daemon::new(
        "ExampleService",
        "Example service",
        "/usr/local/bin/example",
        "/var/lib/example",
    )
    .config_dir("/etc/example")
    .user("example")
    .group("example");
    let model = daemon.systemd_unit_for(false).expect("valid unit");
    assert_eq!(model.service.service_type, "exec");
    assert_eq!(model.install.wanted_by, "multi-user.target");
    let unit = model.render();
    for expected in [
        "Description=Example service",
        "Wants=network.target",
        "Type=exec",
        "ExecStart=/usr/local/bin/example",
        "WorkingDirectory=/var/lib/example",
        "User=example",
        "Group=example",
        "RuntimeDirectory=daemon-rs-%N",
        "PIDFile=%t/daemon-rs-%N/ExampleService.pid",
        "ExecStartPre=/bin/rm -f %t/daemon-rs-%N/ExampleService.pid",
        r#"ExecStartPost=/bin/sh -c 'echo "$$1" > "$$2"' sh ${MAINPID} "%t/daemon-rs-%N/ExampleService.pid""#,
        "Restart=always",
        "RestartSec=0",
        "StartLimitIntervalSec=30",
        "StartLimitBurst=10",
        "RestartPreventExitStatus=SIGKILL",
        "EnvironmentFile=-/etc/example/ExampleService.env",
        "WantedBy=multi-user.target",
    ] {
        assert!(
            unit.contains(expected),
            "unit missing {expected:?}:\n{unit}"
        );
    }
}

#[test]
fn systemd_unit_inherits_manager_identity_by_default() {
    let daemon = Daemon::new("example", "Example", "/usr/bin/example", "/tmp");
    for (user_manager, pid_file, target) in [
        (true, "%t/example.pid", "default.target"),
        (false, "/run/example.pid", "multi-user.target"),
    ] {
        let unit = daemon.systemd_unit_for(user_manager).expect("valid unit");
        assert_eq!(unit.service.user, None);
        assert_eq!(unit.service.group, None);
        assert_eq!(unit.service.pid_file, pid_file);
        assert_eq!(unit.install.wanted_by, target);
        assert!(!unit.render().contains("RuntimeDirectory="));
    }
}

#[test]
fn cli_exposes_single_instance_commands_and_aliases() {
    for (name, expected) in [
        ("install", "install"),
        ("i", "install"),
        ("remove", "uninstall"),
        ("rm", "uninstall"),
        ("uninstall", "uninstall"),
        ("run", "start"),
        ("r", "restart"),
        ("re", "restart"),
        ("info", "status"),
        ("if", "status"),
    ] {
        let cli = Cli::try_parse_from(["example", name]).expect("valid daemon command");
        assert_eq!(cli.daemon.command.to_string(), expected);
    }
}

#[test]
fn install_arguments_are_rendered_in_exec_start() {
    let cli = Cli::try_parse_from([
        "example",
        "install",
        "--",
        "serve",
        "--address",
        "127.0.0.1:8080",
        "two words",
    ])
    .expect("install arguments");
    let Command::Install { args } = cli.daemon.command else {
        panic!("expected install command")
    };
    assert_eq!(
        args,
        ["serve", "--address", "127.0.0.1:8080", "two words"].map(OsString::from)
    );

    let daemon = Daemon::new("example", "Example", "/usr/bin/example", "/tmp");
    let unit = daemon
        .systemd_unit_with_args_for(&args, false)
        .expect("valid unit")
        .render();
    assert!(
        unit.contains("ExecStart=/usr/bin/example serve --address 127.0.0.1:8080 \"two words\"")
    );
    assert_eq!(
        args.last().map(OsString::as_os_str),
        Some(OsStr::new("two words"))
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires an active systemd --user session; run with: cargo test --test systemd systemd_lifecycle -- --ignored"]
fn systemd_lifecycle() {
    Connection::new_session().expect("connect to the user D-Bus session");

    let name = format!("daemon-rs-test-{}", std::process::id());
    let root = tempfile::tempdir().expect("temporary daemon directory");
    let daemon = Daemon::new(
        name.as_str(),
        "daemon-rs integration test",
        "/bin/sleep",
        root.path(),
    );
    let install = Command::Install {
        args: vec!["300".into()],
    };
    daemon.execute(&install).expect("install service");
    let lifecycle = (|| {
        daemon.execute(&Command::Status)?;
        daemon.execute(&Command::Start)?;
        daemon.execute(&Command::Stop)?;
        daemon.execute(&Command::Restart)?;
        Ok::<_, anyhow::Error>(())
    })();
    let cleanup = daemon.execute(&Command::Uninstall);
    lifecycle.expect("systemd lifecycle");
    cleanup.expect("uninstall service");
}
