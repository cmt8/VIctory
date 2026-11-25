//! Under Wine, each host pipeline is made on a thread of its own, which
//! the worker waits for: a thread the host's compiler ends does not take
//! the worker with it, and its pipeline is left out instead of being
//! "made" for ever.
//!
//! Run 13 (2 MiB worker stacks) and run 14 (16 MiB) lost the same three
//! compute pipelines: the stack overflowed some 15 ms after their shaders
//! were translated, inside `CreateComputePipelineState` (vkd3d-proton
//! compiles DXIL on the calling thread). Our overflow handler then ran out
//! of the last page (`virtual_setup_exception stack overflow 3456 bytes`)
//! and Wine ended the thread. The three stayed "being made", and every
//! title frame that used one of them waited the whole pipeline budget for
//! it (`make::wait`): 500 ms of each 600 ms frame in run 14, the title's
//! recording threads standing in `make::DONE` and the rest of the process
//! idle meanwhile.
//!
//! The thread has a large stack (`VICTORY_PIPELINE_STACK_MB`, 256 MiB; only
//! the pages used are committed), so a compile that only recurses deeply
//! completes. One that overflows even that is ended here: a first-chance
//! handler takes the overflow on such a thread, keeps where it happened and
//! what lies on the stack, and ends the thread at once (the standard
//! library's handler would print and let the overflow end the process).
//! What the thread held (the compiler's memory) is lost with it, as when
//! Wine ended the worker.
//!
//! On Windows the pipeline is made on the worker as before: an overflow
//! there ends the process whatever thread it happens on.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{EXCEPTION_STACK_OVERFLOW, HANDLE, NTSTATUS};
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, GetThreadId, INFINITE, TerminateThread,
    WaitForSingleObject,
};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// The exit code of a maker thread ended by its overflow.
const OVERFLOWED: u32 = EXCEPTION_STACK_OVERFLOW as u32;

/// The maker thread's stack.
fn stack() -> usize {
    static MB: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MB.get_or_init(|| {
        std::env::var("VICTORY_PIPELINE_STACK_MB")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&mb| mb >= 1)
            .unwrap_or(256)
    }) << 20
}

/// Whether pipelines are made apart: under Wine, unless
/// `VICTORY_PIPELINE_APART=0`.
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        crate::host::on_wine()
            && std::env::var_os("VICTORY_PIPELINE_APART").is_none_or(|v| v != "0")
    })
}

/// Threads making a pipeline now (their ids; 0 is a free place).
static MAKERS: [AtomicU32; 32] = [const { AtomicU32::new(0) }; 32];

/// The qwords kept from an overflowing maker's stack, from its stack
/// pointer up.
const KEPT_WORDS: usize = 2048;

/// What the overflow handler kept: the thread it was (0: nothing kept, or
/// taken), the instruction and the stack pointer, then [`KEPT_WORDS`]
/// qwords of the stack. One overflow at a time; another one meanwhile is
/// ended without being kept.
struct Kept {
    thread: AtomicU32,
    at: AtomicU64,
    sp: AtomicU64,
    words: [AtomicU64; KEPT_WORDS],
}

static KEPT: Kept = Kept {
    thread: AtomicU32::new(0),
    at: AtomicU64::new(0),
    sp: AtomicU64::new(0),
    words: [const { AtomicU64::new(0) }; KEPT_WORDS],
};

/// Takes a stack overflow on a maker thread: keeps where it was and ends
/// the thread. Little stack is left here (the guarantee the standard
/// library asks for its threads), so nothing is formatted or allocated.
unsafe extern "system" fn on_overflow(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the system passes the exception being dispatched.
    let (record, context) = unsafe { (&*(*info).ExceptionRecord, (*info).ContextRecord) };
    if record.ExceptionCode != EXCEPTION_STACK_OVERFLOW as NTSTATUS {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // SAFETY: a plain query.
    let me = unsafe { GetCurrentThreadId() };
    if !MAKERS.iter().any(|m| m.load(Ordering::Relaxed) == me) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    if KEPT
        .thread
        .compare_exchange(0, me, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
    {
        KEPT.at
            .store(record.ExceptionAddress as u64, Ordering::Relaxed);
        if !context.is_null() {
            // SAFETY: the context of the faulting thread, this one.
            let sp = unsafe { (*context).Rsp } as usize;
            KEPT.sp.store(sp as u64, Ordering::Relaxed);
            // From the stack pointer up is the stack the thread used (the
            // callers' frames), committed.
            for (i, word) in KEPT.words.iter().enumerate() {
                // SAFETY: inside this thread's committed stack (see above);
                // its top is far above what is read.
                let value = unsafe { ((sp + i * 8) as *const u64).read_volatile() };
                word.store(value, Ordering::Relaxed);
            }
        }
        KEPT.thread.store(me, Ordering::Release);
    }
    // SAFETY: ends this thread only; its frames are not unwound (they hold
    // nothing of ours but the result slot, which stays empty).
    unsafe { TerminateThread(GetCurrentThread(), OVERFLOWED) };
    EXCEPTION_CONTINUE_SEARCH
}

/// Stack kept for the overflow handler on a maker thread.
///
/// Wine commits a thread's whole stack and puts one guard page above the
/// last (no-access) page; the overflow is raised when that guard page is
/// touched, with the stack pointer some 8 KiB above the bottom, whatever
/// guarantee the thread asked for. The exception's frame alone, with the
/// processor's extended state, did not fit there: Wine ended the thread
/// before any handler ran (`virtual_setup_exception: stack overflow 3840
/// bytes`, in this module's test; 3456 bytes inside the standard library's
/// handler in runs 13 and 14). So a maker asks for this guarantee and puts
/// a guard page of its own just above it: Wine takes a touched guard page
/// in a stack for the stack growing, moves the guard down a page, and
/// raises the overflow when the guard reaches the guaranteed pages, which
/// leaves them to the handler.
const GUARANTEE: usize = 64 << 10;

/// Makes this thread a maker: its overflow is taken by [`on_overflow`],
/// with [`GUARANTEE`] of stack to run in. Its place among the makers, to
/// give back when the work is done.
fn enlist() -> Option<&'static AtomicU32> {
    use windows_sys::Win32::System::Memory::{PAGE_GUARD, PAGE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::{
        GetCurrentThreadStackLimits, SetThreadStackGuarantee,
    };
    const PAGE: usize = 4096;
    let mut size = GUARANTEE as u32;
    let (mut low, mut high) = (0usize, 0usize);
    // SAFETY: plain calls on this thread; the page made a guard page lies
    // in its own stack, far below what it has used yet.
    unsafe {
        if SetThreadStackGuarantee(&mut size) != 0 {
            GetCurrentThreadStackLimits(&mut low, &mut high);
            let guard = low + PAGE + GUARANTEE;
            if low != 0 && guard + 64 * PAGE < high {
                let mut old = 0;
                VirtualProtect(
                    guard as *const _,
                    PAGE,
                    PAGE_READWRITE | PAGE_GUARD,
                    &mut old,
                );
            }
        }
    }
    // SAFETY: a plain query.
    let me = unsafe { GetCurrentThreadId() };
    MAKERS.iter().find(|m| {
        m.compare_exchange(0, me, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    })
}

fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only reads the exception and this thread's stack.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_overflow));
    });
}

/// A made pipeline, handed back from the maker thread.
struct Made(Option<HostPipeline>);
// SAFETY: D3D12 objects are free-threaded; the rest is plain data.
unsafe impl Send for Made {}

/// What [`make`] is given, for the maker thread.
struct Job(Vec<Arc<Shader>>, Option<*const GraphicsState>);
// SAFETY: the graphics state is only read, and the worker that owns it
// waits until the maker thread has ended.
unsafe impl Send for Job {}

/// Makes the host pipeline on a thread of its own and waits for it; `None`
/// (a pipeline left out) if the thread ended without making it.
pub(crate) fn make(
    shaders: &[Arc<Shader>],
    graphics: Option<&GraphicsState>,
) -> Option<HostPipeline> {
    install();
    let slot: Arc<Mutex<Option<Made>>> = Arc::new(Mutex::new(None));
    let job = Job(shaders.to_vec(), graphics.map(std::ptr::from_ref));
    let out = slot.clone();
    let spawned = std::thread::Builder::new()
        .name("victory-pipeline-make".into())
        .stack_size(stack())
        .spawn(move || {
            let job = job;
            let place = enlist();
            // SAFETY: the worker keeps the state alive until this thread ends.
            let made = HostPipeline::new(&job.0, job.1.map(|g| unsafe { &*g }));
            if let Some(place) = place {
                place.store(0, Ordering::Release);
            }
            *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(Made(made));
        });
    let handle = match spawned {
        Ok(handle) => handle,
        Err(e) => {
            tracing::warn!(error = %e, "no thread to make a host pipeline on: made on the worker");
            return HostPipeline::new(shaders, graphics);
        }
    };
    let thread = {
        use std::os::windows::io::AsRawHandle;
        let raw = handle.as_raw_handle() as HANDLE;
        // The thread's end, not `join`: a thread the system ended has no
        // result and `join` panics for it.
        // SAFETY: the thread's own handle, open while `handle` lives.
        unsafe {
            WaitForSingleObject(raw, INFINITE);
            GetThreadId(raw)
        }
    };
    let made = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    match made {
        Some(Made(made)) => {
            let _ = handle.join();
            made
        }
        None => {
            // Detached: the handle is closed, what the thread held is lost.
            drop(handle);
            ended(shaders, thread);
            None
        }
    }
}

/// Maker threads ended without a pipeline since the start.
pub(crate) static ENDED: AtomicU64 = AtomicU64::new(0);

/// A maker thread ended without a pipeline: says which and, if the handler
/// kept it, where it overflowed and which return addresses repeat on its
/// stack (a recursion shows as the same few addresses many times).
fn ended(shaders: &[Arc<Shader>], thread: u32) {
    ENDED.fetch_add(1, Ordering::Relaxed);
    // Its place among the makers, if the handler ended it inside the work.
    for m in &MAKERS {
        let _ = m.compare_exchange(thread, 0, Ordering::AcqRel, Ordering::Relaxed);
    }
    if thread == 0 || KEPT.thread.load(Ordering::Acquire) != thread {
        tracing::warn!(
            ?shaders,
            stack_mb = stack() >> 20,
            "a host pipeline's maker thread ended without making it (not by a stack overflow we saw): the pipeline is left out"
        );
        return;
    }
    let at = KEPT.at.load(Ordering::Relaxed) as usize;
    let words: Vec<usize> = KEPT
        .words
        .iter()
        .map(|w| w.load(Ordering::Relaxed) as usize)
        .collect();
    KEPT.thread.store(0, Ordering::Release);
    tracing::warn!(
        ?shaders,
        stack_mb = stack() >> 20,
        at = %place(at),
        repeated = %repeated(&words),
        "a host pipeline's maker thread overflowed its stack: the pipeline is left out (the top of the stack follows as module+offset ×count)"
    );
}

/// `module+offset` for a code address, or the address.
fn place(at: usize) -> String {
    match module_of(at) {
        Some((name, base)) => format!("{name}+{:#x}", at - base),
        None => format!("{at:#x}"),
    }
}

/// The loaded module `at` lies in: its file name and base.
fn module_of(at: usize) -> Option<(String, usize)> {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };
    if at < 0x10000 {
        return None;
    }
    let mut module = std::ptr::null_mut();
    // SAFETY: a lookup by address; the reference count is left alone.
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            at as *const u16,
            &mut module,
        )
    } != 0;
    if !found || module.is_null() {
        return None;
    }
    let mut name = [0u16; 260];
    // SAFETY: room for `name.len()` characters.
    let n = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) } as usize;
    let path = String::from_utf16_lossy(&name[..n.min(name.len())]);
    let file = path.rsplit(['\\', '/']).next().unwrap_or(&path).to_owned();
    Some((file, module as usize))
}

/// The words of the stack that point into a loaded module, by how often
/// each occurs, most first (the eight most frequent).
fn repeated(words: &[usize]) -> String {
    let mut counts: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for &w in words {
        *counts.entry(w).or_default() += 1;
    }
    let mut found: Vec<(String, usize)> = counts
        .into_iter()
        .filter(|&(w, n)| n >= 2 && w >= 0x10000)
        .filter_map(|(w, n)| {
            module_of(w).map(|(name, base)| (format!("{name}+{:#x}", w - base), n))
        })
        .collect();
    found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    found.truncate(8);
    if found.is_empty() {
        return "nothing that points into a module repeats".into();
    }
    found
        .iter()
        .map(|(p, n)| format!("{p} ×{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Recurses for ever, keeping a frame per call.
    #[inline(never)]
    #[allow(unconditional_recursion)]
    fn deeper(n: u64) -> u64 {
        let pad = [n; 64];
        std::hint::black_box(&pad);
        deeper(n + 1) + pad[3]
    }

    /// A thread registered as a maker that overflows its stack is ended
    /// by the handler, without taking the process with it, and what it
    /// kept names this test's own code as the repeating frames.
    #[test]
    fn a_maker_thread_that_overflows_is_ended_and_kept() {
        install();
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let handle = std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || {
                assert!(enlist().is_some());
                std::hint::black_box(deeper(0));
                flag.store(true, Ordering::SeqCst);
            })
            .unwrap();
        {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: the thread's handle, open while `handle` lives.
            unsafe { WaitForSingleObject(handle.as_raw_handle() as HANDLE, INFINITE) };
        }
        assert!(
            !done.load(Ordering::SeqCst),
            "the recursion cannot have returned"
        );
        let thread = KEPT.thread.load(Ordering::Acquire);
        assert_ne!(thread, 0, "the overflow was kept");
        // The maker's place is left taken: it ended inside the work.
        for m in &MAKERS {
            let _ = m.compare_exchange(thread, 0, Ordering::AcqRel, Ordering::Relaxed);
        }
        let words: Vec<usize> = KEPT
            .words
            .iter()
            .map(|w| w.load(Ordering::Relaxed) as usize)
            .collect();
        let at = KEPT.at.load(Ordering::Relaxed) as usize;
        KEPT.thread.store(0, Ordering::Release);
        let text = repeated(&words);
        eprintln!("overflow at {}; repeated: {text}", place(at));
        assert!(
            text.contains(".exe+"),
            "repeating frames in the test executable: {text}"
        );
        assert!(
            module_of(at).is_some(),
            "the overflow happened in a module: {}",
            place(at)
        );
        drop(handle);
    }
}
