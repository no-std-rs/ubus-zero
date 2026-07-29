use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

// AF_UNIX `sun_path` is 108 bytes on Linux but only 104 on macOS, and
// macOS `temp_dir()` resolves to `/var/folders/.../T/` (~50 bytes)
// before the filename, so the filename portion has to stay compact or
// `bind`/`connect` start failing with `ENAMETOOLONG`. Keep `label`
// short (≤16) and let the hash carry the per-process uniqueness.
const MAX_LABEL_LEN: usize = 16;

pub fn unique_socket_path(label: &str) -> PathBuf {
    let pid = rustix::process::getpid().as_raw_nonzero().get();
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut hasher = DefaultHasher::new();
    pid.hash(&mut hasher);
    ts.tv_sec.hash(&mut hasher);
    ts.tv_nsec.hash(&mut hasher);
    seq.hash(&mut hasher);
    let h = hasher.finish();
    let trimmed: &str = if label.len() > MAX_LABEL_LEN {
        &label[..MAX_LABEL_LEN]
    } else {
        label
    };
    std::env::temp_dir().join(format!("ub-{trimmed}-{h:016x}"))
}
