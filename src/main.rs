use anyhow::Result;
use clap::Parser;
use duration_string::DurationString;
use pcap::Device;
use tokio::{task::JoinHandle, time::Instant};
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

    // TODO: Base `is_running` on actual container state
    let is_running = true;

    fn suspend_container() {
        debug!("Suspending container");
        // TODO: Actually suspend container
    }

    fn start_container() {
        debug!("Starting container");
        // TODO: Actually start container
    }

    let mut packet_count: u128 = 0;
    let mut interval = tokio::time::interval(args.rate.into());
    let mut join_handle: Option<(JoinHandle<()>, Instant)> = None;
    loop {
        interval.tick().await;

        let stats = cap.stats()?;

        let new_packet_count: u128 = stats.received.try_into()?;
        let delta = new_packet_count - packet_count;
        packet_count = new_packet_count;

        info!("Received {} packets in last {}", delta, args.rate);

        if delta > args.threshold as u128 {
            info!("Threshold hit");

            if let Some((handle, _)) = &join_handle {
                info!("Aborting suspension");
                handle.abort();
                join_handle = None;
            }

            if !is_running {
                start_container()
            }
        } else {
            if is_running {
                if let Some((handle, suspension_started_at)) = &join_handle {
                    if !handle.is_finished() {
                        if let Some(timeout) = args.timeout {
                            let time_until_suspension =
                                (*suspension_started_at + timeout.into()) - Instant::now();

                            info!(
                                "Threshold missed, suspension scheduled in {} seconds",
                                time_until_suspension.as_secs()
                            );
                        }
                    } else {
                        join_handle = None;
                    }
                } else {
                    if let Some(timeout) = args.timeout {
                        info!("Threshold missed. Scheduling suspension in {}", timeout);

                        let handle = tokio::spawn(async move {
                            tokio::time::sleep(timeout.into()).await;
                            suspend_container()
                        });

                        join_handle = Some((handle, Instant::now()))
                    } else {
                        info!("Threshold missed. Suspending container");
                        suspend_container()
                    }
                }
            }
        }
    }
}
