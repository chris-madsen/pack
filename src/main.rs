fn main() {
    if let Err(error) = pack::adapters::cli::run(std::env::args()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
