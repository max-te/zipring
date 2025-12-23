# zipring

A high-performance web server that serves files directly from ZIP archives using io_uring.
I built this, because I got tired of extracting all my test pipeline artifact zips from Gitlab.

## Features

- Serves files from ZIP archives over HTTP
- Zero-copy serving of compressed content
- Directory listings
- Automatic MIME type detection
- Multi-threaded with io_uring-based async runtime (monoio)
- Artisanally hand-assembled HTTP responses

## Usage

```bash
# Build
cargo build --release

# Run
./target/release/zipring <path-to-zip-file>
```

The server will start listening on `http://127.0.0.1:50002`.
