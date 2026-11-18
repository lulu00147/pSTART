//! Windows AF_UNIX transport.
//!
//! AF_UNIX is the product's default IPC. The listener obtains the client PID
//! via `SIO_AF_UNIX_GETPEERPID` and resolves the process SID before exposing the
//! connection to the session layer.

use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, HANDLE, HLOCAL};
use windows_sys::Win32::Networking::WinSock::{
    accept, bind, closesocket, connect, listen, recv, send, setsockopt, socket, WSAGetLastError,
    WSAStartup, AF_UNIX, INVALID_SOCKET, SIO_AF_UNIX_GETPEERPID, SOCKADDR, SOCKADDR_UN, SOCKET,
    SOCKET_ERROR, SOCK_STREAM, SOL_SOCKET, SO_RCVTIMEO, WSADATA,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use pstart_core::limits::MAX_FRAME_LEN;

use crate::error::IpcError;
use crate::message::IpcMessage;
use crate::transport::{PeerIdentity, StreamTransport, TransportKind};

/// Default service socket path expected by the installer.
pub const DEFAULT_SOCKET_PATH: &str = r"C:\ProgramData\pSTART\ipc\pstart.sock";

static WSA_READY: OnceLock<Result<(), i32>> = OnceLock::new();

/// Server-side AF_UNIX listener.
#[derive(Debug)]
pub struct UnixListener {
    socket: SOCKET,
    path: PathBuf,
}

impl UnixListener {
    /// Bind a local AF_UNIX endpoint.
    pub fn bind(path: &Path) -> Result<Self, IpcError> {
        ensure_wsa()?;
        let address = make_address(path)?;
        if path.exists() {
            // A stale socket file is not a live listener; the service owns this
            // path. Removing it avoids EADDRINUSE after a crash.
            std::fs::remove_file(path).map_err(IpcError::Io)?;
        }
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                return Err(IpcError::InvalidMessage(format!(
                    "socket parent directory does not exist: {}",
                    parent.display()
                )));
            }
        }
        // SAFETY: socket has no pointer arguments.
        let socket = unsafe { socket(AF_UNIX as i32, SOCK_STREAM, 0) };
        if socket == INVALID_SOCKET {
            return Err(wsa_error("socket", unsafe { WSAGetLastError() }));
        }
        let listener = Self {
            socket,
            path: path.to_path_buf(),
        };
        // SAFETY: address is a valid SOCKADDR_UN for the call.
        if unsafe {
            bind(
                listener.socket,
                (&address as *const SOCKADDR_UN).cast::<SOCKADDR>(),
                size_of::<SOCKADDR_UN>() as i32,
            )
        } == SOCKET_ERROR
        {
            return Err(wsa_error("bind", unsafe { WSAGetLastError() }));
        }
        // SAFETY: backlog is a plain integer.
        if unsafe { listen(listener.socket, 16) } == SOCKET_ERROR {
            return Err(wsa_error("listen", unsafe { WSAGetLastError() }));
        }
        Ok(listener)
    }

    /// Block until a client connects, then return an authenticated transport.
    pub fn accept(&self) -> Result<StreamTransport<UnixStream>, IpcError> {
        // SAFETY: null address output means the caller does not need peer address.
        let socket = unsafe { accept(self.socket, null_mut(), null_mut()) };
        if socket == INVALID_SOCKET {
            return Err(wsa_error("accept", unsafe { WSAGetLastError() }));
        }
        let peer = match peer_identity(socket) {
            Ok(peer) => peer,
            Err(error) => {
                // SAFETY: accepted socket is owned here and must be closed.
                unsafe {
                    closesocket(socket);
                }
                return Err(error);
            }
        };
        StreamTransport::new(UnixStream { socket }, peer, MAX_FRAME_LEN)
    }

    /// Bound socket path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for UnixListener {
    fn drop(&mut self) {
        // SAFETY: socket is owned by this listener.
        unsafe {
            closesocket(self.socket);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Client-side AF_UNIX connection.
#[derive(Debug)]
pub struct UnixClient {
    stream: UnixStream,
}

impl UnixClient {
    /// Bound all subsequent reads on this client.
    pub fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }

    /// Connect to a local AF_UNIX endpoint.
    pub fn connect(path: &Path) -> Result<Self, IpcError> {
        ensure_wsa()?;
        let address = make_address(path)?;
        // SAFETY: socket has no pointer arguments.
        let socket = unsafe { socket(AF_UNIX as i32, SOCK_STREAM, 0) };
        if socket == INVALID_SOCKET {
            return Err(wsa_error("socket", unsafe { WSAGetLastError() }));
        }
        // SAFETY: address is valid for the call.
        if unsafe {
            connect(
                socket,
                (&address as *const SOCKADDR_UN).cast::<SOCKADDR>(),
                size_of::<SOCKADDR_UN>() as i32,
            )
        } == SOCKET_ERROR
        {
            let code = unsafe { WSAGetLastError() };
            // SAFETY: socket is owned here.
            unsafe {
                closesocket(socket);
            }
            return Err(wsa_error("connect", code));
        }
        Ok(Self {
            stream: UnixStream { socket },
        })
    }

    /// Send one framed message.
    pub fn send(&mut self, message: &IpcMessage) -> Result<(), IpcError> {
        crate::frame::write_frame(&mut self.stream, message)
    }

    /// Receive one framed message.
    pub fn receive(&mut self) -> Result<IpcMessage, IpcError> {
        crate::frame::read_frame(&mut self.stream, MAX_FRAME_LEN)
    }
}

/// Blocking socket stream.
#[derive(Debug)]
pub struct UnixStream {
    socket: SOCKET,
}

impl Read for UnixStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let length = i32::try_from(buffer.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "read buffer too large"))?;
        // SAFETY: socket is valid; buffer is writable for `length` bytes.
        let result = unsafe { recv(self.socket, buffer.as_mut_ptr(), length, 0) };
        if result == SOCKET_ERROR {
            return Err(io::Error::other(format!(
                "recv failed with WSA error {}",
                unsafe { WSAGetLastError() }
            )));
        }
        Ok(result as usize)
    }
}

impl Write for UnixStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let length = i32::try_from(buffer.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "write buffer too large"))?;
        // SAFETY: socket is valid; buffer is readable for `length` bytes.
        let result = unsafe { send(self.socket, buffer.as_ptr(), length, 0) };
        if result == SOCKET_ERROR {
            return Err(io::Error::other(format!(
                "send failed with WSA error {}",
                unsafe { WSAGetLastError() }
            )));
        }
        Ok(result as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for UnixStream {
    fn drop(&mut self) {
        // SAFETY: socket is owned by this stream.
        unsafe {
            closesocket(self.socket);
        }
    }
}

impl UnixStream {
    /// Set the receive timeout for a server-side connection.
    pub fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        // Winsock SO_RCVTIMEO expects a DWORD timeout in milliseconds.
        let millis = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        // SAFETY: millis is a valid DWORD for the socket option.
        let result = unsafe {
            setsockopt(
                self.socket,
                SOL_SOCKET,
                SO_RCVTIMEO,
                (&millis as *const u32).cast::<u8>(),
                size_of::<u32>() as i32,
            )
        };
        if result == SOCKET_ERROR {
            return Err(io::Error::other(format!(
                "setsockopt(SO_RCVTIMEO) failed with WSA error {}",
                unsafe { WSAGetLastError() }
            )));
        }
        Ok(())
    }
}

fn ensure_wsa() -> Result<(), IpcError> {
    let result = WSA_READY.get_or_init(|| {
        let mut data = WSADATA::default();
        // SAFETY: data is writable for the call.
        let code = unsafe { WSAStartup(0x0202, &mut data) };
        if code == 0 {
            Ok(())
        } else {
            Err(code)
        }
    });
    match result {
        Ok(()) => Ok(()),
        Err(code) => Err(IpcError::Windows {
            operation: "WSAStartup",
            code: *code as u32,
        }),
    }
}

fn make_address(path: &Path) -> Result<SOCKADDR_UN, IpcError> {
    let value = path
        .to_str()
        .ok_or_else(|| IpcError::InvalidMessage("socket path must be valid UTF-8".to_owned()))?;
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() >= 108 || value.contains('\0') {
        return Err(IpcError::InvalidMessage(
            "socket path must be 1..107 bytes and contain no NUL".to_owned(),
        ));
    }
    let mut address = SOCKADDR_UN {
        sun_family: AF_UNIX,
        ..SOCKADDR_UN::default()
    };
    for (index, byte) in bytes.iter().enumerate() {
        address.sun_path[index] = *byte as i8;
    }
    Ok(address)
}

fn peer_identity(socket: SOCKET) -> Result<PeerIdentity, IpcError> {
    let mut pid = 0u32;
    let mut returned = 0u32;
    // SAFETY: output buffer is a valid u32; null overlapped and completion.
    let result = unsafe {
        windows_sys::Win32::Networking::WinSock::WSAIoctl(
            socket,
            SIO_AF_UNIX_GETPEERPID,
            null(),
            0,
            (&mut pid as *mut u32).cast::<c_void>(),
            size_of::<u32>() as u32,
            &mut returned,
            null_mut(),
            None,
        )
    };
    if result == SOCKET_ERROR || pid == 0 {
        return Err(wsa_error("WSAIoctl(SIO_AF_UNIX_GETPEERPID)", unsafe {
            WSAGetLastError()
        }));
    }
    // Some Windows builds report zero bytes returned even when the peer PID was
    // written; the non-zero PID is the authoritative success signal.
    // SAFETY: OpenProcess has no pointer arguments beyond the returned handle.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(windows_error("OpenProcess", unsafe { GetLastError() }));
    }
    let process = OwnedHandle(process);
    let mut token = null_mut();
    // SAFETY: token output is writable.
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(windows_error("OpenProcessToken", unsafe { GetLastError() }));
    }
    let token = OwnedHandle(token);
    let mut needed = 0u32;
    // SAFETY: null buffer with zero size is a size query.
    unsafe {
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(windows_error("GetTokenInformation", unsafe {
            GetLastError()
        }));
    }
    let words = (needed as usize).div_ceil(size_of::<u64>());
    let mut buffer = vec![0u64; words];
    // SAFETY: buffer is aligned and at least `needed` bytes.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast::<c_void>(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(windows_error("GetTokenInformation", unsafe {
            GetLastError()
        }));
    }
    // SAFETY: successful token query populated TOKEN_USER at buffer start.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    if user.User.Sid.is_null() {
        return Err(IpcError::PeerIdentityUnavailable(
            "token user SID is null".to_owned(),
        ));
    }
    let mut sid_string: *mut u16 = null_mut();
    // SAFETY: SID comes from the token query; output is LocalAlloc string.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_string) } == 0 {
        return Err(windows_error("ConvertSidToStringSidW", unsafe {
            GetLastError()
        }));
    }
    let sid = wide_ptr_to_string(sid_string);
    // SAFETY: ConvertSidToStringSidW allocates with LocalAlloc.
    unsafe {
        LocalFree(sid_string as HLOCAL);
    }
    PeerIdentity::new(pid, sid, TransportKind::UnixSocket)
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: wrapper owns the handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn wide_ptr_to_string(pointer: *const u16) -> String {
    let mut length = 0usize;
    // SAFETY: API returns a NUL-terminated string.
    unsafe {
        while *pointer.add(length) != 0 {
            length += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(pointer, length))
    }
}

fn wsa_error(operation: &'static str, code: i32) -> IpcError {
    IpcError::Windows {
        operation,
        code: code as u32,
    }
}

fn windows_error(operation: &'static str, code: u32) -> IpcError {
    IpcError::Windows { operation, code }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{decode_payload, encode_payload};
    use crate::transport::Transport;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::thread;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn socket_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join(format!(
            "pstart-{}-{}.sock",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn connect_with_retry(path: &Path) -> UnixClient {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match UnixClient::connect(path) {
                Ok(client) => return client,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("client connect failed: {error}"),
            }
        }
    }

    #[test]
    fn roundtrip_and_peer_identity() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = socket_path(&dir);
        let listener = UnixListener::bind(&path).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut transport = listener.accept().unwrap();
            ready_tx.send(()).unwrap();
            let peer = transport.peer_identity().unwrap();
            let request = transport.receive_frame().unwrap();
            assert_eq!(
                decode_payload(&request).unwrap(),
                IpcMessage::Ping { seq: 7 }
            );
            let reply = encode_payload(&IpcMessage::Pong { seq: 7 }).unwrap();
            transport.send_frame(&reply).unwrap();
            peer
        });
        let mut client = connect_with_retry(&path);
        ready_rx.recv().unwrap();
        client.send(&IpcMessage::Ping { seq: 7 }).unwrap();
        assert_eq!(client.receive().unwrap(), IpcMessage::Pong { seq: 7 });
        let peer = server.join().unwrap();
        assert_eq!(peer.pid, std::process::id());
        assert_eq!(peer.transport, TransportKind::UnixSocket);
        assert!(
            peer.user.starts_with("S-1-5-"),
            "unexpected SID: {}",
            peer.user
        );
    }

    #[test]
    fn bad_paths_are_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("missing").join("x.sock");
        assert!(UnixListener::bind(&missing).is_err());
        let too_long = dir.path().join("x".repeat(120));
        assert!(UnixListener::bind(&too_long).is_err());
        assert!(UnixClient::connect(&dir.path().join("absent.sock")).is_err());
    }

    #[test]
    fn listener_removes_socket_file_on_drop() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = socket_path(&dir);
        {
            let _listener = UnixListener::bind(&path).unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists());
    }
}
