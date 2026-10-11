//! Parks every reader thread off its descriptor, so the server can exec
//! without dropping bytes a reader took but never delivered.

use std::os::fd::AsFd;
use std::sync::{PoisonError, RwLock, RwLockWriteGuard};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};

static PARKED: RwLock<()> = RwLock::new(());

/// How long a reader with nothing else to do waits on its descriptor
/// before letting a park through.
pub const WAIT: Duration = Duration::from_millis(100);

/// Runs `read` once `fd` is readable. `None` when `timeout` passed first.
pub fn when_readable<T>(
    fd: impl AsFd,
    timeout: Duration,
    read: impl FnOnce() -> T,
) -> rustix::io::Result<Option<T>> {
    let _running = PARKED.read().unwrap_or_else(PoisonError::into_inner);
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let timeout = Timespec {
        tv_sec: timeout.as_secs() as _,
        tv_nsec: timeout.subsec_nanos() as _,
    };
    if poll(&mut fds, Some(&timeout))? == 0 {
        return Ok(None);
    }
    Ok(Some(read()))
}

/// Returns once no read is in flight, and keeps every reader parked while
/// the guard lives.
pub fn all() -> RwLockWriteGuard<'static, ()> {
    PARKED.write().unwrap_or_else(PoisonError::into_inner)
}
