use anyhow::Result;
use clap::Parser;
use duration_string::DurationString;
use pcap::Device;
use tracing::{debug, info, Level};

#[derive(Parser)]
#[command(version, about = "Suspend docker containers when they're not used", long_about = None)]
struct Args {
    /// Turn debugging information on
    #[arg(long, short = 'd')]
    debug: bool,

    /// How frequently to count network packets
    #[arg(long, default_value = "10s")]
    rate: DurationString,

    /// How long to wait before suspending the container
    /// in case of insufficient network activity
    #[arg(long, default_value = "30s")]
    timeout: Option<DurationString>,

    /// Which port to monitor
    #[arg(long, short = 'p')]
    port: Option<usize>,

    /// Minimum amount of packets received within
    /// interval to be considered active
    #[arg(long, default_value = "30")]
    threshold: usize,

    /// Which container should be controlled
    #[arg(long)]
    container: String,
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

    // Open packet capture handle

    let mut cap = Device::lookup().unwrap().unwrap().open().unwrap();

    cap.direction(pcap::Direction::In)?;

    if let Some(port) = args.port {
        let filter = format!("port {}", port);
        cap.filter(&filter, true)?;
    }

    // Setup main loop

    info!("Staring main loop");

    let mut packet_count: u128 = 0;
    let mut interval = tokio::time::interval(args.rate.into());
    loop {
        interval.tick().await;

        let stats = cap.stats()?;

        let new_packet_count: u128 = stats.received.try_into()?;
        let delta = new_packet_count - packet_count;
        packet_count = new_packet_count;

        info!("Received {} packets in last {}", delta, args.rate);

        if delta > args.threshold as u128 {
            debug!("Threshold hit");
            // TODO: Cancel suspension of container or start it
        } else {
            debug!("Threshold missed");
            // TODO: Schedule suspension of container in {args.timeout}
        }
    }
}
