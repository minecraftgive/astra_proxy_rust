//! Desktop launchers may give the proxy only 256 file descriptors, far below
//! the default 4096 sessions (each ordinarily needs an inbound and an outbound).

#[cfg(unix)]
const TARGET: libc::rlim_t = 65_536;

pub(crate) fn prepare() {
    #[cfg(unix)]
    match raise() {
        Ok((before, after)) => {
            if before.rlim_cur != after.rlim_cur {
                tracing::info!(
                    previous = before.rlim_cur,
                    soft = after.rlim_cur,
                    hard = after.rlim_max,
                    "raised file descriptor limit"
                );
            }
            if after.rlim_cur < TARGET {
                tracing::warn!(soft = after.rlim_cur, hard = after.rlim_max, requested = TARGET,
                    "file descriptor limit remains low; raise the service limit or reduce max_connections and max_udp_flows");
            }
        }
        Err(error) => tracing::warn!(%error, requested = TARGET,
            "could not raise file descriptor limit; inherited process limit remains in effect"),
    }
}

#[cfg(unix)]
fn raise() -> std::io::Result<(libc::rlimit, libc::rlimit)> {
    let mut before = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: before is a valid, writable rlimit.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut before) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if before.rlim_cur >= TARGET {
        return Ok((before, before));
    }
    let target = TARGET.min(before.rlim_max).min(system_ceiling()?);
    let after = libc::rlimit {
        // Never lower an inherited soft limit, even if the system ceiling fell.
        rlim_cur: before.rlim_cur.max(target),
        rlim_max: before.rlim_max,
    };
    if after.rlim_cur != before.rlim_cur {
        // SAFETY: after is valid; only the soft limit is raised, within the
        // inherited hard limit. This does not require elevated privileges.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &after) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((before, after))
}

#[cfg(target_os = "macos")]
fn system_ceiling() -> std::io::Result<libc::rlim_t> {
    // Darwin rejects limits above kern.maxfilesperproc even when the inherited
    // hard limit is RLIM_INFINITY. Do not change the machine-wide setting.
    let mut maximum: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&maximum);
    // SAFETY: the name is NUL-terminated, the output buffer and its length are
    // valid, and the null input pointer makes this a read-only sysctl query.
    if unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&mut maximum as *mut libc::c_int).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if length != std::mem::size_of_val(&maximum) || maximum <= 0 {
        return Err(std::io::Error::other("invalid kern.maxfilesperproc value"));
    }
    Ok(maximum as libc::rlim_t)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn system_ceiling() -> std::io::Result<libc::rlim_t> {
    Ok(libc::RLIM_INFINITY)
}

#[cfg(all(test, unix))]
#[path = "../test/platform_file_limit_test.rs"]
mod tests;
