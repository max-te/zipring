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

### Tuning

| Variable | Default | Meaning |
| --- | --- | --- |
| `PORT` | `50002` | Port to listen on. |
| `ZIPRING_THREADS` | cores, capped at 8 | Worker threads, each with its own io_uring. |
| `ZIPRING_URING_FLAGS` | none | Comma-separated io_uring setup flags: `single_issuer`, `coop_taskrun`, `defer_taskrun` (which requires `single_issuer`). |
