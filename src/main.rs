extern crate mini_http;
extern crate env_logger;
#[macro_use] extern crate log;


fn run() -> Result<(), Box<std::error::Error>> {
    env_logger::LogBuilder::new()
        .format(|record| {
            format!("[{}] - [{}] -> {}",
                // Local::now().format("%Y-%m-%d_%H:%M:%S"),
                record.level(),
                record.location().module_path(),
                record.args()
                )
            })
        .parse(&::std::env::var("LOG").unwrap_or_default())
        .init()?;
    ::std::env::set_var("LOG", "info");

    mini_http::start("127.0.0.1:3000", |request| {
        info!("request body: {:?}", std::str::from_utf8(request.body()));
        let resp = if request.body().len() > 0 { request.body().to_vec() } else { "hello!".as_bytes().to_vec() };
        mini_http::HttpResponse::builder()
            .status(200)
            .header("X-What-Up", "Nothin")
            .body(resp)
            .unwrap()
    })?;
    Ok(())
}


pub fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {}", e);
    }
}

