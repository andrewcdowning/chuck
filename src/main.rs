#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("eval-tags") {
        std::process::exit(chuck::cli::eval_tags(&args[1..]));
    }
    if let Err(e) = chuck::run(chuck::Options::from_env()).await {
        eprintln!("{e}");
        std::process::exit(2);
    }
}
