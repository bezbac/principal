use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tracing::{debug, info, Level};

#[derive(Parser)]
#[command(version, about = "Suspend docker containers when they're not used", long_about = None)]
struct Args {
    /// Turn debugging information on
    #[arg(long, short = 'd')]
    debug: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Setup tracing subscriber

    let tracing_level = match args.debug {
        true => Level::DEBUG,
        false => Level::INFO,
    };

    let collector = tracing_subscriber::fmt()
        .with_max_level(tracing_level)
        .finish();

    tracing::subscriber::set_global_default(collector)?;

    // Setup main loop

    info!("Staring main loop");

    let mut interval = tokio::time::interval(Duration::from_secs(2));
    loop {
        interval.tick().await;
        debug!("Tick");
    }
}
