//! Same-user, local-only named pipes. The first instance flag arbitrates router
//! ownership without opening a second server underneath a live official app.
use anyhow::{Result, bail};
use std::{
    ffi::c_void,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::{Path, PathBuf},
};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            EqualSid, GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        System::{
            Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
            Threading::{
                GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
    },
    core::{PCWSTR, PWSTR},
};

pub enum Stream {
    Client(NamedPipeClient),
    Server(NamedPipeServer),
}
// Tokio's two pipe endpoints implement identical stream operations.
impl tokio::io::AsyncRead for Stream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Client(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Self::Server(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}
impl tokio::io::AsyncWrite for Stream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Client(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            Self::Server(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Client(s) => std::pin::Pin::new(s).poll_flush(cx),
            Self::Server(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Client(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Self::Server(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}
/// Named pipes vanish with the process, so there is nothing to unlink at quit.
#[derive(Clone)]
pub struct EndpointIdentity;
impl EndpointIdentity {
    pub fn unlink_if_current(&self) {}
}
pub struct Listener {
    next: NamedPipeServer,
    path: PathBuf,
}
impl Listener {
    pub fn identity(&self) -> EndpointIdentity {
        EndpointIdentity
    }
    pub async fn accept(&mut self) -> Result<Stream> {
        self.next.connect().await?;
        let replacement = match create(false, &self.path) {
            Ok(replacement) => replacement,
            Err(error) => {
                // `next` has to go back to listening. Left connected, every
                // later accept would return at once and fail here again, and
                // this client would hold the only instance without being served.
                if let Err(error) = self.next.disconnect() {
                    log::warn!("Codex IPC: cannot reset the pipe instance: {error}");
                }
                return Err(error);
            }
        };
        let connected = std::mem::replace(&mut self.next, replacement);
        let mut pid = 0;
        // SAFETY: `connected` owns the handle for the whole call and `pid` is a
        // live out-parameter.
        unsafe {
            GetNamedPipeClientProcessId(HANDLE(connected.as_raw_handle()), &mut pid)?;
        }
        validate_process(pid)?;
        Ok(Stream::Server(connected))
    }
}
#[cfg(not(test))]
pub fn endpoint(_home: &Path) -> PathBuf {
    PathBuf::from(r"\\.\pipe\codex-ipc")
}
// Tests must never meet the user's real router, so each test home gets a pipe
// of its own.
#[cfg(test)]
pub fn endpoint(home: &Path) -> PathBuf {
    let name = home.file_name().unwrap_or_default().to_string_lossy();
    PathBuf::from(format!(r"\\.\pipe\zed-codex-test-{name}"))
}
fn user_token(process: HANDLE) -> Result<Vec<usize>> {
    let mut token = HANDLE::default();
    // SAFETY: the caller's `process` handle is open, and `token` is a live
    // out-parameter.
    unsafe {
        OpenProcessToken(process, TOKEN_QUERY, &mut token)?;
    }
    // SAFETY: the call above succeeded, so `token` is a handle nobody else
    // owns; wrapping it closes it when this function returns.
    let owned_token = unsafe { OwnedHandle::from_raw_handle(token.0) };
    let token = HANDLE(owned_token.as_raw_handle());
    let mut size = 0;
    // SAFETY: a null buffer of length zero is the documented way to ask for
    // the size. The call is expected to fail with "insufficient buffer", so
    // its result says nothing; `size` staying zero is the failure.
    unsafe {
        GetTokenInformation(token, TokenUser, None, 0, &mut size).ok();
    }
    if size == 0 {
        bail!("cannot read IPC process owner");
    }
    // `usize` elements, so the buffer is aligned for the `TOKEN_USER` that
    // callers read out of it.
    let mut bytes = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: `bytes` is at least `size` bytes long and outlives the call.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(bytes.as_mut_ptr().cast()),
            size,
            &mut size,
        )?;
    }
    Ok(bytes)
}
fn validate_process(pid: u32) -> Result<()> {
    // SAFETY: no pointers are passed; a pid that is gone is an error.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)? };
    // SAFETY: `OpenProcess` succeeded, so this is a handle nobody else owns.
    let process = unsafe { OwnedHandle::from_raw_handle(process.0) };
    let peer = user_token(HANDLE(process.as_raw_handle()))?;
    // SAFETY: returns a pseudo-handle that needs no closing and cannot fail.
    let own = user_token(unsafe { GetCurrentProcess() })?;
    // SAFETY: `user_token` filled both buffers with a `TOKEN_USER`, aligned
    // for it, and the SIDs they point to live inside those same buffers,
    // which outlive the comparison.
    unsafe {
        let peer = &*peer.as_ptr().cast::<TOKEN_USER>();
        let own = &*own.as_ptr().cast::<TOKEN_USER>();
        EqualSid(peer.User.Sid, own.User.Sid)
            .map_err(|_| anyhow::anyhow!("Codex IPC peer belongs to another user"))?;
    }
    Ok(())
}
fn create(first: bool, path: &Path) -> Result<NamedPipeServer> {
    // SAFETY: returns a pseudo-handle that needs no closing and cannot fail.
    let token = user_token(unsafe { GetCurrentProcess() })?;
    let mut sid = PWSTR::null();
    // SAFETY: `token` holds an aligned `TOKEN_USER` whose SID lives in the
    // same buffer; `sid` receives a string the system allocates.
    unsafe {
        ConvertSidToStringSidW((&*token.as_ptr().cast::<TOKEN_USER>()).User.Sid, &mut sid)?;
    }
    // SAFETY: the call above succeeded, so `sid` is a NUL-terminated string.
    let sid_text = unsafe { sid.to_string() };
    // SAFETY: `sid` was allocated by the system for us and is not used again;
    // `sid_text` is a copy. The return value only repeats the pointer on failure.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sid.0.cast())));
    }
    let sddl: Vec<u16> = format!("D:P(A;;GA;;;{})", sid_text?)
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `sddl` is NUL-terminated and outlives the call; `descriptor`
    // receives a buffer the system allocates.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: `attributes` and the descriptor it points to are valid until
    // after this call, which copies what it needs.
    let result = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(path, &attributes as *const _ as *mut c_void)
    };
    // SAFETY: the descriptor was allocated by the system for us and the pipe
    // keeps its own copy.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    Ok(result?)
}
pub async fn connect(path: &Path) -> Result<Stream> {
    let stream = ClientOptions::new().open(path)?;
    let mut pid = 0;
    // SAFETY: `stream` owns the handle for the whole call and `pid` is a live
    // out-parameter.
    unsafe {
        GetNamedPipeServerProcessId(HANDLE(stream.as_raw_handle()), &mut pid)?;
    }
    validate_process(pid)?;
    Ok(Stream::Client(stream))
}
pub async fn connect_or_bind(home: &Path) -> Result<(Option<Listener>, Stream, ())> {
    connect_or_bind_path(&endpoint(home)).await
}
async fn connect_or_bind_path(path: &Path) -> Result<(Option<Listener>, Stream, ())> {
    if let Ok(stream) = connect(&path).await {
        return Ok((None, stream, ()));
    }
    // If any instance already exists, this fails, including when all instances
    // are busy. The service retries instead of taking ownership from that app.
    let listener = Listener {
        next: create(true, path)?,
        path: path.to_path_buf(),
    };
    let stream = connect(&path).await?;
    Ok((Some(listener), stream, ()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn private_pipe_election_same_user_and_restart() {
        let path = PathBuf::from(format!(r"\\.\pipe\zed-codex-test-{}", uuid::Uuid::new_v4()));
        let (owner, mut client, _) = connect_or_bind_path(&path).await.unwrap();
        let mut owner = owner.unwrap();
        let mut server = owner.accept().await.unwrap();
        assert!(
            create(true, &path).is_err(),
            "must not take over an existing pipe"
        );
        client.write_all(b"context").await.unwrap();
        let mut bytes = [0; 7];
        server.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"context");
        let (second_owner, second_client, _) = connect_or_bind_path(&path).await.unwrap();
        assert!(second_owner.is_none());
        drop(second_client);
        drop(client);
        drop(server);
        drop(owner);
        // A dropped pipe keeps its handle until the reactor has seen its
        // cancelled I/O complete, and while any instance exists the
        // first-instance flag is refused with "Access is denied". The service
        // retries every second; the test has to give the reactor a turn too.
        let mut restarted = connect_or_bind_path(&path).await;
        for _ in 0..200 {
            if restarted.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            restarted = connect_or_bind_path(&path).await;
        }
        let (restarted, _, _) = restarted.unwrap();
        assert!(restarted.is_some());
    }
}
