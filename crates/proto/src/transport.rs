use std::io;

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{Listener, ListenerOptions, Name, Stream};

pub use interprocess::local_socket::{RecvHalf, SendHalf};

/// Where the daemon listens: `NODEAUDIO_SOCKET` if set, otherwise a socket in
/// the user's runtime dir (Unix) or a per-user named pipe (Windows).
pub fn default_address() -> String {
    if let Ok(addr) = std::env::var("NODEAUDIO_SOCKET")
        && !addr.is_empty()
    {
        return addr;
    }
    platform_default()
}

#[cfg(unix)]
fn platform_default() -> String {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => format!("{}/nodeaudio.sock", dir.to_string_lossy()),
        _ => format!("/tmp/nodeaudio-{}.sock", unix_user()),
    }
}

#[cfg(unix)]
fn unix_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "user".into())
}

#[cfg(windows)]
fn platform_default() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    format!("nodeaudio-{user}")
}

#[cfg(unix)]
fn name(address: &str) -> io::Result<Name<'_>> {
    use interprocess::local_socket::GenericFilePath;
    address.to_fs_name::<GenericFilePath>()
}

#[cfg(windows)]
fn name(address: &str) -> io::Result<Name<'_>> {
    use interprocess::local_socket::GenericNamespaced;
    address.to_ns_name::<GenericNamespaced>()
}

pub fn connect(address: &str) -> io::Result<Stream> {
    Stream::connect(name(address)?)
}

/// Listens on `address`, accessible only to the current user. Fails with
/// `AddrInUse` when another daemon is already serving it.
pub fn listen(address: &str) -> io::Result<Listener> {
    if connect(address).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another nodeaudio daemon is already running",
        ));
    }
    let options = ListenerOptions::new().name(name(address)?);
    restrict(options)?.try_overwrite(true).create_sync()
}

#[cfg(unix)]
fn restrict(options: ListenerOptions<'_>) -> io::Result<ListenerOptions<'_>> {
    use interprocess::os::unix::local_socket::ListenerOptionsExt;
    Ok(options.mode(0o600))
}

#[cfg(windows)]
fn restrict(options: ListenerOptions<'_>) -> io::Result<ListenerOptions<'_>> {
    use interprocess::os::windows::local_socket::ListenerOptionsExt;
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use widestring::U16CString;
    let sddl = U16CString::from_str("D:P(A;;GA;;;OW)(A;;GA;;;SY)")
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    Ok(options.security_descriptor(SecurityDescriptor::deserialize(&sddl)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{read_frame, write_frame};
    use crate::messages::{ClientMsg, PROTOCOL_VERSION};

    fn temp_address(tag: &str) -> String {
        #[cfg(unix)]
        return format!(
            "{}/nodeaudio-test-{tag}-{}.sock",
            std::env::temp_dir().display(),
            std::process::id()
        );
        #[cfg(windows)]
        return format!("nodeaudio-test-{tag}-{}", std::process::id());
    }

    #[test]
    fn client_and_server_exchange_frames() {
        let address = temp_address("exchange");
        let listener = listen(&address).unwrap();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let msg: ClientMsg = read_frame(&mut stream).unwrap();
            msg
        });
        let mut client = connect(&address).unwrap();
        write_frame(
            &mut client,
            &ClientMsg::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        assert_eq!(
            server.join().unwrap(),
            ClientMsg::Hello {
                version: PROTOCOL_VERSION
            }
        );
    }

    #[test]
    fn second_listener_is_refused_while_first_is_live() {
        let address = temp_address("single");
        let _first = listen(&address).unwrap();
        let second = listen(&address);
        assert_eq!(second.unwrap_err().kind(), io::ErrorKind::AddrInUse);
    }

    #[cfg(unix)]
    #[test]
    fn stale_socket_file_is_replaced() {
        let address = temp_address("stale");
        std::fs::write(&address, b"").unwrap();
        assert!(listen(&address).is_ok());
    }
}
