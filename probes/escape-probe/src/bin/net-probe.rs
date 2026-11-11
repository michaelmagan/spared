use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

fn main() {
    let target = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:80".into());
    let addr = target.to_socket_addrs().ok().and_then(|mut i| i.next());
    match addr {
        Some(addr) => match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
            Ok(_) => println!("CONNECTED {addr}"),
            Err(e) => println!("ERR {:?}", e.kind()),
        },
        None => println!("ERR resolve"),
    }
}
