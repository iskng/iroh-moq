<directory_structure>
examples
├── 0rtt.rs
├── connect-unreliable.rs
├── connect.rs
├── dht_discovery.rs
├── echo-no-router.rs
├── echo.rs
├── listen-unreliable.rs
├── listen.rs
├── locally-discovered-nodes.rs
├── search.rs
└── transfer.rs

</directory_structure>

<file_info>
path: locally-discovered-nodes.rs
name: locally-discovered-nodes.rs
</file_info>
//! A small example showing how to get a list of nodes that were discovered via [`iroh::discovery::MdnsDiscovery`]. MdnsDiscovery uses [`swarm-discovery`](https://crates.io/crates/swarm-discovery), an opinionated implementation of mDNS to discover other nodes in the local network.
//!
//! This example creates an iroh endpoint, a few additional iroh endpoints to discover, waits a few seconds, and reports all of the iroh NodeIds (also called `[iroh::key::PublicKey]`s) it has discovered.
//!
//! This is an async, non-determinate process, so the number of NodeIDs discovered each time may be different. If you have other iroh endpoints or iroh nodes with [`MdnsDiscovery`] enabled, it may discover those nodes as well.
use std::time::Duration;

use iroh::{node_info::UserData, Endpoint, NodeId};
use n0_future::StreamExt;
use n0_snafu::Result;
use tokio::task::JoinSet;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    println!("Discovering Local Nodes Example!");

    let ep = Endpoint::builder().discovery_local_network().bind().await?;
    let node_id = ep.node_id();
    println!("Created endpoint {}", node_id.fmt_short());

    let user_data = UserData::try_from(String::from("local-nodes-example"))?;

    let mut discovery_stream = ep.discovery_stream();

    let ud = user_data.clone();
    let discovery_stream_task = tokio::spawn(async move {
        let mut discovered_nodes: Vec<NodeId> = vec![];
        while let Some(item) = discovery_stream.next().await {
            match item {
                Err(e) => {
                    tracing::error!("{e}");
                    return;
                }
                Ok(item) => {
                    // if there is no user data, or the user data
                    // does not indicate that the discovered node
                    // is a part of the example, ignore it
                    match item.node_info().data.user_data() {
                        Some(user_data) if &ud == user_data => {}
                        _ => {
                            tracing::error!("found node with unexpected user data, ignoring it");
                            continue;
                        }
                    }

                    // if we've already found this node, ignore it
                    // otherwise announce that we have found a new node
                    if discovered_nodes.contains(&item.node_id()) {
                        continue;
                    } else {
                        discovered_nodes.push(item.node_id());
                        println!("Found node {}!", item.node_id().fmt_short());
                    }
                }
            };
        }
    });

    let mut set = JoinSet::new();
    let node_count = 5;
    for _ in 0..node_count {
        let ud = user_data.clone();
        set.spawn(async move {
            let ep = Endpoint::builder().discovery_local_network().bind().await?;
            ep.set_user_data_for_discovery(Some(ud));
            tokio::time::sleep(Duration::from_secs(3)).await;
            ep.close().await;
            Ok::<_, n0_snafu::Error>(())
        });
    }

    set.join_all().await.iter().for_each(|res| {
        if let Err(e) = res {
            tracing::error!("{e}");
        }
    });
    ep.close().await;
    discovery_stream_task.abort();
    Ok(())
}


<file_info>
path: echo.rs
name: echo.rs
</file_info>
//! Very basic example to showcase how to use iroh's APIs.
//!
//! This example implements a simple protocol that echos any data sent to it in the first stream.
//!
//! ## Usage
//!
//!     cargo run --example echo --features=examples

use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, NodeAddr,
};
use n0_snafu::{Result, ResultExt};
use n0_watcher::Watcher as _;

/// Each protocol is identified by its ALPN string.
///
/// The ALPN, or application-layer protocol negotiation, is exchanged in the connection handshake,
/// and the connection is aborted unless both nodes pass the same bytestring.
const ALPN: &[u8] = b"iroh-example/echo/0";

#[tokio::main]
async fn main() -> Result<()> {
    let router = start_accept_side().await?;
    let node_addr = router.endpoint().node_addr().initialized().await?;

    connect_side(node_addr).await?;

    // This makes sure the endpoint in the router is closed properly and connections close gracefully
    router.shutdown().await.e()?;

    Ok(())
}

async fn connect_side(addr: NodeAddr) -> Result<()> {
    let endpoint = Endpoint::builder().discovery_n0().bind().await?;

    // Open a connection to the accepting node
    let conn = endpoint.connect(addr, ALPN).await?;

    // Open a bidirectional QUIC stream
    let (mut send, mut recv) = conn.open_bi().await.e()?;

    // Send some data to be echoed
    send.write_all(b"Hello, world!").await.e()?;

    // Signal the end of data for this particular stream
    send.finish().e()?;

    // Receive the echo, but limit reading up to maximum 1000 bytes
    let response = recv.read_to_end(1000).await.e()?;
    assert_eq!(&response, b"Hello, world!");

    // Explicitly close the whole connection.
    conn.close(0u32.into(), b"bye!");

    // The above call only queues a close message to be sent (see how it's not async!).
    // We need to actually call this to make sure this message is sent out.
    endpoint.close().await;
    // If we don't call this, but continue using the endpoint, we then the queued
    // close call will eventually be picked up and sent.
    // But always try to wait for endpoint.close().await to go through before dropping
    // the endpoint to ensure any queued messages are sent through and connections are
    // closed gracefully.
    Ok(())
}

async fn start_accept_side() -> Result<Router> {
    let endpoint = Endpoint::builder().discovery_n0().bind().await?;

    // Build our protocol handler and add our protocol, identified by its ALPN, and spawn the node.
    let router = Router::builder(endpoint).accept(ALPN, Echo).spawn();

    Ok(router)
}

#[derive(Debug, Clone)]
struct Echo;

impl ProtocolHandler for Echo {
    /// The `accept` method is called for each incoming connection for our ALPN.
    ///
    /// The returned future runs on a newly spawned tokio task, so it can run as long as
    /// the connection lasts.
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // We can get the remote's node id from the connection.
        let node_id = connection.remote_node_id()?;
        println!("accepted connection from {node_id}");

        // Our protocol is a simple request-response protocol, so we expect the
        // connecting peer to open a single bi-directional stream.
        let (mut send, mut recv) = connection.accept_bi().await?;

        // Echo any bytes received back directly.
        // This will keep copying until the sender signals the end of data on the stream.
        let bytes_sent = tokio::io::copy(&mut recv, &mut send).await?;
        println!("Copied over {bytes_sent} byte(s)");

        // By calling `finish` on the send stream we signal that we will not send anything
        // further, which makes the receive stream on the other end terminate.
        send.finish()?;

        // Wait until the remote closes the connection, which it does once it
        // received the response.
        connection.closed().await;

        Ok(())
    }
}


<file_info>
path: transfer.rs
name: transfer.rs
</file_info>
use std::{
    str::FromStr,
    time::{Duration, Instant},
};

use bytes::Bytes;
use clap::{Parser, Subcommand};
use indicatif::HumanBytes;
use iroh::{
    discovery::{
        dns::DnsDiscovery,
        pkarr::{PkarrPublisher, N0_DNS_PKARR_RELAY_PROD, N0_DNS_PKARR_RELAY_STAGING},
    },
    dns::{DnsResolver, N0_DNS_NODE_ORIGIN_PROD, N0_DNS_NODE_ORIGIN_STAGING},
    endpoint::ConnectionError,
    Endpoint, NodeAddr, NodeId, RelayMap, RelayMode, RelayUrl, SecretKey,
};
use iroh_base::ticket::NodeTicket;
use n0_future::task::AbortOnDropHandle;
use n0_snafu::{Result, ResultExt};
use n0_watcher::Watcher as _;
use tokio_stream::StreamExt;
use tracing::{info, warn};
use url::Url;

// Transfer ALPN that we are using to communicate over the `Endpoint`
const TRANSFER_ALPN: &[u8] = b"n0/iroh/transfer/example/0";

const DEV_RELAY_URL: &str = "http://localhost:3340";
const DEV_PKARR_RELAY_URL: &str = "http://localhost:8080/pkarr";
const DEV_DNS_ORIGIN_DOMAIN: &str = "irohdns.example";
const DEV_DNS_SERVER: &str = "127.0.0.1:5300";

/// Transfer data between iroh nodes.
///
/// This is a useful example to test connection establishment and transfer speed.
///
/// Note that some options are only available with optional features:
///
/// --relay-only needs the `test-utils` feature
///
/// --mdns needs the `discovery-local-network` feature
///
/// To enable all features, run the example with --all-features:
///
/// cargo run --release --example transfer --all-features -- ARGS
#[derive(Parser, Debug)]
#[command(name = "transfer")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, Default, Debug, Eq, PartialEq, clap::ValueEnum)]
enum Env {
    /// Use the production servers hosted by number0.
    Prod,
    /// Use the staging servers hosted by number0.
    #[default]
    Staging,
    /// Use localhost servers.
    ///
    /// To run the DNS server:
    ///     cargo run --bin iroh-dns-server
    /// To run the relay server:
    ///     cargo run --bin iroh-relay --features server -- --dev
    Dev,
}

impl Env {
    fn relay_mode(self) -> RelayMode {
        match self {
            Env::Prod => RelayMode::Default,
            Env::Staging => RelayMode::Staging,
            Env::Dev => RelayMode::Custom(RelayMap::from(
                RelayUrl::from_str(DEV_RELAY_URL).expect("valid url"),
            )),
        }
    }

    fn pkarr_relay_url(self) -> Url {
        match self {
            Env::Prod => N0_DNS_PKARR_RELAY_PROD.parse(),
            Env::Staging => N0_DNS_PKARR_RELAY_STAGING.parse(),
            Env::Dev => DEV_PKARR_RELAY_URL.parse(),
        }
        .expect("valid url")
    }

    fn dns_origin_domain(self) -> String {
        match self {
            Env::Prod => N0_DNS_NODE_ORIGIN_PROD.to_string(),
            Env::Staging => N0_DNS_NODE_ORIGIN_STAGING.to_string(),
            Env::Dev => DEV_DNS_ORIGIN_DOMAIN.to_string(),
        }
    }
}

#[derive(Debug, clap::Parser)]
struct EndpointArgs {
    /// Set the environment for relay, pkarr, and DNS servers.
    ///
    /// If other options are set, those will override the environment defaults.
    #[clap(short, long, value_enum, default_value_t)]
    env: Env,
    /// Set one or more relay servers to use.
    #[clap(long)]
    relay_url: Vec<RelayUrl>,
    /// Disable relays completely.
    #[clap(long, conflicts_with = "relay_url")]
    no_relay: bool,
    /// If set no direct connections will be established.
    #[clap(long)]
    relay_only: bool,
    /// Use a custom pkarr server.
    #[clap(long)]
    pkarr_relay_url: Option<Url>,
    /// Disable publishing node info to pkarr.
    #[clap(long, conflicts_with = "pkarr_relay_url")]
    no_pkarr_publish: bool,
    /// Use a custom domain when resolving node info via DNS.
    #[clap(long)]
    dns_origin_domain: Option<String>,
    /// Use a custom DNS server for resolving relay and node info domains.
    #[clap(long)]
    dns_server: Option<String>,
    /// Do not resolve node info via DNS.
    #[clap(long)]
    no_dns_resolve: bool,
    #[cfg(feature = "discovery-local-network")]
    #[clap(long)]
    /// Enable mDNS discovery.
    mdns: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Provide data.
    Provide {
        #[clap(long, default_value = "100M", value_parser = parse_byte_size)]
        size: u64,
        #[clap(flatten)]
        endpoint_args: EndpointArgs,
    },
    /// Fetch data.
    Fetch {
        ticket: String,
        #[clap(flatten)]
        endpoint_args: EndpointArgs,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Commands::Provide {
            size,
            endpoint_args,
        } => {
            let endpoint = endpoint_args.bind_endpoint().await?;
            provide(endpoint, size).await?
        }
        Commands::Fetch {
            ticket,
            endpoint_args,
        } => {
            let endpoint = endpoint_args.bind_endpoint().await?;
            fetch(endpoint, &ticket).await?
        }
    }

    Ok(())
}

impl EndpointArgs {
    async fn bind_endpoint(self) -> Result<Endpoint> {
        let mut builder = Endpoint::builder();

        let secret_key = match std::env::var("IROH_SECRET") {
            Ok(s) => SecretKey::from_str(&s)
                .context("Failed to parse IROH_SECRET environment variable as iroh secret key")?,
            Err(_) => {
                let s = SecretKey::generate(rand::rngs::OsRng);
                println!("Generated a new node secret. To reuse, set");
                println!("\tIROH_SECRET={s}");
                s
            }
        };
        builder = builder.secret_key(secret_key);

        let relay_mode = if self.no_relay {
            RelayMode::Disabled
        } else if !self.relay_url.is_empty() {
            RelayMode::Custom(RelayMap::from_iter(self.relay_url))
        } else {
            self.env.relay_mode()
        };
        builder = builder.relay_mode(relay_mode);

        if !self.no_pkarr_publish {
            let url = self
                .pkarr_relay_url
                .unwrap_or_else(|| self.env.pkarr_relay_url());
            builder = builder
                .add_discovery(|secret_key| Some(PkarrPublisher::new(secret_key.clone(), url)));
        }

        if !self.no_dns_resolve {
            let domain = self
                .dns_origin_domain
                .unwrap_or_else(|| self.env.dns_origin_domain());
            builder = builder.add_discovery(|_| Some(DnsDiscovery::new(domain)));
        }

        #[cfg(feature = "discovery-local-network")]
        if self.mdns {
            builder = builder.add_discovery(|secret_key| {
                Some(
                    iroh::discovery::mdns::MdnsDiscovery::new(secret_key.public())
                        .expect("Failed to create mDNS discovery"),
                )
            });
        }

        #[cfg(feature = "test-utils")]
        if self.relay_only {
            builder = builder.path_selection(iroh::endpoint::PathSelection::RelayOnly)
        }

        if let Some(host) = self.dns_server {
            let addr = tokio::net::lookup_host(host)
                .await
                .context("Failed to resolve DNS server address")?
                .next()
                .context("Failed to resolve DNS server address")?;
            builder = builder.dns_resolver(DnsResolver::with_nameserver(addr));
        } else if self.env == Env::Dev {
            let addr = DEV_DNS_SERVER.parse().expect("valid addr");
            builder = builder.dns_resolver(DnsResolver::with_nameserver(addr));
        }

        let endpoint = builder.alpns(vec![TRANSFER_ALPN.to_vec()]).bind().await?;

        let node_id = endpoint.node_id();
        println!("Our node id:\n\t{node_id}");
        println!("Our direct addresses:");
        for local_endpoint in endpoint.direct_addresses().initialized().await? {
            println!("\t{} (type: {:?})", local_endpoint.addr, local_endpoint.typ)
        }
        if !self.no_relay {
            let relay_url = endpoint
                .home_relay()
                .get()?
                .pop()
                .context("Failed to resolve our home relay")?;
            println!("Our home relay server:\n\t{relay_url}");
        }

        println!();
        Ok(endpoint)
    }
}

async fn provide(endpoint: Endpoint, size: u64) -> Result<()> {
    let node_id = endpoint.node_id();

    let node_addr = endpoint.node_addr().initialized().await?;
    let ticket = NodeTicket::new(node_addr);
    println!("Ticket with our home relay and direct addresses:\n{ticket}\n",);

    let mut node_addr = endpoint.node_addr().initialized().await?;
    node_addr.direct_addresses = Default::default();
    let ticket = NodeTicket::new(node_addr);
    println!("Ticket with our home relay but no direct addresses:\n{ticket}\n",);

    let ticket = NodeTicket::new(NodeAddr::new(node_id));
    println!("Ticket with only our node id:\n{ticket}\n");

    // accept incoming connections, returns a normal QUIC connection
    while let Some(incoming) = endpoint.accept().await {
        let connecting = match incoming.accept() {
            Ok(connecting) => connecting,
            Err(err) => {
                warn!("incoming connection failed: {err:#}");
                // we can carry on in these cases:
                // this can be caused by retransmitted datagrams
                continue;
            }
        };
        // spawn a task to handle reading and writing off of the connection
        let endpoint_clone = endpoint.clone();
        tokio::spawn(async move {
            let conn = connecting.await.e()?;
            let node_id = conn.remote_node_id()?;
            info!(
                "new connection from {node_id} with ALPN {}",
                String::from_utf8_lossy(TRANSFER_ALPN),
            );

            let remote = node_id.fmt_short();
            println!("[{remote}] Connected");

            // Spawn a background task that prints connection type changes. Will be aborted on drop.
            let _guard = watch_conn_type(&endpoint_clone, node_id);

            // accept a bi-directional QUIC connection
            // use the `quinn` APIs to send and recv content
            let (mut send, mut recv) = conn.accept_bi().await.e()?;
            tracing::debug!("accepted bi stream, waiting for data...");
            let message = recv.read_to_end(100).await.e()?;
            let message = String::from_utf8(message).e()?;
            println!("[{remote}] Received: \"{message}\"");

            let start = Instant::now();
            send_data_on_stream(&mut send, size).await?;

            // We sent the last message, so wait for the client to close the connection once
            // it received this message.
            let res = tokio::time::timeout(Duration::from_secs(3), async move {
                let closed = conn.closed().await;
                let remote = node_id.fmt_short();
                if !matches!(closed, ConnectionError::ApplicationClosed(_)) {
                    println!("[{remote}] Node disconnected with an error: {closed:#}");
                }
            })
            .await;
            let duration = start.elapsed();

            println!(
                "[{remote}] Transferred {} in {:.4}s, {}/s",
                HumanBytes(size),
                duration.as_secs_f64(),
                HumanBytes((size as f64 / duration.as_secs_f64()) as u64)
            );
            if res.is_err() {
                println!("[{remote}] Did not disconnect within 3 seconds");
            } else {
                println!("[{remote}] Disconnected");
            }
            Ok::<_, n0_snafu::Error>(())
        });
    }

    // stop with SIGINT (ctrl-c)
    Ok(())
}

async fn fetch(endpoint: Endpoint, ticket: &str) -> Result<()> {
    let me = endpoint.node_id().fmt_short();
    let ticket: NodeTicket = ticket.parse()?;
    let remote_node_id = ticket.node_addr().node_id;
    let start = Instant::now();

    // Attempt to connect, over the given ALPN.
    // Returns a Quinn connection.
    let conn = endpoint
        .connect(NodeAddr::from(ticket), TRANSFER_ALPN)
        .await?;
    println!("Connected to {remote_node_id}");
    // Spawn a background task that prints connection type changes. Will be aborted on drop.
    let _guard = watch_conn_type(&endpoint, remote_node_id);

    // Use the Quinn API to send and recv content.
    let (mut send, mut recv) = conn.open_bi().await.e()?;

    let message = format!("{me} is saying hello!");
    send.write_all(message.as_bytes()).await.e()?;
    // Call `finish` to signal no more data will be sent on this stream.
    send.finish().e()?;
    println!("Sent: \"{message}\"");

    let (len, time_to_first_byte, chnk) = drain_stream(&mut recv, false).await?;

    // We received the last message: close all connections and allow for the close
    // message to be sent.
    tokio::time::timeout(Duration::from_secs(3), endpoint.close())
        .await
        .e()?;

    let duration = start.elapsed();
    println!(
        "Received {} in {:.4}s ({}/s, time to first byte {}s, {} chunks)",
        HumanBytes(len as u64),
        duration.as_secs_f64(),
        HumanBytes((len as f64 / duration.as_secs_f64()) as u64),
        time_to_first_byte.as_secs_f64(),
        chnk
    );
    Ok(())
}

async fn drain_stream(
    stream: &mut iroh::endpoint::RecvStream,
    read_unordered: bool,
) -> Result<(usize, Duration, u64)> {
    let mut read = 0;

    let download_start = Instant::now();
    let mut first_byte = true;
    let mut time_to_first_byte = download_start.elapsed();

    let mut num_chunks: u64 = 0;

    if read_unordered {
        while let Some(chunk) = stream.read_chunk(usize::MAX, false).await.e()? {
            if first_byte {
                time_to_first_byte = download_start.elapsed();
                first_byte = false;
            }
            read += chunk.bytes.len();
            num_chunks += 1;
        }
    } else {
        // These are 32 buffers, for reading approximately 32kB at once
        #[rustfmt::skip]
        let mut bufs = [
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
            Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        ];

        while let Some(n) = stream.read_chunks(&mut bufs[..]).await.e()? {
            if first_byte {
                time_to_first_byte = download_start.elapsed();
                first_byte = false;
            }
            read += bufs.iter().take(n).map(|buf| buf.len()).sum::<usize>();
            num_chunks += 1;
        }
    }

    Ok((read, time_to_first_byte, num_chunks))
}

async fn send_data_on_stream(
    stream: &mut iroh::endpoint::SendStream,
    stream_size: u64,
) -> Result<()> {
    const DATA: &[u8] = &[0xAB; 1024 * 1024];
    let bytes_data = Bytes::from_static(DATA);

    let full_chunks = stream_size / (DATA.len() as u64);
    let remaining = (stream_size % (DATA.len() as u64)) as usize;

    for _ in 0..full_chunks {
        stream
            .write_chunk(bytes_data.clone())
            .await
            .context("failed sending data")?;
    }

    if remaining != 0 {
        stream
            .write_chunk(bytes_data.slice(0..remaining))
            .await
            .context("failed sending data")?;
    }

    stream.finish().context("failed finishing stream")?;
    stream
        .stopped()
        .await
        .context("failed to wait for stream to be stopped")?;

    Ok(())
}

fn parse_byte_size(s: &str) -> std::result::Result<u64, parse_size::Error> {
    let cfg = parse_size::Config::new().with_binary();
    cfg.parse_size(s)
}

fn watch_conn_type(endpoint: &Endpoint, node_id: NodeId) -> AbortOnDropHandle<()> {
    let mut stream = endpoint.conn_type(node_id).unwrap().stream();
    let task = tokio::task::spawn(async move {
        while let Some(conn_type) = stream.next().await {
            println!(
                "[{}] Connection type changed to: {conn_type}",
                node_id.fmt_short()
            );
        }
    });
    AbortOnDropHandle::new(task)
}


<file_info>
path: listen.rs
name: listen.rs
</file_info>
//! The smallest example showing how to use iroh and [`iroh::Endpoint`] to connect two devices.
//!
//! This example uses the default relay servers to attempt to holepunch, and will use that relay server to relay packets if the two devices cannot establish a direct UDP connection.
//! run this example from the project root:
//!     $ cargo run --example listen
use std::time::Duration;

use iroh::{endpoint::ConnectionError, Endpoint, RelayMode, SecretKey};
use n0_snafu::ResultExt;
use n0_watcher::Watcher as _;
use tracing::{debug, info, warn};

// An example ALPN that we are using to communicate over the `Endpoint`
const EXAMPLE_ALPN: &[u8] = b"n0/iroh/examples/magic/0";

#[tokio::main]
async fn main() -> n0_snafu::Result<()> {
    tracing_subscriber::fmt::init();
    println!("\nlisten example!\n");
    let secret_key = SecretKey::generate(rand::rngs::OsRng);
    println!("secret key: {secret_key}");

    // Build a `Endpoint`, which uses PublicKeys as node identifiers, uses QUIC for directly connecting to other nodes, and uses the relay protocol and relay servers to holepunch direct connections between nodes when there are NATs or firewalls preventing direct connections. If no direct connection can be made, packets are relayed over the relay servers.
    let endpoint = Endpoint::builder()
        // The secret key is used to authenticate with other nodes. The PublicKey portion of this secret key is how we identify nodes, often referred to as the `node_id` in our codebase.
        .secret_key(secret_key)
        // set the ALPN protocols this endpoint will accept on incoming connections
        .alpns(vec![EXAMPLE_ALPN.to_vec()])
        // `RelayMode::Default` means that we will use the default relay servers to holepunch and relay.
        // Use `RelayMode::Custom` to pass in a `RelayMap` with custom relay urls.
        // Use `RelayMode::Disable` to disable holepunching and relaying over HTTPS
        // If you want to experiment with relaying using your own relay server, you must pass in the same custom relay url to both the `listen` code AND the `connect` code
        .relay_mode(RelayMode::Default)
        // you can choose a port to bind to, but passing in `0` will bind the socket to a random available port
        .bind()
        .await?;

    let me = endpoint.node_id();
    println!("node id: {me}");
    println!("node listening addresses:");

    let local_addrs = endpoint
        .direct_addresses()
        .initialized()
        .await?
        .into_iter()
        .map(|addr| {
            let addr = addr.addr.to_string();
            println!("\t{addr}");
            addr
        })
        .collect::<Vec<_>>()
        .join(" ");
    let relay_url = endpoint.home_relay().initialized().await?;
    println!("node relay server url: {relay_url}");
    println!("\nin a separate terminal run:");

    println!(
        "\tcargo run --example connect -- --node-id {me} --addrs \"{local_addrs}\" --relay-url {relay_url}\n"
    );
    // accept incoming connections, returns a normal QUIC connection
    while let Some(incoming) = endpoint.accept().await {
        let mut connecting = match incoming.accept() {
            Ok(connecting) => connecting,
            Err(err) => {
                warn!("incoming connection failed: {err:#}");
                // we can carry on in these cases:
                // this can be caused by retransmitted datagrams
                continue;
            }
        };
        let alpn = connecting.alpn().await?;
        let conn = connecting.await.e()?;
        let node_id = conn.remote_node_id()?;
        info!(
            "new connection from {node_id} with ALPN {}",
            String::from_utf8_lossy(&alpn),
        );

        // spawn a task to handle reading and writing off of the connection
        tokio::spawn(async move {
            // accept a bi-directional QUIC connection
            // use the `quinn` APIs to send and recv content
            let (mut send, mut recv) = conn.accept_bi().await.e()?;
            debug!("accepted bi stream, waiting for data...");
            let message = recv.read_to_end(100).await.e()?;
            let message = String::from_utf8(message).e()?;
            println!("received: {message}");

            let message = format!("hi! you connected to {me}. bye bye");
            send.write_all(message.as_bytes()).await.e()?;
            // call `finish` to close the connection gracefully
            send.finish().e()?;

            // We sent the last message, so wait for the client to close the connection once
            // it received this message.
            let res = tokio::time::timeout(Duration::from_secs(3), async move {
                let closed = conn.closed().await;
                if !matches!(closed, ConnectionError::ApplicationClosed(_)) {
                    println!("node {node_id} disconnected with an error: {closed:#}");
                }
            })
            .await;
            if res.is_err() {
                println!("node {node_id} did not disconnect within 3 seconds");
            }
            Ok::<_, n0_snafu::Error>(())
        });
    }
    // stop with SIGINT (ctrl-c)

    Ok(())
}


<file_info>
path: echo-no-router.rs
name: echo-no-router.rs
</file_info>
//! Very basic example showing how to implement a basic echo protocol,
//! without using the `Router` API. (For the router version, check out the echo.rs example.)
//!
//! The echo protocol echos any data sent to it in the first stream.
//!
//! ## Running the Example
//!
//!     cargo run --example echo-no-router --features=examples

use iroh::{Endpoint, NodeAddr};
use n0_snafu::{Error, Result, ResultExt};
use n0_watcher::Watcher as _;

/// Each protocol is identified by its ALPN string.
///
/// The ALPN, or application-layer protocol negotiation, is exchanged in the connection handshake,
/// and the connection is aborted unless both nodes pass the same bytestring.
const ALPN: &[u8] = b"iroh-example/echo/0";

#[tokio::main]
async fn main() -> Result<()> {
    let endpoint = start_accept_side().await?;
    let node_addr = endpoint.node_addr().initialized().await?;

    connect_side(node_addr).await?;

    // This makes sure the endpoint is closed properly and connections close gracefully
    // and will indirectly close the tasks spawned by `start_accept_side`.
    endpoint.close().await;

    Ok(())
}

async fn connect_side(addr: NodeAddr) -> Result<()> {
    let endpoint = Endpoint::builder().discovery_n0().bind().await?;

    // Open a connection to the accepting node
    let conn = endpoint.connect(addr, ALPN).await?;

    // Open a bidirectional QUIC stream
    let (mut send, mut recv) = conn.open_bi().await.e()?;

    // Send some data to be echoed
    send.write_all(b"Hello, world!").await.e()?;

    // Signal the end of data for this particular stream
    send.finish().e()?;

    // Receive the echo, but limit reading up to maximum 1000 bytes
    let response = recv.read_to_end(1000).await.e()?;
    assert_eq!(&response, b"Hello, world!");

    // Explicitly close the whole connection.
    conn.close(0u32.into(), b"bye!");

    // The above call only queues a close message to be sent (see how it's not async!).
    // We need to actually call this to make sure this message is sent out.
    endpoint.close().await;
    // If we don't call this, but continue using the endpoint, then the queued
    // close call will eventually be picked up and sent.
    // But always try to wait for endpoint.close().await to go through before dropping
    // the endpoint to ensure any queued messages are sent through and connections are
    // closed gracefully.

    Ok(())
}

async fn start_accept_side() -> Result<Endpoint> {
    let endpoint = Endpoint::builder()
        .discovery_n0()
        // The accept side needs to opt-in to the protocols it accepts,
        // as any connection attempts that can't be found with a matching ALPN
        // will be rejected.
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;

    // spawn a task so that `start_accept_side` returns immediately and we can continue in main().
    tokio::spawn({
        let endpoint = endpoint.clone();
        async move {
            // This task won't leak, because we call `endpoint.close()` in `main()`,
            // which causes `endpoint.accept().await` to return `None`.
            // In a more serious environment, we recommend avoiding `tokio::spawn` and use either a `TaskTracker` or
            // `JoinSet` instead to make sure you're not accidentally leaking tasks.
            while let Some(incoming) = endpoint.accept().await {
                // spawn a task for each incoming connection, so we can serve multiple connections asynchronously
                tokio::spawn(async move {
                    let connection = incoming.await.e()?;

                    // We can get the remote's node id from the connection.
                    let node_id = connection.remote_node_id()?;
                    println!("accepted connection from {node_id}");

                    // Our protocol is a simple request-response protocol, so we expect the
                    // connecting peer to open a single bi-directional stream.
                    let (mut send, mut recv) = connection.accept_bi().await.e()?;

                    // Echo any bytes received back directly.
                    // This will keep copying until the sender signals the end of data on the stream.
                    let bytes_sent = tokio::io::copy(&mut recv, &mut send).await.e()?;
                    println!("Copied over {bytes_sent} byte(s)");

                    // By calling `finish` on the send stream we signal that we will not send anything
                    // further, which makes the receive stream on the other end terminate.
                    send.finish().e()?;

                    // Wait until the remote closes the connection, which it does once it
                    // received the response.
                    connection.closed().await;

                    Ok::<_, Error>(())
                });
            }

            Ok::<_, Error>(())
        }
    });

    Ok(endpoint)
}


<file_info>
path: listen-unreliable.rs
name: listen-unreliable.rs
</file_info>
//! The smallest example showing how to use iroh and [`iroh::Endpoint`] to connect two devices and pass bytes using unreliable datagrams.
//!
//! This example uses the default relay servers to attempt to holepunch, and will use that relay server to relay packets if the two devices cannot establish a direct UDP connection.
//! run this example from the project root:
//!     $ cargo run --example listen-unreliable
use iroh::{Endpoint, RelayMode, SecretKey};
use n0_snafu::{Error, Result, ResultExt};
use n0_watcher::Watcher as _;
use tracing::{info, warn};

// An example ALPN that we are using to communicate over the `Endpoint`
const EXAMPLE_ALPN: &[u8] = b"n0/iroh/examples/magic/0";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    println!("\nlisten (unreliable) example!\n");
    let secret_key = SecretKey::generate(rand::rngs::OsRng);
    println!("secret key: {secret_key}");

    // Build a `Endpoint`, which uses PublicKeys as node identifiers, uses QUIC for directly connecting to other nodes, and uses the relay servers to holepunch direct connections between nodes when there are NATs or firewalls preventing direct connections. If no direct connection can be made, packets are relayed over the relay servers.
    let endpoint = Endpoint::builder()
        // The secret key is used to authenticate with other nodes. The PublicKey portion of this secret key is how we identify nodes, often referred to as the `node_id` in our codebase.
        .secret_key(secret_key)
        // set the ALPN protocols this endpoint will accept on incoming connections
        .alpns(vec![EXAMPLE_ALPN.to_vec()])
        // `RelayMode::Default` means that we will use the default relay servers to holepunch and relay.
        // Use `RelayMode::Custom` to pass in a `RelayMap` with custom relay urls.
        // Use `RelayMode::Disable` to disable holepunching and relaying over HTTPS
        // If you want to experiment with relaying using your own relay server, you must pass in the same custom relay url to both the `listen` code AND the `connect` code
        .relay_mode(RelayMode::Default)
        // you can choose a port to bind to, but passing in `0` will bind the socket to a random available port
        .bind()
        .await?;

    let me = endpoint.node_id();
    println!("node id: {me}");
    println!("node listening addresses:");

    let node_addr = endpoint.node_addr().initialized().await?;
    let local_addrs = node_addr
        .direct_addresses
        .into_iter()
        .map(|addr| {
            let addr = addr.to_string();
            println!("\t{addr}");
            addr
        })
        .collect::<Vec<_>>()
        .join(" ");
    let relay_url = node_addr
        .relay_url
        .expect("Should have a relay URL, assuming a default endpoint setup.");
    println!("node relay server url: {relay_url}");
    println!("\nin a separate terminal run:");

    println!(
        "\tcargo run --example connect-unreliable -- --node-id {me} --addrs \"{local_addrs}\" --relay-url {relay_url}\n"
    );
    // accept incoming connections, returns a normal QUIC connection

    while let Some(incoming) = endpoint.accept().await {
        let mut connecting = match incoming.accept() {
            Ok(connecting) => connecting,
            Err(err) => {
                warn!("incoming connection failed: {err:#}");
                // we can carry on in these cases:
                // this can be caused by retransmitted datagrams
                continue;
            }
        };
        let alpn = connecting.alpn().await?;
        let conn = connecting.await.e()?;
        let node_id = conn.remote_node_id()?;
        info!(
            "new (unreliable) connection from {node_id} with ALPN {}",
            String::from_utf8_lossy(&alpn),
        );
        // spawn a task to handle reading and writing off of the connection
        tokio::spawn(async move {
            // use the `quinn` API to read a datagram off the connection, and send a datagra, in return
            while let Ok(message) = conn.read_datagram().await {
                let message = String::from_utf8(message.into()).e()?;
                println!("received: {message}");

                let message = format!("hi! you connected to {me}. bye bye");
                conn.send_datagram(message.as_bytes().to_vec().into()).e()?;
            }

            Ok::<_, Error>(())
        });
    }
    // stop with SIGINT (ctrl-c)

    Ok(())
}


<file_info>
path: 0rtt.rs
name: 0rtt.rs
</file_info>
use std::{env, future::Future, str::FromStr, time::Instant};

use clap::Parser;
use iroh::{
    endpoint::{Connecting, Connection},
    SecretKey,
};
use iroh_base::ticket::NodeTicket;
use n0_future::{future, StreamExt};
use n0_snafu::ResultExt;
use n0_watcher::Watcher;
use rand::thread_rng;
use tracing::{info, trace};

const PINGPONG_ALPN: &[u8] = b"0rtt-pingpong";

#[derive(Parser)]
struct Args {
    /// The node id to connect to. If not set, the program will start a server.
    node: Option<NodeTicket>,
    /// Number of rounds to run.
    #[clap(long, default_value = "100")]
    rounds: u64,
    /// Run without 0-RTT for comparison.
    #[clap(long)]
    disable_0rtt: bool,
}

/// Gets a secret key from the IROH_SECRET environment variable or generates a new random one.
/// If the environment variable is set, it must be a valid string representation of a secret key.
pub fn get_or_generate_secret_key() -> n0_snafu::Result<SecretKey> {
    if let Ok(secret) = env::var("IROH_SECRET") {
        // Parse the secret key from string
        SecretKey::from_str(&secret).context("Invalid secret key format")
    } else {
        // Generate a new random key
        let secret_key = SecretKey::generate(&mut thread_rng());
        println!("Generated new secret key: {}", secret_key);
        println!("To reuse this key, set the IROH_SECRET environment variable to this value");
        Ok(secret_key)
    }
}

/// Do a simple ping-pong with the given connection.
///
/// We send the data on the connection. If `proceed` resolves to true,
/// read the response immediately. Otherwise, the stream pair is bad and we need
/// to open a new stream pair.
async fn pingpong(
    connection: &Connection,
    proceed: impl Future<Output = bool>,
    x: u64,
) -> n0_snafu::Result<()> {
    let (mut send, recv) = connection.open_bi().await.e()?;
    let data = x.to_be_bytes();
    send.write_all(&data).await.e()?;
    send.finish().e()?;
    let mut recv = if proceed.await {
        // use recv directly if we can proceed
        recv
    } else {
        // proceed returned false, so we have learned that the 0-RTT send was rejected.
        // at this point we have a fully handshaked connection, so we try again.
        let (mut send, recv) = connection.open_bi().await.e()?;
        send.write_all(&data).await.e()?;
        send.finish().e()?;
        recv
    };
    let echo = recv.read_to_end(8).await.e()?;
    assert!(echo == data);
    Ok(())
}

async fn pingpong_0rtt(connecting: Connecting, i: u64) -> n0_snafu::Result<Connection> {
    let connection = match connecting.into_0rtt() {
        Ok((connection, accepted)) => {
            trace!("0-RTT possible from our side");
            pingpong(&connection, accepted, i).await?;
            connection
        }
        Err(connecting) => {
            trace!("0-RTT not possible from our side");
            let connection = connecting.await.e()?;
            pingpong(&connection, future::ready(true), i).await?;
            connection
        }
    };
    Ok(connection)
}

async fn connect(args: Args) -> n0_snafu::Result<()> {
    let node_addr = args.node.unwrap().node_addr().clone();
    let endpoint = iroh::Endpoint::builder()
        .relay_mode(iroh::RelayMode::Disabled)
        .keylog(true)
        .bind()
        .await?;
    let t0 = Instant::now();
    for i in 0..args.rounds {
        let t0 = Instant::now();
        let connecting = endpoint
            .connect_with_opts(node_addr.clone(), PINGPONG_ALPN, Default::default())
            .await?;
        let connection = if args.disable_0rtt {
            let connection = connecting.await.e()?;
            trace!("connecting without 0-RTT");
            pingpong(&connection, future::ready(true), i).await?;
            connection
        } else {
            pingpong_0rtt(connecting, i).await?
        };
        tokio::spawn(async move {
            // wait for some time for the handshake to complete and the server
            // to send a NewSessionTicket. This is less than ideal, but we
            // don't have a better way to wait for the handshake to complete.
            tokio::time::sleep(connection.rtt() * 2).await;
            connection.close(0u8.into(), b"");
        });
        let elapsed = t0.elapsed();
        println!("round {}: {} us", i, elapsed.as_micros());
    }
    let elapsed = t0.elapsed();
    println!("total time: {} us", elapsed.as_micros());
    println!(
        "time per round: {} us",
        elapsed.as_micros() / (args.rounds as u128)
    );
    Ok(())
}

async fn accept(_args: Args) -> n0_snafu::Result<()> {
    let secret_key = get_or_generate_secret_key()?;
    let endpoint = iroh::Endpoint::builder()
        .alpns(vec![PINGPONG_ALPN.to_vec()])
        .secret_key(secret_key)
        .relay_mode(iroh::RelayMode::Disabled)
        .bind()
        .await?;
    let mut addrs = endpoint.node_addr().stream();
    let addr = loop {
        let Some(addr) = addrs.next().await else {
            snafu::whatever!("Address stream closed");
        };
        if let Some(addr) = addr {
            if !addr.direct_addresses.is_empty() {
                break addr;
            }
        }
    };
    println!("Listening on: {:?}", addr);
    println!("Node ID: {:?}", addr.node_id);
    println!("Ticket: {}", NodeTicket::from(addr));

    let accept = async move {
        while let Some(incoming) = endpoint.accept().await {
            tokio::spawn(async move {
                let connecting = incoming.accept().e()?;
                let (connection, _zero_rtt_accepted) = connecting
                    .into_0rtt()
                    .expect("accept into 0.5 RTT always succeeds");
                let (mut send, mut recv) = connection.accept_bi().await.e()?;
                trace!("recv.is_0rtt: {}", recv.is_0rtt());
                let data = recv.read_to_end(8).await.e()?;
                trace!("recv: {}", data.len());
                send.write_all(&data).await.e()?;
                send.finish().e()?;
                connection.closed().await;
                Ok::<_, n0_snafu::Error>(())
            });
        }
    };
    tokio::select! {
        _ = accept => {
            info!("accept finished, shutting down");
        },
        _ = tokio::signal::ctrl_c()=> {
            info!("Ctrl-C received, shutting down");
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> n0_snafu::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    if args.node.is_some() {
        connect(args).await?;
    } else {
        accept(args).await?;
    };
    Ok(())
}


<file_info>
path: dht_discovery.rs
name: dht_discovery.rs
</file_info>
//! An example chat application using the iroh endpoint and
//! pkarr node discovery.
//!
//! Starting the example without args creates a server that publishes its
//! address to the DHT. Starting the example with a node id as argument
//! looks up the address of the node id in the DHT and connects to it.
//!
//! You can look at the published pkarr DNS record using <https://app.pkarr.org/>.
//!
//! To see what is going on, run with `RUST_LOG=iroh_pkarr_node_discovery=debug`.
use std::str::FromStr;

use clap::Parser;
use iroh::{Endpoint, NodeId};
use n0_snafu::ResultExt;
use tracing::warn;
use url::Url;

const CHAT_ALPN: &[u8] = b"pkarr-discovery-demo-chat";

#[derive(Parser)]
struct Args {
    /// The node id to connect to. If not set, the program will start a server.
    node_id: Option<NodeId>,
    /// Disable using the mainline DHT for discovery and publishing.
    #[clap(long)]
    disable_dht: bool,
    /// Pkarr relay to use.
    #[clap(long, default_value = "iroh")]
    pkarr_relay: PkarrRelay,
}

#[derive(Debug, Clone)]
enum PkarrRelay {
    /// Disable pkarr relay.
    Disabled,
    /// Use the iroh pkarr relay.
    Iroh,
    /// Use a custom pkarr relay.
    Custom(Url),
}

impl FromStr for PkarrRelay {
    type Err = url::ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "disabled" => Ok(Self::Disabled),
            "iroh" => Ok(Self::Iroh),
            s => Ok(Self::Custom(Url::parse(s)?)),
        }
    }
}

fn build_discovery(args: Args) -> iroh::discovery::pkarr::dht::Builder {
    let builder = iroh::discovery::pkarr::dht::DhtDiscovery::builder().dht(!args.disable_dht);
    match args.pkarr_relay {
        PkarrRelay::Disabled => builder,
        PkarrRelay::Iroh => builder.n0_dns_pkarr_relay(),
        PkarrRelay::Custom(url) => builder.pkarr_relay(url),
    }
}

async fn chat_server(args: Args) -> n0_snafu::Result<()> {
    let secret_key = iroh::SecretKey::generate(rand::rngs::OsRng);
    let node_id = secret_key.public();
    let discovery = build_discovery(args)
        .secret_key(secret_key.clone())
        .build()?;
    let endpoint = Endpoint::builder()
        .alpns(vec![CHAT_ALPN.to_vec()])
        .secret_key(secret_key)
        .discovery(Box::new(discovery))
        .bind()
        .await?;
    let zid = pkarr::PublicKey::try_from(node_id.as_bytes()).e()?.to_z32();
    println!("Listening on {}", node_id);
    println!("pkarr z32: {}", zid);
    println!("see https://app.pkarr.org/?pk={}", zid);
    while let Some(incoming) = endpoint.accept().await {
        let connecting = match incoming.accept() {
            Ok(connecting) => connecting,
            Err(err) => {
                warn!("incoming connection failed: {err:#}");
                // we can carry on in these cases:
                // this can be caused by retransmitted datagrams
                continue;
            }
        };
        tokio::spawn(async move {
            let connection = connecting.await.e()?;
            let remote_node_id = connection.remote_node_id()?;
            println!("got connection from {}", remote_node_id);
            // just leave the tasks hanging. this is just an example.
            let (mut writer, mut reader) = connection.accept_bi().await.e()?;
            let _copy_to_stdout = tokio::spawn(async move {
                tokio::io::copy(&mut reader, &mut tokio::io::stdout()).await
            });
            let _copy_from_stdin =
                tokio::spawn(
                    async move { tokio::io::copy(&mut tokio::io::stdin(), &mut writer).await },
                );
            Ok::<_, n0_snafu::Error>(())
        });
    }
    Ok(())
}

async fn chat_client(args: Args) -> n0_snafu::Result<()> {
    let remote_node_id = args.node_id.unwrap();
    let secret_key = iroh::SecretKey::generate(rand::rngs::OsRng);
    let node_id = secret_key.public();
    // note: we don't pass a secret key here, because we don't need to publish our address, don't spam the DHT
    let discovery = build_discovery(args).build()?;
    // we do not need to specify the alpn here, because we are not going to accept connections
    let endpoint = Endpoint::builder()
        .secret_key(secret_key)
        .discovery(Box::new(discovery))
        .bind()
        .await?;
    println!("We are {} and connecting to {}", node_id, remote_node_id);
    let connection = endpoint.connect(remote_node_id, CHAT_ALPN).await?;
    println!("connected to {}", remote_node_id);
    let (mut writer, mut reader) = connection.open_bi().await.e()?;
    let _copy_to_stdout =
        tokio::spawn(async move { tokio::io::copy(&mut reader, &mut tokio::io::stdout()).await });
    let _copy_from_stdin =
        tokio::spawn(async move { tokio::io::copy(&mut tokio::io::stdin(), &mut writer).await });
    _copy_to_stdout.await.e()?.e()?;
    _copy_from_stdin.await.e()?.e()?;
    Ok(())
}

#[tokio::main]
async fn main() -> n0_snafu::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    if args.node_id.is_some() {
        chat_client(args).await?;
    } else {
        chat_server(args).await?;
    }
    Ok(())
}


<file_info>
path: search.rs
name: search.rs
</file_info>
//! Example protocol for running search on a remote node.
//!
//! We are building a very simple protocol here.
//!
//! Our protocol allows querying the text stored on the other node.
//!
//! The example is contrived - we only use memory nodes, and our database is a hashmap in a mutex,
//! and our queries just match if the query string appears as-is.
//!
//! ## Usage
//!
//! In one terminal, run
//!
//!     cargo run --example search --features=examples  -- listen "hello-world" "foo-bar" "hello-moon"
//!
//! This spawns an iroh endpoint with three blobs. It will print the node's node id.
//!
//! In another terminal, run
//!
//!     cargo run --example search --features=examples  -- query <node-id> hello
//!
//! Replace <node-id> with the node id from above. This will connect to the listening node with our
//! protocol and query for the string `hello`. The listening node will return a number of how many
//! strings match the query.
//!
//! For this example, this will print:
//!
//! Found 2 matches
//!
//! That's it! Follow along in the code below, we added a bunch of comments to explain things.

use std::{collections::BTreeSet, sync::Arc};

use clap::Parser;
use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, NodeId,
};
use n0_snafu::{Result, ResultExt};
use tokio::sync::Mutex;
use tracing_subscriber::{prelude::*, EnvFilter};

#[derive(Debug, Parser)]
pub struct Cli {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
pub enum Command {
    /// Spawn a node in listening mode.
    Listen {
        /// Each text string will be imported as a blob and inserted into the search database.
        text: Vec<String>,
    },
    /// Query a remote node for data and print the results.
    Query {
        /// The node id of the node we want to query.
        node_id: NodeId,
        /// The text we want to match.
        query: String,
    },
}

/// Each protocol is identified by its ALPN string.
///
/// The ALPN, or application-layer protocol negotiation, is exchanged in the connection handshake,
/// and the connection is aborted unless both nodes pass the same bytestring.
const ALPN: &[u8] = b"iroh-example/text-search/0";

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging();
    let args = Cli::parse();

    // Build an endpoint
    let endpoint = Endpoint::builder().discovery_n0().bind().await?;

    // Build our protocol handler. The `builder` exposes access to various subsystems in the
    // iroh node. In our case, we need a blobs client and the endpoint.
    let proto = BlobSearch::new(endpoint.clone());

    let builder = Router::builder(endpoint);

    // Add our protocol, identified by our ALPN, to the node, and spawn the node.
    let router = builder.accept(ALPN, proto.clone()).spawn();

    match args.command {
        Command::Listen { text } => {
            let node_id = router.endpoint().node_id();
            println!("our node id: {node_id}");

            // Insert the text strings as blobs and index them.
            for text in text.into_iter() {
                proto.insert(text).await?;
            }

            // Wait for Ctrl-C to be pressed.
            tokio::signal::ctrl_c().await.e()?;
        }
        Command::Query { node_id, query } => {
            // Query the remote node.
            // This will send the query over our protocol, read hashes on the reply stream,
            // and download each hash over iroh-blobs.
            let num_matches = proto.query_remote(node_id, &query).await?;

            // Print out our query results.
            println!("Found {} matches", num_matches);
        }
    }

    router.shutdown().await.e()?;

    Ok(())
}

#[derive(Debug, Clone)]
struct BlobSearch {
    endpoint: Endpoint,
    blobs: Arc<Mutex<BTreeSet<String>>>,
}

impl ProtocolHandler for BlobSearch {
    /// The `accept` method is called for each incoming connection for our ALPN.
    ///
    /// The returned future runs on a newly spawned tokio task, so it can run as long as
    /// the connection lasts.
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // We can get the remote's node id from the connection.
        let node_id = connection.remote_node_id()?;
        println!("accepted connection from {node_id}");

        // Our protocol is a simple request-response protocol, so we expect the
        // connecting peer to open a single bi-directional stream.
        let (mut send, mut recv) = connection.accept_bi().await?;

        // We read the query from the receive stream, while enforcing a max query length.
        let query_bytes = recv.read_to_end(64).await.map_err(AcceptError::from_err)?;

        // Now, we can perform the actual query on our local database.
        let query = String::from_utf8(query_bytes).map_err(AcceptError::from_err)?;
        let num_matches = self.query_local(&query).await;

        // We want to return a list of hashes. We do the simplest thing possible, and just send
        // one hash after the other. Because the hashes have a fixed size of 32 bytes, this is
        // very easy to parse on the other end.
        send.write_all(&num_matches.to_le_bytes())
            .await
            .map_err(AcceptError::from_err)?;

        // By calling `finish` on the send stream we signal that we will not send anything
        // further, which makes the receive stream on the other end terminate.
        send.finish()?;

        // Wait until the remote closes the connection, which it does once it
        // received the response.
        connection.closed().await;

        Ok(())
    }
}

impl BlobSearch {
    /// Create a new protocol handler.
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            blobs: Default::default(),
        }
    }

    /// Query a remote node, download all matching blobs and print the results.
    pub async fn query_remote(&self, node_id: NodeId, query: &str) -> Result<u64> {
        // Establish a connection to our node.
        // We use the default node discovery in iroh, so we can connect by node id without
        // providing further information.
        let conn = self.endpoint.connect(node_id, ALPN).await?;

        // Open a bi-directional in our connection.
        let (mut send, mut recv) = conn.open_bi().await.e()?;

        // Send our query.
        send.write_all(query.as_bytes()).await.e()?;

        // Finish the send stream, signalling that no further data will be sent.
        // This makes the `read_to_end` call on the accepting side terminate.
        send.finish().e()?;

        // The response is a 64 bit integer
        // We simply read it into a byte buffer.
        let mut num_matches = [0u8; 8];

        // Read 8 bytes from the stream.
        recv.read_exact(&mut num_matches).await.e()?;

        let num_matches = u64::from_le_bytes(num_matches);

        // Dropping the connection here will close it.

        Ok(num_matches)
    }

    /// Query the local database.
    ///
    /// Returns how many matches were found.
    pub async fn query_local(&self, query: &str) -> u64 {
        let guard = self.blobs.lock().await;
        let count: usize = guard.iter().filter(|text| text.contains(query)).count();
        count as u64
    }

    /// Insert a text string into the database.
    pub async fn insert(&self, text: String) -> Result<()> {
        let mut guard = self.blobs.lock().await;
        guard.insert(text);
        Ok(())
    }
}

/// Set the RUST_LOG env var to one of {debug,info,warn} to see logging.
fn setup_logging() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(EnvFilter::from_default_env())
        .try_init()
        .ok();
}


<file_info>
path: connect.rs
name: connect.rs
</file_info>
//! The smallest example showing how to use iroh and [`iroh::Endpoint`] to connect to a remote node.
//!
//! We use the node ID (the PublicKey of the remote node), the direct UDP addresses, and the relay url to achieve a connection.
//!
//! This example uses the default relay servers to attempt to holepunch, and will use that relay server to relay packets if the two devices cannot establish a direct UDP connection.
//!
//! Run the `listen` example first (`iroh/examples/listen.rs`), which will give you instructions on how to run this example to watch two nodes connect and exchange bytes.
use std::net::SocketAddr;

use clap::Parser;
use iroh::{Endpoint, NodeAddr, RelayMode, RelayUrl, SecretKey};
use n0_snafu::{Result, ResultExt};
use n0_watcher::Watcher as _;
use tracing::info;

// An example ALPN that we are using to communicate over the `Endpoint`
const EXAMPLE_ALPN: &[u8] = b"n0/iroh/examples/magic/0";

#[derive(Debug, Parser)]
struct Cli {
    /// The id of the remote node.
    #[clap(long)]
    node_id: iroh::NodeId,
    /// The list of direct UDP addresses for the remote node.
    #[clap(long, value_parser, num_args = 1.., value_delimiter = ' ')]
    addrs: Vec<SocketAddr>,
    /// The url of the relay server the remote node can also be reached at.
    #[clap(long)]
    relay_url: RelayUrl,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    println!("\nconnect example!\n");
    let args = Cli::parse();
    let secret_key = SecretKey::generate(rand::rngs::OsRng);
    println!("secret key: {secret_key}");

    // Build a `Endpoint`, which uses PublicKeys as node identifiers, uses QUIC for directly connecting to other nodes, and uses the relay protocol and relay servers to holepunch direct connections between nodes when there are NATs or firewalls preventing direct connections. If no direct connection can be made, packets are relayed over the relay servers.
    let endpoint = Endpoint::builder()
        // The secret key is used to authenticate with other nodes. The PublicKey portion of this secret key is how we identify nodes, often referred to as the `node_id` in our codebase.
        .secret_key(secret_key)
        // Set the ALPN protocols this endpoint will accept on incoming connections
        .alpns(vec![EXAMPLE_ALPN.to_vec()])
        // `RelayMode::Default` means that we will use the default relay servers to holepunch and relay.
        // Use `RelayMode::Custom` to pass in a `RelayMap` with custom relay urls.
        // Use `RelayMode::Disable` to disable holepunching and relaying over HTTPS
        // If you want to experiment with relaying using your own relay server, you must pass in the same custom relay url to both the `listen` code AND the `connect` code
        .relay_mode(RelayMode::Default)
        // You can choose an address to bind to, but passing in `None` will bind the socket to a random available port
        .bind()
        .await?;

    let me = endpoint.node_id();
    println!("node id: {me}");
    println!("node listening addresses:");
    for local_endpoint in endpoint
        .direct_addresses()
        .initialized()
        .await
        .context("no direct addresses")?
    {
        println!("\t{}", local_endpoint.addr)
    }

    let relay_url = endpoint
        .home_relay()
        .get()
        .unwrap()
        .first()
        .cloned()
        .expect("should be connected to a relay server, try calling `endpoint.local_endpoints()` or `endpoint.connect()` first, to ensure the endpoint has actually attempted a connection before checking for the connected relay server");
    println!("node relay server url: {relay_url}\n");
    // Build a `NodeAddr` from the node_id, relay url, and UDP addresses.
    let addr = NodeAddr::from_parts(args.node_id, Some(args.relay_url), args.addrs);

    // Attempt to connect, over the given ALPN.
    // Returns a Quinn connection.
    let conn = endpoint.connect(addr, EXAMPLE_ALPN).await?;
    info!("connected");

    // Use the Quinn API to send and recv content.
    let (mut send, mut recv) = conn.open_bi().await.e()?;

    let message = format!("{me} is saying 'hello!'");
    send.write_all(message.as_bytes()).await.e()?;

    // Call `finish` to close the send side of the connection gracefully.
    send.finish().e()?;
    let message = recv.read_to_end(100).await.e()?;
    let message = String::from_utf8(message).e()?;
    println!("received: {message}");

    // We received the last message: close all connections and allow for the close
    // message to be sent.
    endpoint.close().await;
    Ok(())
}


<file_info>
path: connect-unreliable.rs
name: connect-unreliable.rs
</file_info>
//! The smallest example showing how to use iroh and [`iroh::Endpoint`] to connect to a remote node and pass bytes using unreliable datagrams.
//!
//! We use the node ID (the PublicKey of the remote node), the direct UDP addresses, and the relay url to achieve a connection.
//!
//! This example uses the default relay servers to attempt to holepunch, and will use that relay server to relay packets if the two devices cannot establish a direct UDP connection.
//!
//! Run the `listen-unreliable` example first (`iroh/examples/listen-unreliable.rs`), which will give you instructions on how to run this example to watch two nodes connect and exchange bytes.
use std::net::SocketAddr;

use clap::Parser;
use iroh::{Endpoint, NodeAddr, RelayMode, RelayUrl, SecretKey};
use n0_snafu::ResultExt;
use n0_watcher::Watcher as _;
use tracing::info;

// An example ALPN that we are using to communicate over the `Endpoint`
const EXAMPLE_ALPN: &[u8] = b"n0/iroh/examples/magic/0";

#[derive(Debug, Parser)]
struct Cli {
    /// The id of the remote node.
    #[clap(long)]
    node_id: iroh::NodeId,
    /// The list of direct UDP addresses for the remote node.
    #[clap(long, value_parser, num_args = 1.., value_delimiter = ' ')]
    addrs: Vec<SocketAddr>,
    /// The url of the relay server the remote node can also be reached at.
    #[clap(long)]
    relay_url: RelayUrl,
}

#[tokio::main]
async fn main() -> n0_snafu::Result<()> {
    tracing_subscriber::fmt::init();
    println!("\nconnect (unreliable) example!\n");
    let args = Cli::parse();
    let secret_key = SecretKey::generate(rand::rngs::OsRng);
    println!("secret key: {secret_key}");

    // Build a `Endpoint`, which uses PublicKeys as node identifiers, uses QUIC for directly connecting to other nodes, and uses the relay protocol and relay servers to holepunch direct connections between nodes when there are NATs or firewalls preventing direct connections. If no direct connection can be made, packets are relayed over the relay servers.
    let endpoint = Endpoint::builder()
        // The secret key is used to authenticate with other nodes. The PublicKey portion of this secret key is how we identify nodes, often referred to as the `node_id` in our codebase.
        .secret_key(secret_key)
        // Set the ALPN protocols this endpoint will accept on incoming connections
        .alpns(vec![EXAMPLE_ALPN.to_vec()])
        // `RelayMode::Default` means that we will use the default relay servers to holepunch and relay.
        // Use `RelayMode::Custom` to pass in a `RelayMap` with custom relay urls.
        // Use `RelayMode::Disable` to disable holepunching and relaying over HTTPS
        // If you want to experiment with relaying using your own relay server, you must pass in the same custom relay url to both the `listen` code AND the `connect` code
        .relay_mode(RelayMode::Default)
        // You can choose an address to bind to, but passing in `None` will bind the socket to a random available port
        .bind()
        .await?;

    let node_addr = endpoint.node_addr().initialized().await?;
    let me = node_addr.node_id;
    println!("node id: {me}");
    println!("node listening addresses:");
    node_addr
        .direct_addresses
        .iter()
        .for_each(|addr| println!("\t{addr}"));
    let relay_url = node_addr
        .relay_url
        .expect("Should have a relay URL, assuming a default endpoint setup.");
    println!("node relay server url: {relay_url}\n");
    // Build a `NodeAddr` from the node_id, relay url, and UDP addresses.
    let addr = NodeAddr::from_parts(args.node_id, Some(args.relay_url), args.addrs);

    // Attempt to connect, over the given ALPN.
    // Returns a QUIC connection.
    let conn = endpoint.connect(addr, EXAMPLE_ALPN).await?;
    info!("connected");

    // Send a datagram over the connection.
    let message = format!("{me} is saying 'hello!'");
    conn.send_datagram(message.as_bytes().to_vec().into()).e()?;

    // Read a datagram over the connection.
    let message = conn.read_datagram().await.e()?;
    let message = String::from_utf8(message.into()).e()?;
    println!("received: {message}");

    Ok(())
}


