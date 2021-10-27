use futures_util::{Future, SinkExt, StreamExt};
use hyper::{
    server::conn::AddrStream,
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use log::{debug, error, info, trace, warn};
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
};
use tokio_tungstenite::{
    tungstenite::{protocol::Role, Error, Message},
    WebSocketStream,
};
use twilight_gateway::shard::raw_message::Message as TwilightMessage;

use std::{
    convert::Infallible,
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use crate::{
    deserializer::GatewayEventDeserializer, model::Identify, state::State, upgrade::server_upgrade,
    zlib_sys::Compressor,
};

const HELLO: &str = r#"{"t":null,"s":null,"op":10,"d":{"heartbeat_interval":41250}}"#;
const HEARTBEAT_ACK: &str = r#"{"t":null,"s":null,"op":11,"d":null}"#;
const INVALID_SESSION: &str = r#"{"t":null,"s":null,"op":9,"d":false}"#;

async fn forward_shard(
    mut shard_id_rx: UnboundedReceiver<u64>,
    stream_writer: UnboundedSender<Message>,
    mut shard_send_rx: UnboundedReceiver<TwilightMessage>,
    state: State,
) {
    // Wait for the client's IDENTIFY to finish and acquire the shard ID
    let shard_id = shard_id_rx.recv().await.unwrap();
    // Get a handle to the shard
    let shard_status = state.shards[shard_id as usize].clone();

    // Fake sequence number for the client. We update packets to overwrite it
    let mut seq: usize = 0;

    // Subscribe to events for this shard
    let mut event_receiver = shard_status.events.subscribe();

    debug!("Starting to send events to client");

    // If there is no READY received for the shard yet, wait for it
    if shard_status.ready.get().is_none() {
        shard_status.ready_set.notified().await;
    }

    // Get a fake ready payload to send to the client
    let ready_payload = shard_status
        .guilds
        .get_ready_payload(shard_status.ready.get().unwrap().clone(), &mut seq);

    if let Ok(serialized) = simd_json::to_string(&ready_payload) {
        debug!("Sending newly created READY");
        let _res = stream_writer.send(Message::Text(serialized));
    };

    // Send GUILD_CREATE/GUILD_DELETEs based on guild availability
    for payload in shard_status.guilds.get_guild_payloads(&mut seq) {
        if let Ok(serialized) = simd_json::to_string(&payload) {
            trace!("Sending newly created GUILD_CREATE/GUILD_DELETE payload");
            let _res = stream_writer.send(Message::Text(serialized));
        };
    }

    loop {
        tokio::select! {
            Ok(event) = event_receiver.recv() => {
                // A payload has arrived to be sent to the client
                let (mut payload, sequence) = event;

                // Overwrite the sequence number
                if let Some((_, sequence_range)) = sequence {
                    seq += 1;
                    payload.replace_range(sequence_range, &seq.to_string());
                }

                let _res = stream_writer.send(Message::Text(payload));
            },
            Some(command) = shard_send_rx.recv() => {
                // Has to be done here because else shard would be moved
                let _res = shard_status.shard.send(command).await;
            },
        };
    }
}

pub async fn handle_client<S: 'static + AsyncRead + AsyncWrite + Unpin + Send>(
    stream: S,
    state: State,
    use_zlib: Arc<AtomicBool>,
) -> Result<(), Error> {
    let mut compress = Compressor::new(15);

    let stream = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;

    let (mut sink, mut stream) = stream.split();

    // Write all messages from a queue to the sink
    let (stream_writer, mut stream_receiver) = unbounded_channel::<Message>();

    let use_zlib_clone = use_zlib.clone();
    let sink_task = tokio::spawn(async move {
        while let Some(msg) = stream_receiver.recv().await {
            trace!("Sending {:?}", msg);

            if use_zlib_clone.load(Ordering::Relaxed) {
                let mut compressed = Vec::with_capacity(msg.len());
                compress
                    .compress(&msg.into_data(), &mut compressed)
                    .unwrap();

                sink.send(Message::Binary(compressed)).await?;
            } else {
                sink.send(msg).await?;
            }
        }

        Ok::<(), Error>(())
    });

    // Set up a task that will dump all the messages from the shard to the client
    let (shard_id_tx, shard_id_rx) = unbounded_channel();
    let (shard_send_tx, shard_send_rx) = unbounded_channel();

    let shard_forward_task = tokio::spawn(forward_shard(
        shard_id_rx,
        stream_writer.clone(),
        shard_send_rx,
        state.clone(),
    ));

    let _res = stream_writer.send(Message::Text(HELLO.to_string()));

    while let Some(Ok(msg)) = stream.next().await {
        let data = msg.into_data();
        let mut payload = unsafe { String::from_utf8_unchecked(data) };

        let deserializer = match GatewayEventDeserializer::from_json(&payload) {
            Some(deserializer) => deserializer,
            None => continue,
        };

        match deserializer.op() {
            1 => {
                trace!("Sending heartbeat ACK");
                let _res = stream_writer.send(Message::Text(HEARTBEAT_ACK.to_string()));
            }
            2 => {
                debug!("Client is identifying");

                let identify: Identify = match simd_json::from_str(&mut payload) {
                    Ok(identify) => identify,
                    Err(e) => {
                        warn!("Invalid identify payload: {:?}", e);
                        continue;
                    }
                };

                let (shard_id, shard_count) = (identify.d.shard[0], identify.d.shard[1]);

                if shard_count != state.shard_count {
                    warn!("Shard count from client identify mismatched, disconnecting");
                    break;
                }

                if shard_id >= shard_count {
                    warn!("Shard ID from client is out of range, disconnecting");
                    break;
                }

                trace!("Shard ID is {:?}", shard_id);

                if let Some(compress) = identify.d.compress {
                    use_zlib.store(compress, Ordering::Relaxed);
                }

                let _res = shard_id_tx.send(shard_id);
            }
            6 => {
                debug!("Client is resuming: {:?}", payload);
                // TODO: Keep track of session IDs and choose one that we have active
                // This would be unnecessary if people forked their clients though
                // For now, send an invalid session so they use identify instead
                let _res = stream_writer.send(Message::text(INVALID_SESSION.to_string()));
            }
            _ => {
                trace!("Sending {:?} to Discord directly", payload);
                let _res = shard_send_tx.send(TwilightMessage::Text(payload));
            }
        }
    }

    sink_task.abort();
    shard_forward_task.abort();

    Ok(())
}

fn handle_metrics(
    handle: Arc<PrometheusHandle>,
) -> Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send>> {
    Box::pin(async move {
        Ok(Response::builder()
            .body(Body::from(handle.render()))
            .unwrap())
    })
}

pub async fn run_server(
    port: u16,
    state: State,
    metrics_handle: Arc<PrometheusHandle>,
) -> Result<(), Error> {
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();

    let service = make_service_fn(move |addr: &AddrStream| {
        trace!("Connection from: {:?}", addr);

        let state = state.clone();
        let metrics_handle = metrics_handle.clone();

        async move {
            Ok::<_, Infallible>(service_fn(move |incoming: Request<Body>| {
                if incoming.uri().path() == "/metrics" {
                    // Reply with metrics on /metrics
                    handle_metrics(metrics_handle.clone())
                } else {
                    // On anything else just provide the websocket server
                    Box::pin(server_upgrade(incoming, state.clone()))
                }
            }))
        }
    });

    let server = Server::bind(&addr).serve(service);

    info!("Listening on {}", addr);

    if let Err(why) = server.await {
        error!("Fatal server error: {}", why);
    }

    Ok(())
}
