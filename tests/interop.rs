//! End-to-end tests against fwknop-rs's real `fwknopd`, built from the same
//! pinned commit as `fkospa`. Opt-in (`make interop`, Linux only): they need the
//! server binary, passed in `FWKNOPD`. The server runs unprivileged on `[::1]`
//! in `--test` mode, which decodes and checks packets but runs no firewall
//! commands, with the access stanza the README recommends.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Read};
use std::net::{Ipv6Addr, SocketAddrV6, UdpSocket};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ipv6rd::keys::build_keyset;
use ipv6rd::knock::{Knocker, build_packet, send_packet};
use zeroize::Zeroizing;

const ACCEPTED: &str = "--test mode enabled, skipping action.";

/// A child process whose stderr is collected line by line.
struct Proc {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Proc {
    fn spawn(mut cmd: Command) -> Proc {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawning {cmd:?}: {e}"));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let pipe = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                sink.lock().unwrap().push(line);
            }
        });
        Proc { child, lines }
    }

    fn count(&self, needle: &str) -> usize {
        self.lines.lock().unwrap().iter().filter(|l| l.contains(needle)).count()
    }

    fn wait_for(&self, needle: &str, times: usize) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.count(needle) < times {
            if Instant::now() > deadline {
                panic!(
                    "timed out waiting for {times}x {needle:?}; log:\n{}",
                    self.lines.lock().unwrap().join("\n")
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Server {
    dir: PathBuf,
    port: u16,
    proc: Proc,
    key_b64: String,
    hmac_b64: String,
}

fn fwknopd_path() -> PathBuf {
    PathBuf::from(std::env::var_os("FWKNOPD").expect(
        "set FWKNOPD to an fwknop-rs fwknopd binary (make interop does this)",
    ))
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ipv6rd-interop-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn free_port() -> u16 {
    UdpSocket::bind("[::1]:0").unwrap().local_addr().unwrap().port()
}

/// Starts fwknopd on [::1] with the README's recommended access stanza.
fn start_server(tag: &str) -> Server {
    let dir = temp_dir(tag);
    let port = free_port();
    let key_b64 = fkospa::generate_key(32).unwrap();
    let hmac_b64 = fkospa::generate_key(64).unwrap();
    std::fs::write(
        dir.join("server.yaml"),
        format!(
            "udp_port: {port}\nbind_address: \"::1\"\nlog_file: \"\"\nsyslog_identity: \"\"\n\
             pid_file: {}\nmax_spa_packet_age: 120\nactions:\n\
             \x20 init: \"true\"\n  open: \"true\"\n  close: \"true\"\n  shutdown: \"true\"\n",
            dir.join("fwknopd.pid").display()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("access.yaml"),
        format!(
            "- source: ANY\n  open_ports: [tcp/443, tcp/22]\n  key_base64: \"{key_b64}\"\n  \
             hmac_key_base64: \"{hmac_b64}\"\n  hmac_digest_type: sha256\n  encryption_mode: cbc\n  \
             require_source_address: true\n  access_timeout: 30\n"
        ),
    )
    .unwrap();

    let mut cmd = Command::new(fwknopd_path());
    cmd.arg("-c")
        .arg(dir.join("server.yaml"))
        .arg("-a")
        .arg(dir.join("access.yaml"))
        .args(["-f", "-t"]);
    let proc = Proc::spawn(cmd);
    proc.wait_for("UDP server listening", 1);
    Server { dir, port, proc, key_b64, hmac_b64 }
}

fn decode(b64: &str) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(fkospa::decode_base64_key(b64).unwrap())
}

#[test]
#[ignore = "needs FWKNOPD; run with `make interop`"]
fn fwknopd_accepts_knocks_and_rejects_replays_and_spoofed_sources() {
    let server = start_server("codec");
    let keys = build_keyset(decode(&server.key_b64), decode(&server.hmac_b64)).unwrap();
    let target = SocketAddrV6::new(Ipv6Addr::LOCALHOST, server.port, 0, 0);
    let timeout = Duration::from_secs(5);

    // A packet sent from the address it authorizes is accepted...
    let data = build_packet(&keys, Ipv6Addr::LOCALHOST, "tcp/443".parse().unwrap()).unwrap();
    send_packet(Ipv6Addr::LOCALHOST, target, data.as_str().as_bytes(), timeout).unwrap();
    server.proc.wait_for(ACCEPTED, 1);

    // ...and the identical packet again is a replay.
    send_packet(Ipv6Addr::LOCALHOST, target, data.as_str().as_bytes(), timeout).unwrap();
    server.proc.wait_for("Replay detected", 1);

    // An allow IP that is not the packet's source is refused.
    let spoofed = build_packet(&keys, "2001:db8::1".parse().unwrap(), "tcp/443".parse().unwrap()).unwrap();
    send_packet(Ipv6Addr::LOCALHOST, target, spoofed.as_str().as_bytes(), timeout).unwrap();
    server.proc.wait_for("Source address mismatch", 1);

    // The full knock: one accepted packet per access entry.
    let knocker = Knocker { target, access: vec!["tcp/443".parse().unwrap(), "tcp/22".parse().unwrap()], keys };
    knocker.knock(Ipv6Addr::LOCALHOST, timeout).unwrap();
    server.proc.wait_for(ACCEPTED, 3);
    assert_eq!(server.proc.count(ACCEPTED), 3, "no other packet may be accepted");
    let _ = std::fs::remove_dir_all(&server.dir);
}

/// Writes a fake `op` that prints the right key for each reference.
fn fake_op(dir: &Path, key_b64: &str, hmac_b64: &str) -> PathBuf {
    let path = dir.join("op");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$3\" in\n  op://test/key) printf '%s' '{key_b64}' ;;\n  \
             op://test/hmac) printf '%s' '{hmac_b64}' ;;\n  *) echo \"no such item: $3\" >&2; exit 1 ;;\nesac\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[test]
#[ignore = "needs FWKNOPD; run with `make interop`"]
fn the_daemon_knocks_fwknopd_end_to_end() {
    let server = start_server("daemon");
    let op = fake_op(&server.dir, &server.key_b64, &server.hmac_b64);
    let config = server.dir.join("ipv6rd.toml");
    std::fs::write(
        &config,
        format!(
            "debounce = \"100ms\"\nreconcile = \"1h\"\nop_path = \"{}\"\n\n[fwknop]\ndest = \"::1\"\n\
             port = {}\naccess = [\"tcp/443\", \"tcp/22\"]\nkey_ref = \"op://test/key\"\n\
             hmac_ref = \"op://test/hmac\"\n",
            op.display(),
            server.port
        ),
    )
    .unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ipv6rd"));
    cmd.arg("--config").arg(&config).env("OP_SERVICE_ACCOUNT_TOKEN", "ops_interop_dummy");
    let daemon = Proc::spawn(cmd);

    daemon.wait_for("msg=\"fire succeeded\" addr=::1", 1);
    server.proc.wait_for(ACCEPTED, 2);

    // The token is out of the environment and /proc/<pid>/environ is closed to
    // our own user (dumpable flag cleared).
    if !nix::unistd::geteuid().is_root() {
        let err = std::fs::File::open(format!("/proc/{}/environ", daemon.pid()))
            .and_then(|mut f| f.read_to_end(&mut Vec::new()))
            .expect_err("environ must not be readable");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    // SIGTERM stops it cleanly with exit code 0.
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(daemon.pid()), nix::sys::signal::Signal::SIGTERM)
        .unwrap();
    daemon.wait_for("shutdown signal received, stopping", 1);
    let mut daemon = daemon;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = daemon.child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "ipv6rd did not exit");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status}");
    let _ = std::fs::remove_dir_all(&server.dir);
}
