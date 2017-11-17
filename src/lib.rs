#![recursion_limit="1024"]
#[macro_use] extern crate error_chain;
extern crate mio;
extern crate slab;
extern crate httparse;
extern crate http;
#[macro_use] extern crate log;

use std::io::{self, Read, Write};
use mio::net::{TcpListener};

error_chain! {
    foreign_links {
        Io(::std::io::Error);
        Utf8(::std::str::Utf8Error);
        AddrParse(::std::net::AddrParseError);
        Http(http::Error);
    }
    errors {}
}


// pub trait StreamRead {
//     fn receive_chunk(&mut self, &[u8]) -> usize;
//     fn parse_update(&mut self) -> Result<()>;
//     fn is_complete(&self) -> bool;
//     fn into_request(&mut self) -> Option<http::Request<Vec<u8>>>;
// }

pub type Request = http::Request<Vec<u8>>;


struct HttpStreamReader {
    read_buf: Vec<u8>,
    header_lines: usize,
    headers_length: usize,
    headers_complete: bool,
    request: Option<Request>,

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
    fn empty() -> Self {
        Self { ..Self::default() }
    }
    fn new() -> Self {
        Self {
            read_buf: Vec::with_capacity(1024),
            ..Self::default()
        }
    }

    fn receive_chunk(&mut self, chunk: &[u8]) -> usize {
        self.read_buf.extend_from_slice(chunk);
        self.read_buf.len()
    }
    fn parse_update(&mut self) -> Result<()> {
        Ok(())
    }
    fn try_build_request(&mut self) -> Option<Request> {
        if !self.headers_complete {
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
        debug!("headers: {}, complete: {}", self.header_lines, self.headers_complete);
        // if we don't have a complete headers sections, continue waiting
        if !self.headers_complete { return None }

        // if we haven't parsed our request yet, parse the header content and save it
        if self.request.is_none() {
            let mut headers = vec![httparse::EMPTY_HEADER; self.header_lines];
            let mut req = httparse::Request::new(&mut headers);
            debug!("parsing");
            let status = match req.parse(&self.read_buf[..self.headers_length]) {
                Ok(status) => status,
                Err(e) => {
                    // TODO
                    panic!("{:?}", e);
                }
            };
            debug!("{:?}", status);
            if status.is_partial() {
                debug!("Partial request");
                return None
            }
            assert!(self.headers_length == status.unwrap());

            let mut request = http::Request::builder();
            request.method(req.method.unwrap());
            request.uri(req.path.unwrap());
            // request.version(req.version.unwrap());
            for header in req.headers {
                request.header(header.name, header.value);
            }
            self.request = Some(request.body(vec![]).unwrap());
        }

        if !self.body_complete {
            let buf_len = self.read_buf.len();
            let bytes_accounted = self.headers_length + self.body_bytes_read;
            self.body_bytes_read = buf_len - bytes_accounted;
            self.body_complete = self.body_bytes_read == self.content_length;
        }
        if !self.body_complete { return None }
        self.request.take()
    }
    fn body(&self) -> &[u8] {
        &self.read_buf[self.headers_length..]
    }
}


enum Socket {
    Listener {
        listener: mio::net::TcpListener,
    },
    Stream {
        stream: mio::net::TcpStream,
        reader: HttpStreamReader,
        request: Option<Request>,
        write_buf: Vec<u8>,
        bytes_written: usize,
    },
}
impl Socket {
    fn new_listener(l: mio::net::TcpListener) -> Self {
        Socket::Listener { listener: l }
    }
    fn new_stream(s: mio::net::TcpStream, reader: HttpStreamReader) -> Self {
        Socket::Stream {
            stream: s,
            reader: reader,
            request: None,
            write_buf: Vec::with_capacity(1024),
            bytes_written: 0,
        }
    }
    fn continued_stream(stream: mio::net::TcpStream,
                        reader: HttpStreamReader,
                        request: Option<Request>,
                        write_buf: Vec<u8>, bytes_written: usize) -> Self
    {
        Socket::Stream { stream, reader, request, write_buf, bytes_written }
    }
}


pub fn start() -> Result<()> {
    let mut sockets = slab::Slab::with_capacity(1024);
    let addr = "127.0.0.1:3000".parse()?;
    let server = TcpListener::bind(&addr)?;

    let poll = mio::Poll::new()?;
    {
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
        for e in &events {
            let token = e.token();
            match sockets.remove(token.into()) {
                Socket::Listener { listener } => {
                    let readiness = e.readiness();
                    debug!("listener, {:?}, {:?}", token, readiness);
                    if readiness.is_readable() {
                        debug!("handling listener is readable");
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
                Socket::Stream { mut stream, mut reader, mut request, mut write_buf, mut bytes_written } => {
                    let readiness = e.readiness();
                    let request = if request.is_none() && readiness.is_readable() {
                        let mut buf = [0; 256];
                        let stream_close = loop {
                            match stream.read(&mut buf) {
                                Ok(0) => {
                                    // the stream has ended for real
                                    break true
                                }
                                Ok(n) => {
                                    // read_buf.extend_from_slice(&buf[..n]);
                                    reader.receive_chunk(&buf[..n]);
                                }
                                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    break false
                                }
                                Err(e) => {
                                    panic!("{:?}", e);
                                    // break false
                                }
                            }
                        };
                        if stream_close {
                            // TODO: check for keep-alive
                            debug!("killing socket: {:?}", token);
                            continue
                        }
                        reader.parse_update()?;
                        reader.try_build_request()
                    } else {
                        request
                    };
                    let mut done_write = false;
                    if let Some(ref request) = request {
                        if readiness.is_writable() {
                            debug!("handling stream is done reading and is writable");
                            if write_buf.is_empty() {
                                // debug!("echo: {:?}", std::str::from_utf8(&read_buf)?);
                                write_buf = b"HTTP/1.1 200 OK\r\nServer: HttpMio\r\n\r\n".to_vec();
                                // write_buf.extend_from_slice(&read_buf);
                                write_buf.extend_from_slice(reader.body());
                            }
                            loop {
                                match stream.write(&write_buf[bytes_written..]) {
                                    Ok(0) => {
                                        break
                                    }
                                    Ok(n) => {
                                        bytes_written += n;
                                    }
                                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                        break
                                    }
                                    Err(e) => {
                                        panic!("{:?}", e);
                                        // break
                                    }
                                }
                            }
                            done_write = write_buf.len() == bytes_written;
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
                                stream, reader,
                                request, write_buf, bytes_written
                                )
                            );
                    }
                }
            }
        }
    }
    Ok(())
}

