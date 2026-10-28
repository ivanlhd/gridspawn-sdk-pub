//! The two local pipes the DLL drives the companion through (Windows only).
//! Each is limited to the current user (DACL), rejects remote clients, has
//! one instance, and must start with `hello {v, token, pipe}` carrying the
//! launch token.

use std::{ffi::c_void, io, ptr, time::Duration};

use serde_json::Value;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree, HANDLE},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

use crate::frames::{self, VERSION};

/// The current user's SID as text (S-1-5-21-…).
fn current_user_sid() -> io::Result<String> {
    // SAFETY: standard token query; every handle and buffer is released.
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut len);
        let mut buffer = vec![0u8; len as usize];
        let ok = GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), len, &mut len);
        CloseHandle(token);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut text: *mut u16 = ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut n = 0;
        while *text.add(n) != 0 {
            n += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
        LocalFree(text.cast());
        Ok(sid)
    }
}

/// A security descriptor that grants the current user full access and
/// nobody else anything (protected DACL).
struct UserOnly {
    descriptor: PSECURITY_DESCRIPTOR,
}

impl UserOnly {
    fn new() -> io::Result<Self> {
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{})", current_user_sid()?)
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: valid NUL-terminated SDDL; the descriptor is freed in Drop.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { descriptor })
    }

    fn create(&self, name: &str) -> io::Result<NamedPipeServer> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        };
        // SAFETY: the attributes and descriptor live for the call.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .max_instances(1)
                .create_with_security_attributes_raw(
                    name,
                    (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
                )
        }
    }
}

impl Drop for UserOnly {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

pub fn pipe_names(game_pid: u32) -> (String, String) {
    (
        format!(r"\\.\pipe\gridspawn-rathena-{game_pid}-in"),
        format!(r"\\.\pipe\gridspawn-rathena-{game_pid}-out"),
    )
}

async fn expect_hello(pipe: &mut NamedPipeServer, token: &str, which: &str) -> io::Result<()> {
    let frame = tokio::time::timeout(Duration::from_secs(10), frames::read_frame(pipe))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no hello"))??
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no hello"))?;
    let ok = frame.get("hello").and_then(Value::as_u64) == Some(VERSION)
        && frame.get("pipe").and_then(Value::as_str) == Some(which)
        && frame
            .get("token")
            .and_then(Value::as_str)
            .is_some_and(|t| frames::same_token(t, token));
    if ok {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "bad hello"))
    }
}

/// Creates both pipes, waits for the DLL to open them, and checks both
/// hellos. (DLL → companion, companion → DLL).
pub async fn accept(game_pid: u32, token: &str) -> io::Result<(NamedPipeServer, NamedPipeServer)> {
    let (in_name, out_name) = pipe_names(game_pid);
    // The descriptor (a raw pointer, not Send) is freed before the first await.
    let (mut incoming, mut outgoing) = {
        let security = UserOnly::new()?;
        (security.create(&in_name)?, security.create(&out_name)?)
    };
    let wait = async {
        incoming.connect().await?;
        outgoing.connect().await?;
        io::Result::Ok(())
    };
    tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the DLL did not connect"))??;
    expect_hello(&mut incoming, token, "in").await?;
    expect_hello(&mut outgoing, token, "out").await?;
    Ok((incoming, outgoing))
}
