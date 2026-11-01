use super::*;

use http::{HeaderMap, HeaderValue};
use http_body::Frame;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct FramedBody(std::vec::IntoIter<Frame<Bytes>>);

impl http_body::Body for FramedBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Poll::Ready(self.0.next().map(Ok))
    }
}

fn frame(kind: u8, flags: u8, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = (payload.len() as u32).to_be_bytes()[1..].to_vec();
    bytes.extend_from_slice(&[kind, flags]);
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

async fn read_frame(peer: &mut TcpStream) -> Result<(u8, u8, u32, Vec<u8>)> {
    let mut head = [0; 9];
    peer.read_exact(&mut head).await?;
    let length = usize::from(head[0]) << 16 | usize::from(head[1]) << 8 | usize::from(head[2]);
    ensure!(length <= 64 * 1024, "unexpected fixture frame length");
    let id = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
    let mut payload = vec![0; length];
    peer.read_exact(&mut payload).await?;
    Ok((head[3], head[4], id, payload))
}

fn integer(bytes: &mut &[u8], prefix: u8) -> Result<usize> {
    let (&first, rest) = bytes.split_first().context("missing HPACK integer")?;
    *bytes = rest;
    let mask = (1 << prefix) - 1;
    let mut value = usize::from(first & mask);
    if value < usize::from(mask) {
        return Ok(value);
    }
    for shift in (0..35).step_by(7) {
        let (&next, rest) = bytes.split_first().context("incomplete HPACK integer")?;
        *bytes = rest;
        value += usize::from(next & 127) << shift;
        if next & 128 == 0 {
            return Ok(value);
        }
    }
    anyhow::bail!("HPACK integer exceeds fixture bound")
}

fn skip_string(bytes: &mut &[u8]) -> Result<()> {
    let length = integer(bytes, 7)?;
    ensure!(length <= bytes.len(), "incomplete HPACK string");
    *bytes = &bytes[length..];
    Ok(())
}

fn assert_no_dynamic_fields(mut bytes: &[u8]) -> Result<usize> {
    // Inspect representation prefixes, without decoding Huffman strings or
    // implementing a second HPACK decoder. Static indices and size updates are
    // allowed; application literals must never populate the dynamic table.
    // https://httpwg.org/specs/rfc7541.html#rfc.section.6
    let mut literals = 0;
    while let Some(&first) = bytes.first() {
        if first & 128 != 0 {
            ensure!(integer(&mut bytes, 7)? <= 61, "dynamic indexed field");
        } else if first & 224 == 32 {
            integer(&mut bytes, 5)?;
        } else {
            ensure!(first & 240 == 16, "application field can enter HPACK table");
            let index = integer(&mut bytes, 4)?;
            ensure!(index <= 61, "dynamic indexed name");
            if index == 0 {
                skip_string(&mut bytes)?;
            }
            skip_string(&mut bytes)?;
            literals += 1;
        }
    }
    Ok(literals)
}

#[tokio::test]
async fn large_peer_table_cannot_retain_response_headers_or_trailers() -> Result<()> {
    timeout(Duration::from_secs(15), async {
        let counter = Arc::new(AtomicUsize::new(0));
        let router = Router::new().route(
            "/",
            post(move || {
                let sequence = counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    let mut fields = HeaderMap::new();
                    let value =
                        HeaderValue::from_str(&format!("{sequence}-{}", "metadata".repeat(512)))
                            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                    fields.insert("x-unique-response", value);
                    fields.insert(
                        "x-repeated-response",
                        HeaderValue::from_static("same-value"),
                    );
                    let frames = vec![
                        Frame::data(Bytes::from_static(b"ok")),
                        Frame::trailers(fields.clone()),
                    ];
                    Ok::<_, StatusCode>((fields, Body::new(FramedBody(frames.into_iter()))))
                }
            }),
        );
        let (owner, address) = start(router, limits()).await?;
        let mut peer = TcpStream::connect(address).await?;
        peer.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").await?;
        // Advertise a 64 MiB decoder table to exercise the outbound encoder.
        peer.write_all(&frame(4, 0, 0, &[0, 1, 4, 0, 0, 0])).await?;
        for id in (1..128).step_by(2) {
            peer.write_all(&frame(1, 5, id, b"\x83\x86\x84\x01\x07fixture"))
                .await?;
            let mut header_block = Vec::new();
            let mut blocks = 0;
            let mut data = Vec::new();
            loop {
                let (kind, flags, stream, payload) = read_frame(&mut peer).await?;
                ensure!(kind != 3 && kind != 7, "unexpected reset/GOAWAY");
                if kind == 4 && flags & 1 == 0 {
                    peer.write_all(&frame(4, 1, 0, &[])).await?;
                }
                if stream != id {
                    continue;
                }
                if matches!(kind, 1 | 9) {
                    header_block.extend_from_slice(&payload);
                    if flags & 4 != 0 {
                        ensure!(assert_no_dynamic_fields(&header_block)? >= 2);
                        header_block.clear();
                        blocks += 1;
                    }
                } else if kind == 0 {
                    data.extend_from_slice(&payload);
                }
                if matches!(kind, 0 | 1) && flags & 1 != 0 {
                    break;
                }
            }
            ensure!(
                blocks == 2 && data == b"ok",
                "headers, body and trailers preserved"
            );
        }
        drop(peer);
        stop(owner).await
    })
    .await?
}
