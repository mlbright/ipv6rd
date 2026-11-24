//! Fetching the fwknop keys with the 1Password CLI (ADR-0007).
//!
//! `op read --no-newline <ref>` runs once per key at startup, with a service
//! account token. The child gets an empty environment plus an allowlist, its
//! output is read into a bounded buffer that is zeroed after decoding, and each
//! call is killed if it outlasts its timeout.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError, bounded};
use zeroize::Zeroizing;

/// Per-call timeout for `op read`.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// The most stdout accepted from one `op read`; a base64 key is far smaller.
const MAX_STDOUT: usize = 4096;
/// How much of `op`'s stderr is kept for the error message.
const MAX_STDERR: usize = 1024;

/// Why an `op read` produced no key.
#[derive(Debug)]
pub enum ReadError {
    /// Shutdown was requested while `op` was running.
    Cancelled,
    /// Anything else; the message never contains secret material.
    Failed(String),
}

/// Finds the `op` binary: `configured` if set, else the first `op` in an
/// absolute directory on `path_var`.
pub fn locate(configured: Option<&Path>, path_var: Option<&OsStr>) -> Result<PathBuf, String> {
    if let Some(p) = configured {
        return Ok(p.to_path_buf());
    }
    let path_var = path_var.unwrap_or_default();
    std::env::split_paths(path_var)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("op"))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .ok_or_else(|| {
            "the 1Password CLI `op` was not found on PATH; install it or set op_path in the config"
                .to_string()
        })
}

/// Resolves symlinks and refuses an `op` binary that other users could have
/// replaced: it must be a regular file, not group- or world-writable, and owned
/// by root or by `euid` (the daemon's user). Returns the canonical path, which
/// is what gets executed.
pub fn check_binary(path: &Path, euid: u32) -> Result<PathBuf, String> {
    let real = fs::canonicalize(path).map_err(|e| format!("op binary {}: {e}", path.display()))?;
    let meta = fs::metadata(&real).map_err(|e| format!("op binary {}: {e}", real.display()))?;
    if !meta.is_file() {
        return Err(format!(
            "op binary {} is not a regular file",
            real.display()
        ));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(format!(
            "op binary {} is group- or world-writable (mode {:04o}); refusing to run it",
            real.display(),
            meta.mode() & 0o7777
        ));
    }
    if meta.uid() != 0 && meta.uid() != euid {
        return Err(format!(
            "op binary {} is owned by uid {}, not root or the daemon's user (uid {euid}); \
             refusing to run it",
            real.display(),
            meta.uid()
        ));
    }
    Ok(real)
}

/// Runs `op read` with a service-account token.
#[derive(Debug)]
pub struct OpClient {
    path: PathBuf,
    token: Zeroizing<String>,
    home: Option<OsString>,
    config_dir: Option<OsString>,
    timeout: Duration,
}

impl OpClient {
    /// `home` and `config_dir` are the daemon's own `HOME` and `OP_CONFIG_DIR`,
    /// passed through to `op` when set.
    pub fn new(
        path: PathBuf,
        token: Zeroizing<String>,
        home: Option<OsString>,
        config_dir: Option<OsString>,
        timeout: Duration,
    ) -> OpClient {
        OpClient {
            path,
            token,
            home,
            config_dir,
            timeout,
        }
    }

    /// The complete environment `op` runs with.
    fn child_env(&self) -> Vec<(&'static str, OsString)> {
        let mut env = vec![
            (
                "OP_SERVICE_ACCOUNT_TOKEN",
                OsString::from(self.token.as_str()),
            ),
            // No background `op` process may keep holding the token.
            ("OP_CACHE", OsString::from("false")),
        ];
        if let Some(h) = &self.home {
            env.push(("HOME", h.clone()));
        }
        if let Some(d) = &self.config_dir {
            env.push(("OP_CONFIG_DIR", d.clone()));
        }
        if cfg!(target_os = "macos") {
            // Reported workaround for a permission prompt on every run under launchd.
            env.push(("OP_LOAD_DESKTOP_APP_SETTINGS", OsString::from("false")));
        }
        env
    }

    /// Resolves one `op://` reference and base64-decodes it to key bytes.
    /// `shutdown` disconnects when the daemon is asked to stop.
    pub fn read_key(
        &self,
        reference: &str,
        shutdown: &Receiver<()>,
    ) -> Result<Zeroizing<Vec<u8>>, ReadError> {
        let out = self.run_read(reference, shutdown)?;
        let text = std::str::from_utf8(&out)
            .map_err(|_| ReadError::Failed("op printed something that is not text".into()))?;
        let key =
            Zeroizing::new(fkospa::decode_base64_key(text.trim()).map_err(|_| {
                ReadError::Failed("the resolved secret is not valid base64".into())
            })?);
        if key.is_empty() {
            return Err(ReadError::Failed("the resolved secret is empty".into()));
        }
        Ok(key)
    }

    fn run_read(
        &self,
        reference: &str,
        shutdown: &Receiver<()>,
    ) -> Result<Zeroizing<Vec<u8>>, ReadError> {
        let mut child = Command::new(&self.path)
            .args(["read", "--no-newline", reference])
            .env_clear()
            .envs(self.child_env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ReadError::Failed(format!("running {}: {e}", self.path.display())))?;

        let (out_tx, out_rx) = bounded(1);
        let stdout = child.stdout.take().expect("stdout is piped");
        thread::spawn(move || {
            let _ = out_tx.send(read_bounded(stdout, MAX_STDOUT));
        });
        let (err_tx, err_rx) = bounded(1);
        let stderr = child.stderr.take().expect("stderr is piped");
        thread::spawn(move || {
            let _ = err_tx.send(read_capped(stderr, MAX_STDERR));
        });

        let deadline = Instant::now() + self.timeout;
        let status = wait_until(&mut child, deadline, shutdown)?;

        // The child has exited, so its pipes close and the readers finish.
        let grace = Duration::from_secs(2);
        let stderr = err_rx.recv_timeout(grace).unwrap_or_default();
        let stdout = out_rx
            .recv_timeout(grace)
            .map_err(|_| ReadError::Failed("op's output did not close after it exited".into()))?
            .map_err(|e| ReadError::Failed(format!("reading op's output: {e}")))?;

        if !status.success() {
            let detail = stderr.trim();
            return Err(ReadError::Failed(if detail.is_empty() {
                format!("op read {reference}: {status}")
            } else {
                format!("op read {reference}: {status}: {detail}")
            }));
        }
        stdout.ok_or_else(|| {
            ReadError::Failed(format!(
                "op printed more than {MAX_STDOUT} bytes; that is not a key"
            ))
        })
    }
}

/// Waits for `child` to exit, killing it at `deadline` or on shutdown.
fn wait_until(
    child: &mut Child,
    deadline: Instant,
    shutdown: &Receiver<()>,
) -> Result<ExitStatus, ReadError> {
    let kill = |child: &mut Child| {
        let _ = child.kill();
        let _ = child.wait();
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(e) => {
                kill(child);
                return Err(ReadError::Failed(format!("waiting for op: {e}")));
            }
        }
        if matches!(shutdown.try_recv(), Err(TryRecvError::Disconnected)) {
            kill(child);
            return Err(ReadError::Cancelled);
        }
        if Instant::now() >= deadline {
            kill(child);
            return Err(ReadError::Failed("op read timed out; it was killed".into()));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Reads all of `r` into a zeroed, pre-sized buffer that never reallocates (so
/// no unzeroed copy of a secret is left behind). Returns `None` if `r` holds
/// more than `max` bytes; the excess is drained so the writer is not blocked.
fn read_bounded(mut r: impl Read, max: usize) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    let mut buf = Zeroizing::new(vec![0u8; max]);
    let mut filled = 0;
    loop {
        if filled == max {
            let mut scratch = Zeroizing::new([0u8; 512]);
            let mut overflowed = false;
            loop {
                match r.read(&mut scratch[..]) {
                    Ok(0) => break,
                    Ok(_) => overflowed = true,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            if overflowed {
                return Ok(None);
            }
            break;
        }
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(Some(buf))
}

/// Reads `r` to the end, keeping at most `max` bytes as lossy UTF-8.
fn read_capped(r: impl Read, max: usize) -> String {
    let mut kept = Vec::new();
    let _ = r.take(max as u64).read_to_end(&mut kept);
    String::from_utf8_lossy(&kept).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh private directory under the system temp dir.
    fn temp_dir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ipv6rd-op-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    /// Writes a fake `op` shell script and returns its path.
    fn fake_op(dir: &Path, body: &str, mode: u32) -> PathBuf {
        let path = dir.join("op");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn client(path: PathBuf, timeout: Duration) -> OpClient {
        OpClient::new(
            path,
            Zeroizing::new("ops_test_token".into()),
            Some("/home/test".into()),
            Some("/run/ipv6rd/op".into()),
            timeout,
        )
    }

    fn never_shut_down() -> (crossbeam_channel::Sender<()>, Receiver<()>) {
        bounded(1)
    }

    fn euid() -> u32 {
        nix::unistd::geteuid().as_raw()
    }

    const KEY_B64: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";

    #[test]
    fn reads_and_decodes_a_key_with_only_the_allowlisted_env() {
        let dir = temp_dir();
        let env_file = dir.join("env");
        let args_file = dir.join("args");
        let op = fake_op(
            &dir,
            &format!(
                "export -p > '{}'\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' '{KEY_B64}'",
                env_file.display(),
                args_file.display()
            ),
            0o700,
        );
        let (_tx, rx) = never_shut_down();
        let key = client(op, READ_TIMEOUT)
            .read_key("op://vault/item/field", &rx)
            .unwrap();
        assert_eq!(&key[..], b"0123456789abcdef0123456789abcdef");

        let args = fs::read_to_string(&args_file).unwrap();
        assert_eq!(args, "read\n--no-newline\nop://vault/item/field\n");

        let env = fs::read_to_string(&env_file).unwrap();
        let names: Vec<&str> = env
            .lines()
            .filter_map(|l| l.strip_prefix("export "))
            .map(|l| l.split('=').next().unwrap())
            .collect();
        // What the shell itself may add, plus our allowlist; nothing else.
        let allowed = [
            "OP_SERVICE_ACCOUNT_TOKEN",
            "OP_CACHE",
            "HOME",
            "OP_CONFIG_DIR",
            "OP_LOAD_DESKTOP_APP_SETTINGS",
            "PWD",
            "OLDPWD",
            "SHLVL",
            "_",
        ];
        for n in &names {
            assert!(
                allowed.contains(n),
                "unexpected variable {n} in op's environment:\n{env}"
            );
        }
        assert!(
            env.contains("OP_SERVICE_ACCOUNT_TOKEN='ops_test_token'")
                || env.contains("OP_SERVICE_ACCOUNT_TOKEN=\"ops_test_token\""),
            "{env}"
        );
        assert!(
            env.contains("OP_CACHE='false'") || env.contains("OP_CACHE=\"false\""),
            "{env}"
        );
        assert!(env.contains("/run/ipv6rd/op"), "{env}");
        assert!(
            !names.contains(&"PATH") && !names.contains(&"OP_CONNECT_HOST"),
            "{env}"
        );
    }

    #[test]
    fn nonzero_exit_reports_stderr() {
        let dir = temp_dir();
        let op = fake_op(&dir, "echo '[ERROR] item not found' >&2\nexit 1", 0o700);
        let (_tx, rx) = never_shut_down();
        match client(op, READ_TIMEOUT).read_key("op://v/i/f", &rx) {
            Err(ReadError::Failed(e)) => {
                assert!(
                    e.contains("op read op://v/i/f") && e.contains("item not found"),
                    "{e}"
                )
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn invalid_base64_is_rejected_without_echoing_it() {
        let dir = temp_dir();
        let op = fake_op(&dir, "printf '%s' 'not base64 secret!'", 0o700);
        let (_tx, rx) = never_shut_down();
        match client(op, READ_TIMEOUT).read_key("op://v/i/f", &rx) {
            Err(ReadError::Failed(e)) => {
                assert!(
                    e.contains("not valid base64") && !e.contains("secret!"),
                    "{e}"
                )
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn oversized_output_is_rejected() {
        let dir = temp_dir();
        let op = fake_op(
            &dir,
            "i=0\nwhile [ $i -lt 600 ]; do printf 'AAAAAAAAAA'; i=$((i+1)); done",
            0o700,
        );
        let (_tx, rx) = never_shut_down();
        match client(op, READ_TIMEOUT).read_key("op://v/i/f", &rx) {
            Err(ReadError::Failed(e)) => assert!(e.contains("more than 4096 bytes"), "{e}"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_hung_op_is_killed_at_the_timeout() {
        let dir = temp_dir();
        let op = fake_op(&dir, "exec /bin/sleep 60", 0o700);
        let (_tx, rx) = never_shut_down();
        let start = Instant::now();
        match client(op, Duration::from_millis(300)).read_key("op://v/i/f", &rx) {
            Err(ReadError::Failed(e)) => assert!(e.contains("timed out"), "{e}"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn shutdown_cancels_a_running_op() {
        let dir = temp_dir();
        let op = fake_op(&dir, "exec /bin/sleep 60", 0o700);
        let (tx, rx) = bounded::<()>(1);
        drop(tx);
        assert!(matches!(
            client(op, READ_TIMEOUT).read_key("op://v/i/f", &rx),
            Err(ReadError::Cancelled)
        ));
    }

    #[test]
    fn writable_binaries_are_refused() {
        let dir = temp_dir();
        for mode in [0o720, 0o702, 0o777] {
            let op = fake_op(&dir, "true", mode);
            let e = check_binary(&op, euid()).unwrap_err();
            assert!(e.contains("group- or world-writable"), "{mode:o}: {e}");
        }
        let op = fake_op(&dir, "true", 0o755);
        assert_eq!(
            check_binary(&op, euid()).unwrap(),
            fs::canonicalize(&op).unwrap()
        );
    }

    #[test]
    fn binaries_owned_by_another_user_are_refused() {
        let dir = temp_dir();
        let op = fake_op(&dir, "true", 0o755);
        if euid() == 0 {
            return; // owned by root, which is always allowed
        }
        let e = check_binary(&op, euid() + 1).unwrap_err();
        assert!(e.contains("not root or the daemon's user"), "{e}");
    }

    #[test]
    fn symlinks_are_checked_at_their_target() {
        let dir = temp_dir();
        let real = fake_op(&dir, "true", 0o777);
        let link = dir.join("op-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(
            check_binary(&link, euid())
                .unwrap_err()
                .contains("world-writable")
        );
    }

    #[test]
    fn locate_uses_op_path_or_absolute_path_entries() {
        let dir = temp_dir();
        let op = fake_op(&dir, "true", 0o755);
        let configured = PathBuf::from("/opt/custom/op");
        assert_eq!(locate(Some(&configured), None).unwrap(), configured);

        let path_var =
            std::env::join_paths([Path::new("relative/dir"), Path::new("/nonexistent"), &dir])
                .unwrap();
        assert_eq!(locate(None, Some(&path_var)).unwrap(), op);

        let only_relative = OsString::from(".");
        assert!(
            locate(None, Some(&only_relative))
                .unwrap_err()
                .contains("not found on PATH")
        );
        assert!(locate(None, None).is_err());
    }
}
