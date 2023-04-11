//! Maintains a connection to the Relayer.
//!
//! The external Relayer is responsible for the following:
//! - Acts as a TPU proxy.
//! - Sends transactions to the validator.
//! - Does not bundles to avoid DOS vector.
//! - When validator connects, it changes its TPU and TPU forward address to the relayer.
//! - Expected to send heartbeat to validator as watchdog. If watchdog times out, the validator
//!   disconnects and reverts the TPU and TPU forward settings.

use {
    crate::{
        proto_packet_to_packet,
        proxy::{
            auth::{generate_auth_tokens, maybe_refresh_auth_tokens, AuthInterceptor},
            HeartbeatEvent, ProxyError,
        },
        sigverify::SigverifyTracerPacketStats,
    },
    crossbeam_channel::Sender,
    jito_protos::proto::{
        auth::{auth_service_client::AuthServiceClient, Token},
        relayer::{self, relayer_client::RelayerClient},
    },
    solana_gossip::cluster_info::ClusterInfo,
    solana_perf::packet::PacketBatch,
    solana_sdk::{
        saturating_add_assign,
        signature::{Keypair, Signer},
    },
    std::{
        cmp::min,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    tokio::time::{interval, sleep, timeout},
    tonic::{
        codegen::InterceptedService,
        transport::{Channel, Endpoint},
        Streaming,
    },
};

const CONNECTION_TIMEOUT_S: u64 = 10;

#[derive(Default)]
struct RelayerStageStats {
    num_empty_messages: u64,
    num_packets: u64,
    num_heartbeats: u64,
}

impl RelayerStageStats {
    pub(crate) fn report(&self) {
        datapoint_info!(
            "relayer_stage-stats",
            ("num_empty_messages", self.num_empty_messages, i64),
            ("num_packets", self.num_packets, i64),
            ("num_heartbeats", self.num_heartbeats, i64),
        );
    }
}

#[derive(Clone, Debug)]
pub struct RelayerConfig {
    /// Address to the external auth-service responsible for generating access tokens.
    pub auth_service_endpoint: Endpoint,

    /// Primary backend endpoint.
    pub backend_endpoint: Endpoint,

    /// Interval at which heartbeats are expected.
    pub expected_heartbeat_interval: Duration,

    /// The max tolerable age of the last heartbeat.
    pub oldest_allowed_heartbeat: Duration,

    /// If set then it will be assumed the backend verified packets so signature verification will be bypassed in the validator.
    pub trust_packets: bool,
}

pub struct RelayerStage {
    t_hdls: Vec<JoinHandle<()>>,
}

impl RelayerStage {
    pub fn new(
        relayer_config: RelayerConfig,
        // The keypair stored here is used to sign auth challenges.
        cluster_info: Arc<ClusterInfo>,
        // Channel that server-sent heartbeats are piped through.
        heartbeat_tx: Sender<HeartbeatEvent>,
        // Channel that non-trusted streamed packets are piped through.
        packet_tx: Sender<PacketBatch>,
        // Channel that trusted streamed packets are piped through.
        verified_packet_tx: Sender<(Vec<PacketBatch>, Option<SigverifyTracerPacketStats>)>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let thread = Builder::new()
            .name("relayer-stage".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                rt.block_on(Self::start(
                    relayer_config,
                    cluster_info,
                    heartbeat_tx,
                    packet_tx,
                    verified_packet_tx,
                    exit,
                ));
            })
            .unwrap();

        Self {
            t_hdls: vec![thread],
        }
    }

    pub fn join(self) -> thread::Result<()> {
        for t in self.t_hdls {
            t.join()?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn start(
        relayer_config: RelayerConfig,
        cluster_info: Arc<ClusterInfo>,
        heartbeat_tx: Sender<HeartbeatEvent>,
        packet_tx: Sender<PacketBatch>,
        verified_packet_tx: Sender<(Vec<PacketBatch>, Option<SigverifyTracerPacketStats>)>,
        exit: Arc<AtomicBool>,
    ) {
        const MAX_BACKOFF_S: u64 = 10;
        const CONNECTION_TIMEOUT: Duration = Duration::from_secs(CONNECTION_TIMEOUT_S);
        let mut backoff_sec: u64 = 1;
        let mut error_count: u64 = 0;

        while !exit.load(Ordering::Relaxed) {
            match Self::connect_auth_and_stream(
                &relayer_config,
                &cluster_info,
                &heartbeat_tx,
                &packet_tx,
                &verified_packet_tx,
                &exit,
                &CONNECTION_TIMEOUT,
            )
            .await
            {
                Ok(_) => {
                    backoff_sec = 0;
                }
                Err(e) => {
                    error!("relayer proxy error: {:?}", e);
                    error_count += 1;
                    datapoint_error!(
                        "relayer_stage-proxy_error",
                        ("count", error_count, i64),
                        ("error", e.to_string(), String),
                    );
                    backoff_sec = min(backoff_sec + 1, MAX_BACKOFF_S);
                    sleep(Duration::from_secs(backoff_sec)).await;
                }
            }
        }
    }

    async fn connect_auth_and_stream(
        relayer_config: &RelayerConfig,
        cluster_info: &Arc<ClusterInfo>,
        heartbeat_tx: &Sender<HeartbeatEvent>,
        packet_tx: &Sender<PacketBatch>,
        verified_packet_tx: &Sender<(Vec<PacketBatch>, Option<SigverifyTracerPacketStats>)>,
        exit: &Arc<AtomicBool>,
        connection_timeout: &Duration,
    ) -> crate::proxy::Result<()> {
        // Get Configs here in case they have changed at runtime
        let keypair = cluster_info.keypair().clone();

        debug!(
            "connecting to auth: {:?}",
            relayer_config.auth_service_endpoint.uri()
        );
        let auth_channel = timeout(
            *connection_timeout,
            relayer_config.auth_service_endpoint.connect(),
        )
        .await
        .map_err(|_| ProxyError::AuthenticationConnectionTimeout)?
        .map_err(|e| ProxyError::AuthenticationConnectionError(e.to_string()))?;

        let mut auth_client = AuthServiceClient::new(auth_channel);

        debug!("generating authentication token");
        let (access_token, mut refresh_token) = timeout(
            *connection_timeout,
            generate_auth_tokens(&mut auth_client, &keypair),
        )
        .await
        .map_err(|_| ProxyError::AuthenticationTimeout)??;

        debug!(
            "connecting to relayer: {:?}",
            relayer_config.backend_endpoint.uri()
        );
        let relayer_channel = timeout(
            *connection_timeout,
            relayer_config.backend_endpoint.connect(),
        )
        .await
        .map_err(|_| ProxyError::RelayerConnectionTimeout)?
        .map_err(|e| ProxyError::RelayerConnectionError(e.to_string()))?;

        let access_token = Arc::new(Mutex::new(access_token));
        let relayer_client = RelayerClient::with_interceptor(
            relayer_channel,
            AuthInterceptor::new(access_token.clone()),
        );

        Self::start_consuming_relayer_packets(
            relayer_client,
            heartbeat_tx,
            relayer_config.expected_heartbeat_interval,
            relayer_config.oldest_allowed_heartbeat,
            packet_tx,
            verified_packet_tx,
            relayer_config,
            &exit,
            auth_client,
            access_token,
            &mut refresh_token,
            keypair,
            cluster_info,
            connection_timeout,
        )
        .await
    }

    async fn start_consuming_relayer_packets(
        mut client: RelayerClient<InterceptedService<Channel, AuthInterceptor>>,
        heartbeat_tx: &Sender<HeartbeatEvent>,
        expected_heartbeat_interval: Duration,
        oldest_allowed_heartbeat: Duration,
        packet_tx: &Sender<PacketBatch>,
        verified_packet_tx: &Sender<(Vec<PacketBatch>, Option<SigverifyTracerPacketStats>)>,
        relayer_config: &RelayerConfig,
        exit: &Arc<AtomicBool>,
        mut auth_client: AuthServiceClient<Channel>,
        access_token: Arc<Mutex<Token>>,
        refresh_token: &mut Token,
        keypair: Arc<Keypair>,
        cluster_info: &Arc<ClusterInfo>,
        connection_timeout: &Duration,
    ) -> crate::proxy::Result<()> {
        let heartbeat_event: HeartbeatEvent = {
            // ToDo(JL) - Add Timeout here
            let tpu_config = client
                .get_tpu_configs(relayer::GetTpuConfigsRequest {})
                .await?
                .into_inner();
            let tpu_addr = tpu_config
                .tpu
                .ok_or_else(|| ProxyError::MissingTpuSocket("tpu".into()))?;
            let tpu_forward_addr = tpu_config
                .tpu_forward
                .ok_or_else(|| ProxyError::MissingTpuSocket("tpu_fwd".into()))?;

            let tpu_ip = IpAddr::from(tpu_addr.ip.parse::<Ipv4Addr>()?);
            let tpu_forward_ip = IpAddr::from(tpu_forward_addr.ip.parse::<Ipv4Addr>()?);

            let tpu_socket = SocketAddr::new(tpu_ip, tpu_addr.port as u16);
            let tpu_forward_socket = SocketAddr::new(tpu_forward_ip, tpu_forward_addr.port as u16);
            (tpu_socket, tpu_forward_socket)
        };

        // ToDo(JL) - Add Timeout here
        let packet_stream = client
            .subscribe_packets(relayer::SubscribePacketsRequest {})
            .await?
            .into_inner();

        Self::consume_packet_stream(
            heartbeat_event,
            heartbeat_tx,
            expected_heartbeat_interval,
            oldest_allowed_heartbeat,
            packet_stream,
            packet_tx,
            relayer_config,
            verified_packet_tx,
            exit,
            auth_client,
            access_token,
            refresh_token,
            keypair,
            cluster_info,
            connection_timeout,
        )
        .await
    }

    async fn consume_packet_stream(
        heartbeat_event: HeartbeatEvent,
        heartbeat_tx: &Sender<HeartbeatEvent>,
        expected_heartbeat_interval: Duration,
        oldest_allowed_heartbeat: Duration,
        mut packet_stream: Streaming<relayer::SubscribePacketsResponse>,
        packet_tx: &Sender<PacketBatch>,
        relayer_config: &RelayerConfig,
        verified_packet_tx: &Sender<(Vec<PacketBatch>, Option<SigverifyTracerPacketStats>)>,
        exit: &Arc<AtomicBool>,
        mut auth_client: AuthServiceClient<Channel>,
        access_token: Arc<Mutex<Token>>,
        refresh_token: &mut Token,
        keypair: Arc<Keypair>,
        cluster_info: &Arc<ClusterInfo>,
        connection_timeout: &Duration,
    ) -> crate::proxy::Result<()> {
        const METRICS_TICK: Duration = Duration::from_secs(1);
        const MAINTENANCE_TICK: Duration = Duration::from_secs(10 * 60);
        // Lookahead by Maintenance Tick plus 25%
        const AUTH_REFRESH_LOOKAHEAD: u64 = MAINTENANCE_TICK
            .as_secs()
            .saturating_mul(5)
            .saturating_div(4);

        let mut relayer_stats = RelayerStageStats::default();
        let mut metrics_tick = interval(METRICS_TICK);

        let mut num_full_refreshes: u64 = 0;
        let mut num_refresh_access_token: u64 = 0;
        let mut maintenance_tick = interval(MAINTENANCE_TICK);

        let mut heartbeat_check_interval = interval(expected_heartbeat_interval);
        let mut last_heartbeat_ts = Instant::now();

        let auth_uri_string = relayer_config.auth_service_endpoint.uri().to_string();

        info!("connected to packet stream");

        while !exit.load(Ordering::Relaxed) {
            tokio::select! {
                maybe_msg = packet_stream.message() => {
                    let resp = maybe_msg?.ok_or(ProxyError::GrpcStreamDisconnected)?;
                    Self::handle_relayer_packets(resp, heartbeat_event, heartbeat_tx, &mut last_heartbeat_ts, packet_tx, relayer_config.trust_packets, verified_packet_tx, &mut relayer_stats)?;
                }
                _ = heartbeat_check_interval.tick() => {
                    if last_heartbeat_ts.elapsed() > oldest_allowed_heartbeat {
                        return Err(ProxyError::HeartbeatExpired);
                    }
                }
                _ = metrics_tick.tick() => {
                    relayer_stats.report();
                    relayer_stats = RelayerStageStats::default();
                }
                _ = maintenance_tick.tick() => {
                    if cluster_info.id() != keypair.pubkey() {
                        return Err(ProxyError::AuthenticationConnectionError("Validator ID Changed".to_string()));
                    }

                    maybe_refresh_auth_tokens(&mut auth_client,
                        "relayer_stage-tokens_generated",
                        "relayer_stage-refresh_access_token",
                        &auth_uri_string,
                        &access_token,
                        refresh_token,
                        &cluster_info,
                        connection_timeout,
                        AUTH_REFRESH_LOOKAHEAD,
                        &mut num_full_refreshes,
                        &mut num_refresh_access_token)
                    .await?;
                }
            }
        }

        Ok(())
    }

    fn handle_relayer_packets(
        subscribe_packets_resp: relayer::SubscribePacketsResponse,
        heartbeat_event: HeartbeatEvent,
        heartbeat_tx: &Sender<HeartbeatEvent>,
        last_heartbeat_ts: &mut Instant,
        packet_tx: &Sender<PacketBatch>,
        trust_packets: bool,
        verified_packet_tx: &Sender<(Vec<PacketBatch>, Option<SigverifyTracerPacketStats>)>,
        relayer_stats: &mut RelayerStageStats,
    ) -> crate::proxy::Result<()> {
        match subscribe_packets_resp.msg {
            None => {
                saturating_add_assign!(relayer_stats.num_empty_messages, 1);
            }
            Some(relayer::subscribe_packets_response::Msg::Batch(proto_batch)) => {
                let packet_batch = PacketBatch::new(
                    proto_batch
                        .packets
                        .into_iter()
                        .map(proto_packet_to_packet)
                        .collect(),
                );

                saturating_add_assign!(relayer_stats.num_packets, packet_batch.len() as u64);

                if trust_packets {
                    verified_packet_tx
                        .send((vec![packet_batch], None))
                        .map_err(|_| ProxyError::PacketForwardError)?;
                } else {
                    packet_tx
                        .send(packet_batch)
                        .map_err(|_| ProxyError::PacketForwardError)?;
                }
            }
            Some(relayer::subscribe_packets_response::Msg::Heartbeat(_)) => {
                saturating_add_assign!(relayer_stats.num_heartbeats, 1);

                *last_heartbeat_ts = Instant::now();
                heartbeat_tx
                    .send(heartbeat_event)
                    .map_err(|_| ProxyError::HeartbeatChannelError)?;
            }
        }
        Ok(())
    }
}
