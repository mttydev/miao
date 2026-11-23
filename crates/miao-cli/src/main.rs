mod cli;
mod locate;

fn main() -> std::process::ExitCode {
    cli::run(std::env::args().skip(1).collect())
}
