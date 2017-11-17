# mini_http

> A basic asynchronous&#42; http server using [`mio`](https://docs.rs/mio)


## Usage

&#42;While network IO is performed asynchronously, handler functions are executed synchronously in a thread pool.

```rust
extern crate mini_http;

fn run() -> Result<(), Box<std::error::Error>> {
    mini_http::start("127.0.0.1:3000", |request| {
        println!("{:?}", std::str::from_utf8(request.body()));
        let resp = if request.body().len() > 0 {
            // echo
            request.body().to_vec()
        } else {
            "hello!".as_bytes().to_vec()
        };
        mini_http::HttpResponse::builder()
            .status(200)
            .header("X-What-Up", "Nothin")
            .body(resp)
            .unwrap()
    })?;
    Ok(())
}
```

