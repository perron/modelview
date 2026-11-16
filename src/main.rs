fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--load-worker") {
        let code = modelview::load::worker_main(&args);
        std::process::exit(code);
    }
    if args.first().map(String::as_str) == Some("--internal-sleep") {
        let ms: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1_000);
        std::thread::sleep(std::time::Duration::from_millis(ms));
        return;
    }
    // `modelview <file>` opens the GUI. Flags that belong to subcommands stay with clap.
    if modelview::cli::should_launch_gui(&args) {
        let file = args
            .iter()
            .find(|a| !a.starts_with('-'))
            .map(|s| std::path::PathBuf::from(s));
        let code = modelview::gui::run(file);
        std::process::exit(code);
    }
    let code = modelview::cli::run(&mut args);
    std::process::exit(code);
}
