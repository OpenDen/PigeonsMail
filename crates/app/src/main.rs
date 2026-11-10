fn main() {
    if let Err(e) = mailthin_app::run() {
        eprintln!("PigeonMail failed to start: {e:#}");
        std::process::exit(1);
    }
}
