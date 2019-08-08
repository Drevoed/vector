use crate::{
    buffers::Acker,
    event::{self, Event},
    sinks::util::SinkExt,
    topology::config::{DataType, SinkConfig},
};
use bytes::Bytes;
use futures::{future, try_ready, Async, AsyncSink, Future, Poll, Sink, StartSend};
use serde::{Deserialize, Serialize};
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};
use tokio::{
    codec::{BytesCodec, FramedWrite},
    net::tcp::{ConnectFuture, TcpStream},
    prelude::AsyncWrite,
    timer::Delay,
};
use tokio_retry::strategy::ExponentialBackoff;
use tokio_tls::{Connect as TlsConnect, TlsConnector};
use tracing::field;

#[derive(Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct TcpSinkConfig {
    pub address: String,
    pub encoding: Option<Encoding>,
    pub tls: Option<TcpSinkTlsConfig>,
}

#[derive(Deserialize, Serialize, Debug, Eq, PartialEq, Clone)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Text,
    Json,
}

#[derive(Deserialize, Serialize, Debug, Default, Eq, PartialEq, Clone)]
#[serde(rename_all = "snake_case")]
pub struct TcpSinkTlsConfig {
    pub enabled: Option<bool>,
    pub verify: Option<bool>,
}

impl TcpSinkConfig {
    pub fn new(address: String) -> Self {
        Self {
            address,
            encoding: None,
            tls: None,
        }
    }
}

#[typetag::serde(name = "tcp")]
impl SinkConfig for TcpSinkConfig {
    fn build(&self, acker: Acker) -> Result<(super::RouterSink, super::Healthcheck), String> {
        let addr = self
            .address
            .to_socket_addrs()
            .map_err(|e| format!("IO Error: {}", e))?
            .next()
            .ok_or_else(|| "Unable to resolve DNS for provided address".to_string())?;

        let tls = match self.tls {
            Some(ref tls) => TcpSinkTls {
                enabled: tls.enabled.unwrap_or(false),
                verify: tls.verify.unwrap_or(true),
            },
            None => TcpSinkTls::default(),
        };

        let sink = raw_tcp(
            self.address.clone(),
            addr,
            acker,
            self.encoding.clone(),
            tls,
        );
        let healthcheck = tcp_healthcheck(addr);

        Ok((sink, healthcheck))
    }

    fn input_type(&self) -> DataType {
        DataType::Log
    }
}

pub struct TcpSink {
    hostname: String,
    addr: SocketAddr,
    tls: TcpSinkTls,
    state: TcpSinkState,
    backoff: ExponentialBackoff,
}

enum TcpSinkState {
    Disconnected,
    Connecting(ConnectFuture),
    TlsConnecting(TlsConnect<TcpStream>),
    Connected(Box<FramedConnection + Send>),
    Backoff(Delay),
}

#[derive(Default)]
pub struct TcpSinkTls {
    enabled: bool,
    verify: bool,
}

impl TcpSink {
    pub fn new(hostname: String, addr: SocketAddr, tls: TcpSinkTls) -> Self {
        Self {
            hostname,
            addr,
            tls,
            state: TcpSinkState::Disconnected,
            backoff: Self::fresh_backoff(),
        }
    }

    fn fresh_backoff() -> ExponentialBackoff {
        // TODO: make configurable
        ExponentialBackoff::from_millis(2)
            .factor(250)
            .max_delay(Duration::from_secs(60))
    }

    fn poll_connection(&mut self) -> Poll<&mut Box<FramedConnection + Send>, ()> {
        loop {
            self.state = match self.state {
                TcpSinkState::Disconnected => {
                    debug!(message = "connecting", addr = &field::display(&self.addr));
                    TcpSinkState::Connecting(TcpStream::connect(&self.addr))
                }
                TcpSinkState::Backoff(ref mut delay) => match delay.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    // Err can only occur if the tokio runtime has been shutdown or if more than 2^63 timers have been created
                    Err(err) => unreachable!(err),
                    Ok(Async::Ready(())) => {
                        debug!(
                            message = "disconnected.",
                            addr = &field::display(&self.addr)
                        );
                        TcpSinkState::Disconnected
                    }
                },
                TcpSinkState::Connecting(ref mut connect_future) => match connect_future.poll() {
                    Ok(Async::Ready(socket)) => {
                        let addr = socket.peer_addr().unwrap_or(self.addr);
                        debug!(message = "connected", addr = &field::display(&addr));
                        self.backoff = Self::fresh_backoff();
                        match self.tls.enabled {
                            true => {
                                let c = native_tls::TlsConnector::builder()
                                    .danger_accept_invalid_certs(!self.tls.verify)
                                    .build()
                                    .expect("Could not build TLS connector?!?");
                                TcpSinkState::TlsConnecting(
                                    TlsConnector::from(c).connect(&self.hostname, socket),
                                )
                            }
                            false => TcpSinkState::Connected(Box::new(FramedWrite::new(
                                socket,
                                BytesCodec::new(),
                            ))),
                        }
                    }
                    Ok(Async::NotReady) => {
                        return Ok(Async::NotReady);
                    }
                    Err(err) => {
                        error!("Error connecting to {}: {}", self.addr, err);
                        let delay = Delay::new(Instant::now() + self.backoff.next().unwrap());
                        TcpSinkState::Backoff(delay)
                    }
                },
                TcpSinkState::TlsConnecting(ref mut connect_future) => {
                    match connect_future.poll() {
                        Ok(Async::Ready(socket)) => {
                            debug!(message = "negotiated TLS");
                            self.backoff = Self::fresh_backoff();
                            TcpSinkState::Connected(Box::new(FramedWrite::new(
                                socket,
                                BytesCodec::new(),
                            )))
                        }
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Err(err) => {
                            error!("Error negotiating TLS with {}: {}", self.addr, err);
                            let delay = Delay::new(Instant::now() + self.backoff.next().unwrap());
                            TcpSinkState::Backoff(delay)
                        }
                    }
                }
                TcpSinkState::Connected(ref mut connection) => {
                    return Ok(Async::Ready(connection));
                }
            };
        }
    }
}

impl Sink for TcpSink {
    type SinkItem = Bytes;
    type SinkError = ();

    fn start_send(&mut self, line: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        match self.poll_connection() {
            Ok(Async::Ready(connection)) => {
                debug!(
                    message = "sending event.",
                    bytes = &field::display(line.len())
                );
                match connection.start(line) {
                    Err(err) => {
                        debug!(
                            message = "disconnected.",
                            addr = &field::display(&self.addr)
                        );
                        error!("Error in connection {}: {}", self.addr, err);
                        self.state = TcpSinkState::Disconnected;
                        Ok(AsyncSink::Ready)
                    }
                    Ok(ok) => Ok(ok),
                }
            }
            Ok(Async::NotReady) => Ok(AsyncSink::NotReady(line)),
            Err(_) => unreachable!(),
        }
    }

    fn poll_complete(&mut self) -> Result<Async<()>, Self::SinkError> {
        // Stream::forward will immediately poll_complete the sink it's forwarding to,
        // but we don't want to connect before the first event actually comes through.
        if let TcpSinkState::Disconnected = self.state {
            return Ok(Async::Ready(()));
        }

        let connection = try_ready!(self.poll_connection());

        match connection.poll() {
            Err(err) => {
                debug!(
                    message = "disconnected.",
                    addr = &field::display(&self.addr)
                );
                error!("Error in connection {}: {}", self.addr, err);
                self.state = TcpSinkState::Disconnected;
                Ok(Async::Ready(()))
            }
            Ok(ok) => Ok(ok),
        }
    }
}

pub fn raw_tcp(
    hostname: String,
    addr: SocketAddr,
    acker: Acker,
    encoding: Option<Encoding>,
    tls: TcpSinkTls,
) -> super::RouterSink {
    Box::new(
        TcpSink::new(hostname, addr, tls)
            .stream_ack(acker)
            .with(move |event| encode_event(event, &encoding)),
    )
}

pub fn tcp_healthcheck(addr: SocketAddr) -> super::Healthcheck {
    // Lazy to avoid immediately connecting
    let check = future::lazy(move || {
        TcpStream::connect(&addr)
            .map(|_| ())
            .map_err(|err| err.to_string())
    });

    Box::new(check)
}

fn encode_event(event: Event, encoding: &Option<Encoding>) -> Result<Bytes, ()> {
    let log = event.into_log();

    let b = match (encoding, log.is_structured()) {
        (&Some(Encoding::Json), _) | (_, true) => {
            serde_json::to_vec(&log.unflatten()).map_err(|e| panic!("Error encoding: {}", e))
        }
        (&Some(Encoding::Text), _) | (_, false) => {
            let bytes = log
                .get(&event::MESSAGE)
                .map(|v| v.as_bytes().to_vec())
                .unwrap_or(Vec::new());
            Ok(bytes)
        }
    };

    b.map(|mut b| {
        b.push(b'\n');
        Bytes::from(b)
    })
}

trait FramedConnection {
    fn start(&mut self, line: Bytes) -> std::io::Result<AsyncSink<Bytes>>;
    fn poll(&mut self) -> std::io::Result<Async<()>>;
}

impl<T: AsyncWrite> FramedConnection for FramedWrite<T, BytesCodec> {
    fn start(&mut self, line: Bytes) -> std::io::Result<AsyncSink<Bytes>> {
        FramedWrite::<T, BytesCodec>::start_send(self, line)
    }
    fn poll(&mut self) -> std::io::Result<Async<()>> {
        FramedWrite::<T, BytesCodec>::poll_complete(self)
    }
}
