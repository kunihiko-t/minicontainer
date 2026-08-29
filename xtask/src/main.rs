use xtask::{cli, run};

fn main() {
    let command = match cli::parse_os(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("{error}\n\n{}", cli::help());
            std::process::exit(2);
        }
    };
    if let Err(error) = run(command) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
