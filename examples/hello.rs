extern crate mini_http;


fn run() -> Result<(), Box<std::error::Error>> {
    mini_http::start("127.0.0.1:3000", |_req| {
        mini_http::Response::builder()
            .status(200)
            .body(b"Hello!\n".to_vec())
            .unwrap()
    })?;
    Ok(())
}


pub fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {}", e);
    }
}

