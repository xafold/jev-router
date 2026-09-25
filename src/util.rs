//! Small OS helpers: data dir, ids, timestamps, SIGINT.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

pub fn data_dir() -> PathBuf {
    home().join(".local/share/claude-router")
}

/// 12 hex chars, random per call (RandomState is seeded from the OS).
pub fn random_id() -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    format!("{:016x}", h.finish())[..12].to_string()
}

fn broken_down(local: bool) -> libc::tm {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    // SAFETY: tm is plain data; *_r variants are thread-safe and only write into `tm`.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if local {
            libc::localtime_r(&now, &mut tm);
        } else {
            libc::gmtime_r(&now, &mut tm);
        }
        tm
    }
}

/// e.g. 2026-09-25T10:35:00+00:00, like Python's datetime.isoformat(timespec="seconds").
pub fn utc_iso() -> String {
    let t = broken_down(false);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        t.tm_year + 1900,
        t.tm_mon + 1,
        t.tm_mday,
        t.tm_hour,
        t.tm_min,
        t.tm_sec
    )
}

/// Local HH:MM:SS for proxy.log.
pub fn local_hms() -> String {
    let t = broken_down(true);
    format!("{:02}:{:02}:{:02}", t.tm_hour, t.tm_min, t.tm_sec)
}

/// Ctrl-C belongs to Claude Code while it runs as our child.
pub fn ignore_sigint() {
    // SAFETY: installing SIG_IGN has no handler code to be unsafe.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
}
