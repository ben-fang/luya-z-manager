//! Transfers another thread can stop at once, stalled or not.
//!
//! A ladder run must end within 2 s of a stop, and a stall is what it measures, so short
//! timeouts with reconnects are not an option: they would turn the stall into a disconnect.
//! Every agent made here therefore connects through [`Stopper`], which keeps a handle on each
//! socket and shuts them all down on `stop`; a blocked read or write returns right there.
//!
//! This uses ureq's `unversioned` API, which can break on a minor version (Cargo.lock pins it).
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, RustlsConnector, Transport,
};
use ureq::{Agent, Error};

/// How often a thread waiting on a lookup or a connect looks at the stop flag.
const POLL: Duration = Duration::from_millis(20);
/// Lookup and connect helper threads (getaddrinfo needs some room).
const HELPER_STACK: usize = 256 * 1024;

#[derive(Default)]
struct Shared {
    stopped: AtomicBool,
    next_id: AtomicU64,
    sockets: Mutex<HashMap<u64, TcpStream>>,
    /// ureq resolves before it looks in its pool, so on every request: without this each probe
    /// would cost threads and a DNS query. It also keeps a run on one address.
    resolved: Mutex<HashMap<String, Vec<SocketAddr>>>,
    #[cfg(test)]
    lookups: AtomicU64,
}
impl Shared {
    fn sockets(&self) -> MutexGuard<'_, HashMap<u64, TcpStream>> {
        self.sockets.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
    /// Keep a handle on a new socket. A stop that came first shuts it here: `stop` sets the
    /// flag before it takes the lock, so one of the two always sees the other.
    fn register(&self, stream: &TcpStream) -> Result<u64, Error> {
        let copy = stream.try_clone()?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut sockets = self.sockets();
        if self.is_stopped() {
            let _ = stream.shutdown(Shutdown::Both);
            return Err(stopped());
        }
        sockets.insert(id, copy);
        Ok(id)
    }
}
fn stopped() -> Error {
    Error::Io(io::Error::new(io::ErrorKind::Interrupted, "stopped"))
}
/// Run a call that cannot be interrupted on a helper thread and stop waiting when told to. A
/// helper left behind ends with the call's own timeout and carries no traffic.
fn abortable<T, F>(shared: &Shared, call: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, Error> + Send + 'static,
{
    if shared.is_stopped() {
        return Err(stopped());
    }
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .stack_size(HELPER_STACK)
        .spawn(move || {
            let _ = tx.send(call());
        })?;
    loop {
        match rx.recv_timeout(POLL) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) if shared.is_stopped() => return Err(stopped()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Error::Io(io::Error::other("connect helper died")))
            }
        }
    }
}
/// Try the addresses in order, sharing the time left between those still to try.
fn connect_any(addrs: &[SocketAddr], timeout: Option<Duration>) -> Result<TcpStream, Error> {
    let start = Instant::now();
    let mut last = None;
    for (i, addr) in addrs.iter().enumerate() {
        let left = timeout.map(|t| t.saturating_sub(start.elapsed()));
        if left.is_some_and(|l| l.is_zero()) {
            break;
        }
        let share = left.map(|l| (l / (addrs.len() - i) as u32).max(POLL));
        let attempt = match share {
            Some(t) => TcpStream::connect_timeout(addr, t),
            None => TcpStream::connect(addr),
        };
        match attempt {
            Ok(stream) => return Ok(stream),
            Err(e) => last = Some(e),
        }
    }
    Err(match last {
        Some(e) if e.kind() != io::ErrorKind::TimedOut => Error::Io(e),
        _ => Error::Timeout(ureq::Timeout::Connect),
    })
}

/// One ladder run's stop switch; once stopped it stays stopped.
#[derive(Clone, Default)]
pub struct Stopper(Arc<Shared>);
impl Stopper {
    pub fn new() -> Self {
        Self::default()
    }
    /// A direct agent (no proxy from the environment) whose connections this switch ends.
    pub(crate) fn agent(&self) -> Agent {
        self.agent_idle(None)
    }
    /// The same, with a limit on each socket read and write: a long throttled download cannot
    /// have a limit on the whole request, but a transport silent this long has failed.
    pub(crate) fn agent_idle(&self, idle: Option<Duration>) -> Agent {
        let config = Agent::config_builder().proxy(None).build();
        let connector = StopConnector(self.0.clone(), idle).chain(RustlsConnector::default());
        Agent::with_parts(config, connector, StopResolver(self.0.clone()))
    }
    /// End every transfer: returns right away, the blocked threads wake with an error.
    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::SeqCst);
        for socket in self.0.sockets().values() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
    pub fn is_stopped(&self) -> bool {
        self.0.is_stopped()
    }
}

struct StopResolver(Arc<Shared>);
impl fmt::Debug for StopResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StopResolver")
    }
}
impl Resolver for StopResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, Error> {
        let key = uri.to_string();
        let key = key.split('/').take(3).collect::<Vec<_>>().concat();
        let known = self
            .0
            .resolved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        if let Some(known) = known {
            let mut addrs = self.empty();
            known.into_iter().for_each(|a| addrs.push(a));
            return Ok(addrs);
        }
        #[cfg(test)]
        self.0.lookups.fetch_add(1, Ordering::SeqCst);
        let (uri, config) = (uri.clone(), config.clone());
        let addrs = abortable(&self.0, move || {
            DefaultResolver::default().resolve(&uri, &config, timeout)
        })?;
        self.0
            .resolved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, addrs.iter().copied().collect());
        Ok(addrs)
    }
}

struct StopConnector(Arc<Shared>, Option<Duration>);
impl fmt::Debug for StopConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StopConnector")
    }
}
impl Connector for StopConnector {
    type Out = StopTransport;
    fn connect(
        &self,
        details: &ConnectionDetails,
        _chained: Option<()>,
    ) -> Result<Option<Self::Out>, Error> {
        let addrs: Vec<SocketAddr> = details.addrs.iter().copied().collect();
        let timeout = details.timeout.not_zero().map(|t| *t);
        let stream = abortable(&self.0, move || connect_any(&addrs, timeout))?;
        stream.set_nodelay(true)?;
        let id = self.0.register(&stream)?;
        let config = details.config;
        Ok(Some(StopTransport {
            shared: self.0.clone(),
            id,
            stream,
            idle: self.1,
            buffers: LazyBuffers::new(config.input_buffer_size(), config.output_buffer_size()),
        }))
    }
}

/// ureq's own TCP transport, plus the registration. The socket timeouts are set per call: a
/// syscall each, and a transfer makes few enough calls for that not to matter.
struct StopTransport {
    shared: Arc<Shared>,
    id: u64,
    stream: TcpStream,
    idle: Option<Duration>,
    buffers: LazyBuffers,
}
impl fmt::Debug for StopTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StopTransport")
            .field("id", &self.id)
            .finish()
    }
}
impl Drop for StopTransport {
    fn drop(&mut self) {
        self.shared.sockets().remove(&self.id);
    }
}
/// A timed-out read or write is `WouldBlock` or `TimedOut` depending on the platform.
fn io_result<T>(r: io::Result<T>, timeout: &NextTimeout) -> Result<T, Error> {
    r.map_err(|e| match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Error::Timeout(timeout.reason),
        _ => Error::Io(e),
    })
}
impl StopTransport {
    fn limit(&self, timeout: &NextTimeout) -> Option<Duration> {
        match (timeout.not_zero().map(|t| *t), self.idle) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}
impl Transport for StopTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        self.stream.set_write_timeout(self.limit(&timeout))?;
        let output = &self.buffers.output()[..amount];
        io_result(self.stream.write_all(output), &timeout)
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        self.stream.set_read_timeout(self.limit(&timeout))?;
        let input = self.buffers.input_append_buf();
        let amount = io_result(self.stream.read(input), &timeout)?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }
    fn is_open(&mut self) -> bool {
        if self.shared.is_stopped() || self.stream.set_nonblocking(true).is_err() {
            return false;
        }
        let quiet = matches!(
            self.stream.read(&mut [0]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
        );
        self.stream.set_nonblocking(false).is_ok() && quiet
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;

    /// The spec's limit for a stop, with nothing else in the tests near it.
    const LIMIT: Duration = Duration::from_secs(2);
    /// What the transfers would wait without a stop.
    const LONG: Duration = Duration::from_secs(30);

    /// A loopback server: each accepted connection runs `conn`.
    fn serve(conn: impl Fn(TcpStream) + Send + Sync + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let conn = Arc::new(conn);
        thread::spawn(move || {
            for s in listener.incoming().flatten() {
                let conn = conn.clone();
                thread::spawn(move || conn(s));
            }
        });
        format!("127.0.0.1:{port}")
    }
    fn read_head(s: &mut TcpStream) {
        let mut r = io::BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        while r.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
            line.clear();
        }
    }
    /// Run `transfer` on a thread, stop after 200 ms, and check it ended in time with an error.
    fn stops_in_time(
        stopper: &Stopper,
        transfer: impl FnOnce() -> Result<(), Error> + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || tx.send(transfer()));
        thread::sleep(Duration::from_millis(200));
        assert!(
            rx.try_recv().is_err(),
            "the transfer must be blocked before the stop"
        );
        let t = Instant::now();
        stopper.stop();
        let r = rx
            .recv_timeout(LIMIT)
            .expect("still blocked after the stop");
        assert!(r.is_err(), "a stopped transfer is an error");
        assert!(t.elapsed() < LIMIT);
    }
    fn get(agent: Agent, url: String) -> Result<(), Error> {
        let r = agent
            .get(&url)
            .config()
            .timeout_global(Some(LONG))
            .build()
            .call()?;
        r.into_body().read_to_vec().map(|_| ())
    }

    #[test]
    fn stops_a_server_that_never_answers() {
        let addr = serve(|s| {
            thread::sleep(LONG);
            drop(s);
        });
        let stopper = Stopper::new();
        let agent = stopper.agent();
        stops_in_time(&stopper, move || get(agent, format!("http://{addr}/")));
    }
    #[test]
    fn stops_a_download_stalled_in_the_body() {
        let addr = serve(|mut s| {
            read_head(&mut s);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\nabc");
            thread::sleep(LONG);
        });
        let stopper = Stopper::new();
        let agent = stopper.agent();
        stops_in_time(&stopper, move || get(agent, format!("http://{addr}/")));
    }
    #[test]
    fn stops_an_upload_the_server_does_not_read() {
        let addr = serve(|s| {
            thread::sleep(LONG);
            drop(s);
        });
        let stopper = Stopper::new();
        let agent = stopper.agent();
        // far more than the socket buffers hold, so the write itself blocks
        let body = vec![0u8; 64 * 1024 * 1024];
        stops_in_time(&stopper, move || {
            agent
                .post(format!("http://{addr}/"))
                .config()
                .timeout_global(Some(LONG))
                .build()
                .send(&body[..])
                .map(|_| ())
        });
    }
    #[test]
    fn stops_a_stalled_tls_handshake() {
        let addr = serve(|s| {
            thread::sleep(LONG);
            drop(s);
        });
        let stopper = Stopper::new();
        let agent = stopper.agent();
        stops_in_time(&stopper, move || get(agent, format!("https://{addr}/")));
    }
    #[test]
    fn stops_a_call_that_cannot_be_interrupted() {
        // an unanswered SYN or a slow lookup: no socket to shut down yet
        let stopper = Stopper::new();
        let shared = stopper.0.clone();
        stops_in_time(&stopper, move || {
            abortable(&shared, || {
                thread::sleep(Duration::from_secs(5));
                Ok(())
            })
        });
    }
    #[test]
    fn a_silent_connection_times_out_only_at_the_idle_limit() {
        let addr = serve(|mut s| {
            read_head(&mut s);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc");
            thread::sleep(LONG);
        });
        let agent = Stopper::new().agent_idle(Some(Duration::from_millis(300)));
        let t = Instant::now();
        assert!(agent
            .get(format!("http://{addr}/"))
            .call()
            .unwrap()
            .into_body()
            .read_to_vec()
            .is_err());
        assert!(t.elapsed() < LIMIT);
    }
    #[test]
    fn a_host_is_looked_up_once_per_run() {
        let addr = serve(|mut s| {
            let mut r = io::BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            while r.read_line(&mut line).unwrap_or(0) > 0 {
                if line == "\r\n" {
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                }
                line.clear();
            }
        });
        let port = addr.rsplit(':').next().unwrap();
        let stopper = Stopper::new();
        for agent in [stopper.agent(), stopper.agent()] {
            for path in ["a", "b", "c"] {
                let url = format!("http://localhost:{port}/{path}");
                get(agent.clone(), url).unwrap();
            }
        }
        assert_eq!(stopper.0.lookups.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn nothing_connects_after_a_stop() {
        let connections = Arc::new(AtomicUsize::new(0));
        let seen = connections.clone();
        let addr = serve(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        let stopper = Stopper::new();
        stopper.stop();
        assert!(stopper.is_stopped());
        assert!(get(stopper.agent(), format!("http://{addr}/")).is_err());
        thread::sleep(Duration::from_millis(100));
        assert_eq!(connections.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn a_short_stall_is_not_a_failure_and_connections_are_kept() {
        let connections = Arc::new(AtomicUsize::new(0));
        let seen = connections.clone();
        let addr = serve(move |mut s| {
            seen.fetch_add(1, Ordering::SeqCst);
            let mut r = io::BufReader::new(s.try_clone().unwrap());
            loop {
                let mut line = String::new();
                loop {
                    line.clear();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nab");
                thread::sleep(Duration::from_secs(1));
                let _ = s.write_all(b"cd");
            }
        });
        let stopper = Stopper::new();
        let agent = stopper.agent();
        for _ in 0..2 {
            let r = agent.get(format!("http://{addr}/")).call().unwrap();
            assert_eq!(r.into_body().read_to_vec().unwrap(), b"abcd");
        }
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert_eq!(stopper.0.sockets().len(), 1);
        drop(agent);
        assert_eq!(stopper.0.sockets().len(), 0, "sockets are not kept alive");
    }
}
