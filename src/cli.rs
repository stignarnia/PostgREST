use crate::updater::{self, Outcome};
use clap::Parser;
use service_manager::*;
use std::{path::PathBuf, sync::atomic::Ordering};

#[derive(Parser, Debug)]
#[command(
    name = "PostgREST",
    about = "A lightweight PostgREST-like server",
    version
)]
pub struct Args {
    /// Install and start as a background service
    #[arg(short = 'd', long = "daemon")]
    pub daemon: bool,

    /// Stop and uninstall the background service
    #[arg(long = "disable")]
    pub disable: bool,

    /// Install the latest release now and restart the service
    #[arg(long = "update")]
    pub update: bool,

    /// Run without the automatic updater (kept by --daemon)
    #[arg(long = "no-update")]
    pub no_update: bool,
}

const SERVICE_LABEL: &str = "postgrest-server";

pub fn handle_cli() -> Result<bool, Box<dyn std::error::Error>> {
    let args = Args::parse();
    updater::ENABLED.store(!args.no_update, Ordering::Relaxed);

    if args.update {
        return run_update().map(|()| true);
    }

    if args.daemon {
        install_service(args.no_update)?;
        println!("Service installed and started successfully.");
        return Ok(true);
    }

    if args.disable {
        uninstall_service()?;
        println!("Service stopped and uninstalled successfully.");
        return Ok(true);
    }

    Ok(false)
}

fn install_service(no_update: bool) -> Result<(), Box<dyn std::error::Error>> {
    let label: ServiceLabel = SERVICE_LABEL.parse()?;
    let manager = <dyn ServiceManager>::native()
        .map_err(|e| format!("Failed to detect a supported service manager: {}", e))?;

    let exe_path = std::env::current_exe()?;

    manager.install(ServiceInstallCtx {
        label: label.clone(),
        program: exe_path,
        args: if no_update { vec!["--no-update".into()] } else { vec![] },
        contents: None,
        username: None,
        working_directory: None,
        environment: None,
        autostart: true,
        restart_policy: Default::default(),
    })?;

    // sc.exe create sets no restart policy, and the updater relies on one:
    // it exits with status 1 so the service manager starts the new binary.
    #[cfg(windows)]
    {
        let status = std::process::Command::new("sc.exe")
            .args([
                "failure",
                SERVICE_LABEL,
                "reset=",
                "86400",
                "actions=",
                "restart/5000/restart/5000/restart/5000",
            ])
            .status()?;
        if !status.success() {
            return Err("sc.exe failure could not set the restart policy".into());
        }
    }

    manager.start(ServiceStartCtx { label })?;

    Ok(())
}

fn uninstall_service() -> Result<(), Box<dyn std::error::Error>> {
    let label: ServiceLabel = SERVICE_LABEL.parse()?;
    let manager = <dyn ServiceManager>::native()
        .map_err(|e| format!("Failed to detect a supported service manager: {}", e))?;

    // We ignore errors on stop in case it's already stopped
    let _ = manager.stop(ServiceStopCtx {
        label: label.clone(),
    });

    manager.uninstall(ServiceUninstallCtx { label })?;

    Ok(())
}

/// The executable the installed service runs, if it is installed.
fn service_program() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let unit =
            std::fs::read_to_string(format!("/etc/systemd/system/{SERVICE_LABEL}.service")).ok()?;
        let line = unit.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
        Some(PathBuf::from(line.split_whitespace().next()?))
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("sc.exe")
            .args(["qc", SERVICE_LABEL])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().find(|l| l.trim_start().starts_with("BINARY_PATH_NAME"))?;
        let path = line.split_once(':')?.1.trim();
        let path = match path.strip_prefix('"') {
            Some(quoted) => quoted.split('"').next()?,
            None => path.split(" --").next()?,
        };
        Some(PathBuf::from(path.trim()))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => {
            if cfg!(windows) {
                a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
            } else {
                a == b
            }
        }
        _ => false,
    }
}

fn run_update() -> Result<(), Box<dyn std::error::Error>> {
    let installed = service_program();
    if let Some(program) = &installed {
        if !same_file(program, &std::env::current_exe()?) {
            return Err(format!(
                "the service runs {}; run --update with that executable",
                program.display()
            )
            .into());
        }
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let current = updater::format_version(updater::current());
    match rt.block_on(updater::check_and_install())? {
        Outcome::UpToDate(latest) => {
            println!(
                "Up to date: running {current}, latest release {}.",
                updater::format_version(latest)
            );
        }
        Outcome::Installed(latest) => {
            println!("Installed {} over {current}.", updater::format_version(latest));
            if installed.is_some() {
                let label: ServiceLabel = SERVICE_LABEL.parse()?;
                let manager = <dyn ServiceManager>::native()?;
                let _ = manager.stop(ServiceStopCtx {
                    label: label.clone(),
                });
                manager.start(ServiceStartCtx { label })?;
                println!("Service restarted.");
            }
        }
    }
    Ok(())
}
