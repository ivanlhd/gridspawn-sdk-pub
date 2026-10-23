//! C ABI for `gridspawn-server`; the rules are the preamble of include/gridspawn_server.h.
//!
//! Every exported function runs its body inside `catch_unwind`, validates its
//! pointers, and calls `gridspawn-server`; no business logic lives here. The
//! header `include/gridspawn_server.h` is generated from this file by cbindgen.
#![deny(unsafe_op_in_unsafe_fn)]
// Every exported function shares one safety contract, written once in the
// header preamble (cbindgen.toml) and in the design note §12.2.
#![allow(clippy::missing_safety_doc)]

use std::{
    ffi::{c_char, c_void, CStr, CString},
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr,
    sync::Arc,
    time::Duration,
};

use gridspawn_server as gs;
use zeroize::Zeroize;

/// ABI version of this library. It changes when a function, a struct layout
/// or an enum value changes incompatibly; adding functions or enum values
/// does not change it.
pub const GS_ABI_VERSION: u32 = 1;

/// Ticket modules, combined with |.
pub const GS_MODULE_PRESENCE: u32 = 1;
pub const GS_MODULE_VOICE: u32 = 2;
pub const GS_MODULE_INTERAKT: u32 = 4;

/// What a call returned. Details are in the GsError, when one was requested.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GsStatus {
    Ok = 0,
    /// A NULL or non-UTF-8 argument, or input the SDK refuses before sending.
    InvalidArgument = 1,
    /// The credential is malformed, unreadable, or readable by other users.
    Credential = 2,
    Network = 3,
    Timeout = 4,
    InvalidResponse = 5,
    Bootstrap = 6,
    /// The SDK edge or Core answered with an error: see gs_error_code.
    Api = 7,
    Runtime = 8,
    /// A bug in the library (a caught panic).
    Internal = 9,
}

/// The documented error codes (protocol 02 and 05, and the voice routes).
/// GS_ERROR_CODE_NONE when the status is not GS_STATUS_API;
/// GS_ERROR_CODE_OTHER for codes this version does not know (see
/// gs_error_wire_code). New codes are added after OTHER.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GsErrorCode {
    None = 0,
    AuthenticationRequired = 1,
    ScopeDenied = 2,
    BrowserForbidden = 3,
    ServiceNotReady = 4,
    SessionInvalidRequest = 5,
    SessionNotFound = 6,
    CapabilityDenied = 7,
    AssertionInvalid = 8,
    AssertionReplayed = 9,
    IdempotencyRequired = 10,
    IdempotencyConflict = 11,
    IdempotencyInProgress = 12,
    AppNotFound = 13,
    ProtocolIncompatible = 14,
    GrantRequired = 15,
    GrantInvalid = 16,
    GrantKeysUnavailable = 17,
    FeatureNotFound = 18,
    InvalidBody = 19,
    InvalidIdempotencyKey = 20,
    ContentRejected = 21,
    BoardFull = 22,
    BoardBusy = 23,
    ChatUnavailable = 24,
    RateLimited = 25,
    Other = 26,
    /// voice/invalid_request: see gs_error_field and gs_error_reason.
    VoiceInvalidRequest = 27,
    /// voice/stale_sequence: a newer update was already accepted.
    VoiceStaleSequence = 28,
    VoiceParticipantNotFound = 29,
    VoiceModuleDisabled = 30,
    /// voice/unavailable (503), retryable.
    VoiceUnavailable = 31,
    /// data/invalid_value: see gs_error_field (the field or JSON path) and
    /// gs_error_reason ("personal_data", "size", "count", ...).
    GameDataInvalidValue = 32,
    /// data/module_disabled: game data is off for the app and environment.
    GameDataModuleDisabled = 33,
    /// data/quota_exceeded (409): gs_error_field is "keys" or "bytes".
    GameDataQuotaExceeded = 34,
    /// data/unavailable (503), retryable; nothing was stored.
    GameDataUnavailable = 35,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GsEnvironment {
    Development = 0,
    Staging = 1,
    Production = 2,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GsIdentityKind {
    GridspawnUser = 0,
    ServerVerifiedGuest = 1,
}

/// Options for gs_client_connect. Fill with gs_client_options_init first.
#[repr(C)]
pub struct GsClientOptions {
    /// Required: the SDK application id.
    pub app_id: *const c_char,
    /// A GsEnvironment value (default GS_ENVIRONMENT_PRODUCTION).
    pub environment: u32,
    /// NULL = https://sdk.gridspawn.net
    pub sdk_base_url: *const c_char,
    /// NULL = https://presence-gateway.gridspawn.net/core/v1/game-servers/posts
    pub club_posts_url: *const c_char,
    /// 0 = 10000
    pub timeout_ms: u32,
    /// 0 = 3 (network errors, timeouts and 5xx; never 429)
    pub max_attempts: u32,
}

/// A club post. NULL fields are absent; title or content is required.
#[repr(C)]
pub struct GsPost {
    pub title: *const c_char,
    pub content: *const c_char,
    pub summary: *const c_char,
    /// http(s); link_label is required with it.
    pub link_url: *const c_char,
    pub link_label: *const c_char,
}

#[repr(C)]
pub struct GsCreateSession {
    pub external_session_id: *const c_char,
    pub idempotency_key: *const c_char,
    /// NULL = absent
    pub region: *const c_char,
    /// 0 = absent
    pub max_participants: u32,
    /// metadata_len pairs; both arrays may be NULL when metadata_len is 0.
    pub metadata_keys: *const *const c_char,
    pub metadata_values: *const *const c_char,
    pub metadata_len: usize,
}

#[repr(C)]
pub struct GsAdmitGuest {
    pub external_player_id: *const c_char,
    pub display_name: *const c_char,
    pub idempotency_key: *const c_char,
    /// NULL = absent
    pub role: *const c_char,
    /// NULL = absent
    pub team: *const c_char,
}

#[repr(C)]
pub struct GsAdmitParticipant {
    pub player_assertion: *const c_char,
    pub challenge: *const c_char,
    pub external_player_id: *const c_char,
    pub idempotency_key: *const c_char,
    /// NULL = absent
    pub role: *const c_char,
    /// NULL = absent
    pub team: *const c_char,
}

/// A player's voice state. Fill with gs_voice_player_state_init first (alive,
/// may speak, no channels, every room).
#[repr(C)]
pub struct GsVoicePlayerState {
    /// NULL = every configured room; otherwise rooms_len room names (0-4).
    pub rooms: *const *const c_char,
    pub rooms_len: usize,
    /// channels_len keys (0-8), for example "team:red"; NULL when 0.
    pub channels: *const *const c_char,
    pub channels_len: usize,
    pub alive: bool,
    pub spectator: bool,
    pub can_speak: bool,
    /// NULL = none. Players in different scenes never hear each other.
    pub scene: *const c_char,
}

/// One player's position: right-handed, y up, metres. yaw (radians, used when
/// has_yaw) is where they face: 0 faces -z, growing turns left. 2D games send
/// (x, 0, y): the pan is computed in the x/z plane.
#[repr(C)]
pub struct GsVoicePosition {
    pub participant_id: *const c_char,
    pub position: [f64; 3],
    pub yaw: f64,
    pub has_yaw: bool,
}

/// The answer to a voice update.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GsVoiceAck {
    /// The rules are enforced now (false until a player of the session joins
    /// voice, or while the voice server's link is down). Not an error.
    pub enforced: bool,
    /// A retried update met "a newer update was already accepted": it landed
    /// earlier or was replaced; enforced is false then.
    pub overtaken: bool,
}

/// Called once on a runtime worker thread when gs_post_async finishes. On
/// success id is set and error is NULL; otherwise id is NULL. id and error are
/// borrowed for the duration of the call. Do not call blocking gs_* functions
/// from here.
pub type GsPostCallback = Option<
    unsafe extern "C" fn(
        user_data: *mut c_void,
        status: GsStatus,
        id: *const c_char,
        duplicate: bool,
        error: *const GsError,
    ),
>;

/// A server credential (gsc1.<id>.<secret>). Consumed by gs_client_connect.
pub struct GsCredential(gs::Credential);

/// A connected client. Thread-safe. Free with gs_client_free.
pub struct GsClient {
    rt: Arc<Rt>,
    client: gs::Client,
}

/// A game session. Free with gs_session_free.
pub struct GsSession {
    rt: Arc<Rt>,
    session: gs::Session,
    id: CString,
}

/// An admitted player or guest. Free with gs_participant_free.
pub struct GsParticipant {
    rt: Arc<Rt>,
    participant: gs::Participant,
    id: CString,
    subject: CString,
    display_name: Option<CString>,
}

/// A session's background renewer. gs_renewer_free stops it.
pub struct GsRenewer {
    handle: Option<gs::RenewerHandle>,
    _rt: Arc<Rt>,
}

/// Sends the latest position of each player at a fixed rate. gs_voice_publisher_free stops it.
pub struct GsVoicePublisher {
    handle: Option<gs::PositionPublisher>,
    _rt: Arc<Rt>,
}

/// The answer to a game-data write. Free with gs_data_ack_free.
pub struct GsDataAck {
    accepted: Vec<CString>,
    skipped: Vec<(CString, u64)>,
    quota: GsDataQuota,
}

/// Sends the latest value of each game-data key at a fixed rate.
/// gs_game_data_publisher_free stops it.
pub struct GsDataPublisher {
    handle: Option<gs::DataPublisher>,
    _rt: Arc<Rt>,
}

/// The game-data quota of the app and environment.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GsDataQuota {
    pub keys: u32,
    pub max_keys: u32,
    pub bytes: u64,
    pub max_bytes: u64,
}

/// One game-data document for gs_game_data_set_many.
#[repr(C)]
pub struct GsDataEntry {
    /// ^[a-z][a-z0-9_.-]{0,63}$
    pub key: *const c_char,
    /// Any JSON text, at most 4 KiB.
    pub value_json: *const c_char,
    /// 0 = no TTL; otherwise 10-86400.
    pub ttl_seconds: u32,
}

/// An error. Free with gs_error_free.
pub struct GsError {
    status: GsStatus,
    code: GsErrorCode,
    wire_code: Option<CString>,
    message: CString,
    http_status: u16,
    request_id: Option<CString>,
    retry_after_ms: i64,
    retryable: bool,
    field: Option<CString>,
    reason: Option<CString>,
}

/// One multi-thread runtime per client, shut down without blocking when the
/// last handle that uses it is freed (safe from any thread, a callback included).
struct Rt(Option<tokio::runtime::Runtime>);

impl Rt {
    fn get(&self) -> &tokio::runtime::Runtime {
        self.0
            .as_ref()
            .expect("the runtime lives until Rt is dropped")
    }
}

impl Drop for Rt {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

fn cstring(text: impl Into<String>) -> CString {
    let mut text = text.into();
    text.retain(|c| c != '\0');
    CString::new(text).expect("NUL bytes removed")
}

impl GsError {
    fn new(status: GsStatus, message: impl Into<String>) -> Self {
        Self {
            status,
            code: GsErrorCode::None,
            wire_code: None,
            message: cstring(message),
            http_status: 0,
            request_id: None,
            retry_after_ms: -1,
            retryable: false,
            field: None,
            reason: None,
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(GsStatus::InvalidArgument, message)
    }

    fn from_sdk(error: &gs::Error) -> Self {
        let status = match error {
            gs::Error::InvalidInput {
                field: "credential",
                ..
            }
            | gs::Error::CredentialUnreadable(_)
            | gs::Error::CredentialFileTooOpen { .. } => GsStatus::Credential,
            gs::Error::InvalidInput { .. } => GsStatus::InvalidArgument,
            gs::Error::Network(_) => GsStatus::Network,
            gs::Error::Timeout => GsStatus::Timeout,
            gs::Error::InvalidResponse(_) => GsStatus::InvalidResponse,
            gs::Error::Bootstrap(_) => GsStatus::Bootstrap,
            gs::Error::Runtime(_) => GsStatus::Runtime,
            gs::Error::Api(_) => GsStatus::Api,
            _ => GsStatus::Internal,
        };
        let mut out = Self::new(status, error.to_string());
        out.retryable = error.is_retryable();
        if let Some(api) = error.api() {
            out.code = error_code(&api.code);
            out.wire_code = Some(cstring(api.wire_code.clone()));
            out.http_status = api.status;
            out.request_id = api.request_id.clone().map(cstring);
            out.retry_after_ms = api.retry_after.map_or(-1, |wait| {
                i64::try_from(wait.as_millis()).unwrap_or(i64::MAX)
            });
            out.field = api.field.clone().map(cstring);
            out.reason = api.reason.clone().map(cstring);
        }
        out
    }
}

fn error_code(code: &gs::ErrorCode) -> GsErrorCode {
    use gs::ErrorCode as C;
    match code {
        C::AuthenticationRequired => GsErrorCode::AuthenticationRequired,
        C::ScopeDenied => GsErrorCode::ScopeDenied,
        C::BrowserForbidden => GsErrorCode::BrowserForbidden,
        C::ServiceNotReady => GsErrorCode::ServiceNotReady,
        C::SessionInvalidRequest => GsErrorCode::SessionInvalidRequest,
        C::SessionNotFound => GsErrorCode::SessionNotFound,
        C::CapabilityDenied => GsErrorCode::CapabilityDenied,
        C::AssertionInvalid => GsErrorCode::AssertionInvalid,
        C::AssertionReplayed => GsErrorCode::AssertionReplayed,
        C::IdempotencyRequired => GsErrorCode::IdempotencyRequired,
        C::IdempotencyConflict => GsErrorCode::IdempotencyConflict,
        C::IdempotencyInProgress => GsErrorCode::IdempotencyInProgress,
        C::AppNotFound => GsErrorCode::AppNotFound,
        C::ProtocolIncompatible => GsErrorCode::ProtocolIncompatible,
        C::GrantRequired => GsErrorCode::GrantRequired,
        C::GrantInvalid => GsErrorCode::GrantInvalid,
        C::GrantKeysUnavailable => GsErrorCode::GrantKeysUnavailable,
        C::FeatureNotFound => GsErrorCode::FeatureNotFound,
        C::InvalidBody => GsErrorCode::InvalidBody,
        C::InvalidIdempotencyKey => GsErrorCode::InvalidIdempotencyKey,
        C::ContentRejected(_) => GsErrorCode::ContentRejected,
        C::BoardFull => GsErrorCode::BoardFull,
        C::BoardBusy => GsErrorCode::BoardBusy,
        C::ChatUnavailable => GsErrorCode::ChatUnavailable,
        C::RateLimited => GsErrorCode::RateLimited,
        C::VoiceInvalidRequest => GsErrorCode::VoiceInvalidRequest,
        C::VoiceStaleSequence => GsErrorCode::VoiceStaleSequence,
        C::VoiceParticipantNotFound => GsErrorCode::VoiceParticipantNotFound,
        C::VoiceModuleDisabled => GsErrorCode::VoiceModuleDisabled,
        C::VoiceUnavailable => GsErrorCode::VoiceUnavailable,
        C::GameDataInvalidValue => GsErrorCode::GameDataInvalidValue,
        C::GameDataModuleDisabled => GsErrorCode::GameDataModuleDisabled,
        C::GameDataQuotaExceeded => GsErrorCode::GameDataQuotaExceeded,
        C::GameDataUnavailable => GsErrorCode::GameDataUnavailable,
        _ => GsErrorCode::Other,
    }
}

/// Runs an exported function's body: catches panics and writes the error.
///
/// SAFETY: `error_out` is NULL or a valid, writable `GsError **`.
unsafe fn boundary(
    error_out: *mut *mut GsError,
    body: impl FnOnce() -> Result<(), GsError>,
) -> GsStatus {
    let result = catch_unwind(AssertUnwindSafe(body))
        .unwrap_or_else(|_| Err(GsError::new(GsStatus::Internal, "internal error")));
    let (status, error) = match result {
        Ok(()) => (GsStatus::Ok, None),
        Err(error) => (error.status, Some(error)),
    };
    if !error_out.is_null() {
        let value = error.map_or(ptr::null_mut(), |error| Box::into_raw(Box::new(error)));
        // SAFETY: non-NULL and valid per this function's contract.
        unsafe { error_out.write(value) };
    }
    status
}

/// Runs `body` for a function without a status, swallowing panics.
fn quietly(body: impl FnOnce()) {
    let _ = catch_unwind(AssertUnwindSafe(body));
}

fn require<T>(pointer: *const T, name: &str) -> Result<(), GsError> {
    if pointer.is_null() {
        Err(GsError::invalid(format!("{name} is NULL")))
    } else {
        Ok(())
    }
}

/// SAFETY: `pointer` is NULL or a NUL-terminated string valid for `'a`.
unsafe fn text<'a>(pointer: *const c_char, name: &str) -> Result<&'a str, GsError> {
    require(pointer, name)?;
    // SAFETY: non-NULL and NUL-terminated per the contract.
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .map_err(|_| GsError::invalid(format!("{name} is not UTF-8")))
}

/// SAFETY: as `text`; NULL means absent.
unsafe fn optional_text<'a>(
    pointer: *const c_char,
    name: &str,
) -> Result<Option<&'a str>, GsError> {
    if pointer.is_null() {
        Ok(None)
    } else {
        // SAFETY: as above.
        unsafe { text(pointer, name) }.map(Some)
    }
}

/// SAFETY: `pointer` is NULL or points to a live `T` for `'a`.
unsafe fn handle<'a, T>(pointer: *const T, name: &str) -> Result<&'a T, GsError> {
    require(pointer, name)?;
    // SAFETY: non-NULL and live per the contract.
    Ok(unsafe { &*pointer })
}

/// SAFETY: `out` is a valid, writable pointer (checked non-NULL by the caller).
unsafe fn put<T>(out: *mut T, value: T) {
    // SAFETY: per the contract.
    unsafe { out.write(value) };
}

/// Blocks on `future` with the client's runtime, refusing to do so from inside
/// a runtime (a GsPostCallback, or the caller's own async runtime).
fn block_on<F: Future>(rt: &Rt, future: F) -> Result<F::Output, GsError> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(GsError::invalid(
            "blocking call from a Gridspawn callback or from inside an async runtime",
        ));
    }
    Ok(rt.get().block_on(future))
}

fn sdk<T>(result: Result<T, gs::Error>) -> Result<T, GsError> {
    result.map_err(|error| GsError::from_sdk(&error))
}

// ---- library ---------------------------------------------------------------

/// The library version, e.g. "0.1.0". Static; do not free.
#[no_mangle]
pub extern "C" fn gs_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// GS_ABI_VERSION of the loaded library. Check it equals the header's.
#[no_mangle]
pub extern "C" fn gs_abi_version() -> u32 {
    GS_ABI_VERSION
}

/// Frees a string the library returned through a char ** out-parameter,
/// zeroizing it first. NULL is ignored.
#[no_mangle]
pub unsafe extern "C" fn gs_string_free(value: *mut c_char) {
    if value.is_null() {
        return;
    }
    quietly(|| {
        // SAFETY: the library allocated it with CString::into_raw (header contract).
        let owned = unsafe { CString::from_raw(value) };
        owned.into_bytes_with_nul().zeroize();
    });
}

#[no_mangle]
pub unsafe extern "C" fn gs_environment_parse(
    value: *const c_char,
    out: *mut GsEnvironment,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        require(out, "out")?;
        // SAFETY: header contract.
        let value = unsafe { text(value, "value") }?;
        let environment = match sdk(value.parse::<gs::Environment>())? {
            gs::Environment::Development => GsEnvironment::Development,
            gs::Environment::Staging => GsEnvironment::Staging,
            gs::Environment::Production => GsEnvironment::Production,
        };
        // SAFETY: checked non-NULL above.
        unsafe { put(out, environment) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

// ---- credentials -----------------------------------------------------------

/// SAFETY: header contract for every pointer.
unsafe fn credential_from(
    load: impl FnOnce(&str) -> Result<gs::Credential, gs::Error>,
    input: *const c_char,
    name: &str,
    out: *mut *mut GsCredential,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        require(out, "out")?;
        // SAFETY: header contract.
        let input = unsafe { text(input, name) }?;
        let credential = sdk(load(input))?;
        // SAFETY: checked non-NULL above.
        unsafe { put(out, Box::into_raw(Box::new(GsCredential(credential)))) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// Reads a credential file; refuses a file group or others can read
/// (fix: chmod 600 <path>).
#[no_mangle]
pub unsafe extern "C" fn gs_credential_from_file(
    path: *const c_char,
    out: *mut *mut GsCredential,
    error: *mut *mut GsError,
) -> GsStatus {
    // SAFETY: forwarded header contract.
    unsafe {
        credential_from(
            |path| gs::Credential::from_file(path),
            path,
            "path",
            out,
            error,
        )
    }
}

#[no_mangle]
pub unsafe extern "C" fn gs_credential_from_file_unchecked(
    path: *const c_char,
    out: *mut *mut GsCredential,
    error: *mut *mut GsError,
) -> GsStatus {
    // SAFETY: forwarded header contract.
    unsafe {
        credential_from(
            |path| gs::Credential::from_file_unchecked(path),
            path,
            "path",
            out,
            error,
        )
    }
}

#[no_mangle]
pub unsafe extern "C" fn gs_credential_from_env(
    name: *const c_char,
    out: *mut *mut GsCredential,
    error: *mut *mut GsError,
) -> GsStatus {
    // SAFETY: forwarded header contract.
    unsafe { credential_from(gs::Credential::from_env, name, "name", out, error) }
}

#[no_mangle]
pub unsafe extern "C" fn gs_credential_from_string(
    value: *const c_char,
    out: *mut *mut GsCredential,
    error: *mut *mut GsError,
) -> GsStatus {
    // SAFETY: forwarded header contract.
    unsafe {
        credential_from(
            |value| gs::Credential::parse(value),
            value,
            "value",
            out,
            error,
        )
    }
}

#[no_mangle]
pub unsafe extern "C" fn gs_credential_free(credential: *mut GsCredential) {
    if credential.is_null() {
        return;
    }
    // SAFETY: allocated by the library, freed once (header contract).
    quietly(|| drop(unsafe { Box::from_raw(credential) }));
}

// ---- client ----------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn gs_client_options_init(options: *mut GsClientOptions) {
    if options.is_null() {
        return;
    }
    let defaults = GsClientOptions {
        app_id: ptr::null(),
        environment: GsEnvironment::Production as u32,
        sdk_base_url: ptr::null(),
        club_posts_url: ptr::null(),
        timeout_ms: 0,
        max_attempts: 0,
    };
    // SAFETY: non-NULL; the caller passes writable storage for the struct.
    unsafe { put(options, defaults) };
}

/// Checks bootstrap and returns a client. Always consumes credential.
#[no_mangle]
pub unsafe extern "C" fn gs_client_connect(
    options: *const GsClientOptions,
    credential: *mut GsCredential,
    out: *mut *mut GsClient,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        // Take ownership first, so the credential is consumed on every path.
        let credential = if credential.is_null() {
            None
        } else {
            // SAFETY: allocated by the library and handed over (header contract).
            Some(unsafe { Box::from_raw(credential) }.0)
        };
        let credential = credential.ok_or_else(|| GsError::invalid("credential is NULL"))?;
        require(out, "out")?;
        // SAFETY: header contract.
        let options = unsafe { handle(options, "options") }?;
        // SAFETY: header contract for the strings inside.
        let app_id = unsafe { text(options.app_id, "app_id") }?;
        let environment = match options.environment {
            0 => gs::Environment::Development,
            1 => gs::Environment::Staging,
            2 => gs::Environment::Production,
            _ => return Err(GsError::invalid("environment is not a GsEnvironment value")),
        };
        let mut builder = gs::Client::builder(app_id, environment, credential);
        // SAFETY: header contract.
        if let Some(base) = unsafe { optional_text(options.sdk_base_url, "sdk_base_url") }? {
            builder = builder.sdk_base_url(
                url::Url::parse(base).map_err(|_| GsError::invalid("sdk_base_url is not a URL"))?,
            );
        }
        // SAFETY: header contract.
        if let Some(posts) = unsafe { optional_text(options.club_posts_url, "club_posts_url") }? {
            builder = builder.club_posts_url(
                url::Url::parse(posts)
                    .map_err(|_| GsError::invalid("club_posts_url is not a URL"))?,
            );
        }
        if options.timeout_ms > 0 {
            builder = builder.timeout(Duration::from_millis(u64::from(options.timeout_ms)));
        }
        if options.max_attempts > 0 {
            let mut policy = gs::RetryPolicy::default();
            policy.max_attempts = options.max_attempts;
            builder = builder.retry_policy(policy);
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("gridspawn-server")
            .enable_all()
            .build()
            .map_err(|error| GsError::new(GsStatus::Runtime, error.to_string()))?;
        let rt = Arc::new(Rt(Some(runtime)));
        let client = sdk(block_on(&rt, builder.connect())?)?;
        // SAFETY: checked non-NULL above.
        unsafe { put(out, Box::into_raw(Box::new(GsClient { rt, client }))) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

#[no_mangle]
pub unsafe extern "C" fn gs_client_free(client: *mut GsClient) {
    if client.is_null() {
        return;
    }
    // SAFETY: allocated by the library, freed once (header contract).
    quietly(|| drop(unsafe { Box::from_raw(client) }));
}

// ---- club posts ------------------------------------------------------------

/// SAFETY: header contract for every pointer.
unsafe fn post_arguments(
    feature: *const c_char,
    post: *const GsPost,
    idempotency_key: *const c_char,
) -> Result<(String, gs::Post, gs::IdempotencyKey), GsError> {
    // SAFETY: header contract.
    let feature = unsafe { text(feature, "feature") }?.to_owned();
    // SAFETY: header contract.
    let post = unsafe { handle(post, "post") }?;
    let mut body = gs::Post::default();
    // SAFETY: header contract for the strings inside.
    unsafe {
        body.title = optional_text(post.title, "title")?.map(str::to_owned);
        body.content = optional_text(post.content, "content")?.map(str::to_owned);
        body.summary = optional_text(post.summary, "summary")?.map(str::to_owned);
        match (
            optional_text(post.link_url, "link_url")?,
            optional_text(post.link_label, "link_label")?,
        ) {
            (Some(url), Some(label)) => {
                let url =
                    url::Url::parse(url).map_err(|_| GsError::invalid("link_url is not a URL"))?;
                body = body.link(url, label);
            }
            (None, None) => {}
            _ => return Err(GsError::invalid("link_url and link_label go together")),
        }
    }
    // SAFETY: header contract.
    let key = sdk(gs::IdempotencyKey::new(unsafe {
        text(idempotency_key, "idempotency_key")
    }?))?;
    Ok((feature, body, key))
}

/// Posts to the destination the game's club chose for feature (protocol 05).
/// The same idempotency_key never posts twice. *id_out: free with gs_string_free.
#[no_mangle]
pub unsafe extern "C" fn gs_post(
    client: *mut GsClient,
    feature: *const c_char,
    post: *const GsPost,
    idempotency_key: *const c_char,
    id_out: *mut *mut c_char,
    duplicate_out: *mut bool,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        require(id_out, "id_out")?;
        require(duplicate_out, "duplicate_out")?;
        // SAFETY: header contract.
        let client = unsafe { handle(client, "client") }?;
        // SAFETY: header contract.
        let (feature, body, key) = unsafe { post_arguments(feature, post, idempotency_key) }?;
        let receipt = sdk(block_on(
            &client.rt,
            client.client.post(&feature, body, &key),
        )?)?;
        // SAFETY: both checked non-NULL above.
        unsafe {
            put(id_out, cstring(receipt.id).into_raw());
            put(duplicate_out, receipt.duplicate);
        }
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

struct UserData(*mut c_void);

impl UserData {
    /// Consumes the wrapper as a whole, so async blocks capture it (Send), not the pointer.
    fn into_pointer(self) -> *mut c_void {
        self.0
    }
}
// SAFETY: the caller hands user_data to the library to pass back on another
// thread; that is the documented contract of gs_post_async.
unsafe impl Send for UserData {}

/// Like gs_post without blocking. Arguments are checked and copied before it
/// returns; GS_STATUS_OK means the callback will be called exactly once
/// (unless the client is freed first).
#[no_mangle]
pub unsafe extern "C" fn gs_post_async(
    client: *mut GsClient,
    feature: *const c_char,
    post: *const GsPost,
    idempotency_key: *const c_char,
    callback: GsPostCallback,
    user_data: *mut c_void,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        let callback = callback.ok_or_else(|| GsError::invalid("callback is NULL"))?;
        // SAFETY: header contract.
        let handle = unsafe { handle(client, "client") }?;
        // SAFETY: header contract.
        let (feature, body, key) = unsafe { post_arguments(feature, post, idempotency_key) }?;
        let client = handle.client.clone();
        let user = UserData(user_data);
        handle.rt.get().spawn(async move {
            let result = client.post(&feature, body, &key).await;
            let outcome = catch_unwind(AssertUnwindSafe(|| match &result {
                Ok(receipt) => (
                    GsStatus::Ok,
                    Some(cstring(receipt.id.clone())),
                    receipt.duplicate,
                    None,
                ),
                Err(error) => {
                    let error = GsError::from_sdk(error);
                    (error.status, None, false, Some(error))
                }
            }))
            .unwrap_or_else(|_| {
                let error = GsError::new(GsStatus::Internal, "internal error");
                (GsStatus::Internal, None, false, Some(error))
            });
            let (status, id, duplicate, error) = outcome;
            let user_data = user.into_pointer();
            // SAFETY: the caller's callback with its own user_data, called once;
            // id and error stay alive for the call.
            unsafe {
                callback(
                    user_data,
                    status,
                    id.as_ref().map_or(ptr::null(), |id| id.as_ptr()),
                    duplicate,
                    error
                        .as_ref()
                        .map_or(ptr::null(), |error| error as *const GsError),
                );
            }
        });
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

// ---- sessions --------------------------------------------------------------

fn session_handle(rt: &Arc<Rt>, session: gs::Session) -> *mut GsSession {
    let id = cstring(session.id().as_str());
    Box::into_raw(Box::new(GsSession {
        rt: rt.clone(),
        session,
        id,
    }))
}

#[no_mangle]
pub unsafe extern "C" fn gs_session_create(
    client: *mut GsClient,
    request: *const GsCreateSession,
    out: *mut *mut GsSession,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        require(out, "out")?;
        // SAFETY: header contract.
        let client = unsafe { handle(client, "client") }?;
        // SAFETY: header contract.
        let request = unsafe { handle(request, "request") }?;
        // SAFETY: header contract for the strings and arrays inside.
        let create = unsafe {
            let key = sdk(gs::IdempotencyKey::new(text(
                request.idempotency_key,
                "idempotency_key",
            )?))?;
            let mut create = gs::CreateSession::new(
                text(request.external_session_id, "external_session_id")?,
                key,
            );
            if let Some(region) = optional_text(request.region, "region")? {
                create = create.region(region);
            }
            if request.max_participants > 0 {
                create = create.max_participants(request.max_participants);
            }
            if request.metadata_len > 0 {
                require(request.metadata_keys, "metadata_keys")?;
                require(request.metadata_values, "metadata_values")?;
                for index in 0..request.metadata_len {
                    let key = text(*request.metadata_keys.add(index), "metadata key")?;
                    let value = text(*request.metadata_values.add(index), "metadata value")?;
                    create = create.metadata(key, value);
                }
            }
            create
        };
        let session = sdk(block_on(&client.rt, client.client.create_session(create))?)?;
        // SAFETY: checked non-NULL above.
        unsafe { put(out, session_handle(&client.rt, session)) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// A handle for a session created earlier. No network call.
#[no_mangle]
pub unsafe extern "C" fn gs_session_open(
    client: *mut GsClient,
    session_id: *const c_char,
    out: *mut *mut GsSession,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        require(out, "out")?;
        // SAFETY: header contract.
        let client = unsafe { handle(client, "client") }?;
        // SAFETY: header contract.
        let id = unsafe { text(session_id, "session_id") }?;
        if id.is_empty() {
            return Err(GsError::invalid("session_id is empty"));
        }
        let session = client.client.session(gs::SessionId::new(id));
        // SAFETY: checked non-NULL above.
        unsafe { put(out, session_handle(&client.rt, session)) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// Borrowed; valid until gs_session_free.
#[no_mangle]
pub unsafe extern "C" fn gs_session_id(session: *const GsSession) -> *const c_char {
    // SAFETY: NULL or a live handle (header contract).
    match unsafe { session.as_ref() } {
        Some(session) => session.id.as_ptr(),
        None => ptr::null(),
    }
}

/// renew_after_ms_out may be NULL.
#[no_mangle]
pub unsafe extern "C" fn gs_session_renew(
    session: *mut GsSession,
    renew_after_ms_out: *mut u64,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        // SAFETY: header contract.
        let session = unsafe { handle(session, "session") }?;
        let lease = sdk(block_on(&session.rt, session.session.renew())?)?;
        if !renew_after_ms_out.is_null() {
            let millis = u64::try_from(lease.renew_after.as_millis()).unwrap_or(u64::MAX);
            // SAFETY: non-NULL and writable (header contract).
            unsafe { put(renew_after_ms_out, millis) };
        }
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

#[no_mangle]
pub unsafe extern "C" fn gs_session_spawn_renewer(
    session: *mut GsSession,
    out: *mut *mut GsRenewer,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        require(out, "out")?;
        // SAFETY: header contract.
        let session = unsafe { handle(session, "session") }?;
        let renewer = {
            let _entered = session.rt.get().enter();
            session.session.spawn_renewer()
        };
        let handle = GsRenewer {
            handle: Some(renewer),
            _rt: session.rt.clone(),
        };
        // SAFETY: checked non-NULL above.
        unsafe { put(out, Box::into_raw(Box::new(handle))) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

#[no_mangle]
pub unsafe extern "C" fn gs_renewer_free(renewer: *mut GsRenewer) {
    if renewer.is_null() {
        return;
    }
    quietly(|| {
        // SAFETY: allocated by the library, freed once (header contract).
        let mut renewer = unsafe { Box::from_raw(renewer) };
        // Stop the task before the runtime reference goes.
        drop(renewer.handle.take());
    });
}

#[no_mangle]
pub unsafe extern "C" fn gs_session_free(session: *mut GsSession) {
    if session.is_null() {
        return;
    }
    // SAFETY: allocated by the library, freed once (header contract).
    quietly(|| drop(unsafe { Box::from_raw(session) }));
}

// ---- participants ----------------------------------------------------------

fn participant_handle(rt: &Arc<Rt>, participant: gs::Participant) -> *mut GsParticipant {
    let id = cstring(participant.id.as_str());
    let subject = cstring(participant.subject.clone());
    let display_name = participant.display_name.clone().map(cstring);
    Box::into_raw(Box::new(GsParticipant {
        rt: rt.clone(),
        participant,
        id,
        subject,
        display_name,
    }))
}

#[no_mangle]
pub unsafe extern "C" fn gs_admit_guest(
    session: *mut GsSession,
    request: *const GsAdmitGuest,
    out: *mut *mut GsParticipant,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        require(out, "out")?;
        // SAFETY: header contract.
        let session = unsafe { handle(session, "session") }?;
        // SAFETY: header contract.
        let request = unsafe { handle(request, "request") }?;
        // SAFETY: header contract for the strings inside.
        let admit = unsafe {
            let key = sdk(gs::IdempotencyKey::new(text(
                request.idempotency_key,
                "idempotency_key",
            )?))?;
            let mut admit = gs::AdmitGuest::new(
                text(request.external_player_id, "external_player_id")?,
                text(request.display_name, "display_name")?,
                key,
            );
            if let Some(role) = optional_text(request.role, "role")? {
                admit = admit.role(role);
            }
            if let Some(team) = optional_text(request.team, "team")? {
                admit = admit.team(team);
            }
            admit
        };
        let participant = sdk(block_on(&session.rt, session.session.admit_guest(admit))?)?;
        // SAFETY: checked non-NULL above.
        unsafe { put(out, participant_handle(&session.rt, participant)) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

#[no_mangle]
pub unsafe extern "C" fn gs_admit_participant(
    session: *mut GsSession,
    request: *const GsAdmitParticipant,
    out: *mut *mut GsParticipant,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        require(out, "out")?;
        // SAFETY: header contract.
        let session = unsafe { handle(session, "session") }?;
        // SAFETY: header contract.
        let request = unsafe { handle(request, "request") }?;
        // SAFETY: header contract for the strings inside.
        let admit = unsafe {
            let key = sdk(gs::IdempotencyKey::new(text(
                request.idempotency_key,
                "idempotency_key",
            )?))?;
            let mut admit = gs::AdmitParticipant::new(
                text(request.player_assertion, "player_assertion")?,
                text(request.challenge, "challenge")?,
                text(request.external_player_id, "external_player_id")?,
                key,
            );
            if let Some(role) = optional_text(request.role, "role")? {
                admit = admit.role(role);
            }
            if let Some(team) = optional_text(request.team, "team")? {
                admit = admit.team(team);
            }
            admit
        };
        let participant = sdk(block_on(
            &session.rt,
            session.session.admit_participant(admit),
        )?)?;
        // SAFETY: checked non-NULL above.
        unsafe { put(out, participant_handle(&session.rt, participant)) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// Borrowed; valid until gs_participant_free.
#[no_mangle]
pub unsafe extern "C" fn gs_participant_id(participant: *const GsParticipant) -> *const c_char {
    // SAFETY: NULL or a live handle (header contract).
    unsafe { participant.as_ref() }.map_or(ptr::null(), |p| p.id.as_ptr())
}

#[no_mangle]
pub unsafe extern "C" fn gs_participant_subject(
    participant: *const GsParticipant,
) -> *const c_char {
    // SAFETY: NULL or a live handle (header contract).
    unsafe { participant.as_ref() }.map_or(ptr::null(), |p| p.subject.as_ptr())
}

/// NULL when absent (always set for guests).
#[no_mangle]
pub unsafe extern "C" fn gs_participant_display_name(
    participant: *const GsParticipant,
) -> *const c_char {
    // SAFETY: NULL or a live handle (header contract).
    unsafe { participant.as_ref() }
        .and_then(|p| p.display_name.as_ref())
        .map_or(ptr::null(), |name| name.as_ptr())
}

#[no_mangle]
pub unsafe extern "C" fn gs_participant_identity_kind(
    participant: *const GsParticipant,
) -> GsIdentityKind {
    // SAFETY: NULL or a live handle (header contract).
    match unsafe { participant.as_ref() }.map(|p| p.participant.identity_kind) {
        Some(gs::IdentityKind::ServerVerifiedGuest) => GsIdentityKind::ServerVerifiedGuest,
        _ => GsIdentityKind::GridspawnUser,
    }
}

/// A single-use client ticket (120 s) for modules (GS_MODULE_* combined with |).
/// Never retried. *ticket_out: free with gs_string_free. expires_in_ms_out may be NULL.
#[no_mangle]
pub unsafe extern "C" fn gs_issue_ticket(
    participant: *mut GsParticipant,
    modules: u32,
    ticket_out: *mut *mut c_char,
    expires_in_ms_out: *mut u64,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        require(ticket_out, "ticket_out")?;
        // SAFETY: header contract.
        let participant = unsafe { handle(participant, "participant") }?;
        let known = GS_MODULE_PRESENCE | GS_MODULE_VOICE | GS_MODULE_INTERAKT;
        if modules == 0 || modules & !known != 0 {
            return Err(GsError::invalid("modules must combine GS_MODULE_* flags"));
        }
        let mut list = Vec::new();
        for (flag, module) in [
            (GS_MODULE_PRESENCE, gs::Module::Presence),
            (GS_MODULE_VOICE, gs::Module::Voice),
            (GS_MODULE_INTERAKT, gs::Module::Interakt),
        ] {
            if modules & flag != 0 {
                list.push(module);
            }
        }
        let ticket = sdk(block_on(
            &participant.rt,
            participant.participant.issue_ticket(&list),
        )?)?;
        let expires = u64::try_from(ticket.expires_in().as_millis()).unwrap_or(u64::MAX);
        // SAFETY: checked non-NULL above; expires_in_ms_out is optional.
        unsafe {
            put(ticket_out, cstring(ticket.into_string()).into_raw());
            if !expires_in_ms_out.is_null() {
                put(expires_in_ms_out, expires);
            }
        }
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

#[no_mangle]
pub unsafe extern "C" fn gs_participant_free(participant: *mut GsParticipant) {
    if participant.is_null() {
        return;
    }
    // SAFETY: allocated by the library, freed once (header contract).
    quietly(|| drop(unsafe { Box::from_raw(participant) }));
}

// ---- voice -----------------------------------------------------------------

/// SAFETY: `out` is NULL or a valid, writable pointer.
unsafe fn write_ack(out: *mut GsVoiceAck, ack: gs::VoiceAck) {
    if !out.is_null() {
        // SAFETY: non-NULL and writable per the contract.
        unsafe {
            put(
                out,
                GsVoiceAck {
                    enforced: ack.enforced,
                    overtaken: ack.overtaken,
                },
            )
        };
    }
}

/// SAFETY: `items` is NULL or points to `len` NUL-terminated strings.
unsafe fn text_list(
    items: *const *const c_char,
    len: usize,
    name: &str,
) -> Result<Vec<String>, GsError> {
    if len == 0 {
        return Ok(Vec::new());
    }
    require(items, name)?;
    (0..len)
        // SAFETY: `len` valid entries per the contract.
        .map(|index| unsafe { text(*items.add(index), name) }.map(str::to_owned))
        .collect()
}

/// Defaults: every room, no channels, alive, not a spectator, may speak, no scene.
#[no_mangle]
pub unsafe extern "C" fn gs_voice_player_state_init(state: *mut GsVoicePlayerState) {
    if state.is_null() {
        return;
    }
    quietly(|| {
        let defaults = GsVoicePlayerState {
            rooms: ptr::null(),
            rooms_len: 0,
            channels: ptr::null(),
            channels_len: 0,
            alive: true,
            spectator: false,
            can_speak: true,
            scene: ptr::null(),
        };
        // SAFETY: non-NULL and writable (header contract).
        unsafe { put(state, defaults) };
    });
}

/// Replaces the session's voice rooms. config_json is the wire shape, for
/// example {"rooms": {"match": {"mode": "channels"}}}; it is checked before
/// sending. ack_out may be NULL.
#[no_mangle]
pub unsafe extern "C" fn gs_voice_configure(
    session: *mut GsSession,
    config_json: *const c_char,
    ack_out: *mut GsVoiceAck,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        // SAFETY: header contract.
        let session = unsafe { handle(session, "session") }?;
        // SAFETY: header contract.
        let config = sdk(gs::VoiceConfig::from_json(unsafe {
            text(config_json, "config_json")
        }?))?;
        let ack = sdk(block_on(
            &session.rt,
            session.session.voice().configure(&config),
        )?)?;
        // SAFETY: NULL or writable (header contract).
        unsafe { write_ack(ack_out, ack) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// Sets one player's state. Call it before issuing the player's voice ticket:
/// in a managed room a player without a state hears no one and is heard by
/// no one. state->rooms NULL = every room; non-NULL with rooms_len 0 = no
/// room. ack_out may be NULL.
#[no_mangle]
pub unsafe extern "C" fn gs_voice_set_player_state(
    session: *mut GsSession,
    participant_id: *const c_char,
    state: *const GsVoicePlayerState,
    ack_out: *mut GsVoiceAck,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        // SAFETY: header contract.
        let session = unsafe { handle(session, "session") }?;
        // SAFETY: header contract.
        let participant =
            gs::ParticipantId::new(unsafe { text(participant_id, "participant_id") }?);
        // SAFETY: header contract.
        let state = unsafe { handle(state, "state") }?;
        // SAFETY: header contract for the arrays and strings inside.
        let player = unsafe {
            let mut player = gs::VoicePlayerState::new()
                .channels(text_list(state.channels, state.channels_len, "channels")?)
                .alive(state.alive)
                .spectator(state.spectator)
                .can_speak(state.can_speak);
            if !state.rooms.is_null() {
                player = player.rooms(text_list(state.rooms, state.rooms_len, "rooms")?);
            }
            if let Some(scene) = optional_text(state.scene, "scene")? {
                player = player.scene(scene);
            }
            player
        };
        let ack = sdk(block_on(
            &session.rt,
            session
                .session
                .voice()
                .set_player_state(&participant, &player),
        )?)?;
        // SAFETY: NULL or writable (header contract).
        unsafe { write_ack(ack_out, ack) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// The authoritative mute (also after the player reconnects). ack_out may be NULL.
#[no_mangle]
pub unsafe extern "C" fn gs_voice_mute(
    session: *mut GsSession,
    participant_id: *const c_char,
    muted: bool,
    ack_out: *mut GsVoiceAck,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        // SAFETY: header contract.
        let session = unsafe { handle(session, "session") }?;
        // SAFETY: header contract.
        let participant =
            gs::ParticipantId::new(unsafe { text(participant_id, "participant_id") }?);
        let ack = sdk(block_on(
            &session.rt,
            session.session.voice().mute(&participant, muted),
        )?)?;
        // SAFETY: NULL or writable (header contract).
        unsafe { write_ack(ack_out, ack) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// SAFETY: `entry` points to a live GsVoicePosition with a valid id string.
unsafe fn position(entry: &GsVoicePosition) -> Result<gs::VoicePosition, GsError> {
    // SAFETY: per the contract.
    let id = gs::ParticipantId::new(unsafe { text(entry.participant_id, "participant_id") }?);
    let mut position = gs::VoicePosition::new(&id, entry.position);
    if entry.has_yaw {
        position = position.yaw(entry.yaw);
    }
    Ok(position)
}

/// One batch of 1-256 positions. A position counts fully for 1 s after it
/// arrives: send several times a second, or use gs_voice_publisher_spawn.
/// ack_out may be NULL.
#[no_mangle]
pub unsafe extern "C" fn gs_voice_publish_positions(
    session: *mut GsSession,
    positions: *const GsVoicePosition,
    count: usize,
    ack_out: *mut GsVoiceAck,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        // SAFETY: header contract.
        let session = unsafe { handle(session, "session") }?;
        require(positions, "positions")?;
        let batch = (0..count)
            // SAFETY: `count` live entries (header contract).
            .map(|index| unsafe { position(&*positions.add(index)) })
            .collect::<Result<Vec<_>, _>>()?;
        let ack = sdk(block_on(
            &session.rt,
            session.session.voice().publish_positions(&batch),
        )?)?;
        // SAFETY: NULL or writable (header contract).
        unsafe { write_ack(ack_out, ack) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// Starts a publisher on the client's runtime: hz times a second (1-10; 2 or
/// more keeps every position inside the 1 s hold) it sends the latest
/// position of every player given to gs_voice_publisher_update.
#[no_mangle]
pub unsafe extern "C" fn gs_voice_publisher_spawn(
    session: *mut GsSession,
    hz: u32,
    out: *mut *mut GsVoicePublisher,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        require(out, "out")?;
        // SAFETY: header contract.
        let session = unsafe { handle(session, "session") }?;
        let publisher = {
            let _entered = session.rt.get().enter();
            session.session.voice().spawn_position_publisher(hz)
        };
        let handle = GsVoicePublisher {
            handle: Some(publisher),
            _rt: session.rt.clone(),
        };
        // SAFETY: checked non-NULL above.
        unsafe { put(out, Box::into_raw(Box::new(handle))) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// A player's latest position (cheap; call every tick). Checked here. Never
/// blocks, so it may be called from anywhere.
#[no_mangle]
pub unsafe extern "C" fn gs_voice_publisher_update(
    publisher: *mut GsVoicePublisher,
    participant_id: *const c_char,
    x: f64,
    y: f64,
    z: f64,
    yaw: f64,
    has_yaw: bool,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        // SAFETY: header contract.
        let publisher = unsafe { handle(publisher, "publisher") }?;
        // SAFETY: header contract.
        let id = gs::ParticipantId::new(unsafe { text(participant_id, "participant_id") }?);
        let handle = publisher
            .handle
            .as_ref()
            .ok_or_else(|| GsError::invalid("publisher stopped"))?;
        sdk(handle.update(&id, [x, y, z], has_yaw.then_some(yaw)))
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// Stops sending this player's position (range audibility fades out 1-3 s later).
#[no_mangle]
pub unsafe extern "C" fn gs_voice_publisher_remove(
    publisher: *mut GsVoicePublisher,
    participant_id: *const c_char,
) {
    quietly(|| {
        // SAFETY: NULL or live handles and strings (header contract).
        let (Some(publisher), Ok(id)) = (unsafe { publisher.as_ref() }, unsafe {
            text(participant_id, "participant_id")
        }) else {
            return;
        };
        if let Some(handle) = &publisher.handle {
            handle.remove(&gs::ParticipantId::new(id));
        }
    });
}

/// The failure of the last batch: a new GsError to free with gs_error_free,
/// or NULL when the last batch was sent (or for a NULL publisher).
#[no_mangle]
pub unsafe extern "C" fn gs_voice_publisher_last_error(
    publisher: *const GsVoicePublisher,
) -> *mut GsError {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: NULL or a live handle (header contract).
        unsafe { publisher.as_ref() }
            .and_then(|p| p.handle.as_ref())
            .and_then(gs::PositionPublisher::last_error)
            .map_or(ptr::null_mut(), |error| {
                Box::into_raw(Box::new(GsError::from_sdk(&error)))
            })
    }))
    .unwrap_or(ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn gs_voice_publisher_free(publisher: *mut GsVoicePublisher) {
    if publisher.is_null() {
        return;
    }
    quietly(|| {
        // SAFETY: allocated by the library, freed once (header contract).
        let mut publisher = unsafe { Box::from_raw(publisher) };
        // Stop the task before the runtime reference goes.
        drop(publisher.handle.take());
    });
}

// ---- game data -------------------------------------------------------------

/// Parses a JSON text argument.
fn json_arg(text: &str, name: &str) -> Result<serde_json::Value, GsError> {
    serde_json::from_str(text).map_err(|_| GsError::invalid(format!("{name} is not valid JSON")))
}

/// `ttl_seconds` 0 = none.
fn ttl_arg(ttl_seconds: u32) -> Option<Duration> {
    (ttl_seconds != 0).then(|| Duration::from_secs(u64::from(ttl_seconds)))
}

fn quota(quota: gs::DataQuota) -> GsDataQuota {
    GsDataQuota {
        keys: u32::try_from(quota.keys).unwrap_or(u32::MAX),
        max_keys: u32::try_from(quota.max_keys).unwrap_or(u32::MAX),
        bytes: quota.bytes,
        max_bytes: quota.max_bytes,
    }
}

/// SAFETY: `ack_out` is NULL or a valid, writable `GsDataAck **`.
unsafe fn write_data_ack(ack_out: *mut *mut GsDataAck, ack: gs::DataAck) {
    if ack_out.is_null() {
        return;
    }
    let ack = GsDataAck {
        accepted: ack.accepted.into_iter().map(cstring).collect(),
        skipped: ack
            .skipped
            .into_iter()
            .map(|skipped| (cstring(skipped.key), skipped.seq))
            .collect(),
        quota: quota(ack.quota),
    };
    // SAFETY: non-NULL and writable per the contract.
    unsafe { put(ack_out, Box::into_raw(Box::new(ack))) };
}

/// Sets one document: value_json is any JSON text (at most 4 KiB).
/// ttl_seconds 0 = no TTL; otherwise 10-86400. No game session is needed.
/// The first write reads the stored keys once to seed the sequence counter.
/// ack_out may be NULL; otherwise free *ack_out with gs_data_ack_free.
#[no_mangle]
pub unsafe extern "C" fn gs_game_data_set(
    client: *mut GsClient,
    key: *const c_char,
    value_json: *const c_char,
    ttl_seconds: u32,
    ack_out: *mut *mut GsDataAck,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        // SAFETY: header contract.
        let client = unsafe { handle(client, "client") }?;
        // SAFETY: header contract.
        let key = unsafe { text(key, "key") }?;
        // SAFETY: header contract.
        let value = json_arg(unsafe { text(value_json, "value_json") }?, "value_json")?;
        let data = client.client.game().data();
        let ack = match ttl_arg(ttl_seconds) {
            Some(ttl) => sdk(block_on(&client.rt, data.set_with_ttl(key, value, ttl))?)?,
            None => sdk(block_on(&client.rt, data.set(key, value))?)?,
        };
        // SAFETY: NULL or writable (header contract).
        unsafe { write_data_ack(ack_out, ack) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// Removes one document (the removal is kept 24 h). ack_out may be NULL.
#[no_mangle]
pub unsafe extern "C" fn gs_game_data_remove(
    client: *mut GsClient,
    key: *const c_char,
    ack_out: *mut *mut GsDataAck,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        // SAFETY: header contract.
        let client = unsafe { handle(client, "client") }?;
        // SAFETY: header contract.
        let key = unsafe { text(key, "key") }?;
        let ack = sdk(block_on(
            &client.rt,
            client.client.game().data().remove(key),
        )?)?;
        // SAFETY: NULL or writable (header contract).
        unsafe { write_data_ack(ack_out, ack) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// One all-or-nothing write: set_len entries, then remove_len keys
/// (set_len + remove_len must be 1-40). Either array may be NULL when its
/// length is 0. ack_out may be NULL.
#[no_mangle]
pub unsafe extern "C" fn gs_game_data_set_many(
    client: *mut GsClient,
    set: *const GsDataEntry,
    set_len: usize,
    remove_keys: *const *const c_char,
    remove_len: usize,
    ack_out: *mut *mut GsDataAck,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        // SAFETY: header contract.
        let client = unsafe { handle(client, "client") }?;
        let mut batch = gs::DataBatch::new();
        if set_len > 0 {
            require(set, "set")?;
        }
        for index in 0..set_len {
            // SAFETY: `set_len` live entries (header contract).
            let entry = unsafe { &*set.add(index) };
            // SAFETY: header contract.
            let key = unsafe { text(entry.key, "set[].key") }?;
            // SAFETY: header contract.
            let value = json_arg(
                unsafe { text(entry.value_json, "set[].value_json") }?,
                "set[].value_json",
            )?;
            batch = match ttl_arg(entry.ttl_seconds) {
                Some(ttl) => batch.set_with_ttl(key, value, ttl),
                None => batch.set(key, value),
            };
        }
        if remove_len > 0 {
            require(remove_keys, "remove_keys")?;
        }
        for index in 0..remove_len {
            // SAFETY: `remove_len` live strings (header contract).
            let key = unsafe { text(*remove_keys.add(index), "remove_keys[]") }?;
            batch = batch.remove(key);
        }
        let ack = sdk(block_on(
            &client.rt,
            client.client.game().data().set_many(&batch),
        )?)?;
        // SAFETY: NULL or writable (header contract).
        unsafe { write_data_ack(ack_out, ack) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// The writer's listing as JSON: {"keys": [{"key", "seq", "bytes",
/// "ttlRemaining", "updatedAt", "removed"}], "quota": {...}} (seconds;
/// ttlRemaining null when none). 10 per 10 s: for start-up and checks, not
/// for polling. *json_out: free with gs_string_free.
#[no_mangle]
pub unsafe extern "C" fn gs_game_data_list(
    client: *mut GsClient,
    json_out: *mut *mut c_char,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        require(json_out, "json_out")?;
        // SAFETY: header contract.
        let client = unsafe { handle(client, "client") }?;
        let listing = sdk(block_on(&client.rt, client.client.game().data().list())?)?;
        let keys: Vec<serde_json::Value> = listing
            .keys
            .iter()
            .map(|k| {
                serde_json::json!({
                    "key": k.key, "seq": k.seq, "bytes": k.bytes,
                    "ttlRemaining": k.ttl_remaining.map(|t| t.as_secs_f64()),
                    "updatedAt": k.updated_at, "removed": k.removed,
                })
            })
            .collect();
        let q = listing.quota;
        let json = serde_json::json!({
            "keys": keys,
            "quota": {"keys": q.keys, "maxKeys": q.max_keys, "bytes": q.bytes, "maxBytes": q.max_bytes},
        });
        // SAFETY: checked non-NULL above.
        unsafe { put(json_out, cstring(json.to_string()).into_raw()) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// SAFETY: NULL or a live GsDataAck.
unsafe fn data_ack_ref<'a>(ack: *const GsDataAck) -> Option<&'a GsDataAck> {
    // SAFETY: per the contract.
    unsafe { ack.as_ref() }
}

/// The keys that were stored. 0 for NULL.
#[no_mangle]
pub unsafe extern "C" fn gs_data_ack_accepted_len(ack: *const GsDataAck) -> usize {
    // SAFETY: header contract.
    unsafe { data_ack_ref(ack) }.map_or(0, |a| a.accepted.len())
}

/// Borrowed until gs_data_ack_free; NULL when out of range.
#[no_mangle]
pub unsafe extern "C" fn gs_data_ack_accepted_key(
    ack: *const GsDataAck,
    i: usize,
) -> *const c_char {
    // SAFETY: header contract.
    unsafe { data_ack_ref(ack) }
        .and_then(|a| a.accepted.get(i))
        .map_or(ptr::null(), |key| key.as_ptr())
}

/// Keys still skipped after one resend: another writer owns them.
#[no_mangle]
pub unsafe extern "C" fn gs_data_ack_skipped_len(ack: *const GsDataAck) -> usize {
    // SAFETY: header contract.
    unsafe { data_ack_ref(ack) }.map_or(0, |a| a.skipped.len())
}

/// Borrowed until gs_data_ack_free; NULL when out of range.
#[no_mangle]
pub unsafe extern "C" fn gs_data_ack_skipped_key(ack: *const GsDataAck, i: usize) -> *const c_char {
    // SAFETY: header contract.
    unsafe { data_ack_ref(ack) }
        .and_then(|a| a.skipped.get(i))
        .map_or(ptr::null(), |(key, _)| key.as_ptr())
}

/// The stored sequence number of a skipped key; 0 when out of range.
#[no_mangle]
pub unsafe extern "C" fn gs_data_ack_skipped_seq(ack: *const GsDataAck, i: usize) -> u64 {
    // SAFETY: header contract.
    unsafe { data_ack_ref(ack) }
        .and_then(|a| a.skipped.get(i))
        .map_or(0, |(_, seq)| *seq)
}

/// Writes the quota; zeroes for a NULL ack. `out` must not be NULL.
#[no_mangle]
pub unsafe extern "C" fn gs_data_ack_quota(ack: *const GsDataAck, out: *mut GsDataQuota) {
    if out.is_null() {
        return;
    }
    quietly(|| {
        // SAFETY: header contract.
        let value = unsafe { data_ack_ref(ack) }.map_or_else(GsDataQuota::default, |a| a.quota);
        // SAFETY: checked non-NULL above.
        unsafe { put(out, value) };
    });
}

#[no_mangle]
pub unsafe extern "C" fn gs_data_ack_free(ack: *mut GsDataAck) {
    if ack.is_null() {
        return;
    }
    // SAFETY: allocated by the library, freed once (header contract).
    quietly(|| drop(unsafe { Box::from_raw(ack) }));
}

/// Starts a publisher on the client's runtime: hz times a second (1-5) it
/// sends the keys changed since the last tick, latest value only, oldest
/// change first, within the service's limits.
#[no_mangle]
pub unsafe extern "C" fn gs_game_data_publisher_spawn(
    client: *mut GsClient,
    hz: u32,
    out: *mut *mut GsDataPublisher,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        require(out, "out")?;
        // SAFETY: header contract.
        let client = unsafe { handle(client, "client") }?;
        let publisher = {
            let _entered = client.rt.get().enter();
            client.client.game().spawn_data_publisher(hz)
        };
        let handle = GsDataPublisher {
            handle: Some(publisher),
            _rt: client.rt.clone(),
        };
        // SAFETY: checked non-NULL above.
        unsafe { put(out, Box::into_raw(Box::new(handle))) };
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// A key's latest value (cheap; call every frame). Checked here. Never
/// blocks. ttl_seconds 0 = no TTL; otherwise 10-86400.
#[no_mangle]
pub unsafe extern "C" fn gs_game_data_publisher_set(
    publisher: *mut GsDataPublisher,
    key: *const c_char,
    value_json: *const c_char,
    ttl_seconds: u32,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        // SAFETY: header contract.
        let publisher = unsafe { handle(publisher, "publisher") }?;
        // SAFETY: header contract.
        let key = unsafe { text(key, "key") }?;
        // SAFETY: header contract.
        let value = json_arg(unsafe { text(value_json, "value_json") }?, "value_json")?;
        let handle = publisher
            .handle
            .as_ref()
            .ok_or_else(|| GsError::invalid("publisher stopped"))?;
        match ttl_arg(ttl_seconds) {
            Some(ttl) => sdk(handle.set_with_ttl(key, value, ttl)),
            None => sdk(handle.set(key, value)),
        }
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

/// Sends a removal on the next tick, and stops sending the key.
#[no_mangle]
pub unsafe extern "C" fn gs_game_data_publisher_remove(
    publisher: *mut GsDataPublisher,
    key: *const c_char,
) {
    quietly(|| {
        // SAFETY: NULL or live handles and strings (header contract).
        let (Some(publisher), Ok(key)) =
            (unsafe { publisher.as_ref() }, unsafe { text(key, "key") })
        else {
            return;
        };
        if let Some(handle) = &publisher.handle {
            handle.remove(key);
        }
    });
}

/// Keys another writer owns, as a JSON array; NULL when none (or for a NULL
/// publisher). Free with gs_string_free.
#[no_mangle]
pub unsafe extern "C" fn gs_game_data_publisher_skipped_json(
    publisher: *const GsDataPublisher,
) -> *mut c_char {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: NULL or a live handle (header contract).
        let skipped = unsafe { publisher.as_ref() }
            .and_then(|p| p.handle.as_ref())
            .map(gs::DataPublisher::skipped)
            .unwrap_or_default();
        if skipped.is_empty() {
            ptr::null_mut()
        } else {
            cstring(serde_json::Value::from(skipped).to_string()).into_raw()
        }
    }))
    .unwrap_or(ptr::null_mut())
}

/// The failure of the last batch: a new GsError to free with gs_error_free,
/// or NULL when the last batch was sent (or for a NULL publisher).
#[no_mangle]
pub unsafe extern "C" fn gs_game_data_publisher_last_error(
    publisher: *const GsDataPublisher,
) -> *mut GsError {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: NULL or a live handle (header contract).
        unsafe { publisher.as_ref() }
            .and_then(|p| p.handle.as_ref())
            .and_then(gs::DataPublisher::last_error)
            .map_or(ptr::null_mut(), |error| {
                Box::into_raw(Box::new(GsError::from_sdk(&error)))
            })
    }))
    .unwrap_or(ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn gs_game_data_publisher_free(publisher: *mut GsDataPublisher) {
    if publisher.is_null() {
        return;
    }
    quietly(|| {
        // SAFETY: allocated by the library, freed once (header contract).
        let mut publisher = unsafe { Box::from_raw(publisher) };
        // Stop the task before the runtime reference goes.
        drop(publisher.handle.take());
    });
}

/// Sends an event: payload_json is a JSON object (at most 4 KiB). id is the
/// Idempotency-Key, [A-Za-z0-9_-]{8,64}; NULL = generated by the SDK (its own
/// retries are still de-duplicated). *event_id_out (the server's sequence,
/// a string): free with gs_string_free; it may be NULL. duplicate_out may be
/// NULL.
#[no_mangle]
pub unsafe extern "C" fn gs_game_event_emit(
    client: *mut GsClient,
    name: *const c_char,
    payload_json: *const c_char,
    id: *const c_char,
    event_id_out: *mut *mut c_char,
    duplicate_out: *mut bool,
    error: *mut *mut GsError,
) -> GsStatus {
    let body = || {
        // SAFETY: header contract.
        let client = unsafe { handle(client, "client") }?;
        // SAFETY: header contract.
        let name = unsafe { text(name, "name") }?;
        // SAFETY: header contract.
        let payload = json_arg(
            unsafe { text(payload_json, "payload_json") }?,
            "payload_json",
        )?;
        // SAFETY: header contract.
        let id = unsafe { optional_text(id, "id") }?;
        let events = client.client.game().events();
        let receipt = match id {
            Some(id) => sdk(block_on(
                &client.rt,
                events.emit_with_id(name, payload, id),
            )?)?,
            None => sdk(block_on(&client.rt, events.emit(name, payload))?)?,
        };
        if !event_id_out.is_null() {
            // SAFETY: non-NULL and writable (header contract).
            unsafe { put(event_id_out, cstring(receipt.id).into_raw()) };
        }
        if !duplicate_out.is_null() {
            // SAFETY: non-NULL and writable (header contract).
            unsafe { put(duplicate_out, receipt.duplicate) };
        }
        Ok(())
    };
    // SAFETY: header contract for `error`.
    unsafe { boundary(error, body) }
}

// ---- errors ----------------------------------------------------------------

/// SAFETY: NULL or a live GsError.
unsafe fn error_ref<'a>(error: *const GsError) -> Option<&'a GsError> {
    // SAFETY: per the contract.
    unsafe { error.as_ref() }
}

#[no_mangle]
pub unsafe extern "C" fn gs_error_status(error: *const GsError) -> GsStatus {
    // SAFETY: header contract.
    unsafe { error_ref(error) }.map_or(GsStatus::Ok, |e| e.status)
}

#[no_mangle]
pub unsafe extern "C" fn gs_error_code(error: *const GsError) -> GsErrorCode {
    // SAFETY: header contract.
    unsafe { error_ref(error) }.map_or(GsErrorCode::None, |e| e.code)
}

/// The code as the service sent it; NULL when the status is not GS_STATUS_API.
#[no_mangle]
pub unsafe extern "C" fn gs_error_wire_code(error: *const GsError) -> *const c_char {
    // SAFETY: header contract.
    unsafe { error_ref(error) }
        .and_then(|e| e.wire_code.as_ref())
        .map_or(ptr::null(), |code| code.as_ptr())
}

/// Human-readable; never NULL for a non-NULL error; never contains secrets.
#[no_mangle]
pub unsafe extern "C" fn gs_error_message(error: *const GsError) -> *const c_char {
    // SAFETY: header contract.
    unsafe { error_ref(error) }.map_or(ptr::null(), |e| e.message.as_ptr())
}

/// 0 when the status is not GS_STATUS_API.
#[no_mangle]
pub unsafe extern "C" fn gs_error_http_status(error: *const GsError) -> u16 {
    // SAFETY: header contract.
    unsafe { error_ref(error) }.map_or(0, |e| e.http_status)
}

/// NULL when absent.
#[no_mangle]
pub unsafe extern "C" fn gs_error_request_id(error: *const GsError) -> *const c_char {
    // SAFETY: header contract.
    unsafe { error_ref(error) }
        .and_then(|e| e.request_id.as_ref())
        .map_or(ptr::null(), |id| id.as_ptr())
}

/// -1 when the answer had no Retry-After (voice and game-data 429s send it).
#[no_mangle]
pub unsafe extern "C" fn gs_error_retry_after_ms(error: *const GsError) -> i64 {
    // SAFETY: header contract.
    unsafe { error_ref(error) }.map_or(-1, |e| e.retry_after_ms)
}

/// The request field the error is about (voice and presence payload
/// errors); NULL when the service named none.
#[no_mangle]
pub unsafe extern "C" fn gs_error_field(error: *const GsError) -> *const c_char {
    // SAFETY: header contract.
    unsafe { error_ref(error) }
        .and_then(|e| e.field.as_ref())
        .map_or(ptr::null(), |field| field.as_ptr())
}

/// Why that field was refused (voice payload errors), e.g. "count"; NULL when absent.
#[no_mangle]
pub unsafe extern "C" fn gs_error_reason(error: *const GsError) -> *const c_char {
    // SAFETY: header contract.
    unsafe { error_ref(error) }
        .and_then(|e| e.reason.as_ref())
        .map_or(ptr::null(), |reason| reason.as_ptr())
}

/// Network errors, timeouts, 5xx and 429.
#[no_mangle]
pub unsafe extern "C" fn gs_error_is_retryable(error: *const GsError) -> bool {
    // SAFETY: header contract.
    unsafe { error_ref(error) }.is_some_and(|e| e.retryable)
}

#[no_mangle]
pub unsafe extern "C" fn gs_error_free(error: *mut GsError) {
    if error.is_null() {
        return;
    }
    // SAFETY: allocated by the library, freed once (header contract).
    quietly(|| drop(unsafe { Box::from_raw(error) }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_inside_a_boundary_becomes_internal() {
        let mut error: *mut GsError = ptr::null_mut();
        // SAFETY: a valid out-pointer.
        let status = unsafe {
            boundary(&mut error, || -> Result<(), GsError> {
                panic!("secret panic text")
            })
        };
        assert_eq!(status, GsStatus::Internal);
        assert!(!error.is_null());
        // SAFETY: the error the boundary just created.
        unsafe {
            let message = CStr::from_ptr(gs_error_message(error)).to_str().unwrap();
            assert_eq!(message, "internal error");
            gs_error_free(error);
        }
    }
}
