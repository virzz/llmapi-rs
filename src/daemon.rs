use std::{
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, ensure, Context, Result};
use clap::{Args, Subcommand};

const LABEL: &str = "com.virzz.enyo.llmapi";
const PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    command: DaemonCommand,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Write the user LaunchAgent plist and create its log directory
    Install {
        #[arg(long)]
        workdir: Option<PathBuf>,
        #[arg(long, num_args = 1.., allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Delete the LaunchAgent plist without stopping a loaded service
    #[command(alias = "remove")]
    Uninstall,
    /// Load or restart the LaunchAgent
    Start,
    /// Reload the LaunchAgent plist and restart the service
    Restart,
    /// Unload the LaunchAgent
    Stop,
    /// Print the launchctl service status
    Status,
}

impl DaemonArgs {
    pub fn execute(&self) -> Result<()> {
        let home = dirs::home_dir().context("find home directory")?;
        let plist = home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"));
        match &self.command {
            DaemonCommand::Install { workdir, args } => {
                let executable = std::env::current_exe().context("find llmapi executable")?;
                let workdir = workdir
                    .as_deref()
                    .unwrap_or(Path::new("."))
                    .canonicalize()
                    .context("resolve daemon workdir")?;
                ensure!(workdir.is_dir(), "daemon workdir is not a directory");
                let arguments = if args.is_empty() {
                    vec![OsString::from("server")]
                } else {
                    args.clone()
                };
                let log_dir = home.join("Library/Logs").join(LABEL);
                install(&plist, &log_dir, &executable, &workdir, &arguments)
            }
            DaemonCommand::Uninstall => uninstall(&plist),
            DaemonCommand::Start => {
                ensure!(
                    plist.is_file(),
                    "LaunchAgent is not installed: {}",
                    plist.display()
                );
                let service = service_target()?;
                let output = launchctl(&["print", &service])?;
                if output.status.success() {
                    run_launchctl(&["kickstart", "-k", &service])
                } else if service_not_found(&output.stderr) {
                    run_launchctl(&["bootstrap", &domain_target()?, path_text(&plist)?])
                } else {
                    bail!(
                        "launchctl print failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )
                }
            }
            DaemonCommand::Restart => {
                ensure!(
                    plist.is_file(),
                    "LaunchAgent is not installed: {}",
                    plist.display()
                );
                let service = service_target()?;
                let output = launchctl(&["print", &service])?;
                if output.status.success() {
                    run_launchctl(&["bootout", &service])?;
                } else if !service_not_found(&output.stderr) {
                    bail!(
                        "launchctl print failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                run_launchctl(&["bootstrap", &domain_target()?, path_text(&plist)?])
            }
            DaemonCommand::Stop => run_launchctl(&["bootout", &service_target()?]),
            DaemonCommand::Status => {
                let output = launchctl(&["print", &service_target()?])?;
                if output.status.success() {
                    print!("{}", String::from_utf8_lossy(&output.stdout));
                    Ok(())
                } else if service_not_found(&output.stderr) {
                    println!(
                        "{}",
                        if plist.is_file() {
                            "not loaded"
                        } else {
                            "not installed"
                        }
                    );
                    Ok(())
                } else {
                    bail!(
                        "launchctl print failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )
                }
            }
        }
    }
}

fn install(
    plist: &Path,
    log_dir: &Path,
    executable: &Path,
    workdir: &Path,
    args: &[OsString],
) -> Result<()> {
    fs::create_dir_all(plist.parent().context("LaunchAgent path has no parent")?)?;
    fs::create_dir_all(log_dir)?;
    let body = render_plist(executable, workdir, args, log_dir)?;
    let temp = plist.with_extension(format!("plist.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, plist)?;
        Ok::<_, std::io::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("install LaunchAgent {}", plist.display()))
}

fn uninstall(plist: &Path) -> Result<()> {
    match fs::remove_file(plist) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove LaunchAgent {}", plist.display())),
    }
}

fn render_plist(
    executable: &Path,
    workdir: &Path,
    args: &[OsString],
    log_dir: &Path,
) -> Result<String> {
    let mut arguments = format!(
        "        <string>{}</string>\n",
        xml(path_text(executable)?)?
    );
    for argument in args {
        arguments.push_str(&format!(
            "        <string>{}</string>\n",
            xml(os_text(argument)?)?
        ));
    }
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
    <key>Label</key>\n    <string>{LABEL}</string>\n\
    <key>ProgramArguments</key>\n    <array>\n{arguments}    </array>\n\
    <key>WorkingDirectory</key>\n    <string>{}</string>\n\
    <key>EnvironmentVariables</key>\n    <dict>\n        <key>PATH</key>\n        <string>{PATH}</string>\n    </dict>\n\
    <key>RunAtLoad</key>\n    <true/>\n\
    <key>KeepAlive</key>\n    <true/>\n\
    <key>StandardOutPath</key>\n    <string>{}</string>\n\
    <key>StandardErrorPath</key>\n    <string>{}</string>\n\
</dict>\n</plist>\n",
        xml(path_text(workdir)?)?,
        xml(path_text(&log_dir.join("out.log"))?)?,
        xml(path_text(&log_dir.join("err.log"))?)?,
    ))
}

fn path_text(path: &Path) -> Result<&str> {
    os_text(path.as_os_str())
}

fn os_text(value: &OsStr) -> Result<&str> {
    value
        .to_str()
        .context("LaunchAgent value is not valid UTF-8")
}

fn xml(value: &str) -> Result<String> {
    if value
        .chars()
        .any(|character| character < ' ' && !matches!(character, '\t' | '\n' | '\r'))
    {
        bail!("LaunchAgent value contains an invalid XML character");
    }
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

fn domain_target() -> Result<String> {
    let output = Command::new("id").arg("-u").output().context("run id -u")?;
    ensure!(output.status.success(), "id -u failed");
    let uid = String::from_utf8(output.stdout).context("parse uid")?;
    Ok(format!("gui/{}", uid.trim()))
}

fn service_target() -> Result<String> {
    Ok(format!("{}/{LABEL}", domain_target()?))
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("launchctl")
        .args(args)
        .output()
        .with_context(|| format!("run launchctl {}", args.join(" ")))
}

fn service_not_found(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr)
        .contains(&format!("Could not find service \"{LABEL}\" in domain"))
}

fn run_launchctl(args: &[&str]) -> Result<()> {
    let output = launchctl(args)?;
    ensure!(
        output.status.success(),
        "launchctl {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use tempfile::tempdir;

    #[test]
    fn recognizes_only_missing_service_errors() {
        assert!(service_not_found(b"Bad request.\nCould not find service \"com.virzz.enyo.llmapi\" in domain for user gui: 501"));
        assert!(!service_not_found(b"Permission denied"));
        assert!(!service_not_found(
            b"Could not find service \"other\" in domain"
        ));
    }

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

    #[test]
    fn installs_and_removes_private_plist_without_touching_logs() {
        let directory = tempdir().unwrap();
        let plist = directory
            .path()
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist"));
        let log_dir = directory.path().join("Logs").join(LABEL);
        install(
            &plist,
            &log_dir,
            Path::new("/tmp/llmapi"),
            Path::new("/tmp/work & <test>"),
            &["server".into(), "--default".into(), "one&two".into()],
        )
        .unwrap();
        let body = fs::read_to_string(&plist).unwrap();
        assert!(body.contains("/tmp/work &amp; &lt;test&gt;"));
        assert!(body.contains("<string>one&amp;two</string>"));
        assert!(body.contains(&format!("{LABEL}/out.log")));
        assert!(log_dir.is_dir());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&plist).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let validation = Command::new("plutil")
            .arg("-lint")
            .arg(&plist)
            .output()
            .unwrap();
        assert!(
            validation.status.success(),
            "{}",
            String::from_utf8_lossy(&validation.stderr)
        );
        uninstall(&plist).unwrap();
        uninstall(&plist).unwrap();
        assert!(!plist.exists());
        assert!(log_dir.is_dir());
    }
}
