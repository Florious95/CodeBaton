use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use aisync_core::{AisyncError, DeviceId, DeviceInfo, Direction, OsType, Result};
use aisync_sync::{
    default_config_path, load_config, save_config, ClaudeConfig, PeerConfig, ProjectConfig,
    SyncConfig, SyncModeConfig, WorkspaceConfig,
};
use aisync_transport::{generate_tls_identity, ReceiveService, TcpTransporter, TlsConfig};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};

#[derive(Debug, Parser)]
#[command(name = "aisync", version, about = "AI session and project sync")]
struct Cli {
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Devices,
    Pair {
        device: String,
        #[arg(long)]
        endpoint: Option<SocketAddr>,
        #[arg(long = "server-cert")]
        server_cert: Option<PathBuf>,
        #[arg(long, default_value = "aisync-receiver")]
        server_name: String,
    },
    Unpair {
        device: String,
    },
    Projects,
    Add {
        local: PathBuf,
        remote: PathBuf,
        #[arg(long, default_value = "default")]
        peer: String,
        #[arg(long)]
        name: Option<String>,
    },
    Workspace {
        local: PathBuf,
        remote: PathBuf,
        #[arg(long, default_value = "default")]
        peer: String,
        #[arg(long)]
        name: Option<String>,
    },
    Push {
        project: String,
        #[arg(long = "to")]
        to: Option<String>,
    },
    Pull {
        project: String,
        #[arg(long = "from")]
        from: Option<String>,
    },
    Serve {
        target: Option<PathBuf>,
        #[arg(long = "dir")]
        dir: Option<PathBuf>,
        #[arg(long = "remote-dir")]
        remote_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 47800)]
        port: u16,
        #[arg(long)]
        listen: Option<SocketAddr>,
        #[arg(long = "cert-out")]
        cert_out: Option<PathBuf>,
        #[arg(long)]
        once: bool,
    },
    Send {
        source: Option<PathBuf>,
        #[arg(long = "dir")]
        dir: Option<PathBuf>,
        #[arg(long = "to")]
        to: SocketAddr,
        #[arg(long = "remote-dir")]
        remote_dir: Option<PathBuf>,
        #[arg(long = "server-cert")]
        server_cert: Option<PathBuf>,
        #[arg(long, default_value = "aisync-receiver")]
        server_name: String,
    },
    Sync {
        project: String,
        #[arg(long)]
        auto: bool,
        #[arg(long = "with")]
        device: Option<String>,
    },
    Status,
    Log {
        #[arg(long)]
        project: Option<String>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let config_path = cli
        .config
        .or_else(default_config_path)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let mut config = load_or_default(&config_path)?;

    match cli.command {
        Commands::Devices => print_devices(&config),
        Commands::Pair {
            device,
            endpoint,
            server_cert,
            server_name,
        } => {
            let peer = config
                .peers
                .entry(device.clone())
                .or_insert_with(|| PeerConfig {
                    id: DeviceId::new(),
                    name: device.clone(),
                    endpoint: None,
                    server_cert: None,
                    server_name: None,
                    last_seen: None,
                });
            if endpoint.is_some() {
                peer.endpoint = endpoint;
            }
            if server_cert.is_some() {
                peer.server_cert = server_cert;
            }
            peer.server_name = Some(server_name);
            save_config(&config_path, &config)?;
            println!("paired {device}");
            Ok(())
        }
        Commands::Unpair { device } => {
            let removed_peers = remove_peer(&mut config, &device);
            let removed = !removed_peers.is_empty();
            let removed_mappings = remove_peer_mappings(&mut config, &removed_peers);
            save_config(&config_path, &config)?;
            if removed {
                println!("unpaired {device}; removed {removed_mappings} mapping(s)");
            } else {
                println!("device not paired: {device}");
            }
            Ok(())
        }
        Commands::Projects => print_projects(&config),
        Commands::Add {
            local,
            remote,
            peer,
            name,
        } => {
            let project_name = name.unwrap_or_else(|| path_name(&local, "project"));
            ensure_peer(&mut config, &peer);
            if config.claude_config.local.as_os_str().is_empty() {
                config.claude_config.local = sibling_claude_dir(&local);
            }
            config
                .claude_config
                .peers
                .entry(peer.clone())
                .or_insert_with(|| sibling_claude_dir(&remote));
            config
                .projects
                .retain(|project| project.name != project_name);
            config.projects.push(ProjectConfig {
                name: project_name.clone(),
                local,
                peers: HashMap::from([(peer.clone(), remote)]),
                sync_mode: SyncModeConfig::TwoWayAuto,
                enabled: true,
                exclude_rules: Vec::new(),
                sync_snapshots: HashMap::new(),
            });
            save_config(&config_path, &config)?;
            println!("added project {project_name} for peer {peer}");
            Ok(())
        }
        Commands::Workspace {
            local,
            remote,
            peer,
            name,
        } => {
            let workspace_name = name.unwrap_or_else(|| path_name(&local, "workspace"));
            ensure_peer(&mut config, &peer);
            config
                .workspaces
                .retain(|workspace| workspace.name != workspace_name);
            config.workspaces.push(WorkspaceConfig {
                name: workspace_name.clone(),
                local_root: local.clone(),
                remote_root: remote.clone(),
                peer: peer.clone(),
                children: Vec::new(),
                local,
                peers: HashMap::from([(peer.clone(), remote)]),
                scan_depth: 1,
                auto_enable_new: false,
                sync_mode: SyncModeConfig::TwoWayAuto,
                enabled: true,
                exclude_rules: Vec::new(),
            });
            save_config(&config_path, &config)?;
            println!("added workspace {workspace_name} for peer {peer}");
            Ok(())
        }
        Commands::Push { project, to } => {
            let peer_name = select_peer(&config, to.as_deref())?;
            let report = run_transfer(
                &config_path,
                &config,
                &project,
                &peer_name,
                TransferDirection::Push,
            )?;
            print_report("push complete", &report);
            Ok(())
        }
        Commands::Pull { project, from } => {
            let peer_name = select_peer(&config, from.as_deref())?;
            let report = run_transfer(
                &config_path,
                &config,
                &project,
                &peer_name,
                TransferDirection::Pull,
            )?;
            print_report("pull complete", &report);
            Ok(())
        }
        Commands::Serve {
            target,
            dir,
            remote_dir,
            port,
            listen,
            cert_out,
            once,
        } => run_receiver(
            &config_path,
            listen,
            port,
            target,
            dir,
            remote_dir,
            cert_out,
            once,
        ),
        Commands::Send {
            source,
            dir,
            to,
            remote_dir,
            server_cert,
            server_name,
        } => run_sender(
            &config_path,
            source,
            dir,
            to,
            remote_dir,
            server_cert,
            server_name,
        ),
        Commands::Sync {
            project,
            auto,
            device,
        } => {
            if !auto {
                return Err(AisyncError::InvalidInput(
                    "sync requires --auto in Phase 2".to_string(),
                ));
            }
            let peer_name = select_peer(&config, device.as_deref())?;
            let report = run_transfer(
                &config_path,
                &config,
                &project,
                &peer_name,
                TransferDirection::Push,
            )?;
            print_report("auto sync cycle complete", &report);
            Ok(())
        }
        Commands::Status => {
            println!("device: {}", config.device.name);
            println!("peers: {}", config.peers.len());
            println!("projects: {}", config.projects.len());
            println!("workspaces: {}", config.workspaces.len());
            println!("state: {}", config.state_path().display());
            Ok(())
        }
        Commands::Log { project } => {
            let state_path = config.state_path();
            if !state_path.exists() {
                println!("no sync log yet");
                return Ok(());
            }
            let text = std::fs::read_to_string(&state_path)?;
            if let Some(project) = project {
                if text.contains(&project) {
                    println!("{text}");
                } else {
                    println!("no log entries for {project}");
                }
            } else {
                println!("{text}");
            }
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum TransferDirection {
    Push,
    Pull,
}

fn run_receiver(
    config_path: &Path,
    listen: Option<SocketAddr>,
    port: u16,
    target: Option<PathBuf>,
    dir: Option<PathBuf>,
    remote_dir: Option<PathBuf>,
    cert_out: Option<PathBuf>,
    once: bool,
) -> Result<()> {
    let target = one_receive_dir_arg(target, dir, remote_dir)?;
    let listen = listen.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], port)));
    let cert_out = cert_out.unwrap_or_else(|| default_receiver_cert_path(config_path));
    let runtime = tokio_runtime()?;
    runtime.block_on(async move {
        let identity = generate_tls_identity("aisync-receiver")?;
        let tls = TlsConfig::new(identity.clone(), "aisync-receiver");
        let service = ReceiveService::bind(listen, target, &tls).await?;
        if let Some(parent) = cert_out.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&cert_out, &identity.cert_der)?;
        println!("server-cert\t{}", cert_out.display());
        println!("listening\t{}", service.local_addr()?);
        let _ = io::stdout().flush();

        if once {
            let manifest = service.receive_once(None).await?;
            println!("received\t{} files", manifest.files.len());
            Ok(())
        } else {
            service.serve_forever(None).await
        }
    })
}

fn run_sender(
    config_path: &Path,
    source: Option<PathBuf>,
    dir: Option<PathBuf>,
    to: SocketAddr,
    remote_dir: Option<PathBuf>,
    server_cert: Option<PathBuf>,
    server_name: String,
) -> Result<()> {
    let source = one_dir_arg(source, dir, "--dir")?;
    if !source.is_dir() {
        return Err(AisyncError::InvalidInput(format!(
            "source is not a directory: {}",
            source.display()
        )));
    }
    let server_cert = server_cert.unwrap_or_else(|| default_receiver_cert_path(config_path));

    let runtime = tokio_runtime()?;
    runtime.block_on(async move {
        let server_cert = fs::read(&server_cert).map_err(|error| {
            AisyncError::Transport(format!(
                "server certificate not found at {}: {}; copy the receiver certificate or pass --server-cert",
                server_cert.display(),
                error
            ))
        })?;
        let identity = generate_tls_identity("aisync-client")?;
        let tls = TlsConfig::new(identity, server_name).with_pinned_peer_cert(server_cert);
        let mut transporter = TcpTransporter::connect_addr(to, &tls).await?;
        let manifest = transporter
            .sync_directory_to(&source, remote_dir.as_deref(), None)
            .await?;
        println!("sent\t{} files\t{}", manifest.files.len(), to);
        Ok(())
    })
}

fn one_receive_dir_arg(
    positional: Option<PathBuf>,
    dir: Option<PathBuf>,
    remote_dir: Option<PathBuf>,
) -> Result<PathBuf> {
    let provided = [
        positional.as_ref().map(|_| "positional directory"),
        dir.as_ref().map(|_| "--dir"),
        remote_dir.as_ref().map(|_| "--remote-dir"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    if provided.len() > 1 {
        return Err(AisyncError::InvalidInput(format!(
            "provide only one receive directory ({})",
            provided.join(", ")
        )));
    }

    Ok(positional
        .or(dir)
        .or(remote_dir)
        .unwrap_or_else(|| PathBuf::from(".")))
}

fn one_dir_arg(
    positional: Option<PathBuf>,
    option: Option<PathBuf>,
    fallback: &str,
) -> Result<PathBuf> {
    match (positional, option) {
        (Some(_), Some(_)) => Err(AisyncError::InvalidInput(
            "provide either positional directory or --dir, not both".to_string(),
        )),
        (Some(path), None) | (None, Some(path)) => Ok(path),
        (None, None) if fallback == "--dir" => Err(AisyncError::InvalidInput(
            "send requires --dir <path>".to_string(),
        )),
        (None, None) => Ok(PathBuf::from(fallback)),
    }
}

fn default_receiver_cert_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("receiver.der")
}

fn tokio_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| AisyncError::Transport(format!("tokio runtime: {error}")))
}

fn run_transfer(
    config_path: &Path,
    config: &SyncConfig,
    project_name: &str,
    peer_name: &str,
    direction: TransferDirection,
) -> Result<aisync_sync::SyncReport> {
    if matches!(direction, TransferDirection::Pull) {
        return Err(AisyncError::Transport(
            "pull over TCP requires a remote control channel; start `aisync serve --dir <local-target>` locally and run `aisync send` on the peer".to_string(),
        ));
    }

    let project = config.project_mapping(project_name, peer_name)?;
    let peer = config
        .peers
        .get(peer_name)
        .ok_or_else(|| AisyncError::Config(format!("peer '{peer_name}' not found")))?;
    let endpoint = peer.endpoint.ok_or_else(|| {
        AisyncError::Config(format!(
            "peer '{peer_name}' has no endpoint; run `aisync pair {peer_name} --endpoint <ip:port>`"
        ))
    })?;
    let server_cert = peer
        .server_cert
        .clone()
        .unwrap_or_else(|| default_receiver_cert_path(config_path));
    let server_cert = fs::read(&server_cert).map_err(|error| {
        AisyncError::Transport(format!(
            "server certificate not found at {}: {}; copy the receiver certificate or configure peer.server_cert",
            server_cert.display(),
            error
        ))
    })?;
    let server_name = peer
        .server_name
        .clone()
        .unwrap_or_else(|| "aisync-receiver".to_string());
    let peer_info = DeviceInfo {
        id: peer.id,
        name: peer.name.clone(),
        os: OsType::Other("configured".to_string()),
        addresses: vec![endpoint.ip()],
        protocol_version: 1,
    };

    let progress = ProgressBar::new(100);
    progress.set_style(
        ProgressStyle::with_template("{bar:32} {pos:>3}% {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar()),
    );
    progress.set_position(5);
    progress.set_message("connect");
    let manifest = tcp_send_dir(
        &peer_info,
        endpoint.port(),
        &server_cert,
        &server_name,
        &project.local_code_dir,
        Some(&project.remote_code_dir),
    )?;
    progress.set_position(100);
    progress.set_message("sync_complete");
    progress.finish_and_clear();
    Ok(aisync_sync::SyncReport {
        project_id: project.project_id,
        peer_id: peer.id,
        direction: Direction::LocalToRemote,
        code_files_transferred: manifest.files.len(),
        session_files_transferred: 0,
        deleted_files: 0,
        rewritten_sessions: 0,
        local_version: 0,
        remote_version: 0,
        stages: vec![
            aisync_sync::SyncStage {
                name: "connect",
                percent: 5,
                current_file: None,
            },
            aisync_sync::SyncStage {
                name: "sync_complete",
                percent: 100,
                current_file: None,
            },
        ],
    })
}

fn tcp_send_dir(
    peer: &DeviceInfo,
    port: u16,
    server_cert: &[u8],
    server_name: &str,
    source: &Path,
    remote_dir: Option<&Path>,
) -> Result<aisync_core::SyncManifest> {
    if !source.is_dir() {
        return Err(AisyncError::InvalidInput(format!(
            "source is not a directory: {}",
            source.display()
        )));
    }
    let peer = peer.clone();
    let server_cert = server_cert.to_vec();
    let server_name = server_name.to_string();
    let source = source.to_path_buf();
    let remote_dir = remote_dir.map(Path::to_path_buf);
    let runtime = tokio_runtime()?;
    runtime.block_on(async move {
        let identity = generate_tls_identity("aisync-client")?;
        let tls = TlsConfig::new(identity, server_name).with_pinned_peer_cert(server_cert);
        let mut transporter = TcpTransporter::connect_to_peer(&peer, port, &tls).await?;
        transporter
            .sync_directory_to(&source, remote_dir.as_deref(), None)
            .await
    })
}

fn load_or_default(path: &Path) -> Result<SyncConfig> {
    let existed = path.exists();
    let mut config = if existed {
        load_config(path)?
    } else {
        SyncConfig {
            claude_config: ClaudeConfig::default(),
            ..SyncConfig::new(default_device_name())
        }
    };
    let mut changed = !existed;
    if config.state_path.is_none() {
        config.state_path = Some(path.with_file_name("state.toml"));
        changed = true;
    }
    if changed {
        save_config(path, &config)?;
    }
    Ok(config)
}

fn print_devices(config: &SyncConfig) -> Result<()> {
    println!("local\t{}\t{:?}", config.device.name, config.device.id);
    if config.peers.is_empty() {
        println!("no paired devices");
        return Ok(());
    }
    for (name, peer) in &config.peers {
        println!("peer\t{name}\t{}\t{:?}", peer.name, peer.id);
    }
    Ok(())
}

fn print_projects(config: &SyncConfig) -> Result<()> {
    if config.projects.is_empty() && config.workspaces.is_empty() {
        println!("no projects configured");
        return Ok(());
    }
    for project in &config.projects {
        println!(
            "project\t{}\t{}\tenabled={}",
            project.name,
            project.local.display(),
            project.enabled
        );
        for (peer, remote) in &project.peers {
            println!("  peer\t{peer}\t{}", remote.display());
        }
    }
    for workspace in &config.workspaces {
        println!(
            "workspace\t{}\t{}\tenabled={}",
            workspace.name,
            workspace.local.display(),
            workspace.enabled
        );
        for (peer, remote) in &workspace.peers {
            println!("  peer\t{peer}\t{}", remote.display());
        }
    }
    Ok(())
}

fn print_report(label: &str, report: &aisync_sync::SyncReport) {
    println!(
        "{label}: project={} code_files={} session_files={} deleted={} local_version={} remote_version={}",
        report.project_id,
        report.code_files_transferred,
        report.session_files_transferred,
        report.deleted_files,
        report.local_version,
        report.remote_version
    );
}

fn ensure_peer(config: &mut SyncConfig, peer: &str) {
    config
        .peers
        .entry(peer.to_string())
        .or_insert_with(|| PeerConfig {
            id: DeviceId::new(),
            name: peer.to_string(),
            endpoint: None,
            server_cert: None,
            server_name: None,
            last_seen: None,
        });
}

fn remove_peer(config: &mut SyncConfig, device: &str) -> Vec<String> {
    let mut removed = Vec::new();
    if config.peers.remove(device).is_some() {
        removed.push(device.to_string());
    }
    let names: Vec<String> = config
        .peers
        .iter()
        .filter(|(_, peer)| peer.name == device)
        .map(|(name, _)| name.clone())
        .collect();
    for name in names {
        config.peers.remove(&name);
        if !removed.contains(&name) {
            removed.push(name);
        }
    }
    removed
}

fn remove_peer_mappings(config: &mut SyncConfig, peers: &[String]) -> usize {
    let mut removed = 0;
    for peer in peers {
        if config.claude_config.peers.remove(peer).is_some() {
            removed += 1;
        }
        for project in &mut config.projects {
            if project.peers.remove(peer).is_some() {
                removed += 1;
            }
        }
        for workspace in &mut config.workspaces {
            if workspace.peers.remove(peer).is_some() {
                removed += 1;
            }
        }
    }
    removed
}

fn select_peer(config: &SyncConfig, requested: Option<&str>) -> Result<String> {
    if let Some(peer) = requested {
        if config.peers.contains_key(peer) {
            return Ok(peer.to_string());
        }
        return Err(AisyncError::Config(format!("peer '{peer}' not found")));
    }
    if config.peers.len() == 1 {
        return Ok(config.peers.keys().next().expect("len checked").clone());
    }
    if config.peers.contains_key("default") {
        return Ok("default".to_string());
    }
    Err(AisyncError::Config(
        "select a peer with --to/--from/--with".to_string(),
    ))
}

fn path_name(path: &Path, fallback: &str) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn sibling_claude_dir(path: &Path) -> PathBuf {
    path.parent()
        .map(|parent| parent.join(".claude"))
        .unwrap_or_else(|| path.join(".claude"))
}

fn default_device_name() -> String {
    env::var("HOSTNAME")
        .or_else(|_| env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "aisync-device".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_accepts_all_required_commands() {
        Cli::command().debug_assert();

        for args in [
            ["aisync", "devices"].as_slice(),
            ["aisync", "pair", "desktop"].as_slice(),
            ["aisync", "unpair", "desktop"].as_slice(),
            ["aisync", "projects"].as_slice(),
            ["aisync", "add", "/tmp/a", "/tmp/b"].as_slice(),
            ["aisync", "workspace", "/tmp/a", "/tmp/b"].as_slice(),
            ["aisync", "push", "app", "--to", "desktop"].as_slice(),
            ["aisync", "pull", "app", "--from", "desktop"].as_slice(),
            [
                "aisync",
                "serve",
                "--port",
                "47800",
                "--dir",
                "/tmp/target",
                "--once",
            ]
            .as_slice(),
            [
                "aisync",
                "send",
                "--to",
                "127.0.0.1:47800",
                "--dir",
                "/tmp/source",
            ]
            .as_slice(),
            ["aisync", "sync", "app", "--auto"].as_slice(),
            ["aisync", "status"].as_slice(),
            ["aisync", "log", "--project", "app"].as_slice(),
        ] {
            Cli::try_parse_from(args).unwrap();
        }
    }
}
