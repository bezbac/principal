use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use bollard::{
    container::{ListContainersOptions, StartContainerOptions},
    Docker,
};
use clap::{ArgAction, Parser};
use duration_string::DurationString;
use netstat2::{get_sockets_info, AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState};
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

    /// Should the threshold be ignored if there is an
    /// open connection to the network socket
    #[arg(
        long,
        default_missing_value("true"),
        default_value("true"),
        action = ArgAction::Set
    )]
    preserve_connection: bool,

    /// Which container should be controlled
    #[arg(long)]
    container: String,
}

async fn suspend_container(docker: &Docker, container_name: &str) {
    debug!("Suspending container");
    let res = docker.stop_container(container_name, None).await;
    match res {
        Ok(()) => info!("Container suspended"),
        Err(e) => info!("Failed to suspend container: {}", e),
    }
}

async fn start_container(docker: &Docker, container_name: &str) {
    debug!("Starting container");
    let res = docker
        .start_container(container_name, None::<StartContainerOptions<&str>>)
        .await;
    match res {
        Ok(()) => info!("Container started"),
        Err(e) => info!("Failed to start container: {}", e),
    }
}

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
        Some(names) => names
            .iter()
            .any(|name| name == &format!("/{container_name}")),
        None => false,
    });

    let is_running = container_summary
        .and_then(|container_summary| container_summary.state.as_ref())
        .map(|container_state| {
            container_state == "created"
                || container_state == "running"
                || container_state == "restarting"
        });

    Ok(is_running)
}

fn has_socket_active_connection(port: usize) -> Result<bool> {
    let infos = get_sockets_info(
        AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6,
        ProtocolFlags::TCP,
    )?;

    let has_connection = infos
        .iter()
        .filter_map(|info| match &info.protocol_socket_info {
            ProtocolSocketInfo::Tcp(tcp_si) => Some(tcp_si),
            ProtocolSocketInfo::Udp(_) => None,
        })
        .any(|tcp_info| {
            tcp_info.local_port as usize == port && tcp_info.state == TcpState::Established
        });

    Ok(has_connection)
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
                    if args.debug { "debug" } else { "info" }
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
        let filter = format!("port {port}");
        cap.filter(&filter, true)?;
    }

    // Setup main loop

    info!("Starting main loop");

    let mut packet_count: u128 = 0;
    let mut interval = tokio::time::interval(args.rate.into());
    let mut join_handle: Option<(tokio::task::AbortHandle, Instant)> = None;
    loop {
        interval.tick().await;

        let stats = cap.stats()?;
        let new_packet_count: u128 = stats.received.into();
        let delta = new_packet_count - packet_count;
        packet_count = new_packet_count;

        info!("Received {} packets in last {}", delta, &args.rate);

        let mut should_be_alive: Option<bool> = None;

        if let Some(port) = args.port {
            let has_connection = has_socket_active_connection(port)?;
            if has_connection {
                info!("Socket has an active connection");
                should_be_alive = Some(true);
            }
        }

        let should_be_alive = should_be_alive.unwrap_or(delta > args.threshold as u128);

        let Some(is_running) = check_if_running(&docker, &args.container).await? else {
            warn!(
                "Failed to check if container {} is running",
                &args.container
            );
            continue;
        };

        if should_be_alive {
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
            if !is_running {
                continue;
            }

            if let Some((handle, suspension_started_at)) = &join_handle {
                if handle.is_finished() {
                    join_handle = None;
                    continue;
                }

                if let Some(timeout) = args.timeout {
                    let time_until_suspension =
                        (*suspension_started_at + timeout.into()) - Instant::now();

                    info!(
                        "Threshold missed, suspension scheduled in {} seconds",
                        time_until_suspension.as_secs()
                    );
                }
            } else if let Some(timeout) = args.timeout {
                info!("Threshold missed. Scheduling suspension in {}", timeout);

                let docker = docker.clone();
                let container_name = args.container.clone();

                let handle = tokio::spawn(async move {
                    tokio::time::sleep(timeout.into()).await;
                    let () = suspend_container(&docker, &container_name).await;
                });

                let handle = handle.abort_handle();

                join_handle = Some((handle, Instant::now()));
            } else {
                info!("Threshold missed. Suspending container");
                suspend_container(&docker, &args.container).await;
            }
        }
    }
}
