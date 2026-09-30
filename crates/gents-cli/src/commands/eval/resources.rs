use anyhow::Result;

/// A concurrent eval opens one embedded database per trial. GUI-launched
/// terminals on macOS can inherit a 256-descriptor soft limit. Raise only this
/// process's soft limit, leaving the user's hard limit and system policy intact.
#[cfg(unix)]
pub(super) fn prepare() -> Result<()> {
    use anyhow::Context;
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: limits points to initialized storage for one rlimit value.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) } != 0 {
        return Err(std::io::Error::last_os_error()).context("read eval open-file limit");
    }
    let target = 65_536.min(limits.rlim_max);
    let previous = limits.rlim_cur;
    if previous < target {
        limits.rlim_cur = target;
        // SAFETY: limits is a valid rlimit; only the soft limit increases.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) } != 0 {
            return Err(std::io::Error::last_os_error()).context(
                "raise eval open-file limit; set ulimit -Sn in this terminal before retrying",
            );
        }
        tracing::info!(previous, current = target, "raised eval open-file limit");
    }
    if limits.rlim_cur < 65_536 {
        tracing::warn!(limit = limits.rlim_cur, "eval open-file limit is constrained by the hard limit; reduce concurrency if trials exhaust descriptors");
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn prepare() -> Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn raises_inherited_soft_limit_without_changing_hard_limit() {
        const CHILD: &str = "GENTS_EVAL_LIMIT_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let mut limits = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: limits is valid writable storage; this subprocess owns
            // its resource limits and has not started a runtime.
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) },
                0
            );
            let hard = limits.rlim_max;
            limits.rlim_cur = 256.min(hard);
            // SAFETY: a valid limit within the unchanged hard limit.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) }, 0);
            super::prepare().unwrap();
            // SAFETY: limits remains valid writable storage.
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) },
                0
            );
            assert_eq!(limits.rlim_cur, 65_536.min(hard));
            assert_eq!(limits.rlim_max, hard);
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "commands::eval::resources::tests::raises_inherited_soft_limit_without_changing_hard_limit", "--nocapture"])
            .env(CHILD, "1")
            .status().unwrap();
        assert!(status.success());
    }
}
