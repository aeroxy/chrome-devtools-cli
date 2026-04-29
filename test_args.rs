use clap::{Parser, CommandFactory};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Parser)]
enum Cmd {
    Foo,
    Bar,
}

fn main() {
    match Cli::try_parse() {
        Ok(_) => println!("ok"),
        Err(e) => {
            e.print().unwrap();
            println!();
            let mut cmd = Cli::command();
            cmd.print_help().unwrap();
        }
    }
}
