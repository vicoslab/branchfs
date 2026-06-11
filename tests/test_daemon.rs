//! Daemon socket round-trip tests that need no FUSE mount.
//!
//! This is exactly the interface the CCC agent supervisor (`ccc-agent-run`)
//! drives: create a session branch, simulate agent deltas in the branch
//! store, freeze, status, then commit or abort through the trusted daemon
//! control path.  Mount requests are deliberately not exercised here; FUSE
//! runtime behavior is covered by the `--ignored` integration tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use branchfs::daemon::{is_daemon_running, send_request, Daemon, Request};

struct TmpDir(PathBuf);

impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("branchfs-daemon-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, data: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, data).unwrap();
}

struct DaemonHandle {
    socket: PathBuf,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DaemonHandle {
    fn start(base: &Path, storage: &Path) -> Self {
        let daemon = Arc::new(
            Daemon::new(
                base.to_path_buf(),
                storage.to_path_buf(),
                base.to_path_buf(),
                None,
            )
            .unwrap(),
        );
        let socket = daemon.socket_path().clone();
        let runner = daemon.clone();
        let thread = std::thread::spawn(move || {
            runner.run().unwrap();
        });
        for _ in 0..100 {
            if is_daemon_running(&socket) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(is_daemon_running(&socket), "daemon did not come up");
        DaemonHandle {
            socket,
            thread: Some(thread),
        }
    }

    fn request(&self, request: &Request) -> branchfs::daemon::Response {
        send_request(&self.socket, request).unwrap()
    }

    fn shutdown(&mut self) {
        let _ = send_request(&self.socket, &Request::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[test]
fn supervisor_flow_create_status_freeze_commit() {
    let tmp = TmpDir::new();
    let base = tmp.path().join("base");
    let storage = tmp.path().join("storage");
    write(&base.join("inherited.txt"), b"underlay data");

    let mut daemon = DaemonHandle::start(&base, &storage);

    // create session branch (lazy, O(1)) without any mountpoint
    let resp = daemon.request(&Request::Create {
        name: "agent-session-1".into(),
        parent: "main".into(),
        lazy: true,
        hide: vec![],
    });
    assert!(resp.ok, "create failed: {:?}", resp.error);

    // lazy create must not copy the inherited tree
    assert!(
        !storage
            .join("branches/agent-session-1/inherited/inherited.txt")
            .exists(),
        "lazy branch must not snapshot inherited files"
    );

    // status of a fresh branch: open, lazy, empty diff
    let resp = daemon.request(&Request::Status {
        branch: "agent-session-1".into(),
    });
    assert!(resp.ok);
    let data = resp.data.unwrap();
    assert_eq!(data["state"], "open");
    assert_eq!(data["inheritance"], "lazy");
    assert_eq!(data["diff"].as_array().unwrap().len(), 0);

    // simulate agent writes the way FUSE would land them: delta files
    write(
        &storage.join("branches/agent-session-1/files/Projects/out.txt"),
        b"agent artifact",
    );

    let resp = daemon.request(&Request::Status {
        branch: "agent-session-1".into(),
    });
    let data = resp.data.unwrap();
    let diff = data["diff"].as_array().unwrap();
    // the delta walk reports both the COW directory and the file inside it
    let file_entry = diff
        .iter()
        .find(|e| e["path"] == "/Projects/out.txt")
        .expect("file delta entry missing");
    assert_eq!(file_entry["op"], "delta");
    assert_eq!(file_entry["kind"], "file");
    assert!(diff
        .iter()
        .all(|e| e["op"] == "delta" || e["op"] == "delete"));

    // freeze for stable review
    let resp = daemon.request(&Request::Freeze {
        branch: "agent-session-1".into(),
    });
    assert!(resp.ok, "freeze failed: {:?}", resp.error);
    let resp = daemon.request(&Request::Status {
        branch: "agent-session-1".into(),
    });
    assert_eq!(resp.data.unwrap()["state"], "frozen");

    // trusted commit applies the delta to the real underlay
    let resp = daemon.request(&Request::CommitBranch {
        branch: "agent-session-1".into(),
    });
    assert!(resp.ok, "commit failed: {:?}", resp.error);
    let committed = base.join("Projects/out.txt");
    assert_eq!(fs::read(&committed).unwrap(), b"agent artifact");

    daemon.shutdown();
}

#[test]
fn supervisor_flow_abort_discards_branch() {
    let tmp = TmpDir::new();
    let base = tmp.path().join("base");
    let storage = tmp.path().join("storage");
    write(&base.join("keep.txt"), b"keep me");

    let mut daemon = DaemonHandle::start(&base, &storage);

    let resp = daemon.request(&Request::Create {
        name: "agent-session-2".into(),
        parent: "main".into(),
        lazy: true,
        hide: vec![],
    });
    assert!(resp.ok);

    write(
        &storage.join("branches/agent-session-2/files/junk.txt"),
        b"discard me",
    );

    let resp = daemon.request(&Request::AbortBranch {
        branch: "agent-session-2".into(),
    });
    assert!(resp.ok, "abort failed: {:?}", resp.error);

    // underlay untouched, branch gone
    assert_eq!(fs::read(base.join("keep.txt")).unwrap(), b"keep me");
    assert!(!base.join("junk.txt").exists());
    let resp = daemon.request(&Request::Status {
        branch: "agent-session-2".into(),
    });
    assert!(!resp.ok, "aborted branch should not have status");

    daemon.shutdown();
}

#[test]
fn hidden_paths_mask_inherited_data_and_persist() {
    let tmp = TmpDir::new();
    let base = tmp.path().join("base");
    let storage = tmp.path().join("storage");
    write(&base.join(".ssh/id_rsa"), b"SECRET KEY");
    write(&base.join(".env"), b"TOKEN=hunter2");
    write(&base.join("Projects/code.py"), b"print('ok')");

    let mut daemon = DaemonHandle::start(&base, &storage);

    let resp = daemon.request(&Request::Create {
        name: "agent-hidden".into(),
        parent: "main".into(),
        lazy: true,
        hide: vec![".ssh".into(), "/.env".into()],
    });
    assert!(resp.ok, "create failed: {:?}", resp.error);
    daemon.shutdown();

    // Reload the store from scratch: hide rules must persist in branch
    // metadata, and the manager (the resolver every FUSE op goes through)
    // must refuse to resolve hidden inherited paths.
    let mgr = Daemon::new(
        base.to_path_buf(),
        storage.to_path_buf(),
        base.to_path_buf(),
        None,
    )
    .unwrap()
    .get_manager();
    assert!(mgr
        .resolve_path("agent-hidden", "/.ssh/id_rsa")
        .unwrap()
        .is_none());
    assert!(mgr.resolve_path("agent-hidden", "/.env").unwrap().is_none());
    assert!(mgr
        .resolve_path("agent-hidden", "/Projects/code.py")
        .unwrap()
        .is_some());
    // readdir must not list hidden names
    let names = mgr.collect_dir_names("agent-hidden", "/").unwrap();
    assert!(!names.contains(".ssh"));
    assert!(!names.contains(".env"));
    assert!(names.contains("Projects"));
    // main branch is unaffected
    assert!(mgr.resolve_path("main", "/.ssh/id_rsa").unwrap().is_some());
}

#[test]
fn frozen_branch_survives_daemon_restart() {
    let tmp = TmpDir::new();
    let base = tmp.path().join("base");
    let storage = tmp.path().join("storage");
    fs::create_dir_all(&base).unwrap();

    {
        let mut daemon = DaemonHandle::start(&base, &storage);
        let resp = daemon.request(&Request::Create {
            name: "agent-session-3".into(),
            parent: "main".into(),
            lazy: true,
            hide: vec![],
        });
        assert!(resp.ok);
        let resp = daemon.request(&Request::Freeze {
            branch: "agent-session-3".into(),
        });
        assert!(resp.ok);
        daemon.shutdown();
    }

    // a fresh daemon over the same store must still see the frozen state
    // (pending-review sessions outlive the daemon that created them)
    let mut daemon = DaemonHandle::start(&base, &storage);
    let resp = daemon.request(&Request::Status {
        branch: "agent-session-3".into(),
    });
    assert!(resp.ok, "status failed: {:?}", resp.error);
    assert_eq!(resp.data.unwrap()["state"], "frozen");
    daemon.shutdown();
}
