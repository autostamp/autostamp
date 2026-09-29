#![allow(dead_code)]

use std::{
    ffi::OsString,
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) const TOKEN: &str = "fixture-token";
pub(crate) const SESSION: &str = "session-fixture";
pub(crate) const PROTOCOL: &str = "2025-11-25";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub(crate) fn root() -> PathBuf {
    crate_dir()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

pub(crate) fn wasmtime() -> OsString {
    std::env::var_os("WASMTIME").unwrap_or_else(|| "wasmtime".into())
}

pub(crate) fn cargo() -> Command {
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(root());
    command
}

#[derive(Debug)]
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(label: &str) -> Self {
        assert!(
            label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        );
        let base = root().join("target/mcp-integration-tests");
        fs::create_dir_all(&base).expect("create fixture parent");
        for _ in 0..100 {
            let path = base.join(format!(
                "{label}-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path.canonicalize().expect("canonical fixture path")),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create fixture directory: {error}"),
            }
        }
        panic!("allocate unique fixture directory");
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("fixture cleanup failed at {}: {error}", self.0.display());
            assert!(thread::panicking(), "fixture directory cleanup failed");
        }
    }
}

struct RunningChild(Child);

impl Drop for RunningChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            self.0.kill().expect("terminate timed-out fixture child");
            self.0.wait().expect("reap fixture child");
        }
    }
}

pub(crate) fn output(command: &mut Command) -> Output {
    let logs = TempDir::new("process");
    let stdout_path = logs.path().join("stdout");
    let stderr_path = logs.path().join("stderr");
    command
        .stdin(Stdio::null())
        .stdout(fs::File::create(&stdout_path).unwrap())
        .stderr(fs::File::create(&stderr_path).unwrap());
    let child = command.spawn();
    // Release the parent's log handles before cleanup, including on Windows.
    command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    let mut child = RunningChild(
        child.unwrap_or_else(|error| panic!("start fixture command {command:?}: {error}")),
    );
    let deadline = Instant::now() + Duration::from_secs(240);
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("wait for fixture child") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "fixture command exceeded 240 seconds: {command:?}"
        );
        thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: fs::read(stdout_path).unwrap(),
        stderr: fs::read(stderr_path).unwrap(),
    }
}

pub(crate) fn run(command: &mut Command) -> Output {
    let output = output(command);
    assert!(
        output.status.success(),
        "fixture command {command:?} failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Request {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug)]
pub(crate) struct Stream {
    pub(crate) chunks: Vec<Vec<u8>>,
    pub(crate) gate_before_chunk: Option<(usize, Arc<AtomicBool>)>,
    pub(crate) interval: Duration,
    pub(crate) release: Option<Arc<AtomicBool>>,
    pub(crate) opened: Option<Arc<AtomicBool>>,
    pub(crate) terminate: bool,
}

#[derive(Debug)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
    pub(crate) stream: Option<Stream>,
}

impl Reply {
    pub(crate) fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.into(),
            stream: None,
        }
    }

    pub(crate) fn empty(status: u16) -> Self {
        Self::json(status, Vec::new())
    }
}

#[derive(Debug)]
pub(crate) struct Server {
    endpoint: String,
    shutdown: Arc<AtomicBool>,
    errors: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    pub(crate) fn new(handler: impl Fn(Request) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fixture server");
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let shutdown = Arc::new(AtomicBool::new(false));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let handle = {
            let shutdown = shutdown.clone();
            let errors = errors.clone();
            let handler = Arc::new(handler);
            thread::spawn(move || {
                let mut workers = Vec::new();
                while !shutdown.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((socket, _)) => {
                            let shutdown = shutdown.clone();
                            let errors = errors.clone();
                            let handler = handler.clone();
                            workers.push(thread::spawn(move || {
                                let result =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        socket.set_nonblocking(false)?;
                                        socket.set_read_timeout(Some(Duration::from_secs(2)))?;
                                        socket.set_write_timeout(Some(Duration::from_secs(2)))?;
                                        let mut socket = BufReader::new(socket);
                                        let request = read_request(&mut socket)?;
                                        let reply = handler(request);
                                        write_reply(socket.get_mut(), reply, &shutdown)
                                    }));
                                match result {
                                    Ok(Ok(())) => {}
                                    Ok(Err(error))
                                        if matches!(
                                            error.kind(),
                                            io::ErrorKind::BrokenPipe
                                                | io::ErrorKind::ConnectionReset
                                                | io::ErrorKind::ConnectionAborted
                                        ) => {}
                                    Ok(Err(error)) => {
                                        errors.lock().unwrap().push(error.to_string())
                                    }
                                    Err(_) => errors
                                        .lock()
                                        .unwrap()
                                        .push("fixture handler panicked".into()),
                                }
                            }));
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => {
                            errors
                                .lock()
                                .unwrap()
                                .push(format!("accept fixture connection: {error}"));
                            break;
                        }
                    }
                }
                for worker in workers {
                    if worker.join().is_err() {
                        errors
                            .lock()
                            .unwrap()
                            .push("fixture worker panicked".into());
                    }
                }
            })
        };
        Self {
            endpoint,
            shutdown,
            errors,
            thread: Some(handle),
        }
    }

    pub(crate) fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    pub(crate) fn assert_healthy(&self) {
        let errors = self.errors.lock().unwrap();
        assert!(errors.is_empty(), "{errors:?}");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let result = handle.join();
            if !thread::panicking() {
                result.expect("join fixture server");
                self.assert_healthy();
            }
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn line(reader: &mut impl BufRead) -> io::Result<String> {
    let mut data = Vec::new();
    reader.take(65_537).read_until(b'\n', &mut data)?;
    if data.len() > 65_536 || !data.ends_with(b"\r\n") {
        return Err(invalid("invalid or oversized fixture HTTP line"));
    }
    String::from_utf8(data).map_err(|_| invalid("non-UTF8 fixture HTTP header"))
}

fn read_request(reader: &mut impl BufRead) -> io::Result<Request> {
    let start = line(reader)?;
    let method = start
        .split_whitespace()
        .next()
        .ok_or_else(|| invalid("missing method"))?
        .to_owned();
    let mut headers = Vec::new();
    loop {
        let text = line(reader)?;
        if text == "\r\n" {
            break;
        }
        if headers.len() >= 100 {
            return Err(invalid("too many fixture HTTP headers"));
        }
        let (name, value) = text
            .split_once(':')
            .ok_or_else(|| invalid("invalid fixture header"))?;
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
    let mut request = Request {
        method,
        headers,
        body: Vec::new(),
    };
    if request.header("transfer-encoding") == Some("chunked") {
        loop {
            let length = line(reader)?;
            let length = length.trim().split(';').next().unwrap();
            let length =
                usize::from_str_radix(length, 16).map_err(|_| invalid("invalid chunk length"))?;
            if length == 0 {
                if line(reader)? != "\r\n" {
                    return Err(invalid("unexpected fixture request trailers"));
                }
                break;
            }
            append_body(reader, &mut request.body, length)?;
            if line(reader)? != "\r\n" {
                return Err(invalid("missing chunk delimiter"));
            }
        }
    } else if let Some(length) = request.header("content-length") {
        let length = length
            .parse()
            .map_err(|_| invalid("invalid content length"))?;
        append_body(reader, &mut request.body, length)?;
    }
    Ok(request)
}

fn append_body(reader: &mut impl Read, body: &mut Vec<u8>, length: usize) -> io::Result<()> {
    let total = body
        .len()
        .checked_add(length)
        .filter(|size| *size <= 1024 * 1024)
        .ok_or_else(|| invalid("fixture request exceeds 1 MiB"))?;
    let start = body.len();
    body.resize(total, 0);
    reader.read_exact(body.get_mut(start..).unwrap())
}

fn write_reply(socket: &mut TcpStream, reply: Reply, shutdown: &AtomicBool) -> io::Result<()> {
    write!(
        socket,
        "HTTP/1.1 {} Fixture\r\nConnection: close\r\n",
        reply.status
    )?;
    for (name, value) in &reply.headers {
        write!(socket, "{name}: {value}\r\n")?;
    }
    if let Some(stream) = reply.stream {
        write!(socket, "Transfer-Encoding: chunked\r\n\r\n")?;
        socket.flush()?;
        if let Some(opened) = stream.opened {
            opened.store(true, Ordering::SeqCst);
        }
        for (index, chunk) in stream.chunks.into_iter().enumerate() {
            if let Some((gate_index, ready)) = &stream.gate_before_chunk
                && index == *gate_index
            {
                wait(Duration::from_secs(5), shutdown, Some(ready));
                if !ready.load(Ordering::SeqCst) && !shutdown.load(Ordering::SeqCst) {
                    return Err(invalid(
                        "fixture peer never acknowledged the gated server request",
                    ));
                }
            }
            if shutdown.load(Ordering::SeqCst) {
                return Ok(());
            }
            write!(socket, "{:x}\r\n", chunk.len())?;
            socket.write_all(&chunk)?;
            socket.write_all(b"\r\n")?;
            socket.flush()?;
            wait(stream.interval, shutdown, None);
        }
        if let Some(release) = stream.release {
            wait(Duration::from_secs(5), shutdown, Some(&release));
        }
        if stream.terminate && !shutdown.load(Ordering::SeqCst) {
            socket.write_all(b"0\r\n\r\n")?;
            socket.flush()?;
        }
    } else {
        if !reply
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        {
            write!(socket, "Content-Length: {}\r\n", reply.body.len())?;
        }
        socket.write_all(b"\r\n")?;
        socket.write_all(&reply.body)?;
        socket.flush()?;
    }
    Ok(())
}

fn wait(duration: Duration, shutdown: &AtomicBool, release: Option<&AtomicBool>) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline
        && !shutdown.load(Ordering::SeqCst)
        && !release.is_some_and(|flag| flag.load(Ordering::SeqCst))
    {
        thread::sleep(Duration::from_millis(2));
    }
}
