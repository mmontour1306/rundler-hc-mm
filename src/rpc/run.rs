use std::{net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use anyhow::{bail, Context};
use ethers::{
    providers::{Http, Provider, ProviderExt},
    types::{Address, Chain},
};
use jsonrpsee::{
    server::{middleware::proxy_get_request::ProxyGetRequestLayer, ServerBuilder},
    RpcModule,
};
use tokio::sync::{broadcast, mpsc};
use tonic::transport::{Channel, Uri};
use tonic_health::proto::health_client::HealthClient;

use super::ApiNamespace;
use crate::{
    common::{
        protos::{builder::builder_client, op_pool::op_pool_client},
        server::format_socket_addr,
        simulation,
    },
    rpc::{
        debug::{DebugApi, DebugApiServer},
        eth::{EthApi, EthApiServer},
        health::{SystemApi, SystemApiServer},
        metrics::RpcMetricsLogger,
    },
};

pub struct Args {
    pub port: u16,
    pub host: String,
    pub pool_url: String,
    pub builder_url: String,
    pub entry_points: Vec<Address>,
    pub chain_id: u64,
    pub api_namespaces: Vec<ApiNamespace>,
    pub rpc_url: String,
    pub sim_settings: simulation::Settings,
}

pub async fn run(
    args: Args,
    mut shutdown_rx: broadcast::Receiver<()>,
    _shutdown_scope: mpsc::Sender<()>,
) -> anyhow::Result<()> {
    let addr: SocketAddr = format_socket_addr(&args.host, args.port).parse()?;
    tracing::info!("Starting server on {}", addr);

    let mut module = RpcModule::new(());
    let chain: Chain = args
        .chain_id
        .try_into()
        .with_context(|| format!("{} is not a supported chain", args.chain_id))?;

    let provider: Arc<Provider<Http>> = Arc::new(
        Provider::<Http>::try_from(args.rpc_url)
            .context("Invalid RPC URL")?
            // TODO: revisit a safe default for production
            .interval(Duration::from_millis(100))
            .for_chain(chain),
    );

    let op_pool_uri = Uri::from_str(&args.pool_url).context("should be a valid URI for op_pool")?;
    let op_pool_client = op_pool_client::OpPoolClient::connect(args.pool_url)
        .await
        .context("should have been able to connect to op pool")?;
    let op_pool_health_client = HealthClient::new(
        Channel::builder(op_pool_uri)
            .connect()
            .await
            .context("should have connected to op_pool health service channel")?,
    );

    let builder_uri =
        Uri::from_str(&args.builder_url).context("should be a valid URI for op_pool")?;
    let builder_client = builder_client::BuilderClient::connect(args.builder_url)
        .await
        .context("builder server should be started")?;
    let builder_health_client = HealthClient::new(
        Channel::builder(builder_uri)
            .connect()
            .await
            .context("should have connected to builder health service channel")?,
    );

    if args.entry_points.len() != 1 {
        bail!("Only one entry point is supported at the moment");
    }

    for api in args.api_namespaces {
        match api {
            ApiNamespace::Eth => module.merge(
                EthApi::new(
                    provider.clone(),
                    args.entry_points.clone(),
                    args.chain_id,
                    // NOTE: this clone is cheap according to the docs because all it's doing is copying the reference to the channel
                    op_pool_client.clone(),
                    args.sim_settings,
                )
                .into_rpc(),
            )?,
            ApiNamespace::Debug => module
                .merge(DebugApi::new(op_pool_client.clone(), builder_client.clone()).into_rpc())?,
        }
    }

    // Set up health check endpoint via GET /health
    // registers the jsonrpc handler
    // NOTE: I couldn't use module.register_async_method because it requires async move
    // and neither the clients or the args.*_url are copyable
    module.merge(SystemApi::new(op_pool_health_client, builder_health_client).into_rpc())?;
    let service_builder = tower::ServiceBuilder::new()
        // Proxy `GET /health` requests to internal `system_health` method.
        .layer(ProxyGetRequestLayer::new("/health", "system_health")?)
        .timeout(Duration::from_secs(2));

    let server = ServerBuilder::default()
        .set_logger(RpcMetricsLogger)
        .set_middleware(service_builder)
        .http_only()
        .build(addr)
        .await?;
    let handle = server.start(module)?;

    tokio::select! {
        _ = handle.stopped() => {
            tracing::error!("Server stopped unexpectedly");
            bail!("RPC server stopped unexpectedly")
        }
        _ = shutdown_rx.recv() => {
            tracing::info!("Server shutdown");
            Ok(())
        }
    }
}
