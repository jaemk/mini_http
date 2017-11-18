#![recursion_limit="1024"]
#[macro_use] extern crate error_chain;
#[macro_use] extern crate log;
extern crate mio;
extern crate slab;
extern crate threadpool;
extern crate num_cpus;
extern crate httparse;
extern crate http;

#[macro_use] mod macros;
mod errors;

use std::io::{self, Read, Write};
use std::sync;
use std::sync::mpsc::{channel, Receiver};
use mio::net::{TcpListener};
pub use errors::*;



/// Represent everything about a request except its (possible) body
pub type RequestHead = http::Request<()>;

/// Re-exported `http::Response` for constructing return responses in handlers
pub use http::Response as HttpResponse;


/// Internal `http::Response` wrapper with helpers for constructing the bytes
/// that needs to be written back a Stream
struct Response {
    inner: http::Response<Vec<u8>>,
    header_data: Vec<u8>
}
impl Response {
    fn new(inner: http::Response<Vec<u8>>) -> Self {
        Self { inner, header_data: Vec::with_capacity(1024) }
    }
    fn serialize_headers(&mut self) {
        let status = self.inner.status();
        let s = format!("HTTP/1.1 {} {}\r\n", status.as_str(), status.canonical_reason().unwrap_or("Unsupported Status"));
        self.header_data.extend_from_slice(&s.as_bytes());
        for (key, value) in self.inner.headers().iter() {
            self.header_data.extend_from_slice(key.as_str().as_bytes());
            self.header_data.extend_from_slice(b": ");
            self.header_data.extend_from_slice(value.as_bytes());
            self.header_data.extend_from_slice(b"\r\n");
        }
        self.header_data.extend_from_slice(b"\r\n");
    }
}
impl std::ops::Deref for Response {
    type Target = http::Response<Vec<u8>>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl std::ops::DerefMut for Response {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}


/// `Request` received and used by handlers. Wraps & `deref`s to an `http::Request`
/// and patches `Request::body` to return the correct slice of bytes from the
/// `HttpStreamReader.read_buf`
pub struct Request {
    inner: http::Request<Vec<u8>>,
    body_start: usize,
}
impl Request {
    pub fn body(&self) -> &[u8] {
        &self.inner.body()[self.body_start..]
    }
}
impl std::ops::Deref for Request {
    type Target = http::Request<Vec<u8>>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl std::ops::DerefMut for Request {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}


/// Http reader/parser for incrementally reading a request and
/// parsing its headers
struct HttpStreamReader {
    read_buf: Vec<u8>,
    header_lines: usize,
    headers_length: usize,
    headers_complete: bool,
    request: Option<RequestHead>,

    content_length: usize,
    body_bytes_read: usize,
    body_complete: bool,
}
impl std::default::Default for HttpStreamReader {
    fn default() -> HttpStreamReader {
        HttpStreamReader {
            read_buf: Vec::new(),
            header_lines: 0, headers_length: 0, headers_complete: false, request: None,
            content_length: 0, body_bytes_read: 0, body_complete: false,
        }
    }
}
impl HttpStreamReader {
    fn new() -> Self {
        Self {
            read_buf: Vec::with_capacity(1024),
            ..Self::default()
        }
    }

    /// Save a new chunk of bytes
    fn receive_chunk(&mut self, chunk: &[u8]) -> usize {
        self.read_buf.extend_from_slice(chunk);
        self.read_buf.len()
    }

    /// Try parsing the current bytes into request headers
    /// TODO: Checking if headers are completely received
    ///       could be improved to avoid scanning the whole
    ///       thing everytime.
    fn try_build_request(&mut self) -> Result<Option<RequestHead>> {
        if !self.headers_complete {
            // check if we've got enough data to successfully parse the request
            const R: u8 = '\r' as u8;
            const N: u8 = '\n' as u8;
            let mut header_lines = 0;
            let mut headers_length = 3;
            let mut headers_complete = false;
            for window in self.read_buf.windows(4) {
                if window.len() < 4 { break; }
                headers_length += 1;
                if window[..2] == [R, N] {
                    header_lines += 1;
                }
                if window == [R, N, R, N] {
                    headers_complete = true;
                    break;
                }
            }
            self.header_lines = header_lines;
            self.headers_length = headers_length;
            self.headers_complete = headers_complete;

            // account for body contents that may have come in with this final headers read
            if self.headers_complete {
                self.body_bytes_read += self.read_buf.len() - self.headers_length;
            }
        }
        // if we don't have a complete headers sections, continue waiting
        if !self.headers_complete { return Ok(None) }

        // if we haven't parsed our request yet, parse the header content into a request and save it
        if self.request.is_none() {
            let mut headers = vec![httparse::EMPTY_HEADER; self.header_lines];
            let mut req = httparse::Request::new(&mut headers);
            let header_bytes = &self.read_buf[..self.headers_length];
            let status = match req.parse(header_bytes) {
                Ok(status) => status,
                Err(e) => {
                    bail_fmt!(ErrorKind::MalformedHttpRequest, "Malformed http request: {:?}\n{:?}",
                           e, std::str::from_utf8(header_bytes));
                }
            };
            if status.is_partial() {
                bail_fmt!(ErrorKind::IncompleteHttpRequest, "HTTP request parser found partial info");
            }
            debug_assert!(self.headers_length == status.unwrap());

            // HTTP parsing success. Build an `http::Request`
            let mut request = http::Request::builder();
            request.method(req.method.unwrap());
            request.uri(req.path.unwrap());
            // TODO: http::Request expects consts and not strs. Defaults to HTTP/1.1 for now
            // request.version(req.version.unwrap());
            for header in req.headers {
                request.header(header.name, header.value);
            }
            // use an empty body as a placeholder while we continue to read the request body
            self.request = Some(request.body(()).unwrap());
        }

        if !self.body_complete {
            let buf_len = self.read_buf.len();
            let bytes_accounted = self.headers_length + self.body_bytes_read;
            self.body_bytes_read = buf_len - bytes_accounted;
            if self.body_bytes_read > self.content_length {
                bail_fmt!(ErrorKind::RequestBodyTooLarge, "Body is larger than stated content-length");
            }
            self.body_complete = self.body_bytes_read == self.content_length;
        }
        if !self.body_complete { return Ok(None) }
        Ok(self.request.take())
    }
}


/// Represent the tcp socket & streams being polled by `mio`
enum Socket {
    Listener {
        listener: mio::net::TcpListener,
    },
    Stream {
        stream: mio::net::TcpStream,
        reader: HttpStreamReader,
        request: Option<RequestHead>,
        done_reading: bool,
        receiver: Option<Receiver<Response>>,
        response: Option<Response>,
        bytes_written: usize,
    },
}
impl Socket {
    fn new_listener(l: mio::net::TcpListener) -> Self {
        Socket::Listener { listener: l }
    }

    /// Construct a new `Stream` variant accepts from a tcp listener
    fn new_stream(s: mio::net::TcpStream, reader: HttpStreamReader) -> Self {
        Socket::Stream {
            stream: s,
            reader: reader,
            request: None,
            done_reading: false,
            receiver: None,
            response: None,
            bytes_written: 0,
        }
    }

    /// Construct a "continued" stream. Stream reading hasn't been completed yet
    fn continued_stream(stream: mio::net::TcpStream,
                        reader: HttpStreamReader,
                        request: Option<RequestHead>,
                        done_reading: bool,
                        receiver: Option<Receiver<Response>>,
                        response: Option<Response>,
                        bytes_written: usize) -> Self
    {
        Socket::Stream { stream, reader, request, done_reading, receiver, response, bytes_written }
    }
}


pub fn start<F>(addr: &str, func: F) -> Result<()>
    where F: Send + Sync + 'static + Fn(Request) -> HttpResponse<Vec<u8>>
{
    let func = sync::Arc::new(func);
    let pool = threadpool::ThreadPool::new(num_cpus::get() * 2);

    let mut sockets = slab::Slab::with_capacity(1024);
    let addr = addr.parse()?;
    let server = TcpListener::bind(&addr)?;

    // initialize poll
    let poll = mio::Poll::new()?;
    {
        // register our tcp listener
        let entry = sockets.vacant_entry();
        let server_token = entry.key().into();
        poll.register(&server, server_token,
                      mio::Ready::readable(),
                      mio::PollOpt::edge() | mio::PollOpt::oneshot())?;
        entry.insert(Socket::new_listener(server));
    }

    info!("** Listening on {} **", addr);

    let mut events = mio::Events::with_capacity(1024);
    loop {
        poll.poll(&mut events, None)?;
        'next_event: for e in &events {
            let token = e.token();
            match sockets.remove(token.into()) {
                Socket::Listener { listener } => {
                    let readiness = e.readiness();
                    if readiness.is_readable() {
                        let (sock, addr) = listener.accept()?;
                        debug!("opened socket to: {:?}", addr);

                        // register the newly opened socket
                        let entry = sockets.vacant_entry();
                        let token = entry.key().into();
                        poll.register(&sock, token,
                                      mio::Ready::readable() | mio::Ready::writable(),
                                      mio::PollOpt::edge() | mio::PollOpt::oneshot())?;
                        entry.insert(Socket::new_stream(sock, HttpStreamReader::new()));
                    }
                    // reregister listener
                    let entry = sockets.vacant_entry();
                    let token = entry.key().into();
                    poll.reregister(&listener, token,
                                    mio::Ready::readable(),
                                    mio::PollOpt::edge() | mio::PollOpt::oneshot())?;
                    entry.insert(Socket::new_listener(listener));
                }
                Socket::Stream { mut stream, mut reader, request, mut done_reading,
                                 mut receiver, mut response, mut bytes_written } => {
                    let readiness = e.readiness();

                    // Try reading and parsing a request from this stream.
                    // `try_build_request` will return `None` until the request is parsed and the
                    // body is done being read. After that, `done_reading` will be set
                    // to `true`. At that point, if this socket is readable, we still need to
                    // check if it's been closed, but we will no longer try parsing the request
                    // bytes
                    let mut request = if readiness.is_readable() {
                        let mut buf = [0; 256];
                        let stream_close = loop {
                            match stream.read(&mut buf) {
                                Ok(0) => {
                                    // the stream has ended for real
                                    break true
                                }
                                Ok(n) => {
                                    reader.receive_chunk(&buf[..n]);
                                    debug!("Read {} bytes", n);
                                }
                                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    break false
                                }
                                Err(e) => {
                                    error!("Encountered error while reading from socket: {:?}", e);
                                    // let this socket die, jump to the next event
                                    continue 'next_event
                                }
                            }
                        };
                        if stream_close {
                            debug!("Stream closed. Killing socket. Token: {:?}", token);
                            // jump to the next mio event
                            // TODO: if we have a `receiver` (a handler is running)
                            //       try shutting it down
                            continue 'next_event
                        }
                        if done_reading {
                            request
                        } else {
                            match reader.try_build_request() {
                                Ok(r) => r,
                                Err(e) => {
                                    error!("Encountered error while parsing: {:?}", e);
                                    continue 'next_event
                                }
                            }
                        }
                    } else {
                        request
                    };
                    if request.is_some() { done_reading = true; }

                    // Once the request is parsed, this block will execute once.
                    // The head-only request (RequestHead) will be converted into
                    // a `Request` and the `HttpStreamReader`s `read_buf` will be
                    // swapped into the new `Request`s body. The provided
                    // `func` handler will be started for later retrieval
                    receiver = if let Some(req) = request.take() {
                        let (parts, _) = req.into_parts();
                        let mut body = vec![];
                        std::mem::swap(&mut body, &mut reader.read_buf);
                        let request = Request {
                            inner: http::Request::from_parts(parts, body),
                            body_start: reader.headers_length,
                        };
                        let (send, recv) = channel();
                        let func = func.clone();
                        pool.execute(move || {
                            let resp = func(request);
                            let mut resp = Response::new(resp);
                            resp.serialize_headers();
                            // is sending fails there's nothing we can really do.
                            // the socket was probably closed
                            send.send(resp).ok();
                        });
                        Some(recv)
                    } else {
                        receiver
                    };

                    // See if a `Response` is available
                    response = if let Some(ref recv) = receiver {
                        recv.try_recv().ok()
                    } else {
                        None
                    };

                    // If we have a `Response`, start writing its headers and body
                    // back to the stream
                    let mut done_write = false;
                    if let Some(ref resp) = response {
                        if readiness.is_writable() {
                            let header_data_len = resp.header_data.len();
                            loop {
                                let (data, read_start) = if bytes_written < header_data_len {
                                    (&resp.header_data, bytes_written)
                                } else {
                                    (resp.body(), bytes_written - header_data_len)
                                };
                                match stream.write(&data[read_start..]) {
                                    Ok(0) => {
                                        break
                                    }
                                    Ok(n) => {
                                        bytes_written += n;
                                        debug!("Wrote {} bytes", n);
                                    }
                                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                        break
                                    }
                                    Err(e) => {
                                        error!("Encountered error while writing to socket: {:?}", e);
                                        // let this socket die, jump to the next event
                                        continue 'next_event
                                    }
                                }
                            }
                            done_write = resp.header_data.len() + resp.body().len() == bytes_written;
                        }
                    }

                    if !done_write {
                        // we're not done with this socket yet
                        // reregister stream
                        let entry = sockets.vacant_entry();
                        let token = entry.key().into();
                        poll.reregister(&stream, token,
                                        mio::Ready::readable() | mio::Ready::writable(),
                                        mio::PollOpt::edge() | mio::PollOpt::oneshot())?;
                        entry.insert(
                            Socket::continued_stream(
                                stream, reader, request, done_reading,
                                receiver, response, bytes_written,
                                )
                            );
                    }
                }
            }
        }
    }
}

