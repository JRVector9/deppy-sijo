fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let result = match args.as_slice() {
        [flag] if flag == "--version" => {
            println!("deppy-remote-host {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        #[cfg(unix)]
        [flag] if flag == "--stdio" => deppy_remote_host::supervisor::run_stdio(),
        #[cfg(unix)]
        [flag] if flag == "--supervisor" => deppy_remote_host::supervisor::run_supervisor(
            std::sync::Arc::new(deppy_remote_host::services::handle_rpc),
        ),
        _ => Err(anyhow::anyhow!(
            "usage: deppy-remote-host --stdio | --version"
        )),
    };
    if let Err(error) = result {
        eprintln!("deppy-remote-host: {error}");
        std::process::exit(1);
    }
}
