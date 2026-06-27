use crate::borrowed_file::BorrowedFile;
use crate::fstree::FsTreeNode;
use crate::request::Request;
use crate::response::status::HttpStatus;
use monoio::io::AsyncWriteRent;
use tracing::Instrument as _;
use tracing::field;

use crate::Buf;
use stream::ResponseStream;

pub mod status;
mod stream;
#[cfg(test)]
mod test;

pub async fn respond<'w, W: AsyncWriteRent>(
    request: Request,
    file: &BorrowedFile<'_>,
    tree: &FsTreeNode,
    stream: &mut W,
) -> std::io::Result<Buf> {
    let respond_span = tracing::info_span!("response", path = field::Empty).entered();
    match request {
        Request::Get { path, headers } => {
            let node = str::from_utf8(&path)
                .inspect(|path| {
                    respond_span.record("path", path);
                })
                .map_err(|e| {
                    tracing::warn!("path not utf-8: {e}");
                })
                .ok()
                .and_then(|path| tree.find(path));

            let s = ResponseStream::new(stream, path.into_inner());
            let Some(node) = node else {
                return s
                    .serve_status(HttpStatus::NotFound)
                    .instrument(respond_span.exit())
                    .await
                    .map(ResponseStream::into_buf);
            };
            if let Some(crc32) = headers.if_none_match
                && let Some(entry) = node.entry()
                && entry.crc32 == crc32
            {
                tracing::debug!("etag matches");
                s.serve_not_modified(entry.crc32)
                    .instrument(respond_span.exit())
                    .await
            } else {
                s.serve_node(file, node, headers.accepted_encodings)
                    .instrument(respond_span.exit())
                    .await
            }
        }
        Request::Bad { buf, status } => {
            ResponseStream::new(stream, buf)
                .serve_status(status)
                .instrument(respond_span.exit())
                .await
        }
    }
    .map(ResponseStream::into_buf)
}
