# mini_http
[![Build Status](https://travis-ci.org/jaemk/mini_http.svg?branch=master)](https://travis-ci.org/jaemk/mini_http)

**Note:** This project is a work in progress and shouldn't be used in any critical production environment.

> A basic asynchronous&#42; http server using [`mio`](https://docs.rs/mio)

&#42;While network IO is performed asynchronously, handler functions are executed synchronously in a thread pool.

## Usage

See [`examples`](https://github.com/jaemk/mini_http/tree/master/examples)

```rust
extern crate mini_http;

fn run() -> Result<(), Box<std::error::Error>> {
    mini_http::Server::new("127.0.0.1:3000")?
        .start(|request| {
            println!("{:?}", std::str::from_utf8(request.body()));
            let resp = if request.body().len() > 0 {
                request.body().to_vec()
            } else {
                b"hello!".to_vec()
            };
            mini_http::Response::builder()
                .status(200)
                .header("X-What-Up", "Nothin")
                .body(resp)
                .unwrap()
        })?;
    Ok(())
}
```

