mod rc_zip_monoio;

use std::io::{Cursor, Write};
use std::path::PathBuf;

use monoio::buf::IoBufMut;
use monoio::fs::File;
use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::{TcpListener, TcpStream};
use rc_zip::fsm::EntryFsm;
use rc_zip::parse::{Archive, Entry};

#[monoio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let Some(file_arg) = std::env::args().nth(1) else {
        println!("Usage: zipring ZIPFILE");
        return;
    };
    let filepath = PathBuf::from(file_arg);
    let file = monoio::fs::File::open(filepath).await.unwrap();
    let file: &'static File = Box::leak(Box::new(file));
    let zip = rc_zip_monoio::read_zip_from_file(&file).await.unwrap();
    let zip: &'static Archive = Box::leak(Box::new(zip));
    for entry in zip.entries() {
        println!("{:?}", entry.sanitized_name());
    }

    let listener = TcpListener::bind("127.0.0.1:50002").unwrap();
    println!("listening");
    loop {
        let incoming = listener.accept().await;
        match incoming {
            Ok((stream, addr)) => {
                println!("accepted a connection from {}", addr);
                monoio::spawn(echo(stream, &file, &zip));
            }
            Err(e) => {
                println!("accepted connection failed: {}", e);
                return;
            }
        }
    }
}

async fn find_entry(zip: &Archive, path: &str) -> Option<Entry> {
    for entry in zip.entries() {
        if entry.name.eq_ignore_ascii_case(path) {
            return Some(entry.clone());
        }
    }
    None
}

async fn echo(mut stream: TcpStream, file: &File, zip: &Archive) -> std::io::Result<()> {
    let mut buf = vec![0u8; 8 * 1024].into_boxed_slice();
    let mut intbuf = itoa::Buffer::new();
    let mut res;
    loop {
        (res, buf) = stream.read(buf).await;
        if res? == 0 {
            return Ok(());
        }
        if !buf.starts_with(b"GET ") {
            return Ok(());
        }
        let path = &buf[b"GET /".len()..];
        let Some(br) = path.iter().position(|&c| c == b' ') else {
            return Ok(());
        };
        let path = &path[..br];
        let Ok(path) = percent_encoding::percent_decode(path).decode_utf8() else {
            return Ok(());
        };
        tracing::debug!(?path);

        let Some(entry) = find_entry(zip, &path).await else {
            tracing::warn!("entry not found");
            let (res, _) = stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await;
            res?;
            continue;
        };
        let mime_type = mime_guess::from_path(&entry.name).first_or_text_plain();
        tracing::debug!(?mime_type);

        let mut cur = Cursor::new(buf);
        cur.write(b"HTTP/1.1 200 OK\r\nContent-Type: ")?;
        cur.write(mime_type.essence_str().as_bytes())?;
        cur.write(
            b"\r\nConnection: keep-alive\r\nKeep-Alive: timeout=20, max=200\r\nContent-Length: ",
        )?;
        cur.write(intbuf.format(entry.uncompressed_size).as_bytes())?;
        cur.write(b"\r\n\r\n")?;
        let n = cur.position() as usize;
        buf = cur.into_inner();
        let mut slice = IoBufMut::slice_mut(buf, 0..n);
        (res, slice) = stream.write_all(slice).await;
        buf = slice.into_inner();
        res?;
        let res;
        (res, buf) = send_decompressed_entry(&mut stream, file, buf, entry).await;
        res?;
    }
}

async fn send_decompressed_entry(
    stream: &mut TcpStream,
    file: &File,
    mut buf: Box<[u8]>,
    entry: Entry,
) -> (Result<(), std::io::Error>, Box<[u8]>) {
    let mut res;
    let mut offset = entry.header_offset;
    let mut fsm = EntryFsm::new(Some(entry), None);
    loop {
        if fsm.wants_read() {
            let dst = fsm.space();
            let max_read = dst.len().min(buf.len());
            let mut slice = IoBufMut::slice_mut(buf, 0..max_read);
            (res, slice) = file.read_at(slice, offset).await;
            let Ok(n) = res else {
                return (res.map(|_| ()), slice.into_inner());
            };
            (dst[..n]).copy_from_slice(&slice[..n]);
            fsm.fill(n);
            offset += n as u64;
            buf = slice.into_inner();
        }
        fsm = match fsm.process(&mut buf) {
            Ok(rc_zip::fsm::FsmResult::Continue((fsm, outcome))) => {
                let bytes_out = outcome.bytes_written;
                let mut slice = IoBufMut::slice_mut(buf, 0..bytes_out);
                (res, slice) = stream.write_all(slice).await;
                if let Err(e) = res {
                    return (Err(e), slice.into_inner());
                }
                buf = slice.into_inner();
                fsm
            }
            Ok(rc_zip::fsm::FsmResult::Done(_buffer)) => {
                break;
            }
            Err(err) => {
                eprintln!("ERR {:?}", err);
                return (Ok(()), buf);
            }
        }
    }
    (Ok(()), buf)
}
