use super::{Codec, Command, Event, Rooms, store::Grant};
use bytes::Bytes;
use futures_util::{
    FutureExt, SinkExt, StreamExt,
    future::BoxFuture,
    stream::{FuturesUnordered, SplitSink, SplitStream},
};
use http::{Request, Response, StatusCode, header::SEC_WEBSOCKET_PROTOCOL};
use http_body_util::Empty;
use hyper::{body::Incoming, service::service_fn, upgrade::Upgraded};
use hyper_util::rt::TokioIo;
use hyve::{
    HyveError, HyveResult, Value,
    api::realtime::{BINARY_PROTOCOL, JSON_PROTOCOL, MAX_FRAME, MAX_HINT, sequence},
};
use std::{
    convert::Infallible,
    future::Future,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    sync::oneshot,
    task::JoinSet,
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::server::create_response,
        protocol::{CloseFrame, Role, WebSocketConfig, frame::coding::CloseCode},
    },
};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use wtransport::{
    Connection, Endpoint, RecvStream, SendStream, ServerConfig,
    endpoint::{IncomingSession, endpoint_side},
};

const HANDSHAKE: Duration = Duration::from_secs(10);
const IDLE: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(150);

/// QUIC/TLS and loopback WebSocket listeners sharing the same room store.
pub struct RoomServer {
    quic: Endpoint<endpoint_side::Server>,
    ws: TcpListener,
    rooms: Rooms,
}
impl RoomServer {
    /// Listens for WebTransport with the supplied certificate. Place a TLS
    /// reverse proxy in front of the loopback WebSocket listener, or use
    /// [`accept_websocket`] directly with a TLS-wrapped socket.
    pub fn new(rooms: Rooms, quic: ServerConfig, ws: TcpListener) -> io::Result<Self> {
        if !ws.local_addr()?.ip().is_loopback() {
            return Err(io::Error::other(
                "plain WebSocket requires a loopback listener",
            ));
        }
        Ok(Self {
            quic: Endpoint::server(quic)?,
            ws,
            rooms,
        })
    }
    /// Bound UDP and TCP addresses, respectively.
    pub fn addresses(&self) -> io::Result<(SocketAddr, SocketAddr)> {
        Ok((self.quic.local_addr()?, self.ws.local_addr()?))
    }
    /// Serves until shutdown and releases the connection handlers.
    pub async fn serve(self, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        let mut tasks = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                conn = self.quic.accept() => { tasks.spawn(quic(self.rooms.clone(), conn)); }
                conn = self.ws.accept() => {
                    let (stream, _) = conn?;
                    tasks.spawn(accept_websocket(self.rooms.clone(),stream));
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
        self.quic.close(0u32.into(), b"server shutdown");
        tasks.shutdown().await;
        Ok(())
    }
}

fn ticket(path: &str) -> Option<&str> {
    let value = path.strip_prefix("/realtime?ticket=")?;
    (!value.contains('&') && !value.is_empty()).then_some(value)
}
fn framing() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME)
        .new_codec()
}
async fn quic(rooms: Rooms, incoming: IncomingSession) -> io::Result<()> {
    let (connection, send, recv, grant) = timeout(HANDSHAKE, async {
        let req = incoming.await.map_err(io::Error::other)?;
        let Some(token) = ticket(req.path()) else {
            req.not_found().await;
            return Err(io::Error::other("invalid room path"));
        };
        let Ok(grant) = rooms.consume(token).await else {
            req.forbidden().await;
            return Err(io::Error::other("room ticket refused"));
        };
        let connection = req.accept().await.map_err(io::Error::other)?;
        let (send, mut recv) = connection.accept_bi().await.map_err(io::Error::other)?;
        let mut preface = [0u8; 4];
        recv.read_exact(&mut preface)
            .await
            .map_err(io::Error::other)?;
        if &preface != b"HYV1" {
            return Err(io::Error::other("unsupported room version"));
        }
        Ok((connection, send, recv, grant))
    })
    .await??;
    let result = session(
        rooms,
        grant,
        Codec::Binary,
        Reader::Quic {
            main: FramedRead::new(recv, framing()),
            connection: connection.clone(),
            streams: FuturesUnordered::new(),
        },
        Writer::Quic(FramedWrite::new(send, framing()), connection.clone()),
    )
    .await;
    connection.close(0u32.into(), b"room closed");
    result
}

type Ws = WebSocketStream<TokioIo<Upgraded>>;
type Handshake = (hyper::upgrade::OnUpgrade, Grant, Codec);
type UpgradeTx = Arc<Mutex<Option<oneshot::Sender<Handshake>>>>;

async fn upgrade(
    rooms: Rooms,
    mut req: Request<Incoming>,
    tx: UpgradeTx,
) -> Result<Response<Empty<Bytes>>, Infallible> {
    let failure = |status| {
        Response::builder()
            .status(status)
            .header("connection", "close")
            .body(Empty::new())
            .expect("valid status")
    };
    let upgraded = hyper::upgrade::on(&mut req);
    let req = req.map(|_| ());
    let Ok(mut response) = create_response(&req) else {
        return Ok(failure(StatusCode::BAD_REQUEST));
    };
    let protocols = req
        .headers()
        .get(SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some((name, codec)) = protocols.split(',').find_map(|p| match p.trim() {
        BINARY_PROTOCOL => Some((BINARY_PROTOCOL, Codec::Binary)),
        JSON_PROTOCOL => Some((JSON_PROTOCOL, Codec::Json)),
        _ => None,
    }) else {
        return Ok(failure(StatusCode::BAD_REQUEST));
    };
    let Some(token) = req.uri().path_and_query().and_then(|p| ticket(p.as_str())) else {
        return Ok(failure(StatusCode::NOT_FOUND));
    };
    let Ok(grant) = rooms.consume(token).await else {
        return Ok(failure(StatusCode::UNAUTHORIZED));
    };
    let Some(tx) = tx.lock().expect("WebSocket upgrade").take() else {
        return Ok(failure(StatusCode::BAD_REQUEST));
    };
    if tx.send((upgraded, grant, codec)).is_err() {
        return Ok(failure(StatusCode::SERVICE_UNAVAILABLE));
    }
    response
        .headers_mut()
        .insert(SEC_WEBSOCKET_PROTOCOL, http::HeaderValue::from_static(name));
    Ok(response.map(|_| Empty::new()))
}

/// Authenticates and drives one HTTP/1.1 WebSocket upgrade. The caller
/// supplies the accepted stream (typically TLS outside loopback).
pub async fn accept_websocket<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    rooms: Rooms,
    stream: S,
) -> io::Result<()> {
    let (tx, rx) = oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let service = {
        let rooms = rooms.clone();
        service_fn(move |req| upgrade(rooms.clone(), req, tx.clone()))
    };
    timeout(
        HANDSHAKE,
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades(),
    )
    .await?
    .map_err(io::Error::other)?;
    let Ok((upgraded, grant, codec)) = rx.await else {
        return Ok(());
    };
    let stream = upgraded.await.map_err(io::Error::other)?;
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_FRAME);
    config.max_frame_size = Some(MAX_FRAME);
    let ws =
        WebSocketStream::from_raw_socket(TokioIo::new(stream), Role::Server, Some(config)).await;
    let (send, recv) = ws.split();
    session(rooms, grant, codec, Reader::Ws(recv), Writer::Ws(send)).await
}

enum IncomingFrame {
    Control(Message),
    Single(Bytes),
    Datagram(Bytes),
}

async fn single(mut stream: RecvStream) -> io::Result<Bytes> {
    timeout(HANDSHAKE, async {
        let mut header = [0; 8];
        stream
            .read_exact(&mut header)
            .await
            .map_err(io::Error::other)?;
        let len = u32::from_be_bytes(header[4..].try_into().expect("four bytes")) as usize;
        if &header[..4] != b"HYU1" || !(1..=MAX_FRAME).contains(&len) {
            return Err(io::Error::other("invalid room stream header"));
        }
        let mut data = vec![0; len];
        stream
            .read_exact(&mut data)
            .await
            .map_err(io::Error::other)?;
        if stream
            .read(&mut [0; 1])
            .await
            .map_err(io::Error::other)?
            .is_some()
        {
            return Err(io::Error::other("unexpected bytes after room command"));
        }
        Ok(Bytes::from(data))
    })
    .await?
}

enum Reader {
    Quic {
        main: FramedRead<RecvStream, LengthDelimitedCodec>,
        connection: Connection,
        streams: FuturesUnordered<BoxFuture<'static, io::Result<Bytes>>>,
    },
    Ws(SplitStream<Ws>),
}
impl Reader {
    async fn next(&mut self) -> io::Result<Option<IncomingFrame>> {
        match self {
            Self::Quic {
                main,
                connection,
                streams,
            } => loop {
                tokio::select! {
                    frame = main.next() => return Ok(frame.transpose()?.map(|bytes| IncomingFrame::Control(Message::Binary(bytes.freeze())))),
                    packet = connection.receive_datagram() => {
                        let data = packet.map_err(io::Error::other)?.payload();
                        if data.len() <= MAX_HINT {
                            return Ok(Some(IncomingFrame::Datagram(data)));
                        }
                    },
                    stream = connection.accept_uni(), if streams.len() < 4 => {
                        streams.push(single(stream.map_err(io::Error::other)?).boxed());
                    },
                    Some(bytes) = streams.next(), if !streams.is_empty() => {
                        return Ok(Some(IncomingFrame::Single(bytes?)));
                    },
                }
            },
            Self::Ws(r) => Ok(r
                .next()
                .await
                .transpose()
                .map_err(io::Error::other)?
                .map(IncomingFrame::Control)),
        }
    }
}
enum Writer {
    Quic(FramedWrite<SendStream, LengthDelimitedCodec>, Connection),
    Ws(SplitSink<Ws, Message>),
}
impl Writer {
    async fn event(&mut self, codec: Codec, e: Event) -> io::Result<()> {
        let bytes = codec.encode(&e.envelope()).map_err(io::Error::other)?;
        timeout(HANDSHAKE, async {
            match self {
                Self::Quic(w, _) => w.send(Bytes::from(bytes)).await,
                Self::Ws(w) => {
                    let msg = match codec {
                        Codec::Binary => Message::Binary(bytes.into()),
                        Codec::Json => Message::Text(
                            String::from_utf8(bytes).map_err(io::Error::other)?.into(),
                        ),
                    };
                    w.send(msg).await.map_err(io::Error::other)
                }
            }
        })
        .await?
    }
    async fn hint(&mut self, codec: Codec, reply: Event, datagram: bool) -> io::Result<()> {
        let mut reply = reply;
        let encoded = |e: &Event| codec.encode(&e.envelope()).map_err(io::Error::other);
        let mut bytes = encoded(&reply);
        if bytes.as_ref().map_or(true, |b| b.len() > MAX_HINT) {
            reply = failure(
                HyveError::invalid_request("hint reply too large"),
                0,
                reply.request_id,
            );
            bytes = encoded(&reply);
        }
        if datagram && let Self::Quic(_, connection) = self {
            let bytes = bytes?;
            if connection
                .max_datagram_size()
                .is_some_and(|limit| bytes.len() <= limit)
            {
                return connection.send_datagram(bytes).map_err(io::Error::other);
            }
        }
        self.event(codec, reply).await
    }
    async fn close(&mut self, code: CloseCode, reason: &'static str) {
        if let Self::Quic(w, _) = self {
            let _ = timeout(HANDSHAKE, w.get_mut().finish()).await;
        }
        if let Self::Ws(w) = self {
            let _ = timeout(
                HANDSHAKE,
                w.send(Message::Close(Some(CloseFrame {
                    code,
                    reason: reason.into(),
                }))),
            )
            .await;
        }
    }
}

fn failure(error: HyveError, seq: i64, request_id: Option<String>) -> Event {
    let error = error.to_public();
    let mut body = Value::map();
    body.insert("code", error.code());
    body.insert("message", error.message());
    body.insert("retryable", error.is_retryable());
    Event::new("error", seq, body, request_id)
}
async fn deliver(
    rooms: &Rooms,
    grant: &Grant,
    writer: &mut Writer,
    codec: Codec,
    from: i64,
) -> io::Result<(i64, i64)> {
    let mut seq = from;
    let (events, head) = rooms.replay(grant, from).await.map_err(io::Error::other)?;
    for event in events {
        seq = event.seq;
        writer.event(codec, event).await?;
    }
    Ok((seq, head))
}

async fn session(
    rooms: Rooms,
    grant: Grant,
    codec: Codec,
    mut reader: Reader,
    mut writer: Writer,
) -> io::Result<()> {
    let mut hello = Value::map();
    hello.insert("version", 1);
    hello.insert("hints", true);
    hello.insert("singleStream", matches!(writer, Writer::Quic(_, _)));
    writer
        .event(codec, Event::new("hello", 0, hello, None))
        .await?;
    let mut cursor = None;
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last = Instant::now();
    loop {
        let msg = tokio::select! {
            msg=reader.next()=>match msg? {Some(m)=>Some(m),None=>break},
            _=tick.tick()=>None,
        };
        if let Err(e) = rooms.authorize(&grant).await {
            writer
                .event(codec, failure(e, cursor.unwrap_or(0), None))
                .await?;
            writer.close(CloseCode::Policy, "session unavailable").await;
            break;
        }
        let Some(msg) = msg else {
            if last.elapsed() > IDLE {
                writer.close(CloseCode::Away, "idle timeout").await;
                break;
            }
            if let Some(seq) = cursor {
                cursor = Some(deliver(&rooms, &grant, &mut writer, codec, seq).await?.0);
            }
            continue;
        };
        last = Instant::now();
        let datagram = matches!(&msg, IncomingFrame::Datagram(_));
        let single = matches!(&msg, IncomingFrame::Single(_));
        let msg = match msg {
            IncomingFrame::Control(msg) => msg,
            IncomingFrame::Datagram(bytes) | IncomingFrame::Single(bytes) => Message::Binary(bytes),
        };
        let decoded = match (&msg, codec) {
            (Message::Binary(bytes), Codec::Binary) => codec.decode(bytes),
            (Message::Text(text), Codec::Json) => codec.decode(text.as_bytes()),
            (Message::Close(_), _) => {
                writer.close(CloseCode::Normal, "room closed").await;
                break;
            }
            (Message::Ping(bytes), _) => {
                if let Writer::Ws(w) = &mut writer {
                    w.send(Message::Pong(bytes.clone()))
                        .await
                        .map_err(io::Error::other)?;
                }
                continue;
            }
            (Message::Pong(_), _) => continue,
            _ => Err(HyveError::invalid_request("unexpected room frame type")),
        };
        let cmd = match decoded.and_then(|v| Command::parse(&v)) {
            Ok(c) => c,
            Err(e) => {
                writer
                    .event(codec, failure(e, cursor.unwrap_or(0), None))
                    .await?;
                writer
                    .close(CloseCode::Protocol, "invalid room message")
                    .await;
                break;
            }
        };
        if datagram || cmd.kind == "room.hint" {
            let byte_len = match &msg {
                Message::Binary(b) => b.len(),
                Message::Text(t) => t.len(),
                _ => 0,
            };
            let outcome =
                if cursor.is_none() || cmd.kind != "room.hint" || single || byte_len > MAX_HINT {
                    Err(HyveError::invalid_request(
                        "expected a bounded hint in a resumed room",
                    ))
                } else {
                    rooms.hint(&grant, &cmd.payload)
                };
            let result = match outcome {
                Ok(value) => Event::new("room.hinted", 0, value, Some(cmd.id)),
                Err(e) => failure(e, 0, Some(cmd.id)),
            };
            writer.hint(codec, result, datagram).await?;
            continue;
        }
        let seq = cursor.unwrap_or(0);
        let id = cmd.id.clone();
        let reply: HyveResult<Event> = match cmd.kind.as_str() {
            _ if single && cmd.kind.starts_with("room.") => Err(HyveError::invalid_request(
                "send room controls on the primary stream",
            )),
            "room.resume" => match sequence(&cmd.payload, "lastSeq") {
                Ok(from) => {
                    let (mut position, head) =
                        deliver(&rooms, &grant, &mut writer, codec, from).await?;
                    // Drain to the head observed at resume, even if producers
                    // continue appending. Keep only one replay page in memory.
                    while position < head {
                        rooms.authorize(&grant).await.map_err(io::Error::other)?;
                        position = deliver(&rooms, &grant, &mut writer, codec, position)
                            .await?
                            .0;
                    }
                    cursor = Some(position);
                    Ok(Event::new(
                        "room.ready",
                        position,
                        Value::Null,
                        Some(id.clone()),
                    ))
                }
                Err(e) => Err(e),
            },
            "room.ping" => Ok(Event::new("room.pong", seq, Value::Null, Some(id.clone()))),
            _ if cursor.is_none() => Err(HyveError::invalid_request(
                "resume the room before issuing commands",
            )),
            _ if cmd.kind.starts_with("room.") => {
                Err(HyveError::invalid_request("unknown room control"))
            }
            _ => rooms.apply(&grant, cmd).await,
        };
        writer
            .event(codec, reply.unwrap_or_else(|e| failure(e, seq, Some(id))))
            .await?;
    }
    Ok(())
}
