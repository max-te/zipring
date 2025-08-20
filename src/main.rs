mod rc_zip_monoio;

use std::io::{Cursor, Write};
use std::path::PathBuf;

use monoio::buf::IoBufMut;
use monoio::fs::File;
use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::{TcpListener, TcpStream};
use rc_zip::fsm::EntryFsm;
use rc_zip::parse::{Archive, Entry, Method};

use crate::rc_zip_monoio::find_entry_compressed_data;

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

    let mut tree = FsTreeNode::root();
    for entry in zip.entries() {
        println!("{:?}", entry.sanitized_name());
        tree.insert(entry.clone());
    }
    dbg!(tree);

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
        tracing::debug!(?path, "GET request");

        if path.is_empty() || path.ends_with('/') {
            serve_index(&path, zip, &mut stream).await?;
        }
        let Some(entry) = find_entry(zip, &path).await else {
            tracing::warn!(?path, "entry not found");
            let (res, _) = stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await;
            res?;
            continue;
        };
        buf = serve_entry(&mut stream, file, buf, entry).await?;
    }
}

async fn serve_entry(
    stream: &mut TcpStream,
    file: &File,
    mut buf: Box<[u8]>,
    entry: Entry,
) -> Result<Box<[u8]>, std::io::Error> {
    let mime_type = mime_guess::from_path(&entry.name).first_or_text_plain();
    tracing::debug!(?mime_type);
    let mut send_compressed = false;
    let mut compression_header_size = 0;

    // Assemble header
    let mut cur = Cursor::new(buf);
    cur.write(b"HTTP/1.1 200 OK\r\nContent-Type: ")?;
    cur.write(mime_type.essence_str().as_bytes())?;
    cur.write(b"\r\n")?;

    tracing::debug!(?entry.method);
    match entry.method {
        Method::Deflate => {
            cur.write(b"Content-Encoding: gzip\r\n")?;
            send_compressed = true;
            compression_header_size = 18;
        }
        Method::Zstd => {
            cur.write(b"Content-Encoding: zstd\r\n")?;
            send_compressed = true;
        }
        _ => (),
    }

    cur.write(b"Keep-Alive: timeout=20, max=200\r\nContent-Length: ")?;
    let mut intbuf = itoa::Buffer::new();
    cur.write(
        intbuf
            .format(if send_compressed {
                entry.compressed_size + compression_header_size
            } else {
                entry.uncompressed_size
            })
            .as_bytes(),
    )?;
    cur.write(b"\r\n\r\n")?;

    // Send header
    let n = cur.position() as usize;
    buf = cur.into_inner();
    let mut slice = IoBufMut::slice_mut(buf, 0..n);
    let res;
    (res, slice) = stream.write_all(slice).await;
    buf = slice.into_inner();
    res?;

    // Send body

    buf = if send_compressed {
        send_compressed_entry(stream, file, buf, entry).await?
    } else {
        send_decompressed_entry(stream, file, buf, entry).await?
    };
    Ok(buf)
}

async fn send_compressed_entry(
    stream: &mut TcpStream,
    file: &File,
    buf: Box<[u8]>,
    entry: Entry,
) -> Result<Box<[u8]>, std::io::Error> {
    let mut len = entry.compressed_size as usize;
    let mut gzip_trailer = [0u8; 8];
    if entry.method == Method::Deflate {
        // Write gzip header for deflate
        stream
            .write_all(&[0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF])
            .await
            .0?;
        (gzip_trailer[0..4]).copy_from_slice(&entry.crc32.to_le_bytes());
        (gzip_trailer[4..8]).copy_from_slice(&entry.uncompressed_size.to_le_bytes()[0..4]);
    }

    let (mut offset, mut buf) = find_entry_compressed_data(&file, entry, Some(buf)).await?;
    while len > 0 {
        let bytes_to_read = len.min(buf.len());
        let (res, slice) = file
            .read_at(IoBufMut::slice_mut(buf, 0..bytes_to_read), offset)
            .await;
        let n = res?;
        buf = slice.into_inner();
        offset += n as u64;
        len -= n;
        let slice = IoBufMut::slice_mut(buf, 0..n);
        let (res, slice) = stream.write_all(slice).await;
        res?;
        buf = slice.into_inner();
    }

    (buf[..8]).copy_from_slice(&gzip_trailer);
    let slice = IoBufMut::slice_mut(buf, ..8);
    let (res, slice) = stream.write_all(slice).await;
    res?;
    Ok(slice.into_inner())
}

async fn send_decompressed_entry(
    stream: &mut TcpStream,
    file: &File,
    mut buf: Box<[u8]>,
    entry: Entry,
) -> Result<Box<[u8]>, std::io::Error> {
    let mut res;
    let mut offset = entry.header_offset;
    let mut fsm = EntryFsm::new(Some(entry), None);
    loop {
        if fsm.wants_read() {
            let dst = fsm.space();
            let max_read = dst.len().min(buf.len());
            let mut slice = IoBufMut::slice_mut(buf, 0..max_read);
            (res, slice) = file.read_at(slice, offset).await;
            let n = res?;
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
                res?;
                buf = slice.into_inner();
                fsm
            }
            Ok(rc_zip::fsm::FsmResult::Done(_buffer)) => return Ok(buf),
            Err(err) => {
                tracing::error!("ERR {:?}", err);
                return Err(std::io::Error::other(err));
            }
        }
    }
}

async fn serve_index(path: &str, zip: &Archive, stream: &mut TcpStream) -> std::io::Result<()> {
    let mut listing = Vec::<u8>::with_capacity(32 * 1024);

    listing.write(b"<!doctype html><html><ul>")?;
    if !path.is_empty() {
        listing.write(b"<li><a href=\"..\">..</a>\n")?;
    }
    for entry in zip.entries() {
        if entry.name.starts_with(path) {
            if let Some(name) = entry.sanitized_name() {
                listing.write_fmt(format_args!("<li><a href=\"/{name}\">{name}</a>\n"))?;
            }
        }
    }

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
        listing.len()
    )
    .into_bytes();

    stream.write_all(header).await.0?;
    stream.write_all(listing).await.0?;
    Ok(())
}

enum FsTreeNode {
    Dir {
        name: String,
        children: Vec<FsTreeNode>,
        entry: Option<Entry>,
    },
    File {
        name: String,
        entry: Entry,
    },
}

impl std::fmt::Debug for FsTreeNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dir { name, children, .. } => f.debug_map().key(name).value(children).finish(),
            Self::File { name, .. } => name.fmt(f),
        }
    }
}

impl FsTreeNode {
    fn insert_at(&mut self, entry: Entry, path: String) {
        let FsTreeNode::Dir { children, .. } = self else {
            panic!("Cannot insert into FsTreeNode::File")
        };

        if let Some(separator) = path.chars().position(|c| c == '/') {
            let mut head = path;
            let mut tail = head.split_off(separator);

            let existing_child = children.iter_mut().find(|ch| match ch {
                FsTreeNode::Dir { name, .. } => head.eq(name),
                _ => false,
            });

            let child = match existing_child {
                Some(c) => c,
                None => {
                    let new_child = FsTreeNode::Dir {
                        name: head.to_owned(),
                        children: vec![],
                        entry: None,
                    };
                    children.push(new_child);
                    children.last_mut().unwrap()
                }
            };

            if tail.len() > 1 {
                // Recurse
                let tail = tail.split_off(1);
                child.insert_at(entry, tail);
            } else {
                // We are at the insertion site of a directory.
                match child {
                    FsTreeNode::Dir {
                        entry: entry_slot, ..
                    } => {
                        let _ = entry_slot.insert(entry);
                    }
                    FsTreeNode::File { .. } => unreachable!(),
                }
            }
        } else {
            // Insertion site of a file
            let new_child = FsTreeNode::File {
                name: path.to_owned(),
                entry,
            };
            children.push(new_child);
        }
    }

    fn insert(&mut self, entry: Entry) {
        let path = entry.name.to_owned();
        self.insert_at(entry, path)
    }

    fn root() -> Self {
        FsTreeNode::Dir {
            name: String::new(),
            children: Vec::new(),
            entry: None,
        }
    }

    fn find(path: &str) -> Option<&Self> {
        None
    }
}
