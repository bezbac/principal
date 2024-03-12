use std::{
    collections::HashMap,
    env,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::Result;
use bollard::{
    container::{
        Config, CreateContainerOptions, ListContainersOptions, LogsOptions, StartContainerOptions,
        StopContainerOptions,
    },
    secret::{HostConfig, Mount, MountTypeEnum, PortBinding},
    Docker,
};
use futures_util::stream::StreamExt;
use nanoid::nanoid;
use tokio::time::sleep;
use tokio_postgres::NoTls;

async fn has_log_output(
    docker: &Docker,
    container_name: &str,
    predicate: &dyn Fn(&str) -> bool,
) -> bool {
    let options = Some(LogsOptions::<String> {
        stdout: true,
        stderr: true,
        ..Default::default()
    });

    let mut http_echo_logs = docker.logs(container_name, options);

    while let Some(Ok(log)) = http_echo_logs.next().await {
        let bytes = &log.into_bytes();
        let message = std::str::from_utf8(bytes).unwrap();

        if predicate(message) {
            return true;
        }
    }

    false
}

async fn wait_for_log_output(
    docker: &Docker,
    container_name: &str,
    predicate: &dyn Fn(&str) -> bool,
    timeout: Duration,
) {
    let start = Instant::now();
    while !has_log_output(docker, container_name, predicate).await {
        sleep(Duration::from_secs(1)).await;
        if start.elapsed() > timeout {
            panic!("Timed out waiting for log output");
        }
    }
}

async fn get_container_state(docker: &Docker, container_name: &str) -> Result<Option<String>> {
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
                .any(|name| name == &format!("/{container_name}"))
        }
        None => false,
    });

    let state = container_summary
        .and_then(|container_summary| container_summary.state.as_ref())
        .map(|s| s.to_string());

    Ok(state)
}

use std::sync::Once;

static BUILD: Once = Once::new();

pub fn initialize() {
    BUILD.call_once(|| {
        let cwd = env!("CARGO_MANIFEST_DIR");

        println!("Building docker image");
        let mut cmd = Command::new("docker")
            .arg("build")
            .arg("--file")
            .arg(&format!("{cwd}/Dockerfile"))
            .arg("--force-rm")
            .arg("--tag")
            .arg("principal:test")
            .arg(".")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();

        if !cmd.wait().unwrap().success() {
            panic!("Unable to build image");
        }

        println!("Successfully built image");
    });
}

#[tokio::test]
async fn integration_test_threshold() -> Result<()> {
    initialize();

    let docker = Docker::connect_with_socket_defaults()?;

    let id = nanoid!();
    let principal_container_name = format!("principal-{id}");
    let http_echo_container_name = format!("http-echo-{id}");

    let principal_options = Some(CreateContainerOptions {
        name: &principal_container_name,
        platform: None,
    });

    // TODO: Randomize host port instead of hardcoding 8080

    let mut principal_port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    principal_port_bindings.insert(
        "8080/tcp".to_string(),
        Some(vec![PortBinding {
            host_port: Some("8080".to_string()),
            host_ip: Some("127.0.0.1".to_string()),
        }]),
    );

    let mut exposed_ports = HashMap::new();
    exposed_ports.insert("8080/tcp", HashMap::<(), ()>::new());

    let principal_config = Config {
        image: Some("principal:test"),
        cmd: Some(vec![
            "--port",
            "8080",
            "--threshold",
            "10",
            "--preserve-connection",
            "false",
            "--rate",
            "1s",
            "--timeout",
            "5s",
            "--container",
            &http_echo_container_name,
        ]),
        exposed_ports: Some(exposed_ports),
        host_config: Some(HostConfig {
            port_bindings: Some(principal_port_bindings),
            mounts: Some(vec![Mount {
                target: Some("/var/run/docker.sock".to_string()),
                source: Some("/var/run/docker.sock".to_string()),
                typ: Some(MountTypeEnum::BIND),
                consistency: Some(String::from("default")),
                read_only: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    let http_echo_options = Some(CreateContainerOptions {
        name: &http_echo_container_name,
        platform: None,
    });

    let http_echo_config = Config {
        image: Some("hashicorp/http-echo"),
        cmd: Some(vec!["-listen=:8080", "-text='hello world'"]),
        host_config: Some(HostConfig {
            network_mode: Some(format!("container:{principal_container_name}")),
            ..Default::default()
        }),
        ..Default::default()
    };

    println!("Create principal container");
    docker
        .create_container(principal_options, principal_config)
        .await?;

    println!("Create http-echo container");
    docker
        .create_container(http_echo_options, http_echo_config)
        .await?;

    println!("Start principal container");
    docker
        .start_container(
            &principal_container_name,
            None::<StartContainerOptions<&str>>,
        )
        .await?;

    println!("Start http-echo container");
    docker
        .start_container(
            &http_echo_container_name,
            None::<StartContainerOptions<&str>>,
        )
        .await?;

    println!("Wait for principal to be ready");
    wait_for_log_output(
        &docker,
        &principal_container_name,
        &|message| message.contains("Starting main loop"),
        Duration::from_secs(5),
    )
    .await;

    println!("Wait for http-echo to be ready");
    wait_for_log_output(
        &docker,
        &http_echo_container_name,
        &|message| message.contains("server is listening on"),
        Duration::from_secs(5),
    )
    .await;

    println!("Send request to http-echo & check response");
    let resp = reqwest::get("http://localhost:8080").await?.text().await?;
    assert_eq!(resp, "'hello world'\n");

    println!("Wait for suspension to be initiated");
    wait_for_log_output(
        &docker,
        &principal_container_name,
        &|message| message.contains("Threshold missed. Scheduling suspension in 5s"),
        Duration::from_secs(5),
    )
    .await;

    println!("Wait for container to be suspended");
    sleep(Duration::from_secs(10)).await;

    let container_state = get_container_state(&docker, &http_echo_container_name).await?;
    assert_eq!(container_state, Some("exited".to_string()));

    println!("Send requests to http-echo until it is available again");
    'outer: loop {
        sleep(Duration::from_secs(1)).await;
        for _ in 0..20 {
            let res = reqwest::get("http://localhost:8080").await;
            if res.is_ok() {
                break 'outer;
            }
        }
    }

    let container_state = get_container_state(&docker, &http_echo_container_name).await?;
    assert_eq!(container_state, Some("running".to_string()));

    println!("Sustain requests for 10 seconds");
    for _ in 0..10 {
        sleep(Duration::from_secs(1)).await;
        for _ in 0..20 {
            reqwest::get("http://localhost:8080").await?.text().await?;
            assert_eq!(resp, "'hello world'\n");
        }
    }

    let container_state = get_container_state(&docker, &http_echo_container_name).await?;
    assert_eq!(container_state, Some("running".to_string()));

    println!("Wait ten seconds");
    sleep(Duration::from_secs(10)).await;

    println!("Expect container to be suspended");
    let container_state = get_container_state(&docker, &http_echo_container_name).await?;
    assert_eq!(container_state, Some("exited".to_string()));

    // TODO: Always execute the cleanup

    println!("Stop principal container");
    let options = Some(StopContainerOptions { t: 10 });
    docker
        .stop_container(&principal_container_name, options)
        .await?;

    println!("Stop http-echo container");
    let options = Some(StopContainerOptions { t: 10 });
    docker
        .stop_container(&http_echo_container_name, options)
        .await?;

    println!("Remove principal container");
    docker
        .remove_container(&principal_container_name, None)
        .await?;

    println!("Remove http-echo container");
    docker
        .remove_container(&http_echo_container_name, None)
        .await?;

    Ok(())
}

#[tokio::test]
async fn integration_test_preserve_connection() -> Result<()> {
    initialize();

    let docker = Docker::connect_with_socket_defaults()?;

    let id = nanoid!();
    let principal_container_name = format!("principal-{id}");
    let postgres_container_name = format!("postgres-{id}");

    let principal_options = Some(CreateContainerOptions {
        name: &principal_container_name,
        platform: None,
    });

    // TODO: Randomize host port instead of hardcoding 5432

    let mut principal_port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    principal_port_bindings.insert(
        "5432/tcp".to_string(),
        Some(vec![PortBinding {
            host_port: Some("5432".to_string()),
            host_ip: Some("127.0.0.1".to_string()),
        }]),
    );

    let mut exposed_ports = HashMap::new();
    exposed_ports.insert("5432/tcp", HashMap::<(), ()>::new());

    let principal_config = Config {
        image: Some("principal:test"),
        cmd: Some(vec![
            "--port",
            "5432",
            "--threshold",
            "1000000",
            "--preserve-connection",
            "true",
            "--rate",
            "1s",
            "--timeout",
            "5s",
            "--container",
            &postgres_container_name,
        ]),
        exposed_ports: Some(exposed_ports),
        host_config: Some(HostConfig {
            port_bindings: Some(principal_port_bindings),
            mounts: Some(vec![Mount {
                target: Some("/var/run/docker.sock".to_string()),
                source: Some("/var/run/docker.sock".to_string()),
                typ: Some(MountTypeEnum::BIND),
                consistency: Some(String::from("default")),
                read_only: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    let postgres_options = Some(CreateContainerOptions {
        name: &postgres_container_name,
        platform: None,
    });

    let postgres_password = "example";
    let mut postgres_env: Vec<&str> = vec![];
    let password_env = format!("POSTGRES_PASSWORD={postgres_password}");
    postgres_env.push(&password_env);

    let postgres_config = Config {
        image: Some("postgres"),
        env: Some(postgres_env),
        host_config: Some(HostConfig {
            network_mode: Some(format!("container:{principal_container_name}")),
            ..Default::default()
        }),
        ..Default::default()
    };

    println!("Create principal container");
    docker
        .create_container(principal_options, principal_config)
        .await?;

    println!("Create postgres container");
    docker
        .create_container(postgres_options, postgres_config)
        .await?;

    println!("Start principal container");
    docker
        .start_container(
            &principal_container_name,
            None::<StartContainerOptions<&str>>,
        )
        .await?;

    println!("Start postgres container");
    docker
        .start_container(
            &postgres_container_name,
            None::<StartContainerOptions<&str>>,
        )
        .await?;

    println!("Wait for principal to be ready");
    wait_for_log_output(
        &docker,
        &principal_container_name,
        &|message| message.contains("Starting main loop"),
        Duration::from_secs(5),
    )
    .await;

    println!("Wait for postgres to be ready");
    wait_for_log_output(
        &docker,
        &postgres_container_name,
        &|message| message.contains("database system is ready to accept connections"),
        Duration::from_secs(5),
    )
    .await;

    println!("Connecting to postgres");
    let mut conn_config = tokio_postgres::Config::new();
    conn_config.host("localhost");
    conn_config.port(5432);
    conn_config.user("postgres");
    conn_config.password(postgres_password);
    let (client, connection) = conn_config.connect(NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    println!("Run a simple SQL query");

    for i in 0..100 {
        let message = format!("Hello {i}");
        let rows = client.query("SELECT $1::TEXT", &[&message]).await?;
        assert_eq!(rows.len(), 1);
        let value: &str = rows[0].get(0);
        assert_eq!(value, message);
    }

    sleep(Duration::from_secs(5)).await;

    let packet_counts = docker
        .logs(
            &principal_container_name,
            Some(LogsOptions::<String> {
                stdout: true,
                stderr: true,
                ..Default::default()
            }),
        )
        .filter_map(|log| async move { log.ok() })
        .filter_map(|log| async move {
            let bytes = &log.into_bytes();
            let message = std::str::from_utf8(bytes).unwrap();
            let parts = message.split("Received ").collect::<Vec<&str>>();
            if parts.len() < 2 {
                return None;
            }
            let parts = parts[1].split(" ").collect::<Vec<&str>>();
            if parts.len() < 1 {
                return None;
            }
            parts[0].parse::<usize>().ok()
        })
        .collect::<Vec<usize>>()
        .await;

    println!("Expect a log with >100 packets received");
    assert!(packet_counts.iter().any(|&packet_count| packet_count > 100));

    println!("Send a query every 2 seconds for 10 seconds");
    for _ in 0..5 {
        sleep(Duration::from_secs(2)).await;
        let rows = client.query("SELECT $1::TEXT", &[&"hello world"]).await?;
        assert_eq!(rows.len(), 1);
        let value: &str = rows[0].get(0);
        assert_eq!(value, "hello world");
    }

    println!("Expect container to be running");
    let container_state = get_container_state(&docker, &postgres_container_name).await?;
    assert_eq!(container_state, Some("running".to_string()));

    println!("Close the connection");
    drop(client);

    println!("Wait for 10 seconds");
    sleep(Duration::from_secs(10)).await;

    let new_packet_counts = docker
        .logs(
            &principal_container_name,
            Some(LogsOptions::<String> {
                stdout: true,
                stderr: true,
                ..Default::default()
            }),
        )
        .skip(packet_counts.len())
        .filter_map(|log| async move { log.ok() })
        .filter_map(|log| async move {
            let bytes = &log.into_bytes();
            let message = std::str::from_utf8(bytes).unwrap();
            let parts = message.split("Received ").collect::<Vec<&str>>();
            if parts.len() < 2 {
                return None;
            }
            let parts = parts[1].split(" ").collect::<Vec<&str>>();
            if parts.len() < 1 {
                return None;
            }
            parts[0].parse::<usize>().ok()
        })
        .collect::<Vec<usize>>()
        .await;

    println!("Expect at least one log with >0 packets received, since last check");
    assert!(new_packet_counts
        .iter()
        .any(|&packet_count| packet_count > 0));

    println!("Expect container to be suspended");
    let container_state = get_container_state(&docker, &postgres_container_name).await?;
    assert_eq!(container_state, Some("exited".to_string()));

    // TODO: Always execute the cleanup

    println!("Stop principal container");
    let options = Some(StopContainerOptions { t: 10 });
    docker
        .stop_container(&principal_container_name, options)
        .await?;

    println!("Stop postgres container");
    let options = Some(StopContainerOptions { t: 10 });
    docker
        .stop_container(&postgres_container_name, options)
        .await?;

    println!("Remove principal container");
    docker
        .remove_container(&principal_container_name, None)
        .await?;

    println!("Remove postgres container");
    docker
        .remove_container(&postgres_container_name, None)
        .await?;

    Ok(())
}
