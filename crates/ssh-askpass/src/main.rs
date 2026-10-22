fn main() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--version")
    {
        println!("deppy-ssh-askpass {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    #[cfg(unix)]
    {
        let prompt = std::env::args().nth(1).unwrap_or_default();
        if ssh_askpass::run_from_environment(&prompt, &mut std::io::stdout().lock()).is_ok() {
            return;
        }
    }
    // OpenSSH treats any nonzero result as cancellation. Never print prompt/secret diagnostics.
    std::process::exit(1);
}
