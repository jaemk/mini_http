extern crate mini_http;
extern crate env_logger;


pub fn main() {
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
        .init()
        .expect("failed to initialize logger");
    ::std::env::set_var("LOG", "info");
    if let Err(e) = mini_http::start() {
        eprintln!("Error: {}", e);
    }
}

