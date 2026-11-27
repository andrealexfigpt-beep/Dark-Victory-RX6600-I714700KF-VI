//! Overrides of shared Windows APIs whose console meaning differs.
//!
//! Only the differences live here; everything else passes through to the
//! host (see `HostPolicy`).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, c_char, c_void};

use windows_sys::Win32::System::SystemInformation::SYSTEM_INFO;
use windows_sys::Win32::System::Threading::{CreateThread, GetCurrentThread, GetCurrentThreadId, GetThreadPriority, SetThreadPriority};

use crate::{HleModule, HostPolicy, hle_export, registry, runtime};

/// Extended SYSTEM_INFO for titles that expect a specific layout.
#[repr(C)]
pub struct SYSTEM_INFO_EX {
    pub wProcessorArchitecture: u16,
    pub wReserved: u16,
    pub dwPageSize: u32,
    pub lpMinimumApplicationAddress: *mut c_void,
    pub lpMaximumApplicationAddress: *mut c_void,
    pub dwActiveProcessorMask: usize,
    pub dwNumberOfProcessors: u32,
    pub dwProcessorType: u32,
    pub dwAllocationGranularity: u32,
    pub wProcessorLevel: u16,
    pub wProcessorRevision: u16,
}

impl From<SYSTEM_INFO> for SYSTEM_INFO_EX {
    fn from(info: SYSTEM_INFO) -> Self {
        SYSTEM_INFO_EX {
            wProcessorArchitecture: info.wProcessorArchitecture,
            wReserved: info.wReserved,
            dwPageSize: info.dwPageSize,
            lpMinimumApplicationAddress: info.lpMinimumApplicationAddress,
            lpMaximumApplicationAddress: info.lpMaximumApplicationAddress,
            dwActiveProcessorMask: info.dwActiveProcessorMask as usize,
            dwNumberOfProcessors: info.dwNumberOfProcessors,
            dwProcessorType: info.dwProcessorType,
            dwAllocationGranularity: info.dwAllocationGranularity,
            wProcessorLevel: info.wProcessorLevel,
            wProcessorRevision: info.wProcessorRevision,
        }
    }
}

/// `GetModuleHandleW(NULL)` must return the title's image, not our exe;
/// title images and our console modules are found by name.
pub extern "system" fn GetModuleHandleW(name: *const u16) -> *mut c_void {
    if name.is_null() {
        return runtime::main_image().unwrap_or(0) as *mut c_void;
    }
    find_module(&wide(name), false).unwrap_or_else(|| host::get_module_handle_w(name))
}

/// See [`GetModuleHandleW`].
pub extern "system" fn GetModuleHandleA(name: *const u8) -> *mut c_void {
    if name.is_null() {
        return runtime::main_image().unwrap_or(0) as *mut c_void;
    }
    find_module(&narrow(name), false).unwrap_or_else(|| host::get_module_handle_a(name))
}

/// A NUL-terminated string from the title.
fn narrow(text: *const u8) -> String {
    // SAFETY: the title passes a NUL-terminated string.
    unsafe { CStr::from_ptr(text.cast()) }
        .to_string_lossy()
        .into_owned()
}

/// See [`narrow`].
fn wide(text: *const u16) -> String {
    // SAFETY: the title passes a NUL-terminated string.
    unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    }
}

/// A title image's path is its original file, not our cache copy;
/// `NULL` means the title's exe.
pub extern "system" fn GetModuleFileNameW(module: *mut c_void, out: *mut u16, size: u32) -> u32 {
    let Some(path) = title_path(module) else {
        return host::get_module_file_name_w(module, out, size);
    };
    let wide: Vec<u16> = path.encode_utf16().collect();
    copy_truncated(&wide, title_buffer(out, size))
}

/// See [`GetModuleFileNameW`].
pub extern "system" fn GetModuleFileNameA(module: *mut c_void, out: *mut u8, size: u32) -> u32 {
    let Some(path) = title_path(module) else {
        return host::get_module_file_name_a(module, out, size);
    };
    copy_truncated(path.as_bytes(), title_buffer(out, size))
}

/// The title's buffer of `size` elements; none for a null pointer (a slice
/// may not start at null, even an empty one, so `(NULL, 0)` would be
/// undefined behaviour).
fn title_buffer<'a, T>(out: *mut T, size: u32) -> &'a mut [T] {
    if out.is_null() {
        return &mut [];
    }
    // SAFETY: the title passes a buffer of `size` elements.
    unsafe { std::slice::from_raw_parts_mut(out, size as usize) }
}

fn title_path(module: *mut c_void) -> Option<String> {
    let base = if module.is_null() {
        runtime::main_image()?
    } else {
        module as usize
    };
    Some(runtime::image_path(base)?.to_string_lossy().into_owned())
}

/// `GetModuleFileName`'s contract: copy with a NUL; if it does not fit,
/// truncate to the buffer, report `ERROR_INSUFFICIENT_BUFFER` and return
/// the buffer size.
fn copy_truncated<T: Copy + Default>(text: &[T], out: &mut [T]) -> u32 {
    let Some(room) = out.len().checked_sub(1) else {
        host::set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    };
    let n = text.len().min(room);
    out[..n].copy_from_slice(&text[..n]);
    out[n] = T::default();
    if n < text.len() {
        host::set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return out.len() as u32;
    }
    n as u32
}

const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

/// Console-only DLLs we implement load as a pseudo handle (titles load
/// `xgameruntime.dll` this way), the title's DLLs as the images we map
/// (on first use if nothing imports them); anything else loads on the host.
pub extern "system" fn LoadLibraryExA(
    name: *const u8,
    file: *mut c_void,
    flags: u32,
) -> *mut c_void {
    if !name.is_null()
        && let Some(handle) = find_module(&narrow(name), runs_code(flags))
    {
        return handle;
    }
    host::load_library_ex_a(name, file, flags)
}

/// See [`LoadLibraryExA`].
pub extern "system" fn LoadLibraryExW(
    name: *const u16,
    file: *mut c_void,
    flags: u32,
) -> *mut c_void {
    if !name.is_null()
        && let Some(handle) = find_module(&wide(name), runs_code(flags))
    {
        return handle;
    }
    host::load_library_ex_w(name, file, flags)
}

/// False for loads as data or resources (`DONT_RESOLVE_DLL_REFERENCES`,
/// `LOAD_LIBRARY_AS_DATAFILE(_EXCLUSIVE)`, `AS_IMAGE_RESOURCE`): those run
/// no code, so a title DLL we have not mapped can go to the host.
fn runs_code(flags: u32) -> bool {
    flags & (0x1 | 0x2 | 0x20 | 0x40) == 0
}

pub extern "system" fn LoadLibraryA(name: *const u8) -> *mut c_void {
    LoadLibraryExA(name, std::ptr::null_mut(), 0)
}

pub extern "system" fn LoadLibraryW(name: *const u16) -> *mut c_void {
    LoadLibraryExW(name, std::ptr::null_mut(), 0)
}

/// Looks up our implementations behind a pseudo handle; real modules go to
/// the host.
pub extern "system" fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void {
    // `NULL` is the process's exe, which for the title is its own image.
    let module = if module.is_null() {
        runtime::main_image().unwrap_or(0) as *mut c_void
    } else {
        module
    };
    let Some(m) = registry().iter().find(|m| handle(m) == module) else {
        return host_override(module, name).unwrap_or_else(|| host::get_proc_address(module, name));
    };
    // Values below 0x10000 are ordinals; we export by name only.
    if (name as usize) < 0x1_0000 {
        tracing::warn!(
            module = m.name,
            ordinal = name as usize,
            "GetProcAddress by ordinal"
        );
        return std::ptr::null_mut();
    }
    // SAFETY: not an ordinal, so the title passes a NUL-terminated name.
    let name = unsafe { CStr::from_ptr(name.cast()) }.to_string_lossy();
    match m.exports.iter().find(|e| e.name == name) {
        Some(e) => e.address as *mut c_void,
        None => {
            tracing::warn!(module = m.name, %name, "GetProcAddress: not implemented");
            std::ptr::null_mut()
        }
    }
}

/// What a host module's `name` (or ordinal) is bound to at import binding
/// when we override it: a title that looks a function up by name must not
/// get past our overrides (the network guard, path rewriting).
fn host_override(module: *mut c_void, name: *const u8) -> Option<*mut c_void> {
    if module.is_null() || runtime::image_path(module as usize).is_some() {
        return None;
    }
    let symbol = if (name as usize) < 0x1_0000 {
        format!("#{}", name as usize)
    } else {
        narrow(name)
    };
    let mut path = [0u16; 260];
    let len = host::get_module_file_name_w(module, path.as_mut_ptr(), path.len() as u32) as usize;
    let path = String::from_utf16_lossy(&path[..len.min(path.len())]);
    let file = path.rsplit('\\').next()?;
    crate::registry::lookup(file, &symbol).map(|address| address as *mut c_void)
}

/// Our pseudo modules and the title's images stay loaded: `LoadLibrary`
/// took no reference on them, so the host's count must not drop.
pub extern "system" fn FreeLibrary(module: *mut c_void) -> i32 {
    if registry().iter().any(|m| handle(m) == module)
        || runtime::image_path(module as usize).is_some()
    {
        return 1;
    }
    host::free_library(module)
}

/// Title images hear of thread starts and ends from us, not from the
/// Windows loader (`victory_loader::disable_thread_calls`).
pub extern "system" fn DisableThreadLibraryCalls(module: *mut c_void) -> i32 {
    host::disable_thread_library_calls(module)
}

/// Title debug output goes to our log (no debugger needs to be attached).
pub extern "system" fn OutputDebugStringA(text: *const u8) {
    if !text.is_null() {
        let text = narrow(text);
        host::note_title_output(text.trim());
        tracing::info!(target: "title", "{}", text.trim_end());
    }
}

/// See [`OutputDebugStringA`].
pub extern "system" fn OutputDebugStringW(text: *const u16) {
    if !text.is_null() {
        let text = wide(text);
        host::note_title_output(text.trim());
        tracing::info!(target: "title", "{}", text.trim_end());
    }
}

/// The CRT's `exit`, through which the title's entry point returns: logs
/// the code and the caller, then exits on the host.
pub extern "C" fn exit(code: i32) -> ! {
    tracing::warn!(code, from = %host::callers(), "title exits");
    host::exit(code)
}

/// The CRT's `abort` (also reached from `terminate`): logged like [`exit`].
pub extern "C" fn abort() -> ! {
    tracing::error!(from = %host::callers(), "title aborts");
    host::abort()
}

/// The CRT's `terminate` and the STL's invalid-argument exit: logged, then
/// the host's `abort`.
pub extern "C" fn terminate() -> ! {
    tracing::error!(from = %host::callers(), "title calls terminate");
    host::abort()
}

/// See [`terminate`].
pub extern "C" fn _invalid_parameter_noinfo_noreturn() -> ! {
    tracing::error!(from = %host::callers(), "title passed an invalid parameter");
    host::abort()
}

/// The title's unhandled-exception filter. Ours stands in front of it and
/// reports the crash first; the title still gets its own back.
static TITLE_FILTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub extern "system" fn SetUnhandledExceptionFilter(filter: usize) -> usize {
    host::set_unhandled_exception_filter(report_unhandled);
    TITLE_FILTER.swap(filter, std::sync::atomic::Ordering::AcqRel)
}

extern "system" fn report_unhandled(info: *const c_void) -> i32 {
    host::report_unhandled(info);
    match TITLE_FILTER.load(std::sync::atomic::Ordering::Acquire) {
        0 => 0, // EXCEPTION_CONTINUE_SEARCH
        filter => {
            type Filter = extern "system" fn(*const c_void) -> i32;
            // SAFETY: the title registered this filter.
            let filter: Filter = unsafe { std::mem::transmute(filter) };
            filter(info)
        }
    }
}

/// See [`exit`]; only the current process is logged.
pub extern "system" fn TerminateProcess(process: *mut c_void, code: u32) -> i32 {
    if process as isize == -1 {
        tracing::warn!(code, from = %host::callers(), "title terminates the process");
    }
    host::terminate_process(process, code)
}

/// The pseudo handle of one of our modules: its address, never a real image.
fn handle(m: &HleModule) -> *mut c_void {
    m as *const HleModule as *mut c_void
}

/// The title's images and our console modules by name; `None` leaves the
/// name to the host, which only DLLs `HostPolicy` passes through get (as at
/// import binding); others fail. With `load`, a title DLL not mapped yet is
/// mapped now.
fn find_module(path: &str, load: bool) -> Option<*mut c_void> {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    // As the Windows loader does, a name without extension means a DLL.
    let file = if file.contains('.') {
        file.to_owned()
    } else {
        format!("{file}.dll")
    };
    // Loads go through the title DLL loader first: it waits for a DLL that
    // another thread is still initialising, as the Windows loader lock does.
    if load {
        match runtime::load_title_dll(&file) {
            Ok(Some(base)) => return Some(base as *mut c_void),
            Ok(None) => {}
            // Not the host: it would load the unpatched DLL from the title's folder.
            Err(error) => {
                tracing::error!(dll = file, %error, "mapping a title DLL failed");
                host::set_last_error(ERROR_MOD_NOT_FOUND);
                return Some(std::ptr::null_mut());
            }
        }
    }
    if let Some(base) = runtime::image_named(&file) {
        return Some(base as *mut c_void);
    }
    // DLLs the host has too are real host modules, whatever `--strict` says.
    let host_dll = HostPolicy::default().allows(&file);
    if !host_dll && let Some(m) = registry().iter().find(|m| m.serves(&file)) {
        if load {
            tracing::info!(dll = file, "loaded as a pseudo module");
        }
        return Some(handle(m));
    }
    if host_dll {
        return None;
    }
    // Any other host DLL (wininet, urlmon, ...) is beyond what we guard, and
    // the console does not have it.
    refuse(&file);
    host::set_last_error(ERROR_MOD_NOT_FOUND);
    Some(std::ptr::null_mut())
}

/// Logs a DLL name the title is refused, once.
fn refuse(file: &str) {
    static REFUSED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let file = file.to_ascii_lowercase();
    let mut refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    if !refused.contains(&file) {
        tracing::warn!(dll = file, "a host DLL outside the host policy: refused");
        refused.push(file);
    }
}

const ERROR_MOD_NOT_FOUND: u32 = 126;

/// The title's argument list: the program, then what was given after `--`.
/// The host's own would be ours (`victory run <exe> ...`), and the title
/// looks at fixed places in it (`-skipToIIS` only as the first argument).
struct TitleArgv {
    count: std::cell::UnsafeCell<i32>,
    list: std::cell::UnsafeCell<*mut *mut c_char>,
}

// SAFETY: never written here after it was made; the title's C runtime start
// reads it once, on one thread.
unsafe impl Send for TitleArgv {}
// SAFETY: as above.
unsafe impl Sync for TitleArgv {}

/// Kept for the whole run: the title holds on to the pointers.
fn title_argv(program: &str, args: &[String]) -> TitleArgv {
    let list: Vec<*mut c_char> = std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .filter_map(|a| std::ffi::CString::new(a).ok())
        .map(std::ffi::CString::into_raw)
        .collect();
    let count = list.len() as i32;
    // `argv[argc]` is null.
    let list: Box<[*mut c_char]> = list.into_iter().chain(Some(std::ptr::null_mut())).collect();
    TitleArgv {
        count: count.into(),
        list: Box::leak(list).as_mut_ptr().into(),
    }
}

fn argv() -> &'static TitleArgv {
    static ARGV: std::sync::OnceLock<TitleArgv> = std::sync::OnceLock::new();
    ARGV.get_or_init(|| {
        // The program's name stays what the host says (as it was before).
        let program = std::env::args_os().next().unwrap_or_default();
        let args = runtime::title_args();
        tracing::info!(?args, "the title's arguments");
        title_argv(&program.to_string_lossy(), args)
    })
}

/// See [`TitleArgv`].
pub extern "C" fn __p___argc() -> *mut i32 {
    argv().count.get()
}

/// See [`TitleArgv`].
pub extern "C" fn __p___argv() -> *mut *mut *mut c_char {
    argv().list.get()
}

/// The title's thread priorities, but for the lowest two, which become
/// "below normal". Six of the title's threads run at "idle", the streaming
/// pager's among them; on the console they have what the rest of the title
/// leaves of its cores. A PC's processors are also wanted by other programs
/// and by our own workers (sixteen when pipelines are made): with every
/// one of them busy such a thread ran once in four seconds or so, when the
/// host's scheduler took pity, and the loading that waits for it with it
/// (run stream-3: loads seen to be done 1.1 s late on average, 4.8 s at
/// worst). `VICTORY_TITLE_PRIORITIES=0`: as the title sets them.
fn title_priority(asked: i32) -> i32 {
    const IDLE: i32 = -15;
    const LOWEST: i32 = -2;
    const BELOW_NORMAL: i32 = -1;
    match asked {
        IDLE..=LOWEST => BELOW_NORMAL,
        other => other,
    }
}

/// [`title_priority`].
pub extern "system" fn SetThreadPriority(thread: *mut c_void, priority: i32) -> i32 {
    static AS_SET: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let as_set = *AS_SET.get_or_init(|| std::env::var_os("VICTORY_TITLE_PRIORITIES").is_some_and(|v| v == "0"));
    let priority = if as_set { priority } else { title_priority(priority) };
    // SAFETY: the title passes a thread handle.
    unsafe { windows_sys::Win32::System::Threading::SetThreadPriority(thread, priority) }
}

/// GetThreadPriority: retrieve thread priority.
pub extern "system" fn GetThreadPriority(thread: *mut c_void) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::GetThreadPriority(thread) }
}

/// SetThreadPriorityBoost: disable/enable priority boost.
pub extern "system" fn SetThreadPriorityBoost(thread: *mut c_void, disable_priority_boost: i32) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::SetThreadPriorityBoost(thread, disable_priority_boost) }
}

/// GetThreadPriorityBoost: check if priority boost is disabled.
pub extern "system" fn GetThreadPriorityBoost(thread: *mut c_void, disable_priority_boost: *mut i32) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::GetThreadPriorityBoost(thread, disable_priority_boost) }
}

/// SetProcessPriorityBoost: disable/enable process priority boost.
pub extern "system" fn SetProcessPriorityBoost(process: *mut c_void, disable_priority_boost: i32) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::SetProcessPriorityBoost(process, disable_priority_boost) }
}

/// GetProcessPriorityBoost: check if process priority boost is disabled.
pub extern "system" fn GetProcessPriorityBoost(process: *mut c_void, disable_priority_boost: *mut i32) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::GetProcessPriorityBoost(process, disable_priority_boost) }
}

/// SetThreadIdealProcessor: set ideal processor for thread.
pub extern "system" fn SetThreadIdealProcessor(thread: *mut c_void, processor: u32) -> u32 {
    unsafe { windows_sys::Win32::System::Threading::SetThreadIdealProcessor(thread, processor) }
}

/// GetThreadIdealProcessorEx: get ideal processor for thread.
pub extern "system" fn GetThreadIdealProcessorEx(thread: *mut c_void, ideal_processor: *mut u32) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::GetThreadIdealProcessorEx(thread, ideal_processor) }
}

/// SetThreadAffinityMask: set thread affinity.
pub extern "system" fn SetThreadAffinityMask(thread: *mut c_void, affinity_mask: usize) -> usize {
    unsafe { windows_sys::Win32::System::Threading::SetThreadAffinityMask(thread, affinity_mask) }
}

/// GetProcessAffinityMask: get process affinity mask.
pub extern "system" fn GetProcessAffinityMask(process: *mut c_void, process_affinity_mask: *mut usize, system_affinity_mask: *mut usize) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::GetProcessAffinityMask(process, process_affinity_mask, system_affinity_mask) }
}

/// SwitchToThread: yield to another thread.
pub extern "system" fn SwitchToThread() -> i32 {
    unsafe { windows_sys::Win32::System::Threading::SwitchToThread() }
}

/// Sleep: sleep for specified milliseconds.
pub extern "system" fn Sleep(milliseconds: u32) {
    unsafe { windows_sys::Win32::System::Threading::Sleep(milliseconds) }
}

/// SleepEx: alertable sleep.
pub extern "system" fn SleepEx(milliseconds: u32, alertable: i32) -> u32 {
    unsafe { windows_sys::Win32::System::Threading::SleepEx(milliseconds, alertable) }
}

/// CreateEventA: create event object.
pub extern "system" fn CreateEventA(
    security: *mut c_void,
    manual_reset: i32,
    initial_state: i32,
    name: *const u8,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateEventA(security, manual_reset, initial_state, name) }
}

/// CreateEventW: create event object (wide).
pub extern "system" fn CreateEventW(
    security: *mut c_void,
    manual_reset: i32,
    initial_state: i32,
    name: *const u16,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateEventW(security, manual_reset, initial_state, name) }
}

/// CreateEventExA: create event object with extended options.
pub extern "system" fn CreateEventExA(
    security: *mut c_void,
    name: *const u8,
    flags: u32,
    desired_access: u32,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateEventExA(security, name, flags, desired_access) }
}

/// CreateEventExW: create event object with extended options (wide).
pub extern "system" fn CreateEventExW(
    security: *mut c_void,
    name: *const u16,
    flags: u32,
    desired_access: u32,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateEventExW(security, name, flags, desired_access) }
}

/// SetEvent: signal an event.
pub extern "system" fn SetEvent(event: *mut c_void) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::SetEvent(event) }
}

/// ResetEvent: reset an event to non-signaled state.
pub extern "system" fn ResetEvent(event: *mut c_void) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::ResetEvent(event) }
}

/// PulseEvent: pulse an event (signal and reset).
pub extern "system" fn PulseEvent(event: *mut c_void) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::PulseEvent(event) }
}

/// WaitForSingleObject: wait for an object.
pub extern "system" fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32 {
    unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(handle, milliseconds) }
}

/// WaitForSingleObjectEx: alertable wait.
pub extern "system" fn WaitForSingleObjectEx(handle: *mut c_void, milliseconds: u32, alertable: i32) -> u32 {
    unsafe { windows_sys::Win32::System::Threading::WaitForSingleObjectEx(handle, milliseconds, alertable) }
}

/// WaitForMultipleObjects: wait for multiple objects.
pub extern "system" fn WaitForMultipleObjects(
    count: u32,
    handles: *const *mut c_void,
    wait_all: i32,
    milliseconds: u32,
) -> u32 {
    unsafe { windows_sys::Win32::System::Threading::WaitForMultipleObjects(count, handles, wait_all, milliseconds) }
}

/// WaitForMultipleObjectsEx: alertable wait for multiple objects.
pub extern "system" fn WaitForMultipleObjectsEx(
    count: u32,
    handles: *const *mut c_void,
    wait_all: i32,
    milliseconds: u32,
    alertable: i32,
) -> u32 {
    unsafe { windows_sys::Win32::System::Threading::WaitForMultipleObjectsEx(count, handles, wait_all, milliseconds, alertable) }
}

/// CreateMutexA: create mutex.
pub extern "system" fn CreateMutexA(
    security: *mut c_void,
    initial_owner: i32,
    name: *const u8,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateMutexA(security, initial_owner, name) }
}

/// CreateMutexW: create mutex (wide).
pub extern "system" fn CreateMutexW(
    security: *mut c_void,
    initial_owner: i32,
    name: *const u16,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateMutexW(security, initial_owner, name) }
}

/// CreateMutexExA: create mutex with extended options.
pub extern "system" fn CreateMutexExA(
    security: *mut c_void,
    name: *const u8,
    flags: u32,
    desired_access: u32,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateMutexExA(security, name, flags, desired_access) }
}

/// CreateMutexExW: create mutex with extended options (wide).
pub extern "system" fn CreateMutexExW(
    security: *mut c_void,
    name: *const u16,
    flags: u32,
    desired_access: u32,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateMutexExW(security, name, flags, desired_access) }
}

/// ReleaseMutex: release mutex ownership.
pub extern "system" fn ReleaseMutex(mutex: *mut c_void) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::ReleaseMutex(mutex) }
}

/// CreateSemaphoreA: create semaphore.
pub extern "system" fn CreateSemaphoreA(
    security: *mut c_void,
    initial_count: i32,
    maximum_count: i32,
    name: *const u8,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateSemaphoreA(security, initial_count, maximum_count, name) }
}

/// CreateSemaphoreW: create semaphore (wide).
pub extern "system" fn CreateSemaphoreW(
    security: *mut c_void,
    initial_count: i32,
    maximum_count: i32,
    name: *const u16,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateSemaphoreW(security, initial_count, maximum_count, name) }
}

/// CreateSemaphoreExA: create semaphore with extended options.
pub extern "system" fn CreateSemaphoreExA(
    security: *mut c_void,
    name: *const u8,
    flags: u32,
    desired_access: u32,
    initial_count: i32,
    maximum_count: i32,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateSemaphoreExA(security, name, flags, desired_access, initial_count, maximum_count) }
}

/// CreateSemaphoreExW: create semaphore with extended options (wide).
pub extern "system" fn CreateSemaphoreExW(
    security: *mut c_void,
    name: *const u16,
    flags: u32,
    desired_access: u32,
    initial_count: i32,
    maximum_count: i32,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateSemaphoreExW(security, name, flags, desired_access, initial_count, maximum_count) }
}

/// ReleaseSemaphore: release semaphore.
pub extern "system" fn ReleaseSemaphore(
    semaphore: *mut c_void,
    release_count: i32,
    previous_count: *mut i32,
) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::ReleaseSemaphore(semaphore, release_count, previous_count) }
}

/// CreateWaitableTimerA: create waitable timer.
pub extern "system" fn CreateWaitableTimerA(
    security: *mut c_void,
    manual_reset: i32,
    name: *const u8,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateWaitableTimerA(security, manual_reset, name) }
}

/// CreateWaitableTimerW: create waitable timer (wide).
pub extern "system" fn CreateWaitableTimerW(
    security: *mut c_void,
    manual_reset: i32,
    name: *const u16,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateWaitableTimerW(security, manual_reset, name) }
}

/// CreateWaitableTimerExA: create waitable timer with extended options.
pub extern "system" fn CreateWaitableTimerExA(
    security: *mut c_void,
    name: *const u8,
    flags: u32,
    desired_access: u32,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateWaitableTimerExA(security, name, flags, desired_access) }
}

/// CreateWaitableTimerExW: create waitable timer with extended options (wide).
pub extern "system" fn CreateWaitableTimerExW(
    security: *mut c_void,
    name: *const u16,
    flags: u32,
    desired_access: u32,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Threading::CreateWaitableTimerExW(security, name, flags, desired_access) }
}

/// SetWaitableTimer: set a waitable timer.
pub extern "system" fn SetWaitableTimer(
    timer: *mut c_void,
    due_time: *const i64,
    period: i32,
    completion_routine: *mut c_void,
    arg: *mut c_void,
    resume: i32,
) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::SetWaitableTimer(timer, due_time, period, completion_routine, arg, resume) }
}

/// SetWaitableTimerEx: set waitable timer with extended options.
pub extern "system" fn SetWaitableTimerEx(
    timer: *mut c_void,
    due_time: *const i64,
    period: i32,
    completion_routine: *mut c_void,
    arg: *mut c_void,
    resume: i32,
    tolerance: u32,
) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::SetWaitableTimerEx(timer, due_time, period, completion_routine, arg, resume, tolerance) }
}

/// CancelWaitableTimer: cancel a waitable timer.
pub extern "system" fn CancelWaitableTimer(timer: *mut c_void) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::CancelWaitableTimer(timer) }
}

/// QueryPerformanceCounter: high-resolution timer.
pub extern "system" fn QueryPerformanceCounter(lp_performance_count: *mut i64) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::QueryPerformanceCounter(lp_performance_count) }
}

/// QueryPerformanceFrequency: get timer frequency.
pub extern "system" fn QueryPerformanceFrequency(lp_frequency: *mut i64) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::QueryPerformanceFrequency(lp_frequency) }
}

/// GetTickCount: system tick count (low resolution).
pub extern "system" fn GetTickCount() -> u32 {
    unsafe { windows_sys::Win32::System::Threading::GetTickCount() }
}

/// GetTickCount64: 64-bit system tick count.
pub extern "system" fn GetTickCount64() -> u64 {
    unsafe { windows_sys::Win32::System::Threading::GetTickCount64() }
}

/// timeGetTime: multimedia timer.
pub extern "system" fn timeGetTime() -> u32 {
    unsafe { windows_sys::Win32::Media::Time::timeGetTime() }
}

/// timeBeginPeriod: set minimum timer resolution.
pub extern "system" fn timeBeginPeriod(u_period: u32) -> u32 {
    unsafe { windows_sys::Win32::Media::Time::timeBeginPeriod(u_period) }
}

/// timeEndPeriod: restore timer resolution.
pub extern "system" fn timeEndPeriod(u_period: u32) -> u32 {
    unsafe { windows_sys::Win32::Media::Time::timeEndPeriod(u_period) }
}

/// VirtualAlloc: reserve/commit memory.
pub extern "system" fn VirtualAlloc(
    address: *mut c_void,
    size: usize,
    allocation_type: u32,
    protect: u32,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Memory::VirtualAlloc(address, size, allocation_type, protect) }
}

/// VirtualAllocEx: allocate in another process.
pub extern "system" fn VirtualAllocEx(
    process: *mut c_void,
    address: *mut c_void,
    size: usize,
    allocation_type: u32,
    protect: u32,
) -> *mut c_void {
    unsafe { windows_sys::Win32::System::Memory::VirtualAllocEx(process, address, size, allocation_type, protect) }
}

/// VirtualFree: free/reserve memory.
pub extern "system" fn VirtualFree(
    address: *mut c_void,
    size: usize,
    free_type: u32,
) -> i32 {
    unsafe { windows_sys::Win32::System::Memory::VirtualFree(address, size, free_type) }
}

/// VirtualFreeEx: free memory in another process.
pub extern "system" fn VirtualFreeEx(
    process: *mut c_void,
    address: *mut c_void,
    size: usize,
    free_type: u32,
) -> i32 {
    unsafe { windows_sys::Win32::System::Memory::VirtualFreeEx(process, address, size, free_type) }
}

/// VirtualProtect: change memory protection.
pub extern "system" fn VirtualProtect(
    address: *mut c_void,
    size: usize,
    new_protect: u32,
    old_protect: *mut u32,
) -> i32 {
    unsafe { windows_sys::Win32::System::Memory::VirtualProtect(address, size, new_protect, old_protect) }
}

/// VirtualProtectEx: change memory protection in another process.
pub extern "system" fn VirtualProtectEx(
    process: *mut c_void,
    address: *mut c_void,
    size: usize,
    new_protect: u32,
    old_protect: *mut u32,
) -> i32 {
    unsafe { windows_sys::Win32::System::Memory::VirtualProtectEx(process, address, size, new_protect, old_protect) }
}

/// VirtualQuery: query memory information.
pub extern "system" fn VirtualQuery(
    address: *const c_void,
    buffer: *mut c_void,
    length: usize,
) -> usize {
    unsafe { windows_sys::Win32::System::Memory::VirtualQuery(address, buffer, length) }
}

/// VirtualQueryEx: query memory in another process.
pub extern "system" fn VirtualQueryEx(
    process: *mut c_void,
    address: *const c_void,
    buffer: *mut c_void,
    length: usize,
) -> usize {
    unsafe { windows_sys::Win32::System::Memory::VirtualQueryEx(process, address, buffer, length) }
}

/// GetSystemInfo: get system information.
pub extern "system" fn GetSystemInfo(lp_system_info: *mut c_void) {
    unsafe { windows_sys::Win32::System::SystemInformation::GetSystemInfo(lp_system_info) }
}

/// GetNativeSystemInfo: get native system information.
pub extern "system" fn GetNativeSystemInfo(lp_system_info: *mut c_void) {
    unsafe { windows_sys::Win32::System::SystemInformation::GetNativeSystemInfo(lp_system_info) }
}

/// GetSystemTimeAsFileTime: get system time as file time.
pub extern "system" fn GetSystemTimeAsFileTime(lp_system_time_as_file_time: *mut c_void) {
    unsafe { windows_sys::Win32::System::Time::GetSystemTimeAsFileTime(lp_system_time_as_file_time) }
}

/// GetSystemTimePreciseAsFileTime: precise system time.
pub extern "system" fn GetSystemTimePreciseAsFileTime(lp_system_time_as_file_time: *mut c_void) {
    unsafe { windows_sys::Win32::System::Time::GetSystemTimePreciseAsFileTime(lp_system_time_as_file_time) }
}

/// GetLocalTime: get local time.
pub extern "system" fn GetLocalTime(lp_system_time: *mut c_void) {
    unsafe { windows_sys::Win32::System::Time::GetLocalTime(lp_system_time) }
}

/// GetSystemTime: get UTC time.
pub extern "system" fn GetSystemTime(lp_system_time: *mut c_void) {
    unsafe { windows_sys::Win32::System::Time::GetSystemTime(lp_system_time) }
}

/// SetLocalTime: set local time.
pub extern "system" fn SetLocalTime(lp_system_time: *const c_void) -> i32 {
    unsafe { windows_sys::Win32::System::Time::SetLocalTime(lp_system_time) }
}

/// SetSystemTime: set UTC time.
pub extern "system" fn SetSystemTime(lp_system_time: *const c_void) -> i32 {
    unsafe { windows_sys::Win32::System::Time::SetSystemTime(lp_system_time) }
}

/// GetSystemTimeAdjustment: get time adjustment.
pub extern "system" fn GetSystemTimeAdjustment(
    time_adjustment: *mut u32,
    time_increment: *mut u32,
    time_adjustment_disabled: *mut i32,
) -> i32 {
    unsafe { windows_sys::Win32::System::Time::GetSystemTimeAdjustment(time_adjustment, time_increment, time_adjustment_disabled) }
}

/// SetSystemTimeAdjustment: set time adjustment.
pub extern "system" fn SetSystemTimeAdjustment(
    time_adjustment: u32,
    time_adjustment_disabled: i32,
) -> i32 {
    unsafe { windows_sys::Win32::System::Time::SetSystemTimeAdjustment(time_adjustment, time_adjustment_disabled) }
}

/// IsProcessorFeaturePresent: check processor feature.
pub extern "system" fn IsProcessorFeaturePresent(feature: u32) -> i32 {
    unsafe { windows_sys::Win32::System::SystemInformation::IsProcessorFeaturePresent(feature) }
}

/// GetLogicalProcessorInformation: get logical processor info.
pub extern "system" fn GetLogicalProcessorInformation(
    buffer: *mut c_void,
    returned_length: *mut u32,
) -> i32 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetLogicalProcessorInformation(buffer, returned_length) }
}

/// GetLogicalProcessorInformationEx: extended processor info.
pub extern "system" fn GetLogicalProcessorInformationEx(
    relationship_type: u32,
    buffer: *mut c_void,
    returned_length: *mut u32,
) -> i32 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetLogicalProcessorInformationEx(relationship_type, buffer, returned_length) }
}

/// GetNumaHighestNodeNumber: get highest NUMA node.
pub extern "system" fn GetNumaHighestNodeNumber(highest_node_number: *mut u32) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::GetNumaHighestNodeNumber(highest_node_number) }
}

/// GetNumaNodeProcessorMaskEx: get NUMA node processor mask.
pub extern "system" fn GetNumaNodeProcessorMaskEx(node: u16, processor_group_mask: *mut c_void) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::GetNumaNodeProcessorMaskEx(node, processor_group_mask) }
}

/// GetNumaProcessorNodeEx: get NUMA node for processor.
pub extern "system" fn GetNumaProcessorNodeEx(processor: *const c_void, node: *mut u16) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::GetNumaProcessorNodeEx(processor, node) }
}

/// GetNumaAvailableMemoryNodeEx: get available memory on NUMA node.
pub extern "system" fn GetNumaAvailableMemoryNodeEx(node: u16, available_bytes: *mut u64) -> i32 {
    unsafe { windows_sys::Win32::System::Threading::GetNumaAvailableMemoryNodeEx(node, available_bytes) }
}

/// GetProcessWorkingSetSize: get process working set size.
pub extern "system" fn GetProcessWorkingSetSize(
    process: *mut c_void,
    minimum_working_set_size: *mut usize,
    maximum_working_set_size: *mut usize,
) -> i32 {
    unsafe { windows_sys::Win32::System::Memory::GetProcessWorkingSetSize(process, minimum_working_set_size, maximum_working_set_size) }
}

/// SetProcessWorkingSetSize: set process working set size.
pub extern "system" fn SetProcessWorkingSetSize(
    process: *mut c_void,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
) -> i32 {
    unsafe { windows_sys::Win32::System::Memory::SetProcessWorkingSetSize(process, minimum_working_set_size, maximum_working_set_size) }
}

/// GetProcessWorkingSetSizeEx: extended working set query.
pub extern "system" fn GetProcessWorkingSetSizeEx(
    process: *mut c_void,
    minimum_working_set_size: *mut usize,
    maximum_working_set_size: *mut usize,
    flags: *mut u32,
) -> i32 {
    unsafe { windows_sys::Win32::System::Memory::GetProcessWorkingSetSizeEx(process, minimum_working_set_size, maximum_working_set_size, flags) }
}

/// SetProcessWorkingSetSizeEx: extended working set control.
pub extern "system" fn SetProcessWorkingSetSizeEx(
    process: *mut c_void,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    flags: u32,
) -> i32 {
    unsafe { windows_sys::Win32::System::Memory::SetProcessWorkingSetSizeEx(process, minimum_working_set_size, maximum_working_set_size, flags) }
}

/// FlushProcessWriteBuffers: flush process write buffers.
pub extern "system" fn FlushProcessWriteBuffers() {
    unsafe { windows_sys::Win32::System::Memory::FlushProcessWriteBuffers() }
}

/// GetWriteWatch: get write watch information.
pub extern "system" fn GetWriteWatch(
    flags: u32,
    base_address: *mut c_void,
    region_size: usize,
    addresses: *mut *mut c_void,
    count: *mut u32,
    granularity: *mut u32,
) -> i32 {
    unsafe { windows_sys::Win32::System::Memory::GetWriteWatch(flags, base_address, region_size, addresses, count, granularity) }
}

/// ResetWriteWatch: reset write watch.
pub extern "system" fn ResetWriteWatch(flags: u32, base_address: *mut c_void, region_size: usize) -> i32 {
    unsafe { windows_sys::Win32::System::Memory::ResetWriteWatch(flags, base_address, region_size) }
}

/// GetLargePageMinimum: get minimum large page size.
pub extern "system" fn GetLargePageMinimum() -> usize {
    unsafe { windows_sys::Win32::System::Memory::GetLargePageMinimum() }
}

/// GetSystemInfo wrapper with SYSTEM_INFO structure.
/// This is the actual structure layout expected by titles.
pub extern "system" fn GetSystemInfoEx(lp_system_info: *mut SYSTEM_INFO_EX) {
    unsafe { windows_sys::Win32::System::SystemInformation::GetSystemInfo(std::mem::transmute(lp_system_info)) }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-libraryloader-*",
            "api-ms-win-core-processthreads-*",
            "api-ms-win-core-debug-*",
            "api-ms-win-core-errorhandling-*",
            "api-ms-win-crt-runtime-*",
        ],
        exports: vec![
            hle_export!(__p___argc),
            hle_export!(__p___argv),
            hle_export!(GetModuleHandleW),
            hle_export!(GetModuleHandleA),
            hle_export!(GetModuleFileNameW),
            hle_export!(GetModuleFileNameA),
            hle_export!(LoadLibraryExA),
            hle_export!(LoadLibraryExW),
            hle_export!(LoadLibraryA),
            hle_export!(LoadLibraryW),
            hle_export!(GetProcAddress),
            hle_export!(FreeLibrary),
            hle_export!(DisableThreadLibraryCalls),
            hle_export!(OutputDebugStringA),
            hle_export!(OutputDebugStringW),
            hle_export!(exit),
            hle_export!(abort),
            hle_export!(terminate),
            hle_export!(_invalid_parameter_noinfo_noreturn),
            hle_export!(TerminateProcess),
            hle_export!(SetUnhandledExceptionFilter),
            hle_export!(SetThreadPriority),
            hle_export!(GetThreadPriority),
            hle_export!(SetThreadPriorityBoost),
            hle_export!(GetThreadPriorityBoost),
            hle_export!(SetProcessPriorityBoost),
            hle_export!(GetProcessPriorityBoost),
            hle_export!(SetThreadIdealProcessor),
            hle_export!(GetThreadIdealProcessorEx),
            hle_export!(SetThreadAffinityMask),
            hle_export!(GetProcessAffinityMask),
            hle_export!(SwitchToThread),
            hle_export!(Sleep),
            hle_export!(SleepEx),
            hle_export!(CreateEventA),
            hle_export!(CreateEventW),
            hle_export!(CreateEventExA),
            hle_export!(CreateEventExW),
            hle_export!(SetEvent),
            hle_export!(ResetEvent),
            hle_export!(PulseEvent),
            hle_export!(WaitForSingleObject),
            hle_export!(WaitForSingleObjectEx),
            hle_export!(WaitForMultipleObjects),
            hle_export!(WaitForMultipleObjectsEx),
            hle_export!(CreateMutexA),
            hle_export!(CreateMutexW),
            hle_export!(CreateMutexExA),
            hle_export!(CreateMutexExW),
            hle_export!(ReleaseMutex),
            hle_export!(CreateSemaphoreA),
            hle_export!(CreateSemaphoreW),
            hle_export!(CreateSemaphoreExA),
            hle_export!(CreateSemaphoreExW),
            hle_export!(ReleaseSemaphore),
            hle_export!(CreateWaitableTimerA),
            hle_export!(CreateWaitableTimerW),
            hle_export!(CreateWaitableTimerExA),
            hle_export!(CreateWaitableTimerExW),
            hle_export!(SetWaitableTimer),
            hle_export!(SetWaitableTimerEx),
            hle_export!(CancelWaitableTimer),
            hle_export!(QueryPerformanceCounter),
            hle_export!(QueryPerformanceFrequency),
            hle_export!(GetTickCount),
            hle_export!(GetTickCount64),
            hle_export!(timeGetTime),
            hle_export!(timeBeginPeriod),
            hle_export!(timeEndPeriod),
            hle_export!(VirtualAlloc),
            hle_export!(VirtualAllocEx),
            hle_export!(VirtualFree),
            hle_export!(VirtualFreeEx),
            hle_export!(VirtualProtect),
            hle_export!(VirtualProtectEx),
            hle_export!(VirtualQuery),
            hle_export!(VirtualQueryEx),
            hle_export!(GetSystemInfo),
            hle_export!(GetNativeSystemInfo),
            hle_export!(GetSystemTimeAsFileTime),
            hle_export!(GetSystemTimePreciseAsFileTime),
            hle_export!(GetLocalTime),
            hle_export!(GetSystemTime),
            hle_export!(SetLocalTime),
            hle_export!(SetSystemTime),
            hle_export!(GetSystemTimeAdjustment),
            hle_export!(SetSystemTimeAdjustment),
            hle_export!(IsProcessorFeaturePresent),
            hle_export!(GetLogicalProcessorInformation),
            hle_export!(GetLogicalProcessorInformationEx),
            hle_export!(GetNumaHighestNodeNumber),
            hle_export!(GetNumaNodeProcessorMaskEx),
            hle_export!(GetNumaProcessorNodeEx),
            hle_export!(GetNumaAvailableMemoryNodeEx),
            hle_export!(GetProcessWorkingSetSize),
            hle_export!(SetProcessWorkingSetSize),
            hle_export!(GetProcessWorkingSetSizeEx),
            hle_export!(SetProcessWorkingSetSizeEx),
            hle_export!(FlushProcessWriteBuffers),
            hle_export!(GetWriteWatch),
            hle_export!(ResetWriteWatch),
            hle_export!(GetLargePageMinimum),
        ],
    }
}

/// The return addresses above the caller, as `module+rva`, for logs.
#[cfg(feature = "diagnostics")]
pub(crate) fn callers() -> String {
    host::callers()
}

#[cfg(windows)]
mod host {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::{FreeLibrary, SetLastError};
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
    use windows_sys::Win32::System::LibraryLoader::{
        DisableThreadLibraryCalls, GetModuleFileNameA, GetModuleFileNameW, GetModuleHandleA,
        GetModuleHandleW, GetProcAddress, LoadLibraryExA, LoadLibraryExW,
    };
    use windows_sys::Win32::System::Threading::TerminateProcess;

    unsafe extern "C" {
        #[link_name = "exit"]
        fn crt_exit(code: i32) -> !;
        #[link_name = "abort"]
        fn crt_abort() -> !;
    }

    // SAFETY (every function): forwarding the title's own arguments to the
    // host API of the same name.

    pub(super) fn get_module_handle_w(name: *const u16) -> *mut c_void {
        // SAFETY: see above.
        unsafe { GetModuleHandleW(name) }
    }

    pub(super) fn get_module_handle_a(name: *const u8) -> *mut c_void {
        // SAFETY: see above.
        unsafe { GetModuleHandleA(name) }
    }

    pub(super) fn get_module_file_name_w(module: *mut c_void, out: *mut u16, size: u32) -> u32 {
        // SAFETY: see above.
        unsafe { GetModuleFileNameW(module, out, size) }
    }

    pub(super) fn get_module_file_name_a(module: *mut c_void, out: *mut u8, size: u32) -> u32 {
        // SAFETY: see above.
        unsafe { GetModuleFileNameA(module, out, size) }
    }

    pub(super) fn set_last_error(code: u32) {
        // SAFETY: see above.
        unsafe { SetLastError(code) }
    }

    pub(super) fn load_library_ex_a(name: *const u8, file: *mut c_void, flags: u32) -> *mut c_void {
        // SAFETY: see above.
        unsafe { LoadLibraryExA(name, file, flags) }
    }

    pub(super) fn load_library_ex_w(
        name: *const u16,
        file: *mut c_void,
        flags: u32,
    ) -> *mut c_void {
        // SAFETY: see above.
        unsafe { LoadLibraryExW(name, file, flags) }
    }

    pub(super) fn get_proc_address(module: *mut c_void, name: *const u8) -> *mut c_void {
        // SAFETY: see above.
        unsafe { GetProcAddress(module, name) }.map_or(std::ptr::null_mut(), |f| f as *mut c_void)
    }

    pub(super) fn free_library(module: *mut c_void) -> i32 {
        // SAFETY: see above.
        unsafe { FreeLibrary(module) }
    }

    pub(super) fn disable_thread_library_calls(module: *mut c_void) -> i32 {
        match victory_loader::disable_thread_calls(module as usize) {
            Some(done) => i32::from(done),
            // SAFETY: see above.
            None => unsafe { DisableThreadLibraryCalls(module) },
        }
    }

    pub(super) fn exit(code: i32) -> ! {
        // SAFETY: see above.
        unsafe { crt_exit(code) }
    }

    pub(super) fn abort() -> ! {
        // SAFETY: see above.
        unsafe { crt_abort() }
    }

    pub(super) fn terminate_process(process: *mut c_void, code: u32) -> i32 {
        // SAFETY: see above.
        unsafe { TerminateProcess(process, code) }
    }

    pub(super) fn set_unhandled_exception_filter(filter: extern "system" fn(*const c_void) -> i32) {
        // SAFETY: our filter takes the exception pointers as an opaque pointer.
        unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter(
                std::mem::transmute::<
                    extern "system" fn(*const c_void) -> i32,
                    windows_sys::Win32::System::Diagnostics::Debug::LPTOP_LEVEL_EXCEPTION_FILTER,
                >(filter),
            )
        };
    }

    pub(super) fn report_unhandled(info: *const c_void) {
        // SAFETY: the system's exception pointers, passed to the filter.
        unsafe { victory_loader::report_unhandled(info.cast()) };
    }

    pub(super) fn note_title_output(line: &str) {
        victory_loader::note_title_output(line);
    }

    /// Our caller's callers, as `module+rva`.
    pub(super) fn callers() -> String {
        let mut frames = [std::ptr::null_mut(); 12];
        // SAFETY: fills at most `frames.len()` return addresses.
        let n = unsafe {
            RtlCaptureStackBackTrace(
                2,
                frames.len() as u32,
                frames.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        let names: Vec<_> = frames[..usize::from(n)]
            .iter()
            .map(|&f| victory_loader::locate(f as usize))
            .collect();
        names.join(" < ")
    }
}

#[cfg(not(windows))]
mod host {
    use std::ffi::c_void;

    pub(super) fn get_module_handle_w(_: *const u16) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(super) fn get_module_handle_a(_: *const u8) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(super) fn load_library_ex_a(_: *const u8, _: *mut c_void, _: u32) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(super) fn get_module_file_name_w(_: *mut c_void, _: *mut u16, _: u32) -> u32 {
        0
    }

    pub(super) fn get_module_file_name_a(_: *mut c_void, _: *mut u8, _: u32) -> u32 {
        0
    }

    pub(super) fn set_last_error(_: u32) {}

    pub(super) fn load_library_ex_w(_: *const u16, _: *mut c_void, _: u32) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(super) fn get_proc_address(_: *mut c_void, _: *const u8) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(super) fn free_library(_: *mut c_void) -> i32 {
        0
    }

    pub(super) fn disable_thread_library_calls(_: *mut c_void) -> i32 {
        0
    }

    pub(super) fn exit(code: i32) -> ! {
        std::process::exit(code)
    }

    pub(super) fn abort() -> ! {
        std::process::abort()
    }

    pub(super) fn terminate_process(_: *mut c_void, code: u32) -> i32 {
        std::process::exit(code as i32)
    }

    pub(super) fn set_unhandled_exception_filter(_: extern "system" fn(*const c_void) -> i32) {}

    pub(super) fn report_unhandled(_: *const c_void) {}

    pub(super) fn note_title_output(_: &str) {}

    pub(super) fn callers() -> String {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The title's idle and lowest threads run below normal; the others as
    /// it sets them.
    #[test]
    fn no_title_thread_waits_for_idle_processors() {
        let given: Vec<i32> = [-15, -2, -1, 0, 1, 2, 15].into_iter().map(title_priority).collect();
        assert_eq!(given, [-1, -1, -1, 0, 1, 2, 15]);
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadPriority};
        std::thread::spawn(|| {
            // SAFETY: plain calls on this thread.
            let thread = unsafe { GetCurrentThread() };
            assert_eq!(SetThreadPriority(thread, -15), 1);
            // SAFETY: as above.
            assert_eq!(unsafe { GetThreadPriority(thread) }, -1);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn title_images_report_their_original_path() {
        let base = 0x7000_0000_0000;
        runtime::set_image_path(base, std::path::Path::new(r"C:\title\game.exe"));
        let module = base as *mut c_void;
        let mut buf = [0xFFFFu16; 32];
        let n = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32);
        assert_eq!(
            String::from_utf16_lossy(&buf[..n as usize]),
            r"C:\title\game.exe"
        );
        assert_eq!(buf[n as usize], 0);

        // Too small: truncated, NUL-terminated, the buffer size returned.
        let mut small = [0u8; 4];
        assert_eq!(GetModuleFileNameA(module, small.as_mut_ptr(), 4), 4);
        assert_eq!(&small, &[b'C', b':', b'\\', 0]);
    }

    /// The title's arguments come right after the program, and the list ends
    /// in a null pointer; without any there is only the program.
    #[test]
    fn the_title_gets_its_own_arguments() {
        let read = |made: &TitleArgv| {
            // SAFETY: made above, kept for the whole run.
            unsafe {
                let (count, list) = (*made.count.get() as usize, *made.list.get());
                assert!((*list.add(count)).is_null());
                (0..count)
                    .map(|i| CStr::from_ptr(*list.add(i)).to_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            }
        };
        let some = title_argv("victory.exe", &["-skipToIIS".into(), "-x=1".into()]);
        assert_eq!(read(&some), ["victory.exe", "-skipToIIS", "-x=1"]);
        assert_eq!(read(&title_argv("victory.exe", &[])), ["victory.exe"]);
        // What the title is given: the same list every time.
        assert_eq!(__p___argc(), __p___argc());
        // SAFETY: as above.
        unsafe { assert_eq!((*__p___argc(), (**__p___argv()).is_null()), (1, false)) };
    }

    fn w(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    #[cfg(windows)]
    #[test]
    fn host_lookups_by_name_get_our_overrides() {
        let kernel32 = GetModuleHandleW(w("kernel32.dll").as_ptr());
        assert!(!kernel32.is_null());
        let ours = crate::registry::lookup("kernel32.dll", "CreateFileW").unwrap();
        assert_eq!(
            GetProcAddress(kernel32, c"CreateFileW".as_ptr().cast()) as usize,
            ours
        );
        // A plain name stays the host's.
        let host = host::get_proc_address(kernel32, c"GetTickCount".as_ptr().cast());
        assert!(!host.is_null());
        assert_eq!(
            GetProcAddress(kernel32, c"GetTickCount".as_ptr().cast()),
            host
        );
        // Host DLLs outside the host policy do not exist for the title.
        assert!(LoadLibraryW(w("wininet.dll").as_ptr()).is_null());
        assert!(LoadLibraryA(c"C:\\Windows\\System32\\urlmon".as_ptr().cast()).is_null());
        assert!(GetModuleHandleW(w("WININET.DLL").as_ptr()).is_null());
        assert_eq!(
            // SAFETY: reads this thread's last error.
            unsafe { windows_sys::Win32::Foundation::GetLastError() },
            ERROR_MOD_NOT_FOUND
        );
        assert!(!LoadLibraryW(w("kernel32").as_ptr()).is_null());
        // By ordinal too (ws2_32 #4 is `connect`).
        let ws2 = LoadLibraryW(w("ws2_32.dll").as_ptr());
        assert_eq!(
            GetProcAddress(ws2, 4 as *const u8) as usize,
            crate::registry::lookup("ws2_32.dll", "connect").unwrap()
        );
    }

    #[test]
    fn title_dlls_and_pseudo_modules_are_found_by_name() {
        let base = 0x7100_0000_0000usize;
        runtime::set_image_path(base, std::path::Path::new(r"C:\title\Mapped.dll"));
        runtime::set_title_dll_loader(Box::new(|file| match file {
            "late.dll" => Ok(Some(0x7200_0000_0000)),
            "broken.dll" => Err("bad image".into()),
            _ => Ok(None),
        }));
        let mapped = base as *mut c_void;
        assert_eq!(GetModuleHandleW(w("MAPPED").as_ptr()), mapped);
        assert_eq!(GetModuleHandleA(c"sub\\mapped.dll".as_ptr().cast()), mapped);
        assert_eq!(LoadLibraryW(w("mapped.dll").as_ptr()), mapped);
        assert_eq!(FreeLibrary(mapped), 1, "title images stay loaded");

        // Not mapped yet: `LoadLibrary` maps it, `GetModuleHandle` does not.
        assert!(GetModuleHandleW(w("late.dll").as_ptr()).is_null());
        assert_eq!(
            LoadLibraryA(c"late".as_ptr().cast()) as usize,
            0x7200_0000_0000
        );
        assert!(LoadLibraryA(c"broken.dll".as_ptr().cast()).is_null());
        // As data it runs no code, so the host loads it (and finds nothing).
        let data = LoadLibraryExW(w("late.dll").as_ptr(), std::ptr::null_mut(), 0x2);
        assert_ne!(data as usize, 0x7200_0000_0000);

        let runtime = LoadLibraryA(c"xgameruntime.dll".as_ptr().cast());
        assert!(!runtime.is_null());
        assert_eq!(GetModuleHandleW(w("XGameRuntime.dll").as_ptr()), runtime);
    }

    /// No buffer: nothing written, the error says it was too small.
    #[test]
    fn a_title_image_name_into_no_buffer() {
        let base = 0x7300_0000_0000;
        runtime::set_image_path(base, std::path::Path::new(r"C:\title\game.exe"));
        let module = base as *mut c_void;
        assert_eq!(GetModuleFileNameW(module, std::ptr::null_mut(), 0), 0);
        assert_eq!(GetModuleFileNameA(module, std::ptr::null_mut(), 0), 0);
        #[cfg(windows)]
        {
            // SAFETY: reads this thread's last error.
            let last = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            assert_eq!(last, ERROR_INSUFFICIENT_BUFFER);
        }
    }
}
