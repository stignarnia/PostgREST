use clap::Parser;
use service_manager::*;

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
}

const SERVICE_LABEL: &str = "postgrest-server";

pub fn handle_cli() -> Result<bool, Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.daemon {
        install_service()?;
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

fn install_service() -> Result<(), Box<dyn std::error::Error>> {
    let label: ServiceLabel = SERVICE_LABEL.parse()?;
    let manager = <dyn ServiceManager>::native()
        .map_err(|e| format!("Failed to detect a supported service manager: {}", e))?;

    let exe_path = std::env::current_exe()?;

    manager.install(ServiceInstallCtx {
        label: label.clone(),
        program: exe_path,
        args: vec![],
        contents: None,
        username: None,
        working_directory: None,
        environment: None,
        autostart: true,
    })?;

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
