use crate::primitives::{constraints::SignedConstraints, mev_boost::MevBoostRelay};
use futures::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use tokio_util::sync::CancellationToken;

use tokio::sync::mpsc;
use tracing::{debug, info};

#[derive(Debug)]
pub struct ConstraintSubscriber {
    relays: Vec<MevBoostRelay>,
    global_cancellation: CancellationToken,
}

impl ConstraintSubscriber {
    pub fn new(relays: Vec<MevBoostRelay>, global_cancellation: CancellationToken) -> Self {
        Self {
            relays,
            global_cancellation,
        }
    }

    pub fn spawn(self) -> mpsc::UnboundedReceiver<SignedConstraints> {
        let (send, receive) = mpsc::unbounded_channel();

        let relay = self.relays.first().expect("at least one relay");
        let request = relay.client.build_constraint_stream_request();
        let event_source = EventSource::new(request).unwrap_or_else(|err| {
            panic!("Failed to create EventSource: {:?}", err);
        });

        tokio::spawn(async move {
            let mut event_source = event_source;
            while let Some(event) = event_source.next().await {
                match event {
                    Ok(Event::Message(message)) => {
                        println!("Received SSE message: {:?}", message);
                        if message.event == "signed_constraint" {
                            let data = &message.data;
                            let received_constraints =
                                serde_json::from_str::<Vec<SignedConstraints>>(data)
                                    .unwrap()
                                    .first()
                                    .cloned()
                                    .expect("at least one constraint");
                            info!("Received constraint: {:?}", received_constraints);
                            if send.send(received_constraints).is_err() {
                                debug!("Constraint channel closed");
                                break;
                            }
                        }
                    }
                    Ok(Event::Open) => {
                        println!("SSE connection opened");
                    }
                    Err(err) => {
                        println!("Error receiving SSE event: {:?}", err);
                    }
                }
            }
        });

        self.global_cancellation.cancel();
        receive
    }
}
