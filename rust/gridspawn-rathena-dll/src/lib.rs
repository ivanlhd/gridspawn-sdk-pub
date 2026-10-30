//! `gridspawn_rathena.dll`: the thin game-side half of the Gridspawn companion
//! for rAthena / Ragexe clients (docs/platforms/rathena-companion.md).
//!
//! It starts `gridspawn-companion.exe`, authenticates two local pipes with a
//! per-launch token, forwards the game's calls as requests, and delivers the
//! companion's events through C callbacks on one thread it owns. It has no
//! network code, no audio and no async runtime. The C contract is
//! `include/gridspawn_rathena.h` (all functions `__cdecl`).

mod frames;
mod protocol;

use std::{
    ffi::{c_char, c_void, CStr, CString},
    fs::File,
    io::Write,
    panic::{catch_unwind, AssertUnwindSafe},
    process::{Child, Command, Stdio},
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle, ThreadId},
    time::{Duration, Instant},
};

use serde_json::{json, Value};

pub use protocol::Event;

/// Status codes of the `gs_rag_*` calls.
pub const GS_RAG_OK: i32 = 0;
pub const GS_RAG_NOT_INITIALIZED: i32 = -1;
pub const GS_RAG_INVALID_ARGUMENT: i32 = -2;
pub const GS_RAG_ALREADY_INITIALIZED: i32 = -3;
pub const GS_RAG_COMPANION_START_FAILED: i32 = -4;
pub const GS_RAG_COMPANION_LOST: i32 = -5;
pub const GS_RAG_TIMEOUT: i32 = -6;
pub const GS_RAG_INTERNAL: i32 = -7;

/// One participant of a roster (`on_attached`).
#[repr(C)]
pub struct GsRagParticipant {
    pub id: *const c_char,
    pub display_name: *const c_char,
}

/// Settings for `gs_rag_init`. Zero-initialise it, set `struct_size`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GsRagConfig {
    pub struct_size: u32,
    /// Required: the Gridspawn application id.
    pub app_id: *const c_char,
    /// "production" (NULL), "staging" or "development".
    pub environment: *const c_char,
    /// NULL = https://sdk.gridspawn.net
    pub sdk_base_url: *const c_char,
    /// NULL = gridspawn-companion.exe next to this DLL.
    pub companion_path: *const c_char,
    /// NULL = no log; otherwise the companion appends its log there.
    pub log_path: *const c_char,
    /// Non-zero: show the companion's voice window.
    pub show_window: i32,
    /// 0 = 10000.
    pub init_timeout_ms: u32,
    /// Testing only: send a tone instead of the microphone and play nothing.
    pub test_tone: i32,
}

type CbUser = unsafe extern "C" fn(*mut c_void);
type CbText = unsafe extern "C" fn(*const c_char, *mut c_void);

/// Callbacks; every one may be NULL. They run on one thread the DLL owns,
/// one at a time. Zero-initialise it, set `struct_size`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GsRagCallbacks {
    pub struct_size: u32,
    pub on_ready: Option<CbUser>,
    pub on_companion_lost: Option<CbText>,
    pub on_signed_in: Option<CbText>,
    pub on_signed_out: Option<CbUser>,
    pub on_assertion: Option<unsafe extern "C" fn(u32, *const c_char, *mut c_void)>,
    pub on_attached:
        Option<unsafe extern "C" fn(*const c_char, *const GsRagParticipant, u32, *mut c_void)>,
    pub on_detached: Option<CbText>,
    pub on_voice_state: Option<unsafe extern "C" fn(i32, *mut c_void)>,
    pub on_participant:
        Option<unsafe extern "C" fn(i32, *const c_char, *const c_char, *mut c_void)>,
    pub on_speaking: Option<unsafe extern "C" fn(*const c_char, i32, *mut c_void)>,
    pub on_local_speaking: Option<unsafe extern "C" fn(i32, *mut c_void)>,
    pub on_server_muted: Option<unsafe extern "C" fn(*const c_char, i32, *mut c_void)>,
    pub on_devices:
        Option<unsafe extern "C" fn(i32, *const *const c_char, u32, *const c_char, *mut c_void)>,
    pub on_error: Option<unsafe extern "C" fn(*const c_char, *const c_char, *mut c_void)>,
}

/// Copies `size` bytes of a caller struct that may be older (shorter) than
/// ours into a zeroed one.
///
/// SAFETY: `source` points to at least `size` readable bytes.
unsafe fn versioned<T: Copy>(source: *const T, size: u32) -> T {
    let mut out = std::mem::MaybeUninit::<T>::zeroed();
    let count = (size as usize).min(std::mem::size_of::<T>());
    // SAFETY: both sides have at least `count` bytes; T is plain data whose
    // all-zero pattern is valid (null pointers, None, 0).
    unsafe {
        ptr::copy_nonoverlapping(source.cast::<u8>(), out.as_mut_ptr().cast::<u8>(), count);
        out.assume_init()
    }
}

struct Callbacks {
    table: GsRagCallbacks,
    user: usize,
}

impl Callbacks {
    fn deliver(&self, event: Event) {
        let user = self.user as *mut c_void;
        let t = &self.table;
        let c = |text: &str| CString::new(text.replace('\0', "")).unwrap_or_default();
        // SAFETY: the caller supplied these function pointers for this use.
        unsafe {
            match event {
                Event::Ready => {
                    if let Some(f) = t.on_ready {
                        f(user)
                    }
                }
                Event::CompanionLost(reason) => {
                    if let Some(f) = t.on_companion_lost {
                        f(c(&reason).as_ptr(), user)
                    }
                }
                Event::SignedIn(name) => {
                    if let Some(f) = t.on_signed_in {
                        f(c(&name).as_ptr(), user)
                    }
                }
                Event::SignedOut => {
                    if let Some(f) = t.on_signed_out {
                        f(user)
                    }
                }
                Event::Assertion(request, assertion) => {
                    if let Some(f) = t.on_assertion {
                        let a = c(&assertion);
                        f(request, a.as_ptr(), user);
                        a.into_bytes_with_nul().fill(0);
                    }
                }
                Event::Attached(name, roster) => {
                    if let Some(f) = t.on_attached {
                        let owned: Vec<(CString, CString)> =
                            roster.iter().map(|(id, n)| (c(id), c(n))).collect();
                        let list: Vec<GsRagParticipant> = owned
                            .iter()
                            .map(|(id, n)| GsRagParticipant {
                                id: id.as_ptr(),
                                display_name: n.as_ptr(),
                            })
                            .collect();
                        f(c(&name).as_ptr(), list.as_ptr(), list.len() as u32, user)
                    }
                }
                Event::Detached(reason) => {
                    if let Some(f) = t.on_detached {
                        f(c(&reason).as_ptr(), user)
                    }
                }
                Event::VoiceState(state) => {
                    if let Some(f) = t.on_voice_state {
                        f(state, user)
                    }
                }
                Event::Participant(joined, id, name) => {
                    if let Some(f) = t.on_participant {
                        f(i32::from(joined), c(&id).as_ptr(), c(&name).as_ptr(), user)
                    }
                }
                Event::Speaking(id, active) => {
                    if let Some(f) = t.on_speaking {
                        f(c(&id).as_ptr(), i32::from(active), user)
                    }
                }
                Event::LocalSpeaking(active) => {
                    if let Some(f) = t.on_local_speaking {
                        f(i32::from(active), user)
                    }
                }
                Event::ServerMuted(id, muted) => {
                    if let Some(f) = t.on_server_muted {
                        let id = id.map(|id| c(&id));
                        f(
                            id.as_ref().map_or(ptr::null(), |id| id.as_ptr()),
                            i32::from(muted),
                            user,
                        )
                    }
                }
                Event::Devices(kind, names, selected) => {
                    if let Some(f) = t.on_devices {
                        let owned: Vec<CString> = names.iter().map(|n| c(n)).collect();
                        let ptrs: Vec<*const c_char> = owned.iter().map(|n| n.as_ptr()).collect();
                        let selected = selected.map(|s| c(&s));
                        f(
                            kind,
                            ptrs.as_ptr(),
                            ptrs.len() as u32,
                            selected.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
                            user,
                        )
                    }
                }
                Event::Error(code, message) => {
                    if let Some(f) = t.on_error {
                        f(c(&code).as_ptr(), c(&message).as_ptr(), user)
                    }
                }
            }
        }
    }
}

struct Runtime {
    writer: Mutex<File>,
    next_id: AtomicU32,
    child: Mutex<Option<Child>>,
    events: Mutex<Option<mpsc::Sender<Event>>>,
    lost: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    reader: Mutex<Option<JoinHandle<()>>>,
    dispatcher: Mutex<Option<JoinHandle<()>>>,
    dispatcher_id: ThreadId,
}

static RUNTIME: Mutex<Option<Arc<Runtime>>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn runtime() -> Option<Arc<Runtime>> {
    lock(&RUNTIME).clone()
}

/// SAFETY: NULL or a NUL-terminated string.
unsafe fn text(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: per the contract.
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

fn guard(body: impl FnOnce() -> i32) -> i32 {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(GS_RAG_INTERNAL)
}

/// Sends one request; its failure (if any) comes back as `on_error`.
fn request(op: &str, args: Value) -> i32 {
    let Some(rt) = runtime() else {
        return GS_RAG_NOT_INITIALIZED;
    };
    if rt.lost.load(Ordering::SeqCst) {
        return GS_RAG_COMPANION_LOST;
    }
    let id = rt.next_id.fetch_add(1, Ordering::SeqCst);
    let frame = json!({"id": id, "op": op, "args": args});
    let written = frames::write_frame(&mut *lock(&rt.writer), &frame);
    if written.is_err() {
        rt.lost.store(true, Ordering::SeqCst);
        return GS_RAG_COMPANION_LOST;
    }
    GS_RAG_OK
}

/// The directory of this DLL (for the default companion path).
#[cfg(windows)]
fn module_dir() -> Option<std::path::PathBuf> {
    use windows_sys::Win32::{
        Foundation::HMODULE,
        System::LibraryLoader::{
            GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        },
    };
    let mut module: HMODULE = ptr::null_mut();
    // SAFETY: the address is a function of this module; module is writable.
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            module_dir as *const u16,
            &mut module,
        )
    };
    if ok == 0 {
        return None;
    }
    let mut buffer = vec![0u16; 32_768];
    // SAFETY: buffer is writable for its length.
    let len = unsafe { GetModuleFileNameW(module, buffer.as_mut_ptr(), buffer.len() as u32) };
    if len == 0 {
        return None;
    }
    let path = std::path::PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize]));
    path.parent().map(std::path::Path::to_path_buf)
}

#[cfg(not(windows))]
fn module_dir() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
}

fn random_token() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Starts the companion, connects and authenticates both pipes, and waits
/// for `ready`. Blocking (at most `init_timeout_ms`). Call it once, after
/// loading the DLL, never from DllMain.
///
/// SAFETY: `config` and `callbacks` are valid for the call (copied).
#[no_mangle]
pub unsafe extern "C" fn gs_rag_init(
    config: *const GsRagConfig,
    callbacks: *const GsRagCallbacks,
    user: *mut c_void,
) -> i32 {
    guard(|| {
        if config.is_null() || callbacks.is_null() {
            return GS_RAG_INVALID_ARGUMENT;
        }
        // SAFETY: non-NULL; struct_size bounds the copy.
        let config = unsafe { versioned(config, (*config).struct_size) };
        // SAFETY: as above.
        let table = unsafe { versioned(callbacks, (*callbacks).struct_size) };
        let mut slot = lock(&RUNTIME);
        if slot.is_some() {
            return GS_RAG_ALREADY_INITIALIZED;
        }
        // SAFETY: the config's strings are NULL or NUL-terminated.
        let (app_id, environment, sdk_base_url, companion, log_path) = unsafe {
            (
                text(config.app_id),
                text(config.environment),
                text(config.sdk_base_url),
                text(config.companion_path),
                text(config.log_path),
            )
        };
        let Some(app_id) = app_id.filter(|a| !a.is_empty()) else {
            return GS_RAG_INVALID_ARGUMENT;
        };
        let timeout = Duration::from_millis(match config.init_timeout_ms {
            0 => 10_000,
            ms => u64::from(ms),
        });
        let Some(token) = random_token() else {
            return GS_RAG_INTERNAL;
        };
        let companion = companion
            .map(std::path::PathBuf::from)
            .or_else(|| module_dir().map(|d| d.join("gridspawn-companion.exe")));
        let Some(companion) = companion else {
            return GS_RAG_COMPANION_START_FAILED;
        };
        let pid = std::process::id();
        let mut child = match Command::new(&companion)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return GS_RAG_COMPANION_START_FAILED,
        };
        let launch = json!({
            "v": protocol::VERSION,
            "token": token,
            "gamePid": pid,
            "appId": app_id,
            "environment": environment.unwrap_or_else(|| "production".into()),
            "sdkBaseUrl": sdk_base_url,
            "logPath": log_path,
            "showWindow": config.show_window != 0,
            "testTone": config.test_tone != 0,
        });
        let wrote = child.stdin.take().map(|mut stdin| {
            let line = format!("{launch}\n");
            stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush())
        });
        if !matches!(wrote, Some(Ok(()))) {
            let _ = child.kill();
            return GS_RAG_COMPANION_START_FAILED;
        }
        let deadline = Instant::now() + timeout;
        let (in_name, out_name) = protocol::pipe_names(pid);
        let (mut writer, mut reader) = loop {
            let opened = (
                std::fs::OpenOptions::new().write(true).open(&in_name),
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&out_name),
            );
            if let (Ok(w), Ok(r)) = opened {
                break (w, r);
            }
            if matches!(child.try_wait(), Ok(Some(_))) {
                return GS_RAG_COMPANION_START_FAILED;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                return GS_RAG_TIMEOUT;
            }
            thread::sleep(Duration::from_millis(50));
        };
        let hello = |pipe: &str| json!({"hello": protocol::VERSION, "token": token, "pipe": pipe});
        if frames::write_frame(&mut writer, &hello("in")).is_err()
            || frames::write_frame(&mut reader, &hello("out")).is_err()
        {
            let _ = child.kill();
            return GS_RAG_COMPANION_START_FAILED;
        }

        let (events_tx, events_rx) = mpsc::channel::<Event>();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let lost = Arc::new(AtomicBool::new(false));
        let stopping = Arc::new(AtomicBool::new(false));
        let reader_thread = {
            let events = events_tx.clone();
            let lost = lost.clone();
            let stopping = stopping.clone();
            thread::Builder::new()
                .name("gridspawn-rathena-reader".into())
                .spawn(move || {
                    let mut ready = Some(ready_tx);
                    loop {
                        match frames::read_frame(&mut reader) {
                            Ok(Some(frame)) => {
                                if let Some(event) = protocol::parse(&frame) {
                                    if matches!(event, Event::Ready) {
                                        if let Some(r) = ready.take() {
                                            let _ = r.send(());
                                        }
                                    }
                                    if events.send(event).is_err() {
                                        break;
                                    }
                                }
                            }
                            Ok(None) | Err(_) => {
                                lost.store(true, Ordering::SeqCst);
                                if !stopping.load(Ordering::SeqCst) {
                                    let _ = events.send(Event::CompanionLost(
                                        "the companion closed its pipe".into(),
                                    ));
                                }
                                break;
                            }
                        }
                    }
                })
        };
        let Ok(reader_thread) = reader_thread else {
            let _ = child.kill();
            return GS_RAG_INTERNAL;
        };
        let callbacks = Callbacks {
            table,
            user: user as usize,
        };
        let dispatcher = thread::Builder::new()
            .name("gridspawn-rathena-callbacks".into())
            .spawn(move || {
                for event in events_rx {
                    let _ = catch_unwind(AssertUnwindSafe(|| callbacks.deliver(event)));
                }
            });
        let Ok(dispatcher) = dispatcher else {
            let _ = child.kill();
            return GS_RAG_INTERNAL;
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if ready_rx
            .recv_timeout(remaining.max(Duration::from_millis(1)))
            .is_err()
        {
            stopping.store(true, Ordering::SeqCst);
            let _ = child.kill();
            drop(events_tx);
            return GS_RAG_TIMEOUT;
        }
        let dispatcher_id = dispatcher.thread().id();
        *slot = Some(Arc::new(Runtime {
            writer: Mutex::new(writer),
            next_id: AtomicU32::new(1),
            child: Mutex::new(Some(child)),
            events: Mutex::new(Some(events_tx)),
            lost,
            stopping,
            reader: Mutex::new(Some(reader_thread)),
            dispatcher: Mutex::new(Some(dispatcher)),
            dispatcher_id,
        }));
        GS_RAG_OK
    })
}

/// Asks the companion to leave voice and exit, waits up to `timeout_ms`
/// (then ends it), and stops the DLL's threads. Callbacks already queued
/// are delivered first. May be called from a callback.
#[no_mangle]
pub extern "C" fn gs_rag_shutdown(timeout_ms: u32) -> i32 {
    guard(|| {
        let Some(rt) = lock(&RUNTIME).take() else {
            return GS_RAG_NOT_INITIALIZED;
        };
        rt.stopping.store(true, Ordering::SeqCst);
        let id = rt.next_id.fetch_add(1, Ordering::SeqCst);
        let _ = frames::write_frame(
            &mut *lock(&rt.writer),
            &json!({"id": id, "op": "shutdown", "args": {}}),
        );
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        let mut status = GS_RAG_OK;
        if let Some(mut child) = lock(&rt.child).take() {
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(20))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        status = GS_RAG_TIMEOUT;
                        break;
                    }
                }
            }
        }
        if let Some(reader) = lock(&rt.reader).take() {
            let _ = reader.join();
        }
        drop(lock(&rt.events).take());
        if thread::current().id() != rt.dispatcher_id {
            if let Some(dispatcher) = lock(&rt.dispatcher).take() {
                let _ = dispatcher.join();
            }
        }
        status
    })
}

/// The crate version, e.g. "0.1.1". Static; do not free.
#[no_mangle]
pub extern "C" fn gs_rag_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// Signs the player in with Gridspawn in the system browser → `on_signed_in`.
#[no_mangle]
pub extern "C" fn gs_rag_sign_in() -> i32 {
    guard(|| request("sign_in", json!({})))
}

#[no_mangle]
pub extern "C" fn gs_rag_sign_out() -> i32 {
    guard(|| request("sign_out", json!({})))
}

/// A player assertion for the game server's challenge (01 §5) →
/// `on_assertion(request_id, assertion)`. Signed-in players only.
///
/// SAFETY: `challenge` is NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn gs_rag_player_assertion(request_id: u32, challenge: *const c_char) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let Some(challenge) = (unsafe { text(challenge) }) else {
            return GS_RAG_INVALID_ARGUMENT;
        };
        request(
            "player_assertion",
            json!({"requestId": request_id, "challenge": challenge}),
        )
    })
}

/// Attaches the session of a ticket from the game server (02 §5) →
/// `on_attached`. A session already attached is left first (`on_detached`).
///
/// SAFETY: `ticket` is NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn gs_rag_attach(ticket: *const c_char) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let Some(ticket) = (unsafe { text(ticket) }) else {
            return GS_RAG_INVALID_ARGUMENT;
        };
        request("attach", json!({"ticket": ticket}))
    })
}

/// Leaves voice and detaches the attached session → `on_detached("requested")`.
/// Nothing happens when no session is attached.
#[no_mangle]
pub extern "C" fn gs_rag_detach() -> i32 {
    guard(|| request("detach", json!({})))
}

#[no_mangle]
pub extern "C" fn gs_rag_voice_join() -> i32 {
    guard(|| request("voice_join", json!({})))
}

#[no_mangle]
pub extern "C" fn gs_rag_voice_leave() -> i32 {
    guard(|| request("voice_leave", json!({})))
}

#[no_mangle]
pub extern "C" fn gs_rag_voice_set_muted(muted: i32) -> i32 {
    guard(|| request("set_muted", json!({"muted": muted != 0})))
}

#[no_mangle]
pub extern "C" fn gs_rag_voice_set_deafened(deafened: i32) -> i32 {
    guard(|| request("set_deafened", json!({"deafened": deafened != 0})))
}

/// 0 = voice activity, 1 = push-to-talk.
#[no_mangle]
pub extern "C" fn gs_rag_voice_set_transmit_mode(mode: i32) -> i32 {
    guard(|| match mode {
        0 | 1 => request("set_transmit_mode", json!({"pushToTalk": mode == 1})),
        _ => GS_RAG_INVALID_ARGUMENT,
    })
}

/// The push-to-talk key state.
#[no_mangle]
pub extern "C" fn gs_rag_voice_set_transmitting(pressed: i32) -> i32 {
    guard(|| request("set_transmitting", json!({"pressed": pressed != 0})))
}

/// Local volume for one participant, 0.0–2.0.
///
/// SAFETY: `participant` is NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn gs_rag_voice_set_volume(participant: *const c_char, volume: f32) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let Some(participant) = (unsafe { text(participant) }) else {
            return GS_RAG_INVALID_ARGUMENT;
        };
        if !volume.is_finite() {
            return GS_RAG_INVALID_ARGUMENT;
        }
        request(
            "set_volume",
            json!({"participant": participant, "volume": volume}),
        )
    })
}

/// Mutes one participant for this player only.
///
/// SAFETY: `participant` is NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn gs_rag_voice_set_participant_muted(
    participant: *const c_char,
    muted: i32,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let Some(participant) = (unsafe { text(participant) }) else {
            return GS_RAG_INVALID_ARGUMENT;
        };
        request(
            "set_participant_muted",
            json!({"participant": participant, "muted": muted != 0}),
        )
    })
}

/// → `on_devices` once for inputs (kind 0) and once for outputs (kind 1).
#[no_mangle]
pub extern "C" fn gs_rag_voice_list_devices() -> i32 {
    guard(|| request("list_devices", json!({})))
}

/// kind 0 = input (microphone), 1 = output (speakers); name NULL = default.
///
/// SAFETY: `name` is NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn gs_rag_voice_select_device(kind: i32, name: *const c_char) -> i32 {
    guard(|| {
        if !(0..=1).contains(&kind) {
            return GS_RAG_INVALID_ARGUMENT;
        }
        // SAFETY: per the contract.
        let name = unsafe { text(name) };
        request("select_device", json!({"input": kind == 0, "name": name}))
    })
}

/// Signed-in players only (guests have no presence).
///
/// SAFETY: both are NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn gs_rag_presence_set(details: *const c_char, state: *const c_char) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let (details, state) = unsafe { (text(details), text(state)) };
        request("presence_set", json!({"details": details, "state": state}))
    })
}

#[no_mangle]
pub extern "C" fn gs_rag_presence_clear() -> i32 {
    guard(|| request("presence_clear", json!({})))
}

/// Shows (non-zero) or hides (0) the companion's voice window; its close
/// button only hides it. Refused (`companion/no_window`) when `show_window`
/// was 0 at init; a companion older than 0.1.1 answers `companion/unknown_op`.
#[no_mangle]
pub extern "C" fn gs_rag_show_window(visible: i32) -> i32 {
    guard(|| request("show_window", json!({"visible": visible != 0})))
}
