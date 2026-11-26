//! A coarse clock: milliseconds since the start (`crate::host::now_ms`'s
//! own), read from an atomic instead of asked of the system.
//!
//! Every `Instant::now` is a `QueryPerformanceCounter`, under Wine a full
//! call into its Unix side: some 22 ms a frame across the recording and
//! submitting threads in the perf recordings of the world, plus the time in
//! Wine's syscall dispatcher, most of it for ages that are looked at in
//! seconds (when a host copy, staging buffer or host texture was last used,
//! how long a placed texture's memory has been away, when a placeholder
//! reservation was last looked at).
//!
//! The atomic is brought up to date by a thread of its own every
//! [`TICK_MS`] milliseconds, and at every submission and every frame: it
//! goes on advancing while nothing is submitted (a title loading, a
//! debugger stop), so a time-out read from it still ends. It never goes
//! back, and it is behind the system's clock by at most a tick and the
//! ticking thread's wait to be run. Only for ages and time-outs of 100 ms
//! or more; durations that a line reports, and waits with a deadline, keep
//! the system's clock.
//!
//! `VICTORY_COARSE_CLOCK=0`: every read asks the system, as before (no
//! thread). Tests of the callers read the system's clock too, so that
//! their short waits (40 ms) are not at the mercy of a thread the test
//! process's other tests keep busy; this clock's own tests read it.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering::Relaxed};

/// How often the ticking thread brings the clock up to date. Every reader
/// counts in tenths of a second or more (`crate::host::moment`,
/// `resource::room_lately`, the placeholder looks' 250 ms, the descriptor
/// slots' seconds away), and every submission and frame ticks it as well:
/// 10 ms is a tenth of the finest of them. It was 2 ms, 500 wakeups a
/// second for nothing they could tell apart.
pub(crate) const TICK_MS: u64 = 10;

/// Sleeps `ms` milliseconds without a wineserver request under Wine.
///
/// `std::thread::sleep` waits on a high-resolution waitable timer it makes
/// for each sleep, and under Wine a timer is a server object: making,
/// setting and closing it are three wineserver requests a sleep
/// (NtCreateTimer, NtSetTimer, NtClose), each a round trip through the one
/// server thread that every thread's mapping and waiting also goes
/// through. `Sleep` is `NtDelayExecution` not alertable, which Wine's Unix
/// side waits out with `select` and no request (dlls/ntdll/unix/sync.c
/// `NtDelayExecution`, the same in Proton Experimental's ValveSoftware/wine
/// 5b557605 and Wine 11.22). Elsewhere `Sleep`'s default resolution is the
/// system tick (15.6 ms), so the standard library's sleep stays.
pub fn quiet_sleep(ms: u32) {
    if crate::host::on_wine() {
        // SAFETY: a plain call.
        unsafe { windows_sys::Win32::System::Threading::Sleep(ms) };
    } else {
        std::thread::sleep(std::time::Duration::from_millis(ms.into()));
    }
}

/// The clock: `crate::host::now_ms` as of the last tick, at least 1 once
/// ticked.
static MS: AtomicU64 = AtomicU64::new(0);

/// 0: not started; 1: ticking; 2: the system's clock is read (off, or the
/// thread could not be started).
static STATE: AtomicU8 = AtomicU8::new(0);

fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COARSE_CLOCK").is_none_or(|v| v != "0"))
}

/// Milliseconds since the start, as of at most a tick or so ago.
#[inline]
pub(crate) fn ms() -> u64 {
    #[cfg(test)]
    {
        crate::host::now_ms()
    }
    #[cfg(not(test))]
    {
        ticking_ms()
    }
}

/// [`ms`] from the atomic (the ticking thread started at the first read).
#[inline]
fn ticking_ms() -> u64 {
    match STATE.load(Relaxed) {
        1 => MS.load(Relaxed),
        2 => crate::host::now_ms(),
        _ => start(),
    }
}

/// Brings the clock up to date (a submission, a frame, the ticking
/// thread); the clock's value after.
pub(crate) fn tick() -> u64 {
    if STATE.load(Relaxed) != 1 {
        return 0;
    }
    up_to_date()
}

fn up_to_date() -> u64 {
    let now = crate::host::now_ms().max(1);
    MS.fetch_max(now, Relaxed).max(now)
}

/// Starts the ticking thread (once, whoever reads first); the clock's value.
#[cold]
fn start() -> u64 {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if !on() {
            STATE.store(2, Relaxed);
            return;
        }
        // Up to date before any reader sees the state ticking.
        up_to_date();
        let started = std::thread::Builder::new().name("victory-clock".into()).spawn(|| {
            loop {
                quiet_sleep(TICK_MS as u32);
                up_to_date();
            }
        });
        STATE.store(if started.is_ok() { 1 } else { 2 }, Relaxed);
        if let Err(e) = started {
            tracing::warn!(error = %e, "the coarse clock's thread was not started: the system's clock is read each time");
        }
    });
    if STATE.load(Relaxed) == 1 { MS.load(Relaxed) } else { crate::host::now_ms() }
}

#[cfg(test)]
mod tests {
    use super::{TICK_MS, tick, ticking_ms};

    /// The clock advances by itself, with nothing ticking it but its own
    /// thread; it never goes back, and it stays close behind the system's
    /// clock (a generous bound: the test process's other tests keep the
    /// processor busy).
    #[test]
    fn the_coarse_clock_advances_by_itself_and_stays_close() {
        let first = ticking_ms();
        assert!(first >= 1);
        let mut last = first;
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(5));
            let now = ticking_ms();
            assert!(now >= last, "never back: {now} after {last}");
            let exact = crate::host::now_ms();
            assert!(now <= exact, "never ahead of the system's clock");
            assert!(exact - now <= 100 + TICK_MS, "{} ms behind", exact - now);
            last = now;
        }
        assert!(last >= first + 100, "advanced {} ms over 200 ms of sleeps", last - first);
        // A tick brings it up to date at once.
        let ticked = tick();
        assert!(ticked >= last && ticked <= crate::host::now_ms());
        assert!(ticking_ms() >= ticked);
    }
}
