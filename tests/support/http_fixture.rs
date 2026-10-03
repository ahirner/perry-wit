use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Owns the pinned HTTP host for component tests and releases it even after assertion failures.
pub struct ServingComponent {
    pub address: SocketAddr,
    child: Child,
}

impl ServingComponent {
    pub fn new(wasm: &Path, wasmtime: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let diagnostic_path = wasm.with_extension("serve.log");
        let diagnostics = fs::File::create(&diagnostic_path).unwrap();
        let mut fixture = Self {
            address,
            child: Command::new(wasmtime)
                .args([
                    "serve",
                    "-C",
                    "cache=n",
                    "-O",
                    "pooling-max-tables-per-module=2",
                    "-S",
                    "cli=y",
                    "--addr",
                ])
                .arg(address.to_string())
                .args([
                    "--max-instance-reuse-count",
                    "10000",
                    "--idle-instance-timeout",
                    "30s",
                    "--max-concurrent-requests",
                    "1",
                ])
                .arg(wasm)
                .stdout(Stdio::null())
                .stderr(diagnostics)
                .spawn()
                .unwrap(),
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if TcpStream::connect(address).is_ok() {
                break;
            }
            assert!(
                fixture.child.try_wait().unwrap().is_none() && Instant::now() < deadline,
                "HTTP host did not start: {}",
                fs::read_to_string(&diagnostic_path).unwrap()
            );
            thread::sleep(Duration::from_millis(10));
        }
        fixture
    }
}

impl Drop for ServingComponent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub enum Reply {
    Body(u16, String),
    WithHeaders(u16, Vec<(String, String)>, String),
    Bytes(u16, Vec<u8>),
    Disconnect,
    Stall,
    StallBody,
}

pub struct HttpFixture {
    pub address: SocketAddr,
    pub requests: Arc<Mutex<Vec<Request>>>,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl HttpFixture {
    pub fn new(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (stop, received) = (shutdown.clone(), requests.clone());
        let handler = Arc::new(handler);
        let worker = thread::spawn(move || {
            let mut connections = Vec::new();
            for stream in listener.incoming() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let mut stream = stream.unwrap();
                let (handler, stop, received) = (handler.clone(), stop.clone(), received.clone());
                connections.push(thread::spawn(move || {
                    let Some(request) = read_request(&mut stream) else { return; };
                    received.lock().unwrap().push(request.clone());
                    match handler(&request) {
                        Reply::Body(status, body) => {
                            let _ = write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                            let _ = stream.write_all(body.as_bytes());
                        }
                        Reply::Bytes(status, body) => {
                            let _ = write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                            let _ = stream.write_all(&body);
                        }
                        Reply::WithHeaders(status, headers, body) => {
                            let _ = write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                            for (name, value) in headers {
                                let _ = write!(stream, "{name}: {value}\r\n");
                            }
                            let _ = stream.write_all(b"\r\n");
                            if request.method != "HEAD" {
                                let _ = stream.write_all(body.as_bytes());
                            }
                        }
                        Reply::Disconnect => {}
                        reply @ (Reply::Stall | Reply::StallBody) => {
                            if matches!(reply, Reply::StallBody) {
                                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n");
                            }
                            while !stop.load(Ordering::Relaxed) {
                                thread::sleep(Duration::from_millis(10));
                            }
                        }
                    }
                }));
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });
        Self {
            address,
            requests,
            shutdown,
            worker: Some(worker),
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = Vec::new();
    let header_end = loop {
        let mut buffer = [0u8; 4096];
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..count]);
        if let Some(offset) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break offset + 4;
        }
    };
    let header = String::from_utf8_lossy(&request[..header_end]);
    let mut lines = header.lines();
    let mut first = lines.next()?.split_whitespace();
    let (method, target) = (first.next()?.to_string(), first.next()?.to_string());
    let headers: Vec<_> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map(|(_, value)| value.parse::<usize>().unwrap())
        .unwrap_or(0);
    while request.len() < header_end + length {
        let mut buffer = [0u8; 4096];
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..count]);
    }
    Some(Request {
        method,
        target,
        headers,
        body: request[header_end..header_end + length].to_vec(),
    })
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.address);
        self.worker.take().unwrap().join().unwrap();
    }
}
