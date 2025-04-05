use std::sync::Arc;

use crate::primitives::{constraints::SignedConstraints, mev_boost::MevBoostRelaySlotInfoProvider};
use ahash::HashMap;
use futures::StreamExt;
use parking_lot::RwLock;
use reqwest_eventsource::{Event, EventSource};
use tokio_util::sync::CancellationToken;

use tokio::sync::mpsc;
use tracing::{debug, error, info};

#[derive(Debug)]
pub struct ConstraintSubscriber {
    relays: Vec<MevBoostRelaySlotInfoProvider>,
    global_cancellation: CancellationToken,
}

impl ConstraintSubscriber {
    pub fn new(
        relays: Vec<MevBoostRelaySlotInfoProvider>,
        global_cancellation: CancellationToken,
    ) -> Self {
        Self {
            relays,
            global_cancellation,
        }
    }

    pub fn subscribe(self) -> mpsc::UnboundedReceiver<SignedConstraints> {
        let (send, receive) = mpsc::unbounded_channel();

        info!("Starting constraint subscriber");

        let relay = self.relays.first().expect("at least one relay");
        let request = relay.client().build_constraint_stream_request();
        let event_source = EventSource::new(request).unwrap_or_else(|err| {
            panic!("Failed to create EventSource: {:?}", err);
        });

        tokio::spawn(async move {
            let mut event_source = event_source;
            while let Some(event) = event_source.next().await {
                match event {
                    Ok(Event::Message(message)) => {
                        if message.event == "signed_constraint" {
                            let data = &message.data;
                            let received_constraints =
                                serde_json::from_str::<Vec<SignedConstraints>>(data)
                                    .unwrap()
                                    // NOTE: Its safe to pick first element because an event can only contain one constraint
                                    .first()
                                    .cloned()
                                    .expect("at least one constraint");
                            if send.send(received_constraints).is_err() {
                                debug!("Constraint channel closed");
                                break;
                            }
                        }
                    }
                    Ok(Event::Open) => {
                        info!("SSE connection opened");
                    }
                    Err(err) => {
                        error!("Error receiving SSE event: {:?}", err);
                    }
                }
            }

            info!("Stopping constraint subscriber");
            self.global_cancellation.cancel();
        });

        receive
    }
}


/// Spawn constraint subscriber
pub async fn spawn_constraint_subscriber(
    constraint_subscriber: Option<ConstraintSubscriber>,
    constraint_store: Arc<RwLock<HashMap<u64, Vec<SignedConstraints>>>>,
    global_cancel: CancellationToken,
) -> eyre::Result<()> {
    if let Some(subscriber) = constraint_subscriber {
        let mut constraint_stream_channel = subscriber.subscribe();
        tokio::spawn({
            async move {
                while let Some(constraint) = constraint_stream_channel.recv().await {
                    constraint_store
                        .write()
                        .entry(constraint.message.slot)
                        .or_default()
                        .push(constraint.clone());
                    info!("Wrote constraint to constraint-store: {:?}", constraint);
                }

                info!("Constraint stream channel closed");
                global_cancel.cancel();
            }
        });
    } else {
        error!("No constraint subscriber provided");
    }

    Ok(())
}