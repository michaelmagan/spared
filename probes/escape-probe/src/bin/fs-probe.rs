use std::hint::black_box;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("read") => match std::fs::read(&a[2]) {
            Ok(b) => println!("READ {}", b.len()),
            Err(e) => println!("ERR {:?}", e.kind()),
        },
        Some("write") => match std::fs::write(&a[2], b"x") {
            Ok(()) => println!("WROTE"),
            Err(e) => println!("ERR {:?}", e.kind()),
        },
        Some("alloc") => {
            let mb: usize = a[2].parse().unwrap();
            let mut v: Vec<u8> = Vec::new();
            match v.try_reserve_exact(mb << 20) {
                Ok(()) => {
                    v.resize(mb << 20, 1);
                    println!("ALLOC_OK {}", black_box(v.iter().map(|b| *b as u64).sum::<u64>()));
                }
                Err(_) => println!("ALLOC_FAIL"),
            }
        }
        Some("spin") => {
            let mut x = 0u64;
            loop {
                x = black_box(x.wrapping_add(1));
            }
        }
        _ => println!("usage"),
    }
}
