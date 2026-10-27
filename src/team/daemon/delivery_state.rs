//! Message delivery bookkeeping owned by the daemon: messages waiting for a
//! member to become ready, shim messages awaiting acknowledgement, deliveries
//! that failed and will be retried, and outbound user channels.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use tracing::{info, warn};

use super::{PendingAckInboxMessage, PendingShimAck};
use crate::team::comms::Channel;
use crate::team::delivery::{FailedDelivery, PendingMessage};
use crate::team::inbox;

pub(in crate::team) struct DeliveryState {
    /// Messages deferred because the target agent was still starting.
    /// Drained automatically when the agent transitions to ready.
    pub(in crate::team) pending_queue: HashMap<String, Vec<PendingMessage>>,
    /// Shim messages accepted by the daemon-side channel but not yet
    /// acknowledged by a MessageDelivered/DeliveryFailed event.
    pub(in crate::team) pending_shim_acks: HashMap<String, PendingShimAck>,
    /// Deliveries that failed and are retried on later ticks.
    pub(in crate::team) failed: Vec<FailedDelivery>,
    /// Outbound channels for user roles, keyed by role name.
    pub(in crate::team) channels: HashMap<String, Box<dyn Channel>>,
}

impl DeliveryState {
    pub(in crate::team) fn new(channels: HashMap<String, Box<dyn Channel>>) -> Self {
        Self {
            pending_queue: HashMap::new(),
            pending_shim_acks: HashMap::new(),
            failed: Vec::new(),
            channels,
        }
    }

    /// True when an unacknowledged shim message carries inbox message `inbox_id`.
    pub(in crate::team) fn pending_ack_contains_inbox_id(&self, inbox_id: &str) -> bool {
        self.pending_shim_acks.values().any(|pending| {
            pending
                .inbox_messages
                .iter()
                .any(|message| message.id == inbox_id)
        })
    }

    pub(in crate::team) fn pending_ack_contains_any_inbox_id(
        &self,
        messages: &[inbox::InboxMessage],
    ) -> bool {
        messages
            .iter()
            .any(|message| self.pending_ack_contains_inbox_id(&message.id))
    }

    pub(in crate::team) fn enqueue_pending_shim_ack(
        &mut self,
        message_id: String,
        recipient: &str,
        from: &str,
        body: &str,
        inbox_messages: Vec<PendingAckInboxMessage>,
    ) {
        self.pending_shim_acks.insert(
            message_id,
            PendingShimAck {
                recipient: recipient.to_string(),
                from: from.to_string(),
                body: body.to_string(),
                inbox_messages,
            },
        );
    }

    /// Forget a failed delivery once the same message got through.
    pub(in crate::team) fn clear_failed(&mut self, recipient: &str, from: &str, body: &str) {
        self.failed.retain(|delivery| {
            delivery.recipient != recipient || delivery.from != from || delivery.body != body
        });
    }

    /// Move messages queued longer than `max_age_secs` to the recipient's
    /// inbox under `inbox_root`, collapsing bursts from one sender into a
    /// digest. `max_age_secs` must be non-zero.
    pub(in crate::team) fn expire_stale(
        &mut self,
        max_age_secs: u64,
        inbox_root: &Path,
        tiered: bool,
    ) {
        let max_age = Duration::from_secs(max_age_secs);

        let recipients: Vec<String> = self.pending_queue.keys().cloned().collect();

        for recipient in recipients {
            let Some(messages) = self.pending_queue.get_mut(&recipient) else {
                continue;
            };

            let mut expired: Vec<PendingMessage> = Vec::new();
            let mut kept = Vec::new();
            for msg in messages.drain(..) {
                if msg.queued_at.elapsed() >= max_age {
                    expired.push(msg);
                } else {
                    kept.push(msg);
                }
            }

            if !expired.is_empty() {
                let total_expired = expired.len();
                warn!(
                    recipient = recipient.as_str(),
                    count = total_expired,
                    max_age_secs,
                    "expiring stale pending messages to inbox fallback"
                );

                // Group expired messages by sender and deliver digests
                // to avoid flooding the recipient with hundreds of individual messages.
                const DIGEST_THRESHOLD: usize = 3;
                let mut by_sender: std::collections::HashMap<String, Vec<&PendingMessage>> =
                    std::collections::HashMap::new();
                for msg in &expired {
                    by_sender.entry(msg.from.clone()).or_default().push(msg);
                }

                for (sender, sender_msgs) in &by_sender {
                    if sender_msgs.len() <= DIGEST_THRESHOLD {
                        // Few messages — deliver individually
                        for msg in sender_msgs {
                            let inbox_msg =
                                inbox::InboxMessage::new_send(&msg.from, &recipient, &msg.body);
                            if let Err(error) = crate::team::inbox_tiered::deliver_flag_aware(
                                inbox_root, &inbox_msg, tiered,
                            ) {
                                warn!(
                                    from = msg.from.as_str(),
                                    to = recipient.as_str(),
                                    error = %error,
                                    "failed to deliver expired pending message to inbox"
                                );
                            }
                        }
                    } else {
                        // Many messages — collapse into a single digest
                        let oldest_age_secs = sender_msgs
                            .iter()
                            .map(|m| m.queued_at.elapsed().as_secs())
                            .max()
                            .unwrap_or(0);
                        let newest = sender_msgs.last().unwrap();
                        let newest_preview: String = newest
                            .body
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(200)
                            .collect();
                        let digest = format!(
                            "[digest] {} messages from {} expired after {}s (oldest: {}s ago). Most recent:\n{}",
                            sender_msgs.len(),
                            sender,
                            max_age_secs,
                            oldest_age_secs,
                            newest_preview,
                        );
                        let inbox_msg = inbox::InboxMessage::new_send(sender, &recipient, &digest);
                        if let Err(error) = crate::team::inbox_tiered::deliver_flag_aware(
                            inbox_root, &inbox_msg, tiered,
                        ) {
                            warn!(
                                from = sender.as_str(),
                                to = recipient.as_str(),
                                error = %error,
                                "failed to deliver digest message to inbox"
                            );
                        }
                        info!(
                            from = sender.as_str(),
                            to = recipient.as_str(),
                            count = sender_msgs.len(),
                            "collapsed expired messages into digest"
                        );
                    }
                }
            }

            if kept.is_empty() {
                self.pending_queue.remove(&recipient);
            } else {
                *self.pending_queue.get_mut(&recipient).unwrap() = kept;
            }
        }
    }
}
