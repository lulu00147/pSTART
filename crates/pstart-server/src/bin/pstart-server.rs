//! pSTART server executable: SCM host, console mode, install/uninstall.

#[cfg(not(windows))]
fn main() {
    eprintln!("pstart-server is only supported on Windows");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    if let Err(error) = host::run() {
        host::log_error(&*error);
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod host {
    use std::error::Error;
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::Duration;

    use pstart_server::command::{parse_command, usage, ServerCommand};
    use pstart_server::config::ServiceConfig;
    use pstart_server::pipeline::NtfsSource;
    use pstart_server::runtime::{RuntimeConfig, ServerRuntime};
    use pstart_server::SERVICE_NAME;
    use windows_service::define_windows_service;
    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    define_windows_service!(ffi_service_main, service_main);

    pub fn run() -> Result<(), Box<dyn Error>> {
        match parse_command(std::env::args().skip(1))? {
            ServerCommand::Service => {
                service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
            }
            ServerCommand::Console => run_console()?,
            ServerCommand::Install => install()?,
            ServerCommand::Uninstall => uninstall()?,
            ServerCommand::Help => println!("{}", usage()),
            ServerCommand::Version => {
                println!("pstart-server {}", env!("CARGO_PKG_VERSION"));
            }
        }
        Ok(())
    }

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(error) = run_service() {
            log_error(&*error);
        }
    }

    fn run_service() -> Result<(), Box<dyn Error>> {
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let event_handler = move |control| -> ServiceControlHandlerResult {
            match control {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = shutdown_tx.send(());
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };
        let status = service_control_handler::register(SERVICE_NAME, event_handler)?;
        status.set_service_status(service_status(
            ServiceState::StartPending,
            ServiceControlAccept::empty(),
            30,
            ServiceExitCode::NO_ERROR,
        ))?;

        let result = run_loaded(&status, &shutdown_rx);
        if result.is_err() {
            let _ = status.set_service_status(service_status(
                ServiceState::Stopped,
                ServiceControlAccept::empty(),
                0,
                ServiceExitCode::ServiceSpecific(1),
            ));
        }
        result
    }

    fn run_loaded(
        status: &service_control_handler::ServiceStatusHandle,
        shutdown_rx: &mpsc::Receiver<()>,
    ) -> Result<(), Box<dyn Error>> {
        let config = ServiceConfig::from_env()?;
        config.ensure_directories()?;
        let source = NtfsSource::open(&config.root)?;
        let runtime =
            ServerRuntime::start(config.engine_config()?, source, runtime_config(&config))?;

        status.set_service_status(service_status(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            0,
            ServiceExitCode::NO_ERROR,
        ))?;
        let _ = shutdown_rx.recv();

        status.set_service_status(service_status(
            ServiceState::StopPending,
            ServiceControlAccept::empty(),
            10,
            ServiceExitCode::NO_ERROR,
        ))?;
        runtime.shutdown()?;
        status.set_service_status(service_status(
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            0,
            ServiceExitCode::NO_ERROR,
        ))?;
        Ok(())
    }

    fn run_console() -> Result<(), Box<dyn Error>> {
        let config = ServiceConfig::from_env()?;
        config.ensure_directories()?;
        let source = NtfsSource::open(&config.root)?;
        let runtime =
            ServerRuntime::start(config.engine_config()?, source, runtime_config(&config))?;
        println!(
            "pSTART server listening on {}",
            config.socket_path.display()
        );
        println!("Press Enter to stop.");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        runtime.shutdown()?;
        Ok(())
    }

    fn install() -> Result<(), Box<dyn Error>> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from("pSTART Server"),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: std::env::current_exe()?,
            launch_arguments: Vec::new(),
            dependencies: Vec::new(),
            account_name: None,
            account_password: None,
        };
        let service = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG)?;
        service.set_description("pSTART local file search index service")?;
        println!("installed {SERVICE_NAME}");
        Ok(())
    }

    fn uninstall() -> Result<(), Box<dyn Error>> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(SERVICE_NAME, ServiceAccess::DELETE)?;
        service.delete()?;
        println!("removed {SERVICE_NAME}");
        Ok(())
    }

    fn runtime_config(config: &ServiceConfig) -> RuntimeConfig {
        RuntimeConfig {
            socket_path: config.socket_path.clone(),
            poll_interval: config.poll_interval,
            max_connections: config.max_connections,
            query_limits: pstart_server::ConnectionLimits {
                max_queries: config.query_limit_per_second,
                window: Duration::from_secs(1),
            },
            global_query_limits: Some(pstart_server::ConnectionLimits {
                max_queries: config.global_query_limit_per_second,
                window: Duration::from_secs(1),
            }),
        }
    }

    fn service_status(
        state: ServiceState,
        controls: ServiceControlAccept,
        wait_seconds: u64,
        exit_code: ServiceExitCode,
    ) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: controls,
            exit_code,
            checkpoint: 0,
            wait_hint: Duration::from_secs(wait_seconds),
            process_id: None,
        }
    }

    const MAX_LOG_BYTES: u64 = 1_048_576;

    fn append_bounded_log(path: &Path, line: &str) -> std::io::Result<()> {
        let length = fs::metadata(path).map_or(0, |metadata| metadata.len());
        if length >= MAX_LOG_BYTES {
            let backup = path.with_extension("log.1");
            let _ = fs::remove_file(&backup);
            let _ = fs::rename(path, &backup);
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{line}")
    }

    pub fn log_error(error: &dyn Error) {
        eprintln!("pSTART server error: {error}");
        if let Ok(config) = ServiceConfig::from_env() {
            let _ = config.ensure_directories();
            let line = format!("pSTART server error: {error}");
            let _ = append_bounded_log(&config.data_dir.join("server.log"), &line);
        }
    }

    #[allow(dead_code)]
    fn _assert_os_str(_value: &OsStr) {}

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rotates_log_at_limit() {
            let dir = std::env::temp_dir().join(format!("pstart-log-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("server.log");
            fs::write(&path, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).unwrap();
            append_bounded_log(&path, "new").unwrap();
            assert!(path.with_extension("log.1").exists());
            assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
