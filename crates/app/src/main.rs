fn main() {
    if let Err(e) = mailthin_app::run() {
        eprintln!("Mailthin failed to start: {e:#}");
        std::process::exit(1);
    }
}
