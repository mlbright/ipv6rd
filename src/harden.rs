//! Keeping secrets out of the process's leak paths (ADR-0006, threat d).

use zeroize::Zeroizing;

/// Disables core dumps. On Linux it also clears the "dumpable" flag, which
/// additionally stops other processes of the same user from attaching a
/// debugger or reading `/proc/<pid>/environ` and `/proc/<pid>/mem`.
pub fn disable_core_dumps() -> Result<(), String> {
    use nix::sys::resource::{Resource, setrlimit};
    setrlimit(Resource::RLIMIT_CORE, 0, 0).map_err(|e| format!("setting RLIMIT_CORE to 0: {e}"))?;
    #[cfg(target_os = "linux")]
    nix::sys::prctl::set_dumpable(false).map_err(|e| format!("clearing the dumpable flag: {e}"))?;
    Ok(())
}

/// Reads `name` from the environment and removes it, returning the trimmed
/// value in a buffer that is zeroed on drop. Blank counts as unset.
///
/// Must be called while the process is still single-threaded: it modifies the
/// environment, which is not thread-safe.
#[allow(unsafe_code)]
pub fn take_env_secret(name: &str) -> Option<Zeroizing<String>> {
    let raw = Zeroizing::new(std::env::var(name).ok()?);
    // SAFETY: only called from `main` before any other thread is started, so
    // nothing can be reading or writing the environment concurrently.
    unsafe { std::env::remove_var(name) };
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| Zeroizing::new(trimmed.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_dumps_are_disabled() {
        // Runs in its own test-binary process; lowering the limit is harmless.
        disable_core_dumps().unwrap();
        let (soft, hard) =
            nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_CORE).unwrap();
        assert_eq!((soft, hard), (0, 0));
        #[cfg(target_os = "linux")]
        assert!(!nix::sys::prctl::get_dumpable().unwrap());
    }
}
