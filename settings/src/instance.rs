//! One settings window per session.
//!
//! The tray icon and the launcher both start this binary, so a second click has to
//! reach the window that is already open rather than stack another. Linux gets an
//! abstract socket for the handshake: the kernel owns the name, so a crashed
//! window leaves nothing stale and no file has to be cleaned up. Windows uses the
//! named mutex it already had.
//!
//! The pane request still travels through the file the window polls. That is the
//! mechanism the tray already used, and one writer — the instance that receives
//! the handshake — is simpler than two processes racing for the same file.

/// Claim this session's window. `false` means another one owns it and has been
/// told which pane was asked for.
#[cfg(target_os = "linux")]
pub fn claim(pane: &str) -> bool {
    use std::io::{BufRead, BufReader, Write};
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
    use std::time::Duration;

    let Ok(address) = SocketAddr::from_abstract_name(b"myware-voice-settings") else {
        return true;
    };

    match UnixListener::bind_addr(&address) {
        Ok(listener) => {
            std::thread::Builder::new()
                .name("instance".into())
                .spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(mut stream) = stream else { continue };
                        let mut requested = String::new();
                        {
                            let mut reader = BufReader::new(&mut stream);
                            if reader.read_line(&mut requested).is_err() {
                                continue;
                            }
                        }
                        let requested = requested.trim();
                        if requested.is_empty() {
                            continue;
                        }
                        if let Ok(path) = myware_voice::dirs::settings_request_file() {
                            if let Some(dir) = path.parent() {
                                let _ = std::fs::create_dir_all(dir);
                            }
                            let _ = std::fs::write(&path, requested);
                        }
                        // Answer only after the request is on disk. A window that
                        // is alive but stopped accepts the connection and never
                        // reads, and the caller has to be able to tell.
                        let _ = stream.write_all(b"ok\n");
                    }
                })
                .ok();
            true
        }
        Err(_) => {
            let mut handled = false;
            if let Ok(mut stream) = UnixStream::connect_addr(&address) {
                let _ = writeln!(stream, "{pane}");
                // Short patience on purpose: a stopped window must not make the
                // click do nothing at all.
                let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
                let mut answer = String::new();
                let mut reader = BufReader::new(&mut stream);
                if reader.read_line(&mut answer).is_ok() && answer.trim() == "ok" {
                    handled = true;
                }
            }
            if handled {
                focus_existing();
                false
            } else {
                // Nobody took it. Opening a second window beats a menu item that
                // silently does nothing, even though it is the lesser evil.
                true
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn claim(_pane: &str) -> bool {
    true
}

/// Raise the window that is already open.
///
/// Wayland has no portable "my other window" call, and iced does not expose the
/// xdg-activation token. The compositor's own client can do it, and on sway —
/// the target — that is one command. Elsewhere the pane still switches; the
/// window is simply not raised, which is a smaller failure than a second window.
#[cfg(target_os = "linux")]
fn focus_existing() {
    let _ = std::process::Command::new("swaymsg")
        .arg("[app_id=myware-voice-settings] focus")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
