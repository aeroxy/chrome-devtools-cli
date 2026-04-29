use clap::{Parser, CommandFactory};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Parser)]
enum Cmd {
    Foo,
}

fn main() {
    match Cli::try_parse() {
        Ok(_) => (),
        Err(e) => {
            let err_str = e.render().to_string();
            let clean = err_str.replace("For more information, try '--help'.\n", "");
            eprintln!("{}", clean);
        }
    }
}
