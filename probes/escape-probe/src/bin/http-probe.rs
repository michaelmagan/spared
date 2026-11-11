use wstd::http::{Body, Client, Request};

#[wstd::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    for url in std::env::args().skip(1) {
        let req = match Request::get(url.as_str()).body(Body::empty()) {
            Ok(r) => r,
            Err(e) => {
                println!("{url} BADREQ {e}");
                continue;
            }
        };
        match Client::new().send(req).await {
            Ok(resp) => println!("{url} STATUS {}", resp.status().as_u16()),
            Err(e) => println!("{url} ERR {e}"),
        }
    }
    Ok(())
}
