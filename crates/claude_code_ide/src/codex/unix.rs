use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};
use tokio::net::{UnixListener, UnixStream};

pub type Stream = UnixStream;
pub struct Listener {
    listener: UnixListener,
    identity: EndpointIdentity,
}
#[derive(Clone)]
pub struct EndpointIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl EndpointIdentity {
    pub fn is_current(&self) -> bool {
        fs::symlink_metadata(&self.path)
            .is_ok_and(|meta| meta.dev() == self.device && meta.ino() == self.inode)
    }
    /// Never remove an endpoint installed by a successor.
    pub fn unlink_if_current(&self) {
        if self.is_current() {
            if let Err(error) = fs::remove_file(&self.path) {
                log::warn!("Codex IPC socket cleanup: {error}");
            }
        }
    }
}
impl Listener {
    pub fn is_current(&self) -> bool {
        self.identity.is_current()
    }
    pub fn identity(&self) -> EndpointIdentity {
        self.identity.clone()
    }

    pub async fn accept(&mut self) -> Result<Stream> {
        let (stream, _) = self.listener.accept().await?;
        validate_peer(&stream)?;
        Ok(stream)
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        self.identity.unlink_if_current();
    }
}
fn uid() -> u32 {
    unsafe { libc::geteuid() }
}
fn validate_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    // Group write is tolerated: the socket uid and peer credential checks stop
    // a hijack, and a umask of 002 would otherwise lock the provider out.
    if !meta.is_dir() || meta.uid() != uid() || meta.mode() & 0o002 != 0 {
        bail!(
            "Codex IPC directory {} (mode {:o}) must be a directory owned by the current user and not world-writable",
            path.display(),
            meta.mode() & 0o777
        );
    }
    Ok(())
}
fn socket_metadata(path: &Path) -> Result<fs::Metadata> {
    validate_directory(path.parent().context("missing IPC parent")?)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_socket() || meta.uid() != uid() {
        bail!("Codex IPC endpoint is not a socket owned by the current user");
    }
    Ok(meta)
}
fn validate_peer(stream: &Stream) -> Result<()> {
    if stream.peer_cred()?.uid() != uid() {
        bail!("Codex IPC peer belongs to another user");
    }
    Ok(())
}
#[cfg(test)]
pub async fn connect(path: &Path) -> Result<Stream> {
    Ok(connect_identified(path).await?.0)
}
async fn connect_identified(path: &Path) -> Result<(Stream, EndpointIdentity)> {
    let meta = socket_metadata(path)?;
    let identity = EndpointIdentity {
        path: path.to_owned(),
        device: meta.dev(),
        inode: meta.ino(),
    };
    let stream = UnixStream::connect(path).await?;
    validate_peer(&stream)?;
    if !identity.is_current() {
        bail!("Codex endpoint changed during connection");
    }
    Ok((stream, identity))
}
pub fn endpoint(home: &Path) -> PathBuf {
    home.join("ipc/ipc.sock")
}

pub async fn connect_or_bind(home: &Path) -> Result<(Option<Listener>, Stream, EndpointIdentity)> {
    let path = endpoint(home);
    let directory = path.parent().context("missing IPC parent")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    validate_directory(directory)?;
    // Respect an existing primary or legacy router, including incompatible live
    // endpoints. A failed handshake must never lead to unlinking a live socket.
    if let Ok((stream, identity)) = connect_identified(&path).await {
        return Ok((None, stream, identity));
    }
    #[cfg(not(test))]
    let legacy_dir = std::env::temp_dir().join("codex-ipc");
    // Tests must never discover the user's real legacy router.
    #[cfg(test)]
    let legacy_dir = home.join("legacy");
    let mut legacy = vec![legacy_dir.join(if uid() == 0 {
        "ipc.sock".to_owned()
    } else {
        format!("ipc-{}.sock", uid())
    })];
    if uid() == 0 {
        legacy.push(legacy_dir.join("ipc-0.sock"));
    }
    for candidate in legacy {
        if let Ok((stream, identity)) = connect_identified(&candidate).await {
            return Ok((None, stream, identity));
        }
    }
    // Serialize elections between Zed processes. The official router does not
    // use this lock; bind still arbitrates creation with it atomically.
    let lock: File = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join("zed-router.lock"))?;
    let meta = lock.metadata()?;
    if !meta.is_file() || meta.uid() != uid() || meta.mode() & 0o077 != 0 {
        bail!("unsafe Codex election lock");
    }
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("Codex router election in progress");
    }
    if let Ok((stream, identity)) = connect_identified(&path).await {
        return Ok((None, stream, identity));
    }
    if fs::symlink_metadata(&path).is_ok() {
        let previous = socket_metadata(&path)?;
        // Only connection-refused proves a stale socket. Permission errors,
        // timeouts and transient resource failures never authorize replacement.
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                validate_peer(&stream)?;
                return Ok((
                    None,
                    stream,
                    EndpointIdentity {
                        path: path.clone(),
                        device: previous.dev(),
                        inode: previous.ino(),
                    },
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
            Err(error) => return Err(error.into()),
        }
        let current = socket_metadata(&path)?;
        if previous.dev() != current.dev() || previous.ino() != current.ino() {
            bail!("Codex endpoint changed during election");
        }
        fs::remove_file(&path)?;
    }
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            let (stream, identity) = connect_identified(&path).await?;
            return Ok((None, stream, identity));
        }
        Err(error) => return Err(error.into()),
    };
    let meta = socket_metadata(&path)?;
    let listener = Listener {
        listener,
        identity: EndpointIdentity {
            path: path.clone(),
            device: meta.dev(),
            inode: meta.ino(),
        },
    };
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let (stream, identity) = connect_identified(&path).await?;
    Ok((Some(listener), stream, identity))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn home() -> PathBuf {
        std::env::temp_dir().join(format!(
            "zi-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ))
    }
    #[tokio::test]
    async fn owner_permissions_coexistence_restart_and_stale_socket() {
        let home = home();
        let (owner, stream, _) = connect_or_bind(&home).await.unwrap();
        assert!(owner.is_some());
        assert_eq!(fs::metadata(endpoint(&home)).unwrap().mode() & 0o777, 0o600);
        let (second, _, _) = connect_or_bind(&home).await.unwrap();
        assert!(second.is_none());
        drop(stream);
        drop(owner);
        assert!(!endpoint(&home).exists());
        let stale = std::os::unix::net::UnixListener::bind(endpoint(&home)).unwrap();
        drop(stale);
        let (recovered, _, _) = connect_or_bind(&home).await.unwrap();
        assert!(recovered.is_some());
        drop(recovered);
        fs::remove_dir_all(home).unwrap();
    }
    #[tokio::test]
    async fn simultaneous_zed_election_has_one_owner() {
        let home = home();
        let (first, second) = tokio::join!(connect_or_bind(&home), connect_or_bind(&home));
        let mut owners = vec![];
        for result in [first, second] {
            // Losing the nonblocking election is transient; retry while the
            // winning listener is still alive.
            let (owner, _, _) = match result {
                Ok(result) => result,
                Err(_) => connect_or_bind(&home).await.unwrap(),
            };
            if let Some(owner) = owner {
                owners.push(owner);
            }
        }
        assert_eq!(owners.len(), 1);
        drop(owners);
        fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn refuses_files_symlinks_and_insecure_directories() {
        let home = home();
        fs::create_dir_all(home.join("ipc")).unwrap();
        fs::write(endpoint(&home), "keep").unwrap();
        assert!(connect_or_bind(&home).await.is_err());
        assert_eq!(fs::read_to_string(endpoint(&home)).unwrap(), "keep");
        fs::remove_file(endpoint(&home)).unwrap();
        std::os::unix::fs::symlink("missing", endpoint(&home)).unwrap();
        assert!(connect_or_bind(&home).await.is_err());
        fs::remove_file(endpoint(&home)).unwrap();
        fs::set_permissions(home.join("ipc"), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(connect_or_bind(&home).await.is_err());
        fs::remove_dir_all(home).unwrap();
    }
}
