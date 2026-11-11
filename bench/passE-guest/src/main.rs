//! Pass E guest: the file-write and request-body paths the per-app caps and the body budget sit on.
//! /file-write: one file, 256 separate 1 KiB writes (one fstat each under fsguard), then closed and left for the next request to truncate.
//! /file-small: 32 files created, one 128-byte write each, then all removed (entry and size recount).
//! POST /body: read the body, answer with its length.
use std::fs::File;
use std::io::Write;
use wstd::http::{Body, Error, Request, Response, StatusCode};

fn text(s: String) -> Result<Response<Body>, Error> {
    Ok(Response::builder().status(StatusCode::OK).header("content-type", "text/plain").body(Body::from(s))?)
}

#[wstd::http_server]
async fn main(req: Request<Body>) -> Result<Response<Body>, Error> {
    match req.uri().path() {
        "/file-write" => {
            let chunk = [0x5au8; 1024];
            let mut f = File::create("/data/fw.bin")?;
            for _ in 0..256 {
                f.write_all(&chunk)?;
            }
            text("256\n".into())
        }
        "/file-small" => {
            let line = [0x61u8; 128];
            for i in 0..32 {
                let mut f = File::create(format!("/data/s{i:02}"))?;
                f.write_all(&line)?;
            }
            for i in 0..32 {
                std::fs::remove_file(format!("/data/s{i:02}"))?;
            }
            text("32\n".into())
        }
        "/body" => {
            let mut body = req.into_body();
            let n = body.contents().await?.len();
            text(format!("{n}\n"))
        }
        _ => text("ok\n".into()),
    }
}
