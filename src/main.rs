use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;

use anyhow::Result;
use clap::{Parser, Subcommand};

use branchfs::daemon::{self, Request, Response};

#[derive(Parser)]
#[command(name = "branchfs", version)]
#[command(about = "FUSE filesystem with atomic branching")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Mount the filesystem
    Mount {
        /// Base directory to branch from (required on first mount)
        #[arg(long)]
        base: Option<PathBuf>,

        /// Storage directory for branch data
        #[arg(long, default_value = "/var/lib/branchfs")]
        storage: PathBuf,

        /// Branch to expose at the mount root
        #[arg(long, default_value = "main")]
        branch: String,

        /// Hide .branchfs_ctl and @branch virtual directories from this mount
        #[arg(long)]
        no_control: bool,

        /// Agent-safe alias for --no-control
        #[arg(long)]
        agent: bool,

        /// Enable FUSE passthrough for near-native I/O performance (requires root)
        #[arg(long)]
        passthrough: bool,

        /// Allow access from uids other than the mounting process (FUSE
        /// allow_other).  Needed when a root daemon mounts a view that a
        /// non-root agent must read/write (privilege-separated chroot model).
        #[arg(long)]
        allow_other: bool,

        /// Maximum storage size for all branch deltas (e.g. "500M", "2G", bytes)
        #[arg(long, value_parser = parse_size)]
        max_storage: Option<u64>,

        /// Mount point
        mountpoint: PathBuf,
    },

    /// Start the storage daemon without mounting (trusted-control path)
    StartDaemon {
        /// Base directory to branch from (required on first start)
        #[arg(long)]
        base: Option<PathBuf>,

        /// Storage directory for branch data
        #[arg(long, default_value = "/var/lib/branchfs")]
        storage: PathBuf,

        /// Maximum storage size for all branch deltas (e.g. "500M", "2G", bytes)
        #[arg(long, value_parser = parse_size)]
        max_storage: Option<u64>,
    },

    /// Create a new branch (and switch a mountpoint to it, if given)
    Create {
        /// Branch name
        name: String,

        /// Mount point to switch to the new branch (omit to only create:
        /// supervisors mount the branch later with `mount --branch`)
        mountpoint: Option<PathBuf>,

        /// Parent branch name
        #[arg(long, short, default_value = "main")]
        parent: String,

        /// Use legacy eager snapshot inheritance instead of O(1) lazy inheritance
        #[arg(long)]
        snapshot: bool,

        /// Hide an inherited path from this branch's view (repeatable;
        /// relative to the branch root, e.g. --hide .ssh --hide .env)
        #[arg(long = "hide")]
        hide: Vec<String>,

        /// Storage directory
        #[arg(long, default_value = "/var/lib/branchfs")]
        storage: PathBuf,
    },

    /// Commit branch to base
    Commit {
        /// Mount point of the branch to commit
        mountpoint: PathBuf,

        /// Storage directory
        #[arg(long, default_value = "/var/lib/branchfs")]
        storage: PathBuf,
    },

    /// Abort branch
    Abort {
        /// Mount point of the branch to abort
        mountpoint: PathBuf,

        /// Storage directory
        #[arg(long, default_value = "/var/lib/branchfs")]
        storage: PathBuf,
    },

    /// Commit a named branch through the daemon (trusted-control path)
    CommitBranch {
        /// Branch name to commit
        branch: String,

        /// Storage directory
        #[arg(long, default_value = "/var/lib/branchfs")]
        storage: PathBuf,

        /// Emit raw JSON commit outcome
        #[arg(long)]
        json: bool,
    },

    /// Abort a named branch through the daemon (trusted-control path)
    AbortBranch {
        /// Branch name to abort
        branch: String,

        /// Storage directory
        #[arg(long, default_value = "/var/lib/branchfs")]
        storage: PathBuf,
    },

    /// Revert/drop one path's branch delta and tombstones through the daemon
    RevertPath {
        /// Branch name to modify
        branch: String,

        /// Path inside the branch to revert/drop
        path: String,

        /// Storage directory
        #[arg(long, default_value = "/var/lib/branchfs")]
        storage: PathBuf,
    },

    /// Freeze a branch read-only for stable review/commit
    Freeze {
        /// Branch name
        branch: String,

        /// Storage directory
        #[arg(long, default_value = "/var/lib/branchfs")]
        storage: PathBuf,
    },

    /// Thaw a frozen branch, allowing writes again
    Thaw {
        /// Branch name
        branch: String,

        /// Storage directory
        #[arg(long, default_value = "/var/lib/branchfs")]
        storage: PathBuf,
    },

    /// Show branch status/diff entries
    Status {
        /// Branch name
        branch: String,

        /// Storage directory
        #[arg(long, default_value = "/var/lib/branchfs")]
        storage: PathBuf,

        /// Emit raw JSON
        #[arg(long)]
        json: bool,
    },

    /// List branches
    List {
        /// Storage directory
        #[arg(long, default_value = "/var/lib/branchfs")]
        storage: PathBuf,
    },

    /// Unmount a branch (daemon auto-exits when last mount is removed)
    Unmount {
        /// Mount point to unmount
        mountpoint: PathBuf,

        /// Storage directory
        #[arg(long, default_value = "/var/lib/branchfs")]
        storage: PathBuf,
    },

    /// Internal: run the daemon (used by `mount` to spawn the daemon process)
    #[command(hide = true)]
    RunDaemon {
        #[arg(long)]
        base: PathBuf,

        #[arg(long)]
        storage: PathBuf,

        #[arg(long)]
        max_storage: Option<u64>,
    },
}

/// Parse a human-readable size string like "500M", "2G", "1024", "1T".
fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size string".to_string());
    }
    let (num_str, multiplier) = match s.as_bytes().last() {
        Some(b'K' | b'k') => (&s[..s.len() - 1], 1024u64),
        Some(b'M' | b'm') => (&s[..s.len() - 1], 1024 * 1024),
        Some(b'G' | b'g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        Some(b'T' | b't') => (&s[..s.len() - 1], 1024 * 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid size: {}", s))?;
    num.checked_mul(multiplier)
        .ok_or_else(|| format!("size overflow: {}", s))
}

fn get_socket_path(storage: &Path) -> PathBuf {
    storage.join("daemon.sock")
}

fn send_request(storage: &Path, request: &Request) -> Result<Response> {
    let socket_path = get_socket_path(storage);
    daemon::send_request(&socket_path, request)
        .map_err(|e| anyhow::anyhow!("Failed to communicate with daemon: {}", e))
}

/// Get the current mount branch from the daemon.
fn get_mount_branch(storage: &Path, mountpoint: &Path) -> Result<String> {
    let resp = send_request(
        storage,
        &Request::GetMountBranch {
            mountpoint: mountpoint.to_string_lossy().to_string(),
        },
    )?;
    if !resp.ok {
        anyhow::bail!("{}", resp.error.unwrap_or_else(|| "unknown error".into()));
    }
    resp.data
        .and_then(|d| d.as_str().map(|s| s.to_string()))
        .ok_or_else(|| anyhow::anyhow!("daemon returned no branch info"))
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Mount {
            base,
            storage,
            branch,
            no_control,
            agent,
            passthrough,
            allow_other,
            max_storage,
            mountpoint,
        } => {
            if passthrough && nix::unistd::geteuid().as_raw() != 0 {
                eprintln!("Error: --passthrough requires root (CAP_SYS_ADMIN)");
                process::exit(1);
            }

            std::fs::create_dir_all(&storage)?;
            let storage = storage.canonicalize()?;

            // Canonicalize base if provided
            let base = base.map(|b| b.canonicalize()).transpose()?;

            // Ensure daemon is running (auto-start if needed)
            daemon::ensure_daemon(base.as_deref(), &storage, max_storage)
                .map_err(|e| anyhow::anyhow!("{}", e))?;

            // Create mountpoint
            std::fs::create_dir_all(&mountpoint)?;
            let mountpoint = mountpoint.canonicalize()?;

            // Send mount request for the selected branch.
            let control = !(no_control || agent);
            let response = send_request(
                &storage,
                &Request::Mount {
                    branch: branch.clone(),
                    mountpoint: mountpoint.to_string_lossy().to_string(),
                    passthrough,
                    control,
                    allow_other,
                },
            )?;

            if response.ok {
                println!(
                    "Mounted branch '{}' at {:?} (control={})",
                    branch, mountpoint, control
                );
            } else {
                eprintln!("Error: {}", response.error.unwrap_or_default());
                process::exit(1);
            }
        }

        Commands::StartDaemon {
            base,
            storage,
            max_storage,
        } => {
            std::fs::create_dir_all(&storage)?;
            let storage = storage.canonicalize()?;
            let base = base.map(|b| b.canonicalize()).transpose()?;

            daemon::ensure_daemon(base.as_deref(), &storage, max_storage)
                .map_err(|e| anyhow::anyhow!("{}", e))?;

            println!("Daemon ready at {:?}", get_socket_path(&storage));
        }

        Commands::Create {
            name,
            mountpoint,
            parent,
            snapshot,
            hide,
            storage,
        } => {
            let storage = storage.canonicalize()?;
            let mountpoint = mountpoint.map(|m| m.canonicalize()).transpose()?;

            let response = send_request(
                &storage,
                &Request::Create {
                    name: name.clone(),
                    parent: parent.clone(),
                    lazy: !snapshot,
                    hide,
                },
            )?;

            if response.ok {
                if let Some(mountpoint) = mountpoint {
                    // Switch to the new branch via ctl file
                    // (FUSE handler updates manager.mount_branches internally)
                    let ctl_path = mountpoint.join(".branchfs_ctl");

                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&ctl_path)
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "Failed to open control file (is {} mounted?): {}",
                                mountpoint.display(),
                                e
                            )
                        })?;

                    file.write_all(format!("switch:{}", name).as_bytes())
                        .map_err(|e| anyhow::anyhow!("Failed to switch to branch: {}", e))?;

                    println!(
                        "Created and switched to branch '{}' (parent: '{}', inheritance: '{}')",
                        name,
                        parent,
                        if snapshot { "snapshot" } else { "lazy" }
                    );
                } else {
                    println!(
                        "Created branch '{}' (parent: '{}', inheritance: '{}')",
                        name,
                        parent,
                        if snapshot { "snapshot" } else { "lazy" }
                    );
                }
            } else {
                eprintln!("Error: {}", response.error.unwrap_or_default());
                process::exit(1);
            }
        }

        Commands::Commit {
            mountpoint,
            storage,
        } => {
            let mountpoint = mountpoint.canonicalize()?;
            let storage = storage.canonicalize()?;

            let branch = get_mount_branch(&storage, &mountpoint)?;
            if branch == "main" {
                anyhow::bail!("Cannot commit main branch");
            }

            // Write to the per-branch ctl file
            // (FUSE handler does commit + switch_mount_branch internally)
            let ctl_path = mountpoint
                .join(format!("@{}", branch))
                .join(".branchfs_ctl");
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&ctl_path)
                .map_err(|e| anyhow::anyhow!("Failed to open branch control file: {}", e))?;

            file.write_all(b"commit")
                .map_err(|e| anyhow::anyhow!("Commit failed: {}", e))?;

            println!("Committed branch at {:?}", mountpoint);
        }

        Commands::Abort {
            mountpoint,
            storage,
        } => {
            let mountpoint = mountpoint.canonicalize()?;
            let storage = storage.canonicalize()?;

            let branch = get_mount_branch(&storage, &mountpoint)?;
            if branch == "main" {
                anyhow::bail!("Cannot abort main branch");
            }

            // Write to the per-branch ctl file
            // (FUSE handler does abort + switch_mount_branch internally)
            let ctl_path = mountpoint
                .join(format!("@{}", branch))
                .join(".branchfs_ctl");
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&ctl_path)
                .map_err(|e| anyhow::anyhow!("Failed to open branch control file: {}", e))?;

            file.write_all(b"abort")
                .map_err(|e| anyhow::anyhow!("Abort failed: {}", e))?;

            println!("Aborted branch at {:?}", mountpoint);
        }

        Commands::List { storage } => {
            let storage = storage.canonicalize()?;

            let response = send_request(&storage, &Request::List)?;

            if response.ok {
                println!(
                    "{:<20} {:<20} {:<10} {:<10} {:>8} {:>8}",
                    "BRANCH", "PARENT", "STATE", "MODE", "DELTAS", "DELETES"
                );
                println!(
                    "{:<20} {:<20} {:<10} {:<10} {:>8} {:>8}",
                    "------", "------", "-----", "----", "------", "-------"
                );

                if let Some(data) = response.data {
                    if let Some(branches) = data.as_array() {
                        for branch in branches {
                            let name = branch["name"].as_str().unwrap_or("-");
                            let parent = branch["parent"].as_str().unwrap_or("-");
                            let state = branch["state"].as_str().unwrap_or("-");
                            let mode = branch["inheritance"].as_str().unwrap_or("-");
                            let deltas = branch["delta_entries"].as_u64().unwrap_or(0);
                            let deletes = branch["tombstones"].as_u64().unwrap_or(0);
                            println!(
                                "{:<20} {:<20} {:<10} {:<10} {:>8} {:>8}",
                                name, parent, state, mode, deltas, deletes
                            );
                        }
                    }
                }
            } else {
                eprintln!("Error: {}", response.error.unwrap_or_default());
                process::exit(1);
            }
        }

        Commands::CommitBranch {
            branch,
            storage,
            json,
        } => {
            let storage = storage.canonicalize()?;
            let response = send_request(
                &storage,
                &Request::CommitBranch {
                    branch: branch.clone(),
                },
            )?;
            if response.ok {
                if json {
                    let data = response
                        .data
                        .ok_or_else(|| anyhow::anyhow!("daemon returned no commit outcome"))?;
                    println!("{}", serde_json::to_string_pretty(&data)?);
                } else {
                    let conflicts = response
                        .data
                        .as_ref()
                        .and_then(|d| d.get("conflicts"))
                        .and_then(|v| v.as_array())
                        .map(|items| items.len())
                        .unwrap_or(0);
                    let auto_merges = response
                        .data
                        .as_ref()
                        .and_then(|d| d.get("auto_merges"))
                        .and_then(|v| v.as_array())
                        .map(|items| items.len())
                        .unwrap_or(0);
                    if conflicts > 0 || auto_merges > 0 {
                        println!(
                            "Committed branch '{}' ({} auto-merge(s), {} conflict(s); latest session won remaining conflicts)",
                            branch, auto_merges, conflicts
                        );
                    } else {
                        println!("Committed branch '{}'", branch);
                    }
                }
            } else {
                eprintln!("Error: {}", response.error.unwrap_or_default());
                process::exit(1);
            }
        }

        Commands::AbortBranch { branch, storage } => {
            let storage = storage.canonicalize()?;
            let response = send_request(
                &storage,
                &Request::AbortBranch {
                    branch: branch.clone(),
                },
            )?;
            if response.ok {
                println!("Aborted branch '{}'", branch);
            } else {
                eprintln!("Error: {}", response.error.unwrap_or_default());
                process::exit(1);
            }
        }

        Commands::RevertPath {
            branch,
            path,
            storage,
        } => {
            let storage = storage.canonicalize()?;
            let response = send_request(
                &storage,
                &Request::RevertPath {
                    branch: branch.clone(),
                    path: path.clone(),
                },
            )?;
            if response.ok {
                println!("Reverted '{}' in branch '{}'", path, branch);
            } else {
                eprintln!("Error: {}", response.error.unwrap_or_default());
                process::exit(1);
            }
        }

        Commands::Freeze { branch, storage } => {
            let storage = storage.canonicalize()?;
            let response = send_request(
                &storage,
                &Request::Freeze {
                    branch: branch.clone(),
                },
            )?;
            if response.ok {
                println!("Frozen branch '{}'", branch);
            } else {
                eprintln!("Error: {}", response.error.unwrap_or_default());
                process::exit(1);
            }
        }

        Commands::Thaw { branch, storage } => {
            let storage = storage.canonicalize()?;
            let response = send_request(
                &storage,
                &Request::Thaw {
                    branch: branch.clone(),
                },
            )?;
            if response.ok {
                println!("Thawed branch '{}'", branch);
            } else {
                eprintln!("Error: {}", response.error.unwrap_or_default());
                process::exit(1);
            }
        }

        Commands::Status {
            branch,
            storage,
            json,
        } => {
            let storage = storage.canonicalize()?;
            let response = send_request(
                &storage,
                &Request::Status {
                    branch: branch.clone(),
                },
            )?;
            if !response.ok {
                eprintln!("Error: {}", response.error.unwrap_or_default());
                process::exit(1);
            }
            let Some(data) = response.data else {
                anyhow::bail!("daemon returned no status data");
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&data)?);
            } else {
                println!("Branch:      {}", data["name"].as_str().unwrap_or("-"));
                println!("Parent:      {}", data["parent"].as_str().unwrap_or("-"));
                println!("State:       {}", data["state"].as_str().unwrap_or("-"));
                println!(
                    "Inheritance: {}",
                    data["inheritance"].as_str().unwrap_or("-")
                );
                println!(
                    "Deltas:      {}",
                    data["delta_entries"].as_u64().unwrap_or(0)
                );
                println!("Deletes:     {}", data["tombstones"].as_u64().unwrap_or(0));
                println!();
                println!("{:<8} {:<10} {:>10} PATH", "OP", "KIND", "BYTES");
                if let Some(diff) = data["diff"].as_array() {
                    for entry in diff {
                        println!(
                            "{:<8} {:<10} {:>10} {}",
                            entry["op"].as_str().unwrap_or("-"),
                            entry["kind"].as_str().unwrap_or("-"),
                            entry["bytes"].as_u64().unwrap_or(0),
                            entry["path"].as_str().unwrap_or("-")
                        );
                    }
                }
            }
        }

        Commands::Unmount {
            mountpoint,
            storage,
        } => {
            let storage = storage.canonicalize()?;
            let mountpoint = mountpoint.canonicalize()?;

            let response = send_request(
                &storage,
                &Request::Unmount {
                    mountpoint: mountpoint.to_string_lossy().to_string(),
                },
            )?;

            if response.ok {
                println!("Unmounted {:?}", mountpoint);
            } else {
                eprintln!("Error: {}", response.error.unwrap_or_default());
                process::exit(1);
            }
        }
        Commands::RunDaemon {
            base,
            storage,
            max_storage,
        } => {
            std::fs::create_dir_all(&storage)?;
            let storage = storage.canonicalize()?;
            let base = base.canonicalize()?;

            let d = daemon::Daemon::new(base.clone(), storage, base, max_storage)
                .map_err(|e| anyhow::anyhow!("Failed to create daemon: {}", e))?;
            d.run()
                .map_err(|e| anyhow::anyhow!("Daemon error: {}", e))?;
        }
    }

    Ok(())
}
