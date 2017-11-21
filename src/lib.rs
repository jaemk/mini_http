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
mod http_stream;

use std::io::{self, Read, Write};
use std::sync;
use std::sync::mpsc::{channel, Receiver};
use std::time;
use std::ascii::AsciiExt;
use mio::net::{TcpListener};
pub use http::header;
pub use http::method;
pub use http::response;
pub use http::status;
pub use http::uri;
pub use http::version;

use http_stream::HttpStreamReader;
pub use errors::*;


/// Re-exported `http::Response` for constructing return responses in handlers
pub use http::Response;


/// Internal `http::Response` wrapper with helpers for constructing the bytes
/// that needs to be written back a Stream
struct ResponseWrapper {
    inner: http::Response<Vec<u8>>,
    header_data: Vec<u8>
}
impl ResponseWrapper {
    fn new(inner: http::Response<Vec<u8>>) -> Self {
        Self { inner, header_data: Vec::with_capacity(1024) }
    }

    fn serialize_headers(&mut self) {
        {
            let body_len = self.inner.body().len();
            let hdrs = self.inner.headers_mut();
            hdrs.insert(header::SERVER, header::HeaderValue::from_static("mini-http (rust)"));
            if body_len > 0 {
                let len = header::HeaderValue::from_str(&body_len.to_string()).unwrap();
                hdrs.insert(header::CONTENT_LENGTH, len);
            }
        }
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
impl std::ops::Deref for ResponseWrapper {
    type Target = http::Response<Vec<u8>>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl std::ops::DerefMut for ResponseWrapper {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}


/// Represent everything about a request except its (possible) body
type RequestHead = http::Request<()>;


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


#[derive(Debug, Clone, PartialEq, Eq)]
enum SocketStatus {
    New,
    Reused,
}

/// Represent the tcp socket & streams being polled by `mio`
enum Socket {
    Listener {
        listener: mio::net::TcpListener,
    },
    Stream {
        stream: mio::net::TcpStream,
        keep_alive: bool,
        socket_status: SocketStatus,
        reader: HttpStreamReader,
        request: Option<RequestHead>,
        done_reading: bool,
        receiver: Option<Receiver<ResponseWrapper>>,
        response: Option<ResponseWrapper>,
        bytes_written: usize,
    },
}
impl Socket {
    fn new_listener(l: mio::net::TcpListener) -> Self {
        Socket::Listener { listener: l }
    }

    /// Construct a new `Stream` variant accepts from a tcp listener
    fn new_stream(s: mio::net::TcpStream, reader: HttpStreamReader, socket_status: SocketStatus) -> Self {
        Socket::Stream {
            stream: s,
            keep_alive: true,
            socket_status: socket_status,
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
                        keep_alive: bool,
                        socket_status: SocketStatus,
                        reader: HttpStreamReader,
                        request: Option<RequestHead>,
                        done_reading: bool,
                        receiver: Option<Receiver<ResponseWrapper>>,
                        response: Option<ResponseWrapper>,
                        bytes_written: usize) -> Self
    {
        Socket::Stream { stream, keep_alive, socket_status, reader, request, done_reading, receiver, response, bytes_written }
    }
}


pub fn start<F>(addr: &str, func: F) -> Result<()>
    where F: Send + Sync + 'static + Fn(Request) -> Response<Vec<u8>>
{
    let func = sync::Arc::new(func);
    let pool = threadpool::ThreadPool::new(num_cpus::get() * 8);

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
                        entry.insert(Socket::new_stream(sock, HttpStreamReader::new(), SocketStatus::New));
                    }
                    // reregister listener
                    let entry = sockets.vacant_entry();
                    let token = entry.key().into();
                    poll.reregister(&listener, token,
                                    mio::Ready::readable(),
                                    mio::PollOpt::edge() | mio::PollOpt::oneshot())?;
                    entry.insert(Socket::new_listener(listener));
                }
                Socket::Stream { mut stream, mut keep_alive, socket_status, mut reader, request, mut done_reading,
                                 mut receiver, mut response, mut bytes_written } => {
                    let readiness = e.readiness();

                    // Try reading and parsing a request from this stream.
                    // `try_build_request` will return `None` until the request is parsed and the
                    // body is done being read. After that, `done_reading` will be set
                    // to `true`. At that point, if this socket is readable, we still need to
                    // check if it's been closed, but we will no longer try parsing the request
                    // bytes
                    let (mut request, err_response): (Option<RequestHead>, Option<ResponseWrapper>) = if readiness.is_readable() {
                        let mut buf = [0; 256];
                        let stream_close = loop {
                            match stream.read(&mut buf) {
                                Ok(0) => {
                                    // the stream has ended for real
                                    break true
                                }
                                Ok(n) => {
                                    reader.receive_chunk(&buf[..n]);
                                    debug!("{:?} - Read {} bytes", token, n);
                                }
                                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    break false
                                }
                                Err(ref e) if e.kind() == io::ErrorKind::ConnectionReset => {
                                    break false
                                }
                                Err(e) => {
                                    error!("{:?} - Encountered error while reading from socket: {:?}", token, e);
                                    // let this socket die, jump to the next event
                                    continue 'next_event
                                }
                            }
                        };
                        if stream_close {
                            debug!("{:?} - Stream closed. Killing socket.", token);
                            // jump to the next mio event
                            // TODO: if we have a `receiver` (a handler is running)
                            //       try shutting it down
                            continue 'next_event
                        }
                        if done_reading {
                            (request, None)
                        } else {
                            match reader.try_build_request() {
                                Ok(r) => (r, None),
                                Err(e) => {
                                    // TODO: return the proper status-code per error
                                    error!("{:?} - Encountered error while parsing: {}", token, e);
                                    (None,
                                     Some(ResponseWrapper::new(
                                             Response::builder().status(400).body(b"bad request".to_vec()).unwrap())))
                                }
                            }
                        }
                    } else {
                        (request, None)
                    };
                    if request.is_some() || err_response.is_some() { done_reading = true; }

                    // Once the request is parsed, this block will execute once.
                    // The head-only request (RequestHead) will be converted into
                    // a public `Request` and the `HttpStreamReader`s `read_buf` will be
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

                        // Check for an explicit connection header, default to keep-alive = true
                        // TODO: This parsing needs to be improved to support all possible
                        //       values
                        keep_alive = {
                            request.headers().get(header::CONNECTION)
                                .map(|v| v.as_bytes().eq_ignore_ascii_case(b"keep-alive"))
                                .unwrap_or(keep_alive)
                        };
                        if socket_status == SocketStatus::New {
                            // Disable Nagle algorithm.
                            // The default setting (no_delay=false, Nagle enabled) causes a
                            // significant drop in performance for keep-alive connections
                            // TODO: Default: false, expose a builder option to enable this
                            stream.set_nodelay(true).unwrap();
                            if keep_alive {
                                debug!("{:?} setting keep-alive", token);
                                stream.set_keepalive(Some(time::Duration::from_secs(2))).unwrap();
                            }
                        }

                        // Kick-off the handler
                        let (send, recv) = channel();
                        let func = func.clone();
                        pool.execute(move || {
                            let resp = func(request);
                            let mut resp = ResponseWrapper::new(resp);
                            resp.serialize_headers();
                            // If sending fails there's nothing we can really do.
                            // The socket was probably closed and its receiver dropped
                            send.send(resp).ok();
                        });
                        Some(recv)
                    } else {
                        receiver
                    };

                    // See if a `ResponseWrapper` is available
                    response = if let Some(ref recv) = receiver {
                        recv.try_recv().ok()
                    } else {
                        if let Some(err_response) = err_response {
                            Some(err_response)
                        } else {
                            None
                        }
                    };

                    // If we have a `ResponseWrapper`, start writing its headers and body
                    // back to the stream
                    let mut done_write = false;
                    if let Some(ref resp) = response {
                        if readiness.is_writable() {
                            let header_data_len = resp.header_data.len();
                            let body_len = resp.body().len();
                            let total_len = header_data_len + body_len;
                            'write: loop {
                                let (data, read_start) = if bytes_written < header_data_len {
                                    (&resp.header_data, bytes_written)
                                } else if bytes_written < total_len {
                                    (resp.body(), bytes_written - header_data_len)
                                } else {
                                    done_write = true;
                                    debug!("{:?} - flushing", token);
                                    // If flushing fails, something bad probably happened.
                                    // If it didn't fail because of a connection error (connection
                                    // is still alive), it will eventually be flushed by the os
                                    stream.flush().ok();
                                    break 'write
                                };
                                match stream.write(&data[read_start..]) {
                                    Ok(n) => {
                                        bytes_written += n;
                                        debug!("{:?} - Wrote {} bytes", token, n);
                                    }
                                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                        break 'write
                                    }
                                    Err(e) => {
                                        error!("{:?} - Encountered error while writing to socket: {:?}", token, e);
                                        // let this socket die, jump to the next event
                                        continue 'next_event
                                    }
                                }
                            }
                        }
                    }

                    if !done_write {
                        // we're not done writing our response to this socket yet
                        // reregister stream
                        let entry = sockets.vacant_entry();
                        let token = entry.key().into();
                        poll.reregister(&stream, token,
                                        mio::Ready::readable() | mio::Ready::writable(),
                                        mio::PollOpt::edge() | mio::PollOpt::oneshot())?;
                        entry.insert(
                            Socket::continued_stream(
                                stream, keep_alive, socket_status, reader, request, done_reading,
                                receiver, response, bytes_written,
                                )
                            );
                    } else if keep_alive {
                        // we're done writing, but we need to keep the socket open and reuse it
                        debug!("{:?} - Reusing stream", token);
                        let entry = sockets.vacant_entry();
                        let token = entry.key().into();
                        poll.reregister(&stream, token,
                                        mio::Ready::readable() | mio::Ready::writable(),
                                        mio::PollOpt::edge() | mio::PollOpt::oneshot())?;
                        entry.insert(Socket::new_stream(stream, HttpStreamReader::new(), SocketStatus::Reused));
                    } else {
                        debug!("{:?} - Done writing, killing socket", token);
                    }
                }
            }
        }
    }
}

