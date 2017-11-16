extern crate mio;
extern crate slab;
extern crate httparse;
extern crate http;


use std::io::{self, Read, Write};
use mio::net::{TcpListener};


enum Socket {
    Listener {
        listener: mio::net::TcpListener,
    },
    Stream {
        stream: mio::net::TcpStream,
        read_buf: Vec<u8>,
        done_read: bool,
        write_buf: Vec<u8>,
        bytes_written: usize,
    },
}
impl Socket {
    fn new_listener(l: mio::net::TcpListener) -> Self {
        Socket::Listener { listener: l }
    }
    fn new_stream(s: mio::net::TcpStream) -> Self {
        Socket::Stream {
            stream: s,
            read_buf: Vec::with_capacity(1024),
            done_read: false,
            write_buf: Vec::with_capacity(1024),
            bytes_written: 0,
        }
    }
    fn continued_stream(stream: mio::net::TcpStream,
                        read_buf: Vec<u8>, done_read: bool,
                        write_buf: Vec<u8>, bytes_written: usize) -> Self
    {
        Socket::Stream { stream, read_buf, done_read, write_buf, bytes_written }
    }
}


pub fn start() -> Result<(), Box<::std::error::Error>> {
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

    println!("** Listening on {} **", addr);

    let mut events = mio::Events::with_capacity(1024);
    loop {
        poll.poll(&mut events, None)?;
        for e in &events {
            let token = e.token();
            match sockets.remove(token.into()) {
                Socket::Listener { listener } => {
                    let readiness = e.readiness();
                    println!("listener, {:?}, {:?}", token, readiness);
                    if readiness.is_readable() {
                        println!("handling listener is readable");
                        let (sock, addr) = listener.accept()?;
                        println!("opened socket to: {:?}", addr);

                        // register the newly opened socket
                        let entry = sockets.vacant_entry();
                        let token = entry.key().into();
                        poll.register(&sock, token,
                                      mio::Ready::readable(),
                                      mio::PollOpt::edge() | mio::PollOpt::oneshot())?;
                        entry.insert(Socket::new_stream(sock));
                    }
                    // reregister listener
                    let entry = sockets.vacant_entry();
                    let token = entry.key().into();
                    poll.reregister(&listener, token,
                                    mio::Ready::readable() | mio::Ready::writable(),
                                    mio::PollOpt::edge() | mio::PollOpt::oneshot())?;
                    entry.insert(Socket::new_listener(listener));
                }
                Socket::Stream { mut stream, mut read_buf, mut done_read, mut write_buf, mut bytes_written } => {
                    let readiness = e.readiness();
                    // println!("stream, {:?}, {:?}, done_read: {:?}", token, readiness, done_read);
                    if readiness.is_readable() {
                        let mut buf = [0; 256];
                        let stream_close = loop {
                            match stream.read(&mut buf) {
                                Ok(0) => {
                                    // the stream has ended for real
                                    break true
                                }
                                Ok(n) => {
                                    read_buf.extend_from_slice(&buf[..n]);
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
                            println!("killing socket: {:?}", token);
                            continue
                        }
                        // TODO: Parse headers/body for real
                        //       this only works for requests without a body
                        let buf_len = read_buf.len();
                        if buf_len > 3 && &read_buf[buf_len-4..] == b"\r\n\r\n" {
                            println!("{:?}", std::str::from_utf8(&read_buf)?);
                            done_read = true;
                        }
                    }
                    let mut done_write = false;
                    if readiness.is_writable() && done_read {
                        println!("handling stream is done reading and is writable");
                        if write_buf.is_empty() {
                            println!("echo: {:?}", std::str::from_utf8(&read_buf)?);
                            write_buf = b"HTTP/1.1 200 OK\r\nServer: HttpMio\r\n\r\n".to_vec();
                            write_buf.extend_from_slice(&read_buf);
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
                    if !done_write {
                        // we're not done with this socket yet
                        // reregister stream
                        let entry = sockets.vacant_entry();
                        let token = entry.key().into();
                        poll.reregister(&stream, token,
                                        mio::Ready::readable() | mio::Ready::writable(),
                                        mio::PollOpt::edge() | mio::PollOpt::oneshot())?;
                        entry.insert(Socket::continued_stream(stream, read_buf, done_read, write_buf, bytes_written));
                    }
                }
            }
        }
    }
    Ok(())
}

