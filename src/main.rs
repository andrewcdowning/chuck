#[tokio::main]
async fn main() {
    if let Err(e) = chuck::run(chuck::Options::from_env()).await {
        eprintln!("{e}");
        std::process::exit(2);
    }
}
