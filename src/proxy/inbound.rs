// Copyright Istio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::Error;
use crate::baggage::parse_baggage_header;
use crate::config::Config;
use crate::identity::SecretManager;
use crate::metrics::traffic::{ConnectionOpen, Reporter};
use crate::metrics::{traffic, Metrics, Recorder};
use crate::proxy;
use crate::proxy::inbound::InboundConnect::{DirectPath, Hbone};
use crate::proxy::{ProxyInputs, TraceParent, BAGGAGE_HEADER, TRACEPARENT_HEADER};
use crate::rbac::Connection;
use crate::socket::to_canonical;
use crate::state::workload::{address, gatewayaddress, GatewayAddress, NetworkAddress, Workload};
use crate::state::DemandProxyState;
use crate::tls::TlsError;
use bytes::Bytes;
use drain::Watch;
use futures::stream::StreamExt;
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use std::fmt;
use std::fmt::{Display, Formatter};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, instrument, trace, trace_span, warn, Instrument};

pub(super) struct Inbound {
    cfg: Config,
    listener: TcpListener,
    cert_manager: Arc<SecretManager>,
    state: DemandProxyState,
    drain: Watch,
    metrics: Arc<Metrics>,
}

impl Inbound {
    pub(super) async fn new(mut pi: ProxyInputs, drain: Watch) -> Result<Inbound, Error> {
        let listener: TcpListener = TcpListener::bind(pi.cfg.inbound_addr)
            .await
            .map_err(|e| Error::Bind(pi.cfg.inbound_addr, e))?;
        let transparent = super::maybe_set_transparent(&pi, &listener)?;
        // Override with our explicitly configured setting
        pi.cfg.enable_original_source = Some(transparent);
        info!(
            address=%listener.local_addr().unwrap(),
            component="inbound",
            transparent,
            "listener established",
        );
        Ok(Inbound {
            cfg: pi.cfg,
            state: pi.state,
            listener,
            cert_manager: pi.cert_manager,
            metrics: pi.metrics,
            drain,
        })
    }

    pub(super) fn address(&self) -> SocketAddr {
        self.listener.local_addr().unwrap()
    }

    pub(super) async fn run(self) {
        // let (tx, rx) = oneshot::channel();
        let acceptor = InboundCertProvider {
            state: self.state.clone(),
            cert_manager: self.cert_manager.clone(),
            network: self.cfg.network.clone(),
        };
        let drain_stream = self.drain.clone();
        let stream = crate::hyper_util::tls_server(acceptor, self.listener);
        let mut stream = stream.take_until(Box::pin(drain_stream.signaled()));
        while let Some(socket) = stream.next().await {
            let state = self.state.clone();
            let metrics = self.metrics.clone();
            let drain = self.drain.clone();
            let network = self.cfg.network.clone();
            tokio::task::spawn(async move {
                let dst = crate::socket::orig_dst_addr_or_default(socket.get_ref());
                let conn = Connection {
                    src_identity: socket
                        .ssl()
                        .peer_certificate()
                        .and_then(|x| crate::tls::boring::extract_sans(&x).first().cloned()),
                    src_ip: to_canonical(socket.get_ref().peer_addr().unwrap()).ip(),
                    dst_network: network, // inbound request must be on our network
                    dst,
                };
                debug!(%conn, "accepted connection");
                let enable_original_source = self.cfg.enable_original_source;
                let serve = crate::hyper_util::http2_server()
                    .initial_stream_window_size(self.cfg.window_size)
                    .initial_connection_window_size(self.cfg.connection_window_size)
                    .max_frame_size(self.cfg.frame_size)
                    .serve_connection(
                        socket,
                        service_fn(move |req| {
                            Self::serve_connect(
                                state.clone(),
                                conn.clone(),
                                enable_original_source.unwrap_or_default(),
                                req,
                                metrics.clone(),
                            )
                        }),
                    );
                // Wait for drain to signal or connection serving to complete
                match futures_util::future::select(Box::pin(drain.signaled()), serve).await {
                    // We got a shutdown request. Start gracful shutdown and wait for the pending requests to complete.
                    futures_util::future::Either::Left((_shutdown, mut server)) => {
                        let drain = std::pin::Pin::new(&mut server);
                        drain.graceful_shutdown();
                        server.await
                    }
                    // Serving finished, just return the result.
                    futures_util::future::Either::Right((server, _shutdown)) => server,
                }
            });
        }
        info!("all inbound connections drained");
    }

    /// handle_inbound serves an inbound connection with a target address `addr`.
    pub(super) async fn handle_inbound(
        request_type: InboundConnect,
        orig_src: Option<IpAddr>,
        addr: SocketAddr,
        metrics: Arc<Metrics>,
        connection_metrics: ConnectionOpen,
        extra_connection_metrics: Option<ConnectionOpen>,
    ) -> Result<(), std::io::Error> {
        let start = Instant::now();
        let stream = super::freebind_connect(orig_src, addr).await;
        match stream {
            Err(err) => {
                warn!(dur=?start.elapsed(), "connection to {} failed: {}", addr, err);
                Err(err)
            }
            Ok(stream) => {
                let mut stream = stream;
                stream.set_nodelay(true)?;
                trace!(dur=?start.elapsed(), "connected to: {addr}");
                tokio::task::spawn(
                    (async move {
                        let _connection_close = metrics
                            .increment_defer::<_, traffic::ConnectionClose>(&connection_metrics);

                        let _extra_conn_close = extra_connection_metrics
                            .as_ref()
                            .map(|co| metrics.increment_defer::<_, traffic::ConnectionClose>(co));

                        let transferred_bytes =
                            traffic::BytesTransferred::from(&connection_metrics);
                        match request_type {
                            DirectPath(mut incoming) => {
                                match proxy::relay(
                                    &mut incoming,
                                    &mut stream,
                                    &metrics,
                                    transferred_bytes,
                                )
                                .await
                                {
                                    Ok(transferred) => {
                                        if let Some(co) = extra_connection_metrics.as_ref() {
                                            metrics.record(
                                                &traffic::BytesTransferred::from(co),
                                                transferred,
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        error!(dur=?start.elapsed(), "internal server copy: {}", e)
                                    }
                                }
                            }
                            Hbone(req) => match hyper::upgrade::on(req).await {
                                Ok(mut upgraded) => {
                                    if let Err(e) = super::copy_hbone(
                                        &mut upgraded,
                                        &mut stream,
                                        &metrics,
                                        transferred_bytes,
                                    )
                                    .instrument(trace_span!("hbone server"))
                                    .await
                                    {
                                        error!(dur=?start.elapsed(), "hbone server copy: {}", e);
                                    }
                                }
                                Err(e) => {
                                    // Not sure if this can even happen
                                    error!(dur=?start.elapsed(), "No upgrade {e}");
                                }
                            },
                        }
                    })
                    .in_current_span(),
                );
                // Send back our 200. We do this regardless of if our spawned task copies the data;
                // we need to respond with headers immediately once connection is established for the
                // stream of bytes to begin.
                Ok(())
            }
        }
    }

    fn extract_traceparent(req: &Request<Incoming>) -> TraceParent {
        req.headers()
            .get(TRACEPARENT_HEADER)
            .and_then(|b| b.to_str().ok())
            .and_then(|b| TraceParent::try_from(b).ok())
            .unwrap_or_else(TraceParent::new)
    }

    #[instrument(name="inbound", skip_all, fields(
        id=%Self::extract_traceparent(&req),
        peer_ip=%conn.src_ip,
        peer_id=%OptionDisplay(&conn.src_identity)
    ))]
    async fn serve_connect(
        state: DemandProxyState,
        conn: Connection,
        enable_original_source: bool,
        req: Request<Incoming>,
        metrics: Arc<Metrics>,
    ) -> Result<Response<Empty<Bytes>>, hyper::Error> {
        match req.method() {
            &Method::CONNECT => {
                let uri = req.uri();
                info!("got {} request to {}", req.method(), uri);
                let addr: Result<SocketAddr, _> = uri.to_string().as_str().parse();
                if addr.is_err() {
                    info!("Sending 400, {:?}", addr.err());
                    return Ok(Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Empty::new())
                        .unwrap());
                }

                let addr: SocketAddr = addr.unwrap();
                if addr.ip() != conn.dst.ip() {
                    info!("Sending 400, ip mismatch {addr} != {}", conn.dst);
                    return Ok(Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Empty::new())
                        .unwrap());
                }
                // Orig has 15008, swap with the real port
                let conn = Connection { dst: addr, ..conn };
                let dst_network_addr = NetworkAddress {
                    network: conn.dst_network.to_string(), // dst must be on our network
                    address: addr.ip(),
                };
                let Some(upstream) = state.fetch_workload(&dst_network_addr).await else {
                    info!(%conn, "unknown destination");
                    return Ok(Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Empty::new())
                        .unwrap());
                };
                let has_waypoint = upstream.waypoint.is_some();
                let from_waypoint = Self::check_waypoint(state.clone(), &upstream, &conn)
                    .await
                    .unwrap();
                let from_gateway = Self::check_gateway(state.clone(), &upstream, &conn)
                    .await
                    .unwrap();

                if from_gateway {
                    debug!("request from gateway");
                }
                if from_waypoint {
                    debug!("request from waypoint, skipping policy");
                } else if !state.assert_rbac(&conn).await {
                    info!(%conn, "RBAC rejected");
                    return Ok(Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .body(Empty::new())
                        .unwrap());
                }
                if has_waypoint && !from_waypoint {
                    info!(%conn, "bypassed waypoint");
                    return Ok(Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .body(Empty::new())
                        .unwrap());
                }
                let source_ip = if from_waypoint {
                    // If the request is from our waypoint, trust the Forwarded header.
                    // For other request types, we can only trust the source from the connection.
                    // Since our own waypoint is in the same trust domain though, we can use Forwarded,
                    // which drops the requirement of spoofing IPs from waypoints
                    super::get_original_src_from_fwded(&req).unwrap_or(conn.src_ip)
                } else {
                    conn.src_ip
                };

                let baggage =
                    parse_baggage_header(req.headers().get_all(BAGGAGE_HEADER)).unwrap_or_default();

                let source = match from_gateway {
                    true => None, // we cannot lookup source workload since we don't know the network, see https://github.com/istio/ztunnel/issues/515
                    false => {
                        let src_network_addr = NetworkAddress {
                            // we can assume source network is our network because we did not traverse a gateway
                            network: conn.dst_network.to_string(),
                            address: source_ip,
                        };
                        // Find source info. We can lookup by XDS or from connection attributes
                        state.fetch_workload(&src_network_addr).await
                    }
                };

                let derived_source = traffic::DerivedWorkload {
                    identity: conn.src_identity,
                    cluster_id: baggage.cluster_id,
                    namespace: baggage.namespace,
                    workload_name: baggage.workload_name,
                    revision: baggage.revision,
                    ..Default::default()
                };
                let connection_metrics = ConnectionOpen {
                    reporter: Reporter::destination,
                    source,
                    derived_source: Some(derived_source),
                    destination: Some(upstream),
                    connection_security_policy: traffic::SecurityPolicy::mutual_tls,
                    destination_service: None,
                    destination_service_namespace: None,
                    destination_service_name: None,
                };
                let status_code = match Self::handle_inbound(
                    Hbone(req),
                    enable_original_source.then_some(source_ip),
                    addr,
                    metrics,
                    connection_metrics,
                    None,
                )
                .in_current_span()
                .await
                {
                    Ok(_) => StatusCode::OK,
                    Err(_) => StatusCode::SERVICE_UNAVAILABLE,
                };

                Ok(Response::builder()
                    .status(status_code)
                    .body(Empty::new())
                    .unwrap())
            }
            // Return the 404 Not Found for other routes.
            method => {
                info!("Sending 404, got {method}");
                Ok(Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Empty::new())
                    .unwrap())
            }
        }
    }

    async fn check_waypoint(
        state: DemandProxyState,
        upstream: &Workload,
        conn: &Connection,
    ) -> Result<bool, Error> {
        Self::check_gateway_address(state, conn, upstream.waypoint.as_ref()).await
    }

    async fn check_gateway(
        state: DemandProxyState,
        upstream: &Workload,
        conn: &Connection,
    ) -> Result<bool, Error> {
        Self::check_gateway_address(state, conn, upstream.network_gateway.as_ref()).await
    }

    async fn check_gateway_address(
        state: DemandProxyState,
        conn: &Connection,
        gateway_address: Option<&GatewayAddress>,
    ) -> Result<bool, Error> {
        let gateway_nw_addr = match gateway_address.as_ref() {
            Some(addr) => match &addr.destination {
                gatewayaddress::Destination::Address(gateway_ip) => Ok(gateway_ip),
                gatewayaddress::Destination::Hostname(_) => Err(Error::UnsupportedFeature(
                    "hostname lookup not supported yet".to_string(),
                )),
            },
            None => return Ok(false),
        }?;
        let from_gateway = match state.fetch_address(gateway_nw_addr).await {
            Some(address::Address::Workload(wl)) => Some(wl.identity()) == conn.src_identity,
            Some(address::Address::Service(svc)) => {
                for (ip, _ep) in svc.endpoints.iter() {
                    if state.fetch_workload(ip).await.map(|w| w.identity()) == conn.src_identity {
                        return Ok(true);
                    }
                }
                false
            }
            None => false,
        };
        Ok(from_gateway)
    }
}

struct OptionDisplay<'a, T>(&'a Option<T>);

impl<'a, T: Display> Display for OptionDisplay<'a, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.0 {
            None => write!(f, "None"),
            Some(i) => write!(f, "{i}"),
        }
    }
}

pub(super) enum InboundConnect {
    /// DirectPath is an optimization when we are connecting to an endpoint on the same node.
    /// Rather than doing a full HBONE connection over the localhost network, we just pass the outbound
    /// context directly to the inbound handling in memory.
    DirectPath(TcpStream),
    /// Hbone is a standard HBONE request coming from the network.
    Hbone(Request<Incoming>),
}

#[derive(Clone)]
struct InboundCertProvider {
    cert_manager: Arc<SecretManager>,
    state: DemandProxyState,
    network: String,
}

#[async_trait::async_trait]
impl crate::tls::CertProvider for InboundCertProvider {
    async fn fetch_cert(&mut self, fd: &TcpStream) -> Result<boring::ssl::SslAcceptor, TlsError> {
        let orig_dst_addr = crate::socket::orig_dst_addr_or_default(fd);
        let identity = {
            let wip = NetworkAddress {
                network: self.network.clone(), // inbound cert provider gets cert for the dest, which must be on our network
                address: orig_dst_addr.ip(),
            };
            self.state
                .fetch_workload(&wip)
                .await
                .ok_or(TlsError::CertificateLookup(wip))?
                .identity()
        };
        debug!(
            destination=?orig_dst_addr,
            %identity,
            "fetching cert"
        );
        let cert = self.cert_manager.fetch_certificate(&identity).await?;
        let acc = cert.mtls_acceptor(Some(&identity))?;
        Ok(acc)
    }
}
