extern crate mini_http;


pub fn main() {
    if let Err(e) = mini_http::start() {
        eprintln!("Error: {}", e);
    }
}

