use std;
use http;
use httparse;

use {RequestHead};
use errors::*;


/// Http reader/parser for incrementally reading a request and
/// parsing its headers
pub(crate) struct HttpStreamReader {
    pub read_buf: Vec<u8>,
    pub headers_length: usize,
    header_lines: usize,
    headers_complete: bool,
    header_cursor: usize,
    request: Option<RequestHead>,

    content_length: usize,
    body_bytes_read: usize,
    body_complete: bool,
}
impl std::default::Default for HttpStreamReader {
    fn default() -> HttpStreamReader {
        HttpStreamReader {
            read_buf: Vec::new(),
            header_lines: 0, headers_length: 0, headers_complete: false, header_cursor: 0, request: None,
            content_length: 0, body_bytes_read: 0, body_complete: false,
        }
    }
}
impl HttpStreamReader {
    pub fn new() -> Self {
        Self {
            read_buf: Vec::with_capacity(1024),
            ..Self::default()
        }
    }

    /// Save a new chunk of bytes
    pub fn receive_chunk(&mut self, chunk: &[u8]) -> usize {
        self.read_buf.extend_from_slice(chunk);
        self.read_buf.len()
    }

    /// Try parsing the current bytes into request headers.
    ///
    /// After headers are parsed, collect the remaining body bytes.
    /// After Content-length bytes are parsed, return the parsed request headers.
    pub fn try_build_request(&mut self) -> Result<Option<RequestHead>> {
        if !self.headers_complete {
            // check if we've got enough data to successfully parse the request
            const R: u8 = b'\r';
            const N: u8 = b'\n';
            // slide back 3 spaces in case the previous chunk ended with "\r\n\r"
            let cursor = if self.header_cursor >= 3 { self.header_cursor - 3 } else { 0 };
            let mut headers_length = if self.headers_length < 4 { 3 } else { self.headers_length - 3 };
            let data = &self.read_buf[cursor..];
            for window in data.windows(4) {
                if window.len() < 4 { break }
                headers_length += 1;
                if window == [R, N, R, N] {
                    self.headers_complete = true;
                    break;
                }
                if window[..2] == [R, N] {
                    self.header_lines += 1;
                }
            }
            self.headers_length = headers_length;
            self.header_cursor = headers_length - 1;

            if self.headers_complete {
                debug!("headers complete: {}, {:?}",
                       self.headers_length, std::str::from_utf8(&self.read_buf[..self.headers_length]));
                // account for body contents that may have come in with this final headers read
                self.body_bytes_read += self.read_buf.len() - self.headers_length;
                debug!("trailing body bytes read: {}, {:?}",
                       self.body_bytes_read, std::str::from_utf8(&self.read_buf[self.headers_length..]));
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
            if self.body_bytes_read > self.content_length {
                bail_fmt!(ErrorKind::RequestBodyTooLarge, "Body is larger than stated content-length: {}", self.content_length);
            }
            self.body_complete = self.body_bytes_read == self.content_length;
        }
        if !self.body_complete { return Ok(None) }
        Ok(self.request.take())
    }
}


