use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use ::daemon::{Command, Daemon};
use anyhow::{Context, Result};
use clap::{Args, Subcommand};

#[cfg(target_os = "macos")]
const SERVICE_NAME: &str = "com.virzz.enyo.llmapi";
#[cfg(target_os = "linux")]
const SERVICE_NAME: &str = "llmapi";

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    command: DaemonCommand,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Install the service definition
    Install {
        #[arg(long)]
        workdir: Option<PathBuf>,
        #[arg(long, num_args = 1.., allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Stop and remove the service definition
    #[command(alias = "remove")]
    Uninstall,
    /// Start the service
    Start,
    /// Reload the service definition and restart
    Restart,
    /// Stop the service
    Stop,
    /// Show the service status
    Status,
}

impl DaemonArgs {
    pub fn execute(&self) -> Result<()> {
        let (workdir, command) = self.command.backend_command()?;
        let executable = std::env::current_exe().context("find llmapi executable")?;
        Daemon::new(SERVICE_NAME, "LLMApi proxy", executable, workdir).execute(&command)
    }
}

impl DaemonCommand {
    fn backend_command(&self) -> Result<(PathBuf, Command)> {
        let mut directory = PathBuf::from(".");
        let command = match self {
            Self::Install { workdir, args } => {
                directory = workdir
                    .as_deref()
                    .unwrap_or(Path::new("."))
                    .canonicalize()
                    .context("resolve daemon workdir")?;
                anyhow::ensure!(directory.is_dir(), "daemon workdir is not a directory");
                let args = if args.is_empty() {
                    vec!["server".into()]
                } else {
                    args.clone()
                };
                Command::Install { args }
            }
            Self::Uninstall => Command::Uninstall,
            Self::Start => Command::Start,
            Self::Restart => Command::Restart,
            Self::Stop => Command::Stop,
            Self::Status => Command::Status,
        };
        Ok((directory, command))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_install_arguments() {
        let command = crate::Cmd::try_parse_from([
            "llmapi",
            "daemon",
            "install",
            "--workdir",
            "/tmp",
            "--args",
            "server",
            "--config",
            "config.yaml",
        ])
        .unwrap();
        let crate::Command::Daemon(DaemonArgs {
            command: DaemonCommand::Install { workdir, args },
        }) = command.command
        else {
            panic!("expected daemon install");
        };
        assert_eq!(workdir, Some(PathBuf::from("/tmp")));
        assert_eq!(args, ["server", "--config", "config.yaml"]);
        assert!(command.config.is_none());
        let (workdir, command) = DaemonCommand::Install { workdir, args }
            .backend_command()
            .unwrap();
        assert_eq!(workdir, Path::new("/tmp").canonicalize().unwrap());
        assert!(matches!(
            command,
            ::daemon::Command::Install { args } if args == ["server", "--config", "config.yaml"]
        ));

        let (_, command) = DaemonCommand::Install {
            workdir: None,
            args: vec![],
        }
        .backend_command()
        .unwrap();
        assert!(matches!(
            command,
            ::daemon::Command::Install { args } if args == ["server"]
        ));

        assert!(crate::Cmd::try_parse_from(["llmapi", "daemon", "remove"]).is_ok());
        assert!(matches!(
            crate::Cmd::try_parse_from(["llmapi", "daemon", "restart"])
                .unwrap()
                .command,
            crate::Command::Daemon(DaemonArgs {
                command: DaemonCommand::Restart
            })
        ));
    }
}
