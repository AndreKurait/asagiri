//! Bounded HTTP/2 and gRPC transport. Bulk data stays on the private relay,
//! outside the agent control channel and the engine's log subscribers.

use std::future::{Future, poll_fn};
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use h2::{RecvStream, SendStream};
use hyper::http::HeaderMap;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub(super) const WINDOW: usize = 256 << 10;
pub(super) const MAX_MESSAGE: usize = 16 << 20;
pub(super) const MAX_DATA_MESSAGE: usize = 4 << 20;

pub(super) fn error(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::other(error)
}

pub(super) struct Task<T>(pub tokio::task::JoinHandle<T>);

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) fn spawn<F>(future: F) -> Task<F::Output>
where F: Future + Send + 'static, F::Output: Send + 'static {
    Task(tokio::spawn(future))
}

/// An HTTP/1 parser may have read the start of HTTP/2 in the same packet.
pub(super) struct Prefixed<T> {
    pub io: T,
    pub prefix: Bytes,
}

impl<T: AsyncRead + Unpin> AsyncRead for Prefixed<T> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.prefix.is_empty() {
            let n = this.prefix.len().min(out.remaining());
            out.put_slice(&this.prefix[..n]);
            this.prefix.advance(n);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.io).poll_read(cx, out)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Prefixed<T> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, data)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// Replies from the guest use big-endian lengths and an explicit zero end
/// frame. Retain the VZ connection owner until all I/O has stopped.
pub(super) struct Framed<T, O> {
    io: T,
    _owner: O,
    header: [u8; 4],
    have: usize,
    remaining: usize,
    ended: bool,
}

impl<T, O> Framed<T, O> {
    pub fn new(io: T, owner: O) -> Self {
        Self { io, _owner: owner, header: [0; 4], have: 0, remaining: 0, ended: false }
    }
}

impl<T: AsyncRead + Unpin, O: Unpin> AsyncRead for Framed<T, O> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if out.remaining() == 0 || this.ended {
            return Poll::Ready(Ok(()));
        }
        while this.remaining == 0 {
            let mut head = ReadBuf::new(&mut this.header[this.have..]);
            match Pin::new(&mut this.io).poll_read(cx, &mut head) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {}
            }
            let n = head.filled().len();
            if n == 0 {
                return Poll::Ready(Err(io::Error::new(io::ErrorKind::UnexpectedEof, "BuildKit relay ended before its EOF frame")));
            }
            this.have += n;
            if this.have != 4 { continue; }
            this.have = 0;
            this.remaining = u32::from_be_bytes(this.header) as usize;
            if this.remaining > 64 << 10 {
                return Poll::Ready(Err(error("oversized BuildKit relay frame")));
            }
            if this.remaining == 0 {
                this.ended = true;
                return Poll::Ready(Ok(()));
            }
        }
        let n = this.remaining.min(out.remaining());
        let mut part = ReadBuf::new(out.initialize_unfilled_to(n));
        match Pin::new(&mut this.io).poll_read(cx, &mut part) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {
                let n = part.filled().len();
                if n == 0 {
                    return Poll::Ready(Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated BuildKit relay frame")));
                }
                this.remaining -= n;
                out.advance(n);
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl<T: AsyncWrite + Unpin, O: Unpin> AsyncWrite for Framed<T, O> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, data)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

pub(super) async fn client<T>(io: T) -> io::Result<(h2::client::SendRequest<Bytes>, Task<Result<(), h2::Error>>)>
where T: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    let mut builder = h2::client::Builder::new();
    builder.initial_window_size(WINDOW as u32).initial_connection_window_size((8 * WINDOW) as u32)
        .max_header_list_size(64 << 10).max_send_buffer_size(WINDOW);
    let (client, connection) = tokio::time::timeout(Duration::from_secs(15), builder.handshake(io))
        .await.map_err(error)?.map_err(error)?;
    Ok((client, spawn(connection)))
}

pub(super) async fn server<T>(io: T) -> io::Result<h2::server::Connection<T, Bytes>>
where T: AsyncRead + AsyncWrite + Unpin {
    let mut builder = h2::server::Builder::new();
    builder.initial_window_size(WINDOW as u32).initial_connection_window_size((8 * WINDOW) as u32)
        .max_header_list_size(64 << 10).max_send_buffer_size(WINDOW).max_concurrent_streams(64)
        .max_concurrent_reset_streams(64).max_pending_accept_reset_streams(64)
        .max_local_error_reset_streams(Some(16));
    tokio::time::timeout(Duration::from_secs(15), builder.handshake(io)).await.map_err(error)?.map_err(error)
}

/// Never let h2's send queue become a second, unbounded application buffer.
pub(super) async fn send(stream: &mut SendStream<Bytes>, mut data: Bytes) -> io::Result<()> {
    while !data.is_empty() {
        stream.reserve_capacity(data.len());
        let room = poll_fn(|cx| stream.poll_capacity(cx)).await
            .ok_or_else(|| error("BuildKit stream closed"))?.map_err(error)?;
        if room == 0 { continue; }
        let next = data.split_to(room.min(data.len()));
        stream.send_data(next, false).map_err(error)?;
    }
    Ok(())
}

pub(super) fn finish(stream: &mut SendStream<Bytes>, trailers: Option<HeaderMap>) -> io::Result<()> {
    match trailers {
        Some(trailers) => stream.send_trailers(trailers).map_err(error),
        None => stream.send_data(Bytes::new(), true).map_err(error),
    }
}

pub(super) async fn pump(mut input: RecvStream, mut output: SendStream<Bytes>) -> io::Result<()> {
    loop {
        let data = tokio::select! {
            next = input.data() => next,
            _ = poll_fn(|cx| output.poll_reset(cx)) => return Err(error("BuildKit stream cancelled")),
        };
        let Some(data) = data else { break };
        let data = data.map_err(error)?;
        let n = data.len();
        send(&mut output, data).await?;
        input.flow_control().release_capacity(n).map_err(error)?;
    }
    let trailers = input.trailers().await.map_err(error)?;
    finish(&mut output, trailers)
}

pub(super) fn frame(data: &[u8]) -> Bytes {
    let mut frame = Vec::with_capacity(data.len() + 5);
    frame.push(0);
    frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
    frame.extend_from_slice(data);
    frame.into()
}

/// gRPC frames may cross any number of HTTP/2 DATA frames. The size is
/// checked before allocation. A transport EOF never completes half a message.
pub(super) struct Grpc {
    stream: RecvStream,
    pending: BytesMut,
    limit: usize,
    ended: bool,
    pub trailers: Option<HeaderMap>,
}

impl Grpc {
    pub fn new(stream: RecvStream, limit: usize) -> Self {
        Self { stream, pending: BytesMut::new(), limit, ended: false, trailers: None }
    }

    pub async fn next(&mut self) -> io::Result<Option<Bytes>> {
        loop {
            if self.pending.len() >= 5 {
                if self.pending[0] != 0 {
                    return Err(error("compressed BuildKit messages are not supported by this adapter"));
                }
                let len = u32::from_be_bytes(self.pending[1..5].try_into().expect("four bytes")) as usize;
                if len > self.limit {
                    return Err(error("BuildKit message exceeds the adapter limit"));
                }
                if self.pending.len() >= len + 5 {
                    self.pending.advance(5);
                    return Ok(Some(self.pending.split_to(len).freeze()));
                }
            }
            if self.ended {
                return if self.pending.is_empty() { Ok(None) }
                    else { Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated BuildKit message")) };
            }
            match self.stream.data().await {
                Some(data) => {
                    let data = data.map_err(error)?;
                    if self.pending.len().saturating_add(data.len()) > self.limit + (64 << 10) {
                        return Err(error("BuildKit message buffer exceeds its limit"));
                    }
                    self.pending.extend_from_slice(&data);
                    self.stream.flow_control().release_capacity(data.len()).map_err(error)?;
                }
                None => {
                    self.trailers = self.stream.trailers().await.map_err(error)?;
                    self.ended = true;
                }
            }
        }
    }
}

pub(super) async fn unary(stream: RecvStream) -> io::Result<(Option<Bytes>, Option<HeaderMap>)> {
    let mut reader = Grpc::new(stream, MAX_MESSAGE);
    let data = reader.next().await?;
    if reader.next().await?.is_some() {
        return Err(error("multiple messages in a unary BuildKit request"));
    }
    Ok((data, reader.trailers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn framed_replies_survive_split_headers_and_preserve_request_bytes() {
        let (io, mut peer) = tokio::io::duplex(128);
        let mut framed = Framed::new(io, ());
        let writer = spawn(async move {
            for byte in [0, 0, 0, 3, b'a', b'b', b'c', 0, 0, 0, 0] {
                peer.write_all(&[byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
            let mut request = [0; 3];
            peer.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"raw");
        });
        let mut data = Vec::new();
        framed.read_to_end(&mut data).await.unwrap();
        assert_eq!(data, b"abc");
        framed.write_all(b"raw").await.unwrap();
        let mut writer = writer;
        (&mut writer.0).await.unwrap();
    }

    #[tokio::test]
    async fn missing_end_and_truncated_or_oversized_frames_are_errors() {
        for input in [&[0, 0, 0, 1, b'x'][..], &[0, 0, 0, 3, b'x'], &[0, 2, 0, 0], &[0, 0]] {
            let (io, mut peer) = tokio::io::duplex(128);
            peer.write_all(input).await.unwrap();
            drop(peer);
            assert!(Framed::new(io, ()).read_to_end(&mut Vec::new()).await.is_err(), "{input:?}");
        }
    }

    #[tokio::test]
    async fn upgraded_prefix_is_consumed_before_socket_data() {
        let (io, mut peer) = tokio::io::duplex(128);
        peer.write_all(b"socket").await.unwrap();
        drop(peer);
        let mut stream = Prefixed { io, prefix: Bytes::from_static(b"preface-") };
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"preface-socket");
    }
}
