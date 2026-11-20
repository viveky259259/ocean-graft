//! Wake-on-request for session services.
//!
//! `ocean-graft serve --on-demand` runs the service on a private port and this
//! gateway on the session's public port. The gateway is a plain TCP relay
//! (so HTTP, WebSockets and HMR all pass through) that:
//!   - starts the service again with `ocean-graft wake` when a connection arrives
//!     and nothing answers on the private port (because `reap` stopped it),
//!     holding the connection until the service is up;
//!   - records each connection as activity for the session, so a preview
//!     that someone is using is never reaped.
//!
//! It is a small separate process, started detached by `serve` and stopped
//! by `stop`, `rm` and `down`. `reap` leaves it running.

use anyhow::{bail, Context, Result};
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const MAX_CONNECTIONS: usize = 128;

/// The file whose modification time records a session's latest request.
pub fn activity_file(project_dir: &Path, session: &str) -> PathBuf {
    project_dir.join("activity").join(session)
}

/// A free TCP port on the loopback interface.
pub fn free_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("could not find a free port")?;
    Ok(listener.local_addr()?.port())
}

/// The loopback addresses a service may listen on: dev servers bound to
/// "localhost" listen on ::1 only on macOS (Node 17 and later).
fn loopbacks(port: u16) -> [std::net::SocketAddr; 2] {
    [
        std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
    ]
}

fn answers(port: u16) -> bool {
    loopbacks(port)
        .iter()
        .any(|a| TcpStream::connect_timeout(a, Duration::from_millis(200)).is_ok())
}

fn connect_upstream(port: u16) -> std::io::Result<TcpStream> {
    let mut last = None;
    for addr in loopbacks(port) {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.expect("two addresses tried"))
}

struct Gateway {
    project_dir: PathBuf,
    session: String,
    internal_port: u16,
    ocean_graft: PathBuf,
    /// Only one connection at a time wakes the service; the rest wait for it.
    waking: Mutex<()>,
    last_touch: Mutex<Option<Instant>>,
    active: AtomicUsize,
}

struct ConnGuard(Arc<Gateway>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Gateway {
    fn touch(&self) {
        let mut last = self.last_touch.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        *last = Some(Instant::now());
        drop(last);
        let path = activity_file(&self.project_dir, &self.session);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path);
        if let Ok(file) = file {
            let _ = file.set_modified(SystemTime::now());
        }
    }

    fn ensure_up(&self) -> Result<()> {
        if answers(self.internal_port) {
            return Ok(());
        }
        let _guard = self.waking.lock().unwrap_or_else(|e| e.into_inner());
        if answers(self.internal_port) {
            return Ok(()); // another connection woke it while we waited
        }
        eprintln!(
            "gateway: nothing on port {}, waking {}",
            self.internal_port, self.session
        );
        let started = Instant::now();
        let status = std::process::Command::new(&self.ocean_graft)
            .arg("--project")
            .arg(&self.project_dir)
            .args(["wake", &self.session])
            .stdin(std::process::Stdio::null())
            .status()
            .context("could not run ocean-graft wake")?;
        if !status.success() {
            bail!("ocean-graft wake {} failed ({status})", self.session);
        }
        while started.elapsed() < Duration::from_secs(60) {
            if answers(self.internal_port) {
                eprintln!(
                    "gateway: {} is up after {:.1} s",
                    self.session,
                    started.elapsed().as_secs_f64()
                );
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!(
            "{} did not answer on port {} within 60 s",
            self.session,
            self.internal_port
        )
    }

    fn relay(self: &Arc<Self>, client: TcpStream) {
        self.touch();
        // Try connecting directly first to avoid a redundant probe connection on
        // every request; only wake the service if nothing is listening.
        let upstream = match connect_upstream(self.internal_port) {
            Ok(s) => s,
            Err(_) => {
                if let Err(err) = self.ensure_up() {
                    eprintln!("gateway: {err:#}");
                    let _ = client.shutdown(Shutdown::Both);
                    return;
                }
                match connect_upstream(self.internal_port) {
                    Ok(s) => s,
                    Err(err) => {
                        eprintln!(
                            "gateway: could not reach port {}: {err}",
                            self.internal_port
                        );
                        let _ = client.shutdown(Shutdown::Both);
                        return;
                    }
                }
            }
        };
        let _ = client.set_nodelay(true);
        let _ = upstream.set_nodelay(true);
        let client = Arc::new(client);
        let upstream = Arc::new(upstream);
        let c_in = Arc::clone(&client);
        let u_out = Arc::clone(&upstream);
        let gw_fwd = Arc::clone(self);
        let forward = std::thread::Builder::new().spawn(move || pipe(&c_in, &u_out, &gw_fwd));
        let Ok(forward) = forward else {
            let _ = client.shutdown(Shutdown::Both);
            let _ = upstream.shutdown(Shutdown::Both);
            return;
        };
        pipe(&upstream, &client, self);
        let _ = forward.join();
    }
}

/// Copy one direction until it closes, then close the other side's writer.
fn pipe(mut from: &TcpStream, mut to: &TcpStream, gateway: &Gateway) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
            Ok(n) => {
                gateway.touch();
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}

/// Run the gateway in the foreground until killed.
pub fn run(project_dir: &Path, session: &str, public_port: u16, internal_port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", public_port))
        .with_context(|| format!("could not listen on port {public_port}"))?;
    // Browsers try ::1 first for http://localhost; answer there too when
    // the machine has IPv6 loopback.
    let listener6 = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, public_port)).ok();
    eprintln!("gateway: port {public_port} -> {internal_port} for {session}, waking it on demand");
    let gateway = Arc::new(Gateway {
        project_dir: project_dir.to_path_buf(),
        session: session.to_string(),
        internal_port,
        ocean_graft: std::env::current_exe().context("could not locate the ocean-graft binary")?,
        waking: Mutex::new(()),
        last_touch: Mutex::new(None),
        active: AtomicUsize::new(0),
    });
    let serve = |listener: TcpListener, gateway: Arc<Gateway>| {
        for stream in listener.incoming() {
            match stream {
                Ok(client) => {
                    let prev = gateway.active.fetch_add(1, Ordering::Relaxed);
                    if prev >= MAX_CONNECTIONS {
                        gateway.active.fetch_sub(1, Ordering::Relaxed);
                        let _ = client.shutdown(Shutdown::Both);
                        continue;
                    }
                    let gw = Arc::clone(&gateway);
                    let guard = ConnGuard(Arc::clone(&gateway));
                    if std::thread::Builder::new()
                        .spawn(move || {
                            let _guard = guard;
                            gw.relay(client);
                        })
                        .is_err()
                    {
                        eprintln!("gateway: could not spawn connection worker");
                    }
                }
                Err(err) => {
                    eprintln!("gateway: accept failed: {err}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    };
    if let Some(l6) = listener6 {
        let g = Arc::clone(&gateway);
        let _ = std::thread::Builder::new().spawn(move || serve(l6, g));
    }
    serve(listener, gateway);
    Ok(())
}
