use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use bollard::{
    container::{ListContainersOptions, StartContainerOptions},
    Docker,
};
use clap::Parser;
use duration_string::DurationString;
use pcap::Device;
use tokio::time::Instant;
use tracing::{debug, info, level_filters::LevelFilter, warn};
use tracing_subscriber::EnvFilter;

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

    let filter = EnvFilter::try_from_default_env().unwrap_or(
        EnvFilter::default()
            // Set the base level when not matched by other directives to WARN.
            .add_directive(LevelFilter::WARN.into())
            // Set the level for the this program according to the debug arg
            .add_directive(
                format!(
                    "{}={}",
                    env!("CARGO_PKG_NAME"),
                    match args.debug {
                        true => "debug",
                        false => "info",
                    }
                )
                .parse()?,
            ),
    );

    let collector = tracing_subscriber::fmt().with_env_filter(filter).finish();

    tracing::subscriber::set_global_default(collector)?;

    // Connect to docker daemon

    let docker = Docker::connect_with_socket_defaults()?;
    let docker = Arc::new(docker);

    // Open packet capture handle

    let mut cap = Device::lookup().unwrap().unwrap().open().unwrap();

    cap.direction(pcap::Direction::In)?;

    if let Some(port) = args.port {
        let filter = format!("port {}", port);
        cap.filter(&filter, true)?;
    }

    // Setup main loop

    info!("Staring main loop");

    async fn check_if_running(docker: &Docker, container_name: &str) -> Result<Option<bool>> {
        let mut filters = HashMap::new();
        filters.insert("name", vec![container_name]);

        let opts = ListContainersOptions {
            all: true,
            limit: None,
            size: false,
            filters,
        };

        let containers = docker.list_containers(Some(opts)).await?;

        let container_summary = containers.iter().find(|c| match &c.names {
            Some(names) => {
                return names
                    .iter()
                    .any(|name| name == &format!("/{}", container_name))
            }
            None => return false,
        });

        let is_running = container_summary
            .map(|container_summary| container_summary.state.as_ref())
            .flatten()
            .map(|container_state| {
                container_state == "created"
                    || container_state == "running"
                    || container_state == "restarting"
            });

        Ok(is_running)
    }

    async fn suspend_container(docker: &Docker, container_name: &str) {
        debug!("Suspending container");
        let res = docker.stop_container(&container_name, None).await;
        match res {
            Ok(_) => info!("Container suspended"),
            Err(e) => info!("Failed to suspend container: {}", e),
        }
    }

    async fn start_container(docker: &Docker, container_name: &str) {
        debug!("Starting container");
        let res = docker
            .start_container(&container_name, None::<StartContainerOptions<&str>>)
            .await;
        match res {
            Ok(_) => info!("Container started"),
            Err(e) => info!("Failed to start container: {}", e),
        }
    }

    let mut packet_count: u128 = 0;
    let mut interval = tokio::time::interval(args.rate.into());
    let mut join_handle: Option<(tokio::task::AbortHandle, Instant)> = None;
    loop {
        interval.tick().await;

        let stats = cap.stats()?;

        let new_packet_count: u128 = stats.received.try_into()?;
        let delta = new_packet_count - packet_count;
        packet_count = new_packet_count;

        info!("Received {} packets in last {}", delta, &args.rate);

        let is_running = match check_if_running(&docker, &args.container).await? {
            Some(is_running) => is_running,
            None => {
                warn!(
                    "Failed to check if container {} is running",
                    &args.container
                );
                continue;
            }
        };

        if delta > args.threshold as u128 {
            info!("Threshold hit");

            if let Some((handle, _)) = &join_handle {
                info!("Aborting suspension");
                handle.abort();
                join_handle = None;
            }

            if !is_running {
                start_container(&docker, &args.container).await;
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

                        let docker = docker.clone();
                        let container_name = args.container.clone();

                        let handle = tokio::spawn(async move {
                            tokio::time::sleep(timeout.into()).await;
                            let _ = suspend_container(&docker, &container_name).await;
                        });

                        let handle = handle.abort_handle();

                        join_handle = Some((handle, Instant::now()))
                    } else {
                        info!("Threshold missed. Suspending container");
                        suspend_container(&docker, &args.container).await;
                    }
                }
            }
        }
    }
}
