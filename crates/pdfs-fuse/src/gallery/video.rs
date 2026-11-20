use std::{collections::VecDeque, io, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use http_body_util::{BodyExt, Empty, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Frame, Incoming},
    header,
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use pdfs_core::{CoreResult, control::Response as ControlResponse};
use proton_gallery_app_rs::video::VideoStream;
use proton_gallery_rs::CancellationToken;
use tokio::net::TcpListener;

type Body = UnsyncBoxBody<Bytes, io::Error>;

const LEASE: Duration = Duration::from_secs(4 * 60 * 60);
const MAX_STREAMS: usize = 4;
const CHUNK: u32 = 256 * 1024;

#[derive(Default)]
pub(super) struct Server {
    active: parking_lot::Mutex<VecDeque<(String, CancellationToken)>>,
}

impl Server {
    pub(super) fn close(&self, token: &str) {
        let mut active = self.active.lock();
        if let Some(index) = active.iter().position(|(id, _)| id == token)
            && let Some((_, cancel)) = active.remove(index)
        {
            cancel.cancel();
        }
    }

    pub(super) fn close_all(&self) {
        for (_, cancel) in self.active.lock().drain(..) {
            cancel.cancel();
        }
    }

    pub(super) async fn open(
        &self,
        stream: Arc<VideoStream>,
        account: &CancellationToken,
    ) -> CoreResult<ControlResponse> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(super::gallery_error)?;
        let address = listener.local_addr().map_err(super::gallery_error)?;
        let mut nonce = [0; 32];
        getrandom::fill(&mut nonce).map_err(super::gallery_error)?;
        let token = URL_SAFE_NO_PAD.encode(nonce);
        let path: Arc<str> = format!("/{token}").into();
        let cancel = account.child_token();
        {
            let mut active = self.active.lock();
            active.retain(|(_, cancel)| !cancel.is_cancelled());
            if active.len() >= MAX_STREAMS
                && let Some((_, oldest)) = active.pop_front()
            {
                oldest.cancel();
            }
            active.push_back((token.clone(), cancel.clone()));
        }
        let size = stream.length();
        tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            let lifetime = tokio::time::sleep(LEASE);
            tokio::pin!(lifetime);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = &mut lifetime => break,
                    _ = connections.join_next(), if !connections.is_empty() => {},
                    incoming = listener.accept(), if connections.len() < 8 => {
                        let Ok((socket, _)) = incoming else { break };
                        let stream = stream.clone();
                        let path = path.clone();
                        connections.spawn(async move {
                            let service = service_fn(move |request| reply(request, stream.clone(), path.clone()));
                            let _ = http1::Builder::new()
                                .timer(hyper_util::rt::TokioTimer::new())
                                .header_read_timeout(Duration::from_secs(10))
                                .keep_alive(false)
                                .serve_connection(TokioIo::new(socket), service).await;
                        });
                    }
                }
            }
            stream.stop();
            cancel.cancel();
            connections.abort_all();
        });
        Ok(ControlResponse::GalleryVideo {
            url: format!("http://{address}/{token}"),
            token,
            size,
        })
    }
}

fn range(value: Option<&str>, size: u64) -> Option<(u64, u64)> {
    let Some(value) = value else {
        return Some((0, size));
    };
    let (first, last) = value.strip_prefix("bytes=")?.split_once('-')?;
    if first.is_empty() {
        let suffix = last.parse::<u64>().ok()?.min(size);
        return (suffix > 0).then_some((size - suffix, size));
    }
    let start = first.parse::<u64>().ok()?;
    let end = if last.is_empty() {
        size
    } else {
        last.parse::<u64>().ok()?.saturating_add(1).min(size)
    };
    (start < end).then_some((start, end))
}

fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed_unsync()
}

async fn reply(
    request: Request<Incoming>,
    stream: Arc<VideoStream>,
    path: Arc<str>,
) -> Result<Response<Body>, hyper::http::Error> {
    let response = Response::builder().header(header::CACHE_CONTROL, "no-store");
    if request
        .uri()
        .path_and_query()
        .is_none_or(|actual| actual.as_str() != path.as_ref())
    {
        return response.status(StatusCode::NOT_FOUND).body(empty());
    }
    if !matches!(*request.method(), Method::HEAD | Method::GET) {
        return response
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(empty());
    }
    let size = stream.length();
    let requested = request.headers().get(header::RANGE);
    let parsed = match requested {
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| range(Some(value), size)),
        None => range(None, size),
    };
    let Some((start, end)) = parsed else {
        return response
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{size}"))
            .body(empty());
    };
    let response = response
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, end - start)
        .header(header::CONTENT_TYPE, "application/octet-stream");
    let response = if requested.is_some() {
        response.status(StatusCode::PARTIAL_CONTENT).header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{size}", end - 1),
        )
    } else {
        response
    };
    if request.method() == Method::HEAD {
        return response.body(empty());
    }
    let body = futures::stream::try_unfold((stream, start), move |(stream, at)| async move {
        if at == end {
            return Ok(None);
        }
        let read = (end - at).min(u64::from(CHUNK)) as u32;
        let bytes = stream.read(at, read).await.map_err(io::Error::other)?;
        if bytes.is_empty() {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        let next = at + bytes.len() as u64;
        Ok(Some((Frame::data(Bytes::from(bytes)), (stream, next))))
    });
    response.body(BodyExt::boxed_unsync(StreamBody::new(body)))
}

#[cfg(test)]
mod tests {
    use super::range;

    #[test]
    fn resolves_seek_ranges_and_rejects_invalid_ranges() {
        assert_eq!(range(None, 10), Some((0, 10)));
        assert_eq!(range(Some("bytes=3-5"), 10), Some((3, 6)));
        assert_eq!(range(Some("bytes=9-"), 10), Some((9, 10)));
        assert_eq!(range(Some("bytes=-3"), 10), Some((7, 10)));
        assert_eq!(range(Some("bytes=10-"), 10), None);
        assert_eq!(range(Some("bytes=0-1,3-4"), 10), None);
    }
}
