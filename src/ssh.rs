//! `--ssh=<destination>`: every request this browser makes leaves from the other end of an ssh
//! connection.
//!
//! ```sh
//! bru --ssh=user@host http://localhost:3000/   # the remote machine's localhost
//! ```
//!
//! ## Why a tunnel and not a browser over there
//!
//! Running bru on the remote machine and forwarding its window is the obvious answer and the wrong
//! one: every frame the page paints has to cross the network, and so does every keystroke before
//! the page can react to it. What actually needs to be remote is the *socket* — so the page renders
//! here, at the speed of this machine's GPU, and only the bytes it fetches take the long way.
//! `ssh -D` has done exactly this since before browsers had tabs; the whole of this module is
//! starting it, waiting for it, and telling Chromium it is there.
//!
//! ## What it is not
//!
//! **It is not privacy and must not be sold as it.** It is one hop: the remote machine sees every
//! host bru asks for, and so does everything between it and them. What it buys is *reachability* —
//! the remote machine's `localhost`, its private network, its view of the internet.
//!
//! **It is not repaired if it dies.** ssh authentication can be a passphrase, a hardware key or a
//! prompt on a phone, and none of those can be repeated by a browser that noticed a dead child
//! three hours later. A tunnel that fell over says so, once, and loads fail until bru is restarted
//! — which is the honest failure, and it is loud. Auto-reconnect is a decision for the day
//! something here can prove it needs no interaction.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How the destination is spelled. One switch, one value, and everything else about the connection
/// — port, identity, jump host, keepalives — belongs in `~/.ssh/config` where the user already
/// keeps it and where `ssh` itself will read it.
pub const SWITCH: &str = "--ssh=";

/// How long to wait for the tunnel before giving up.
///
/// **Long, because the wait is a human's.** A key with a passphrase, a touch on a hardware token or
/// a push notification all happen inside this window, and a browser that gave up at five seconds
/// would be unusable with any of them. Nothing is blocked by it that a person is not already
/// waiting for: this runs before CEF is initialised, so there is no window to be unresponsive.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// The port the SOCKS proxy is listening on, once it is.
static PORT: OnceLock<u16> = OnceLock::new();

/// The ssh process, so that it can be killed when bru exits rather than outliving it.
static CHILD: Mutex<Option<Child>> = Mutex::new(None);

/// The destination `--ssh=` names, read from the raw argv.
///
/// Read from argv rather than from CEF's parsed command line because this runs before
/// `initialize()` and because the rule it has to share is `--socket=`'s: **only before `--remote`**.
/// `--remote` takes the rest of the line as its message, so an `--ssh=` after it is text — part of
/// a command, or a URL's query string — and reading it as a switch would open a tunnel because
/// somebody linked to one.
pub fn destination_from(args: &[String]) -> Option<&str> {
    let before = args.iter().position(|arg| arg == "--remote").unwrap_or(args.len());
    args[..before]
        .iter()
        .filter_map(|arg| arg.strip_prefix(SWITCH))
        .rfind(|dest| !dest.trim().is_empty())
        .map(str::trim)
}

/// The value for Chromium's `--proxy-server`, or `None` when no tunnel was asked for.
///
/// `socks5://` and **not** `socks5h://`: Chromium resolves hostnames through the SOCKS5 proxy
/// already, so the `h` spelling buys nothing here — and if it did not, this would be leaking every
/// hostname bru visits to the local resolver while the traffic itself went down the tunnel, which
/// is the failure that looks like it is working.
pub fn proxy_switch() -> Option<String> {
    PORT.get().map(|port| format!("socks5://127.0.0.1:{port}"))
}

/// Start the tunnel and wait for it to answer. Called from `main`, before `initialize`.
///
/// The order is load-bearing. The port is taken first so ssh can be told where to listen; ssh is
/// started with **bru's own stdio**, so a passphrase prompt reaches the terminal bru was launched
/// from; and only then is the port polled, because a connection that succeeds is the only proof
/// that the far end is actually forwarding rather than that ssh is still asking for a password.
pub fn start(destination: &str) -> Result<(), String> {
    let port = free_port()?;
    let mut command = Command::new("ssh");
    command.args(argv(destination, port));
    let child = command
        .spawn()
        .map_err(|e| format!("could not run ssh: {e}"))?;
    if let Ok(mut slot) = CHILD.lock() {
        *slot = Some(child);
    }
    wait_until_ready(port)?;
    let _ = PORT.set(port);
    eprintln!("bru: --ssh: every request goes through {destination} (SOCKS5 on 127.0.0.1:{port})");
    watch();
    Ok(())
}

/// What ssh is asked to do.
///
/// - `-N` — no remote command. This is a tunnel and nothing else, so there is no shell to be left
///   running on the far end.
/// - `-T` — no pty either, which keeps ssh from allocating one for a session that has no command.
/// - `-D 127.0.0.1:<port>` — the SOCKS proxy, bound to loopback **explicitly**. `-D <port>` alone
///   binds by `GatewayPorts`, and a tunnel other machines can borrow is not what was asked for.
/// - `-o ExitOnForwardFailure=yes` — without it ssh connects happily with no forwarding at all when
///   the port is taken, and bru would then browse through a proxy that answers nothing.
/// - `-o ServerAliveInterval=30` — so a tunnel that has silently died is noticed by ssh exiting,
///   which is what [`watch`] reports, rather than by every page load hanging.
fn argv(destination: &str, port: u16) -> Vec<String> {
    [
        "-N",
        "-T",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=30",
        "-D",
        &format!("127.0.0.1:{port}"),
        destination,
    ]
    .iter()
    .map(|word| word.to_string())
    .collect()
}

/// A port nothing is using, by taking one and letting it go.
///
/// **The gap between the drop and ssh's bind is a real race and is not engineered away.** Closing
/// it would mean handing ssh an inherited descriptor, which `-D` has no spelling for. What makes it
/// tolerable is `ExitOnForwardFailure=yes`: if something did take the port in that gap, ssh fails
/// loudly instead of connecting without a forward, and [`start`] reports it.
fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .map_err(|e| format!("no free local port for the tunnel: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("could not read the tunnel's port: {e}"))?
        .port();
    drop(listener);
    Ok(port)
}

/// Poll the port until the proxy answers, ssh exits, or the wait runs out.
fn wait_until_ready(port: u16) -> Result<(), String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    loop {
        match TcpStream::connect_timeout(&address.into(), Duration::from_millis(200)) {
            Ok(_) => return Ok(()),
            // Nothing is listening yet, which is the normal case for most of this loop.
            Err(e) if e.kind() == ErrorKind::ConnectionRefused || e.kind() == ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("could not reach the tunnel on 127.0.0.1:{port}: {e}")),
        }
        // ssh giving up is the answer, and a faster one than the deadline.
        if let Ok(mut slot) = CHILD.lock() {
            if let Some(child) = slot.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        *slot = None;
                        return Err(format!(
                            "ssh exited before the tunnel came up ({status}); its own message is \
                             above this line"
                        ));
                    }
                    Ok(None) => {}
                    Err(e) => return Err(format!("could not wait for ssh: {e}")),
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the tunnel did not come up within {}s",
                READY_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Notice an ssh that dies under a running browser, and say so where the user is looking.
///
/// A message and not a repair, for the reason in the module header. It is a `message::error` rather
/// than a line on stderr because by this point bru has a window and the terminal it was started
/// from may be gone.
fn watch() {
    let _ = std::thread::Builder::new()
        .name("bru-ssh".to_string())
        .spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let Ok(mut slot) = CHILD.lock() else {
                    return;
                };
                let Some(child) = slot.as_mut() else {
                    return;
                };
                match child.try_wait() {
                    Ok(Some(status)) => {
                        *slot = None;
                        crate::message::error(&format!(
                            "the ssh tunnel died ({status}); loads will fail until bru is restarted"
                        ));
                        return;
                    }
                    Ok(None) => {}
                    Err(_) => return,
                }
            }
        });
}

/// Kill the tunnel. Called from `main` on the way out, after `shutdown`.
///
/// bru started this process, so bru ends it: an `ssh -N` whose parent is gone has nothing to notice
/// and would sit holding a port until somebody found it in `ps`.
pub fn stop() {
    if let Ok(mut slot) = CHILD.lock() {
        if let Some(mut child) = slot.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(rest: &[&str]) -> Vec<String> {
        std::iter::once("bru".to_string())
            .chain(rest.iter().map(|arg| arg.to_string()))
            .collect()
    }

    #[test]
    fn the_switch_names_the_destination() {
        assert_eq!(destination_from(&args(&["--ssh=user@host"])), Some("user@host"));
        assert_eq!(destination_from(&args(&["--ssh=host", "https://x/"])), Some("host"));
        assert_eq!(destination_from(&args(&["https://x/"])), None);
    }

    /// The last wins, so appending to a wrapper script overrides what the script already passed —
    /// `--socket=`'s rule, and Chromium's for its own switches.
    #[test]
    fn the_last_switch_wins() {
        assert_eq!(destination_from(&args(&["--ssh=a", "--ssh=b"])), Some("b"));
    }

    /// An empty or blank value is not a destination. Falling through to no tunnel beats running
    /// `ssh -D … ""`, which would fail with ssh's usage rather than with bru's reason.
    #[test]
    fn an_empty_destination_is_not_one() {
        assert_eq!(destination_from(&args(&["--ssh="])), None);
        assert_eq!(destination_from(&args(&["--ssh=   "])), None);
        // A real one behind an empty one still counts.
        assert_eq!(destination_from(&args(&["--ssh=", "--ssh=host"])), Some("host"));
    }

    /// **`--remote` takes the rest of the line as its message**, so an `--ssh=` inside a URL is a
    /// URL. Reading it as a switch would open a tunnel because somebody linked to one.
    #[test]
    fn an_ssh_after_remote_is_part_of_the_message() {
        let line = args(&["--remote", ":open", "https://x/?q=--ssh=user@evil"]);
        assert_eq!(destination_from(&line), None);
    }

    /// The forward is bound to loopback by name, and the two options that keep a dead tunnel from
    /// looking like a working one are both there.
    #[test]
    fn ssh_is_asked_for_a_loopback_forward_that_fails_loudly() {
        let line = argv("user@host", 1080);
        assert_eq!(line.first().map(String::as_str), Some("-N"));
        assert_eq!(line.last().map(String::as_str), Some("user@host"));
        assert!(line.contains(&"-D".to_string()));
        assert!(line.contains(&"127.0.0.1:1080".to_string()));
        assert!(line.contains(&"ExitOnForwardFailure=yes".to_string()));
        assert!(line.contains(&"ServerAliveInterval=30".to_string()));
    }

    /// No tunnel, no proxy switch — and nothing that could half-configure Chromium.
    #[test]
    fn without_a_tunnel_there_is_no_proxy_switch() {
        assert_eq!(proxy_switch(), None);
    }

    /// A port that was free a moment ago, which is all this promises.
    #[test]
    fn a_free_port_is_a_port() {
        assert!(free_port().expect("a port") > 0);
    }
}
