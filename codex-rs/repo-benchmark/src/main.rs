fn main() {
    if let Err(error) = repo_benchmark::cli::run(std::env::args().skip(1).collect()) {
        eprintln!("Repo Benchmark: {error:#}");
        std::process::exit(1);
    }
}
