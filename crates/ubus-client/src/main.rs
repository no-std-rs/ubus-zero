#[cfg(target_os = "linux")]
mod app;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    {
        app::main()
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("ubus-client currently supports Linux Unix sockets");
        std::process::ExitCode::from(1)
    }
}
