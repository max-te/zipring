use crate::fstree::FsTreeNode;
use crate::request::Request;
use crate::response::status::HttpStatus;
use compio::BufResult;
use compio::fs::File;
use compio::io::AsyncWriteExt;
use tracing::Instrument as _;
use tracing::field;

use crate::Buf;
use stream::ResponseStream;

mod coding;
pub mod status;
mod stream;
#[cfg(test)]
mod test;

pub async fn respond<W: AsyncWriteExt>(
    request: Request,
    buf: Buf,
    file: &File,
    tree: &FsTreeNode,
    stream: &mut W,
) -> BufResult<(), Buf> {
    let respond_span = tracing::info_span!("response", path = field::Empty).entered();
    match request {
        Request::Get { path_len, headers } => {
            let target = str::from_utf8(&buf[..path_len])
                .inspect(|path| {
                    respond_span.record("path", path);
                })
                .map_err(|e| {
                    tracing::warn!("path not utf-8: {e}");
                })
                .ok()
                .and_then(|path| tree.resolve(path));

            let s = ResponseStream::new(stream, buf);
            match target {
                None => {
                    s.serve_status(HttpStatus::NotFound)
                        .instrument(respond_span.exit())
                        .await
                }
                Some(target) => {
                    s.serve_target(file, target, headers)
                        .instrument(respond_span.exit())
                        .await
                }
            }
        }
        Request::Bad { status } => {
            ResponseStream::new(stream, buf)
                .serve_status(status)
                .instrument(respond_span.exit())
                .await
        }
    }
    .map_buffer(ResponseStream::into_buf)
}
