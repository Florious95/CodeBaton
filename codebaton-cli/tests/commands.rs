use std::fs;
use std::net::TcpListener;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn all_cli_commands_execute_against_temp_config() {
    let bin = env!("CARGO_BIN_EXE_codebaton-cli");
    let root = temp_dir("cli");
    let config = root.join("config.toml");
    let local = root.join("local");
    let remote = root.join("remote");
    let workspace_local = root.join("workspace-local");
    let workspace_remote = root.join("workspace-remote");
    fs::create_dir_all(&local).unwrap();
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&workspace_local).unwrap();
    fs::create_dir_all(&workspace_remote).unwrap();
    fs::write(local.join("main.rs"), "fn main() {}\n").unwrap();

    run_ok(bin, &config, &["devices"]);
    run_ok(bin, &config, &["pair", "desktop"]);
    run_ok(
        bin,
        &config,
        &[
            "add",
            local.to_str().unwrap(),
            remote.to_str().unwrap(),
            "--peer",
            "desktop",
            "--name",
            "app",
        ],
    );
    run_ok(bin, &config, &["projects"]);
    run_err_contains(
        bin,
        &config,
        &["push", "app", "--to", "desktop"],
        "has no endpoint",
    );
    assert!(
        !remote.join("main.rs").exists(),
        "push must not fake success by writing the local remote path"
    );

    let port = free_port();
    let cert = root.join("desktop-receiver.der");
    let mut server = start_server_once(bin, &config, &remote, port, &cert);
    let endpoint = format!("127.0.0.1:{port}");
    run_ok(
        bin,
        &config,
        &[
            "pair",
            "desktop",
            "--endpoint",
            &endpoint,
            "--server-cert",
            cert.to_str().unwrap(),
        ],
    );
    run_ok(bin, &config, &["push", "app", "--to", "desktop"]);
    assert!(server.wait().unwrap().success());
    assert_eq!(
        fs::read_to_string(remote.join("main.rs")).unwrap(),
        "fn main() {}\n"
    );

    fs::write(
        remote.join("main.rs"),
        "fn main() { println!(\"remote\"); }\n",
    )
    .unwrap();
    run_err_contains(
        bin,
        &config,
        &["pull", "app", "--from", "desktop"],
        "pull over TCP requires",
    );

    let sync_port = free_port();
    let sync_endpoint = format!("127.0.0.1:{sync_port}");
    let mut sync_server = start_server_once(bin, &config, &remote, sync_port, &cert);
    run_ok(
        bin,
        &config,
        &[
            "pair",
            "desktop",
            "--endpoint",
            &sync_endpoint,
            "--server-cert",
            cert.to_str().unwrap(),
        ],
    );
    run_ok(
        bin,
        &config,
        &["sync", "app", "--auto", "--with", "desktop"],
    );
    assert!(sync_server.wait().unwrap().success());
    run_ok(bin, &config, &["status"]);
    run_ok(bin, &config, &["log", "--project", "app"]);
    run_ok(
        bin,
        &config,
        &[
            "workspace",
            workspace_local.to_str().unwrap(),
            workspace_remote.to_str().unwrap(),
            "--peer",
            "desktop",
            "--name",
            "all",
        ],
    );
    run_ok(bin, &config, &["unpair", "desktop"]);
    let projects = run_ok(bin, &config, &["projects"]);
    assert!(
        !projects.contains("peer\tdesktop"),
        "unpair must remove project/workspace peer mappings"
    );
}

#[test]
fn devices_persists_generated_device_id_on_first_read() {
    let bin = env!("CARGO_BIN_EXE_codebaton-cli");
    let root = temp_dir("identity");
    let config = root.join("config.toml");

    let first = run_ok(bin, &config, &["devices"]);
    let second = run_ok(bin, &config, &["devices"]);

    assert!(config.exists(), "first devices call must persist config");
    assert_eq!(device_id_line(&first), device_id_line(&second));
}

#[test]
fn serve_and_send_transfer_directory_over_loopback_tcp() {
    let bin = env!("CARGO_BIN_EXE_codebaton-cli");
    let root = temp_dir("tcp");
    let config = root.join("config.toml");
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir_all(source.join("sub")).unwrap();
    fs::write(source.join("sub/file.txt"), "hello tcp\n").unwrap();

    let port = free_port();
    let port_text = port.to_string();
    let mut server = Command::new(bin)
        .arg("--config")
        .arg(&config)
        .args([
            "serve",
            "--port",
            &port_text,
            "--dir",
            target.to_str().unwrap(),
            "--once",
        ])
        .spawn()
        .unwrap();

    let cert = config.with_file_name("receiver.der");
    wait_for_file(&cert);
    let addr = format!("127.0.0.1:{port}");
    let output = Command::new(bin)
        .arg("--config")
        .arg(&config)
        .args(["send", "--to", &addr, "--dir", source.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "send failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let status = server.wait().unwrap();
    assert!(status.success(), "server exited with {status}");
    assert_eq!(
        fs::read_to_string(target.join("sub/file.txt")).unwrap(),
        "hello tcp\n"
    );
}

fn run_ok(bin: &str, config: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new(bin)
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "command {:?} failed\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn run_err_contains(bin: &str, config: &std::path::Path, args: &[&str], needle: &str) {
    let output = Command::new(bin)
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "command {:?} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !needle.is_empty() {
        assert!(
            stderr.contains(needle),
            "stderr did not contain {needle:?}\nstderr:\n{stderr}"
        );
    }
}

fn device_id_line(output: &str) -> String {
    output
        .lines()
        .find(|line| line.starts_with("local\t"))
        .unwrap()
        .to_string()
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "aisync-cli-{name}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn wait_for_file(path: &std::path::Path) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {}", path.display());
}

fn start_server_once(
    bin: &str,
    config: &std::path::Path,
    target: &std::path::Path,
    port: u16,
    cert: &std::path::Path,
) -> std::process::Child {
    let port_text = port.to_string();
    let _ = fs::remove_file(cert);
    let server = Command::new(bin)
        .arg("--config")
        .arg(config)
        .args([
            "serve",
            "--port",
            &port_text,
            "--dir",
            target.to_str().unwrap(),
            "--cert-out",
            cert.to_str().unwrap(),
            "--once",
        ])
        .spawn()
        .unwrap();
    wait_for_file(cert);
    server
}
