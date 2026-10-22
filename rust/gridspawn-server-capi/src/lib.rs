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

/// The documented error codes (protocol 02 and 05). GS_ERROR_CODE_NONE when
/// the status is not GS_STATUS_API; GS_ERROR_CODE_OTHER for codes this
/// version does not know (see gs_error_wire_code).
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

/// -1 when absent (neither service sends Retry-After today).
#[no_mangle]
pub unsafe extern "C" fn gs_error_retry_after_ms(error: *const GsError) -> i64 {
    // SAFETY: header contract.
    unsafe { error_ref(error) }.map_or(-1, |e| e.retry_after_ms)
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
