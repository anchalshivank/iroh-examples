use anyhow::{Context, Result};
use async_channel::Sender;
use clap::Parser;
use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, NodeId,
};
use n0_future::{task, Stream, StreamExt, boxed::BoxStream};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tracing::info;

/// The main application node that can either accept or initiate connections.
#[derive(Debug, Clone)]
pub struct AppNode {
    router: Router,
    accept_events: broadcast::Sender<AcceptEvent>,
}

impl AppNode {
    pub async fn spawn() -> Result<Self> {
        let endpoint = Endpoint::builder()
            .discovery_n0()
            .alpns(vec![ConnectionHandler::ALPN.to_vec()])
            .bind(true)
            .await?;

        let (event_sender, _) = broadcast::channel(128);
        let handler = ConnectionHandler::new(event_sender.clone());

        let router = Router::builder(endpoint)
            .accept(ConnectionHandler::ALPN, handler)
            .spawn();

        Ok(Self {
            router,
            accept_events: event_sender,
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    pub fn accept_events(&self) -> BoxStream<AcceptEvent> {
        let receiver = self.accept_events.subscribe();
        Box::pin(BroadcastStream::new(receiver).filter_map(|event| event.ok()))
    }

    pub fn connect(
        &self,
        node_id: NodeId,
        payload: String,
    ) -> impl Stream<Item = ConnectEvent> + Unpin {
        let (event_sender, event_receiver) = async_channel::bounded(16);
        let endpoint = self.router.endpoint().clone();

        task::spawn(async move {
            let res = connect(&endpoint, node_id, payload, event_sender.clone()).await;
            let error = res.as_ref().err().map(|e| e.to_string());
            event_sender.send(ConnectEvent::Closed { error }).await.ok();
        });

        Box::pin(event_receiver)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ConnectEvent {
    Connected,
    Sent { bytes_sent: u64 },
    Received { bytes_received: u64 },
    Closed { error: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AcceptEvent {
    Accepted { node_id: NodeId },
    Processed { node_id: NodeId, bytes: u64 },
    Closed { node_id: NodeId, error: Option<String> },
}

#[derive(Debug, Clone)]
pub struct ConnectionHandler {
    event_sender: broadcast::Sender<AcceptEvent>,
}

impl ConnectionHandler {
    pub const ALPN: &'static [u8] = b"iroh/app-protocol/0";

    pub fn new(event_sender: broadcast::Sender<AcceptEvent>) -> Self {
        Self { event_sender }
    }

    async fn handle_connection(
        self,
        connection: Connection,
    ) -> std::result::Result<(), AcceptError> {
        let node_id = connection.remote_node_id()?;

        self.event_sender
            .send(AcceptEvent::Accepted { node_id })
            .ok();

        let res = self.process_streams(&connection).await;
        let error = res.as_ref().err().map(|e| e.to_string());

        self.event_sender
            .send(AcceptEvent::Closed { node_id, error })
            .ok();

        res
    }

    async fn process_streams(&self, connection: &Connection) -> Result<(), AcceptError> {
        let node_id = connection.remote_node_id()?;
        info!("Accepted connection from {node_id}");

        let (mut send, mut recv) = connection.accept_bi().await?;

        let bytes = tokio::io::copy(&mut recv, &mut send).await?;
        info!("Transferred {bytes} bytes");

        self.event_sender
            .send(AcceptEvent::Processed { node_id, bytes })
            .ok();

        send.finish()?;
        connection.closed().await;

        Ok(())
    }
}

impl ProtocolHandler for ConnectionHandler {
    async fn accept(&self, connection: Connection) -> std::result::Result<(), AcceptError> {
        self.clone().handle_connection(connection).await
    }
}

async fn connect(
    endpoint: &Endpoint,
    node_id: NodeId,
    payload: String,
    event_sender: Sender<ConnectEvent>,
) -> Result<()> {
    let connection = endpoint.connect(node_id, ConnectionHandler::ALPN).await?;
    event_sender.send(ConnectEvent::Connected).await?;

    let (mut send_stream, mut recv_stream) = connection.open_bi().await?;

    let send_task = task::spawn({
        let event_sender = event_sender.clone();
        async move {
            let bytes_sent = payload.len();
            send_stream.write_all(payload.as_bytes()).await?;
            event_sender
                .send(ConnectEvent::Sent {
                    bytes_sent: bytes_sent as u64,
                })
                .await?;
            Ok::<_, anyhow::Error>(())
        }
    });

    let bytes_received = tokio::io::copy(&mut recv_stream, &mut tokio::io::sink()).await?;

    connection.close(1u8.into(), b"done");

    event_sender
        .send(ConnectEvent::Received {
            bytes_received,
        })
        .await?;

    send_task.await??;
    Ok(())
}



/// Simple CLI for connecting or hosting a node.
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Optional: connect to a remote node by NodeId.
    #[arg(long)]
    connect: Option<String>,

    /// Optional: message to send to the remote node.
    #[arg(long, default_value = "Hello from CLI")]
    message: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init(); // Optional: enable logging

    let args = Args::parse();

    let node = AppNode::spawn().await?;
    let my_id = node.endpoint().node_id();

    println!("🚀 My Node ID: {my_id}");

    if let Some(peer_id_str) = args.connect {
        let peer_id: NodeId = peer_id_str
            .parse()
            .context("Invalid NodeId format for --connect")?;
        println!("🔗 Connecting to {peer_id}...");

        let mut stream = node.connect(peer_id, args.message);
        while let Some(event) = stream.next().await {
            println!("📡 Event: {:?}", event);
        }
    } else {
        println!("📬 Listening for connections...");
        let mut events = node.accept_events();
        while let Some(event) = events.next().await {
            println!("📥 Incoming: {:?}", event);
        }
    }

    Ok(())
}
