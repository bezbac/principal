use std::{
    collections::HashMap,
    env,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{bail, Result};
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

#[tokio::test]
async fn integration_test() -> Result<()> {
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
        .spawn()?;

    if !cmd.wait()?.success() {
        bail!("Unable to build image");
    }

    println!("Successfully built image");

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

    {
        println!("Create principal container");

        docker
            .create_container(principal_options, principal_config)
            .await?;
    }

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
