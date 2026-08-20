fn main() {
    if let Err(err) = boringbuilder::cli::run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
