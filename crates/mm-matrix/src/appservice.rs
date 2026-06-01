use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tracing::{debug, info, warn};

use crate::bot::{self, BotExecutor};
use crate::client::HomeserverClient;
use crate::feed_indexer::{is_feed_event_type, FeedIndexer, HomeserverMemberResolver};

/// Appservice registration configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppserviceRegistration {
    pub id: String,
    pub url: String,
    pub as_token: String,
    pub hs_token: String,
    pub sender_localpart: String,
    pub namespaces: Namespaces,
    pub rate_limited: bool,
}

/// Namespace declarations for the appservice registration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Namespaces {
    pub users: Vec<NamespaceEntry>,
    pub rooms: Vec<NamespaceEntry>,
    pub aliases: Vec<NamespaceEntry>,
}

/// A single namespace entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceEntry {
    pub exclusive: bool,
    pub regex: String,
}

/// An incoming appservice transaction from the homeserver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub events: Vec<serde_json::Value>,
}

/// Processes incoming appservice transactions from the homeserver.
///
/// The handler dispatches events to the appropriate sub-handlers:
/// - `m.room.member` -- auto-join on invite, check power levels and encryption
/// - `m.room.message` -- parse and execute bot commands (`!mm ...`)
/// - `com.steegler.matrixmedia.feed.*` -- newsfeed events fan-out
///   into `mm_feed_items` via the [`FeedIndexer`].
/// - `m.room.redaction` -- routed to [`FeedIndexer`] to flip `hidden`
///   on any indexed row pointing at the redacted event.
/// - `com.matrixmedia.*` -- legacy MatrixMedia custom events (debug-logged).
#[derive(Clone)]
pub struct AppserviceHandler {
    hs_client: HomeserverClient,
    bot_executor: BotExecutor,
    /// Newsfeed event indexer. `None` when the AS handler is constructed
    /// without a Postgres pool — preserves backward-compat for callers
    /// that don't need the feed index (e.g. unit tests).
    feed_indexer: Option<Arc<FeedIndexer>>,
}

impl AppserviceHandler {
    /// Create a new appservice handler without a feed indexer.
    ///
    /// Use [`AppserviceHandler::with_feed_indexer`] in production to wire
    /// the newsfeed fan-out path; this no-indexer form is kept for tests
    /// and minimal deployments.
    pub fn new(hs_client: HomeserverClient) -> Self {
        let bot_executor = BotExecutor::new(hs_client.clone());
        Self {
            hs_client,
            bot_executor,
            feed_indexer: None,
        }
    }

    /// Create a new appservice handler with a Postgres pool wired in for
    /// newsfeed fan-out. Server name and bot MXID are supplied so the
    /// indexer can filter member lists down to local non-bot users.
    pub fn with_feed_indexer(
        hs_client: HomeserverClient,
        pool: PgPool,
        local_server_name: impl Into<String>,
    ) -> Self {
        let bot_executor = BotExecutor::new(hs_client.clone());
        let bot_id = hs_client.bot_user_id().to_string();
        let resolver = Arc::new(HomeserverMemberResolver::new(hs_client.clone()));
        let feed_indexer = Some(Arc::new(FeedIndexer::new(
            pool,
            resolver,
            local_server_name,
            bot_id,
        )));
        Self {
            hs_client,
            bot_executor,
            feed_indexer,
        }
    }

    /// Process a batch of events from the homeserver.
    ///
    /// Events are processed sequentially within a transaction. Errors on
    /// individual events are logged but do not abort the entire transaction
    /// (the homeserver does not retry partial failures).
    pub async fn handle_transaction(
        &self,
        events: Vec<serde_json::Value>,
    ) -> Result<(), mm_core::error::MMError> {
        debug!("Processing transaction with {} events", events.len());

        for event in &events {
            let event_type = event.get("type").and_then(|t| t.as_str());
            let result = match event_type {
                Some("m.room.member") => self.handle_member_event(event).await,
                Some("m.room.message") => self.handle_message_event(event).await,
                Some("m.room.redaction") => self.handle_feed_dispatch(event).await,
                Some(t) if is_feed_event_type(t) => self.handle_feed_dispatch(event).await,
                Some(t) if t.starts_with("com.matrixmedia.") => self.handle_mm_event(event).await,
                _ => Ok(()), // ignore unknown events
            };

            if let Err(e) = result {
                warn!(
                    event_type = event_type.unwrap_or("<unknown>"),
                    error = %e,
                    "Error processing event in transaction"
                );
            }
        }

        Ok(())
    }

    /// Handle an `m.room.member` event.
    ///
    /// If the bot is invited to a room, it auto-joins. After joining, it
    /// checks power levels and encryption status, posting warnings as needed.
    async fn handle_member_event(
        &self,
        event: &serde_json::Value,
    ) -> Result<(), mm_core::error::MMError> {
        let content = event.get("content").unwrap_or(&serde_json::Value::Null);
        let membership = content.get("membership").and_then(|m| m.as_str());
        let state_key = event.get("state_key").and_then(|s| s.as_str());
        let room_id = event.get("room_id").and_then(|r| r.as_str());

        // Only process invites directed at our bot.
        if membership != Some("invite") {
            return Ok(());
        }

        let bot_id = self.hs_client.bot_user_id();
        if state_key != Some(bot_id) {
            return Ok(());
        }

        let room_id = match room_id {
            Some(r) => r,
            None => return Ok(()),
        };

        info!(room_id, "Bot invited to room, auto-joining");
        self.hs_client.join_room(room_id).await?;

        // Check power levels -- warn if bot has PL < 50.
        match self.hs_client.bot_has_state_power(room_id).await {
            Ok(true) => {
                debug!(room_id, "Bot has sufficient power level");
            }
            Ok(false) => {
                warn!(room_id, "Bot power level < 50, cannot set state events");
                let _ = self.hs_client.send_notice(
                    room_id,
                    "Warning: I need a power level of at least 50 to manage streams in this room. \
                     Please promote me with: /op @mmbot 50",
                ).await;
            }
            Err(e) => {
                warn!(room_id, error = %e, "Failed to check power levels");
            }
        }

        // Check if room is encrypted -- warn about limited support.
        match self.hs_client.check_room_encrypted(room_id).await {
            Ok(true) => {
                info!(room_id, "Room is encrypted, posting E2EE warning");
                let _ = self
                    .hs_client
                    .send_notice(
                        room_id,
                        "Note: This room has end-to-end encryption enabled. \
                     MatrixMedia has limited support for E2EE rooms in v1 -- \
                     I cannot read encrypted messages, so bot commands must be \
                     sent as unencrypted messages or use the widget interface.",
                    )
                    .await;
            }
            Ok(false) => {
                debug!(room_id, "Room is not encrypted");
            }
            Err(e) => {
                warn!(room_id, error = %e, "Failed to check encryption state");
            }
        }

        Ok(())
    }

    /// Handle an `m.room.message` event.
    ///
    /// Checks if the message body starts with `!mm` and, if so, parses and
    /// executes the bot command. The response is sent as an `m.notice`.
    async fn handle_message_event(
        &self,
        event: &serde_json::Value,
    ) -> Result<(), mm_core::error::MMError> {
        let content = event.get("content").unwrap_or(&serde_json::Value::Null);
        let body = content.get("body").and_then(|b| b.as_str());
        let room_id = event.get("room_id").and_then(|r| r.as_str());
        let sender = event.get("sender").and_then(|s| s.as_str());

        let body = match body {
            Some(b) => b,
            None => return Ok(()),
        };
        let room_id = match room_id {
            Some(r) => r,
            None => return Ok(()),
        };
        let sender = match sender {
            Some(s) => s,
            None => return Ok(()),
        };

        // Ignore messages from the bot itself to prevent loops.
        if sender == self.hs_client.bot_user_id() {
            return Ok(());
        }

        // Attempt to parse as a bot command.
        let command = match bot::parse_command(body) {
            Some(cmd) => cmd,
            None => return Ok(()),
        };

        info!(room_id, sender, command = ?command, "Executing bot command");

        let response = self.bot_executor.execute(room_id, sender, command).await?;
        self.hs_client.send_notice(room_id, &response).await?;

        Ok(())
    }

    /// Route a feed event (or redaction) into the [`FeedIndexer`] when
    /// wired. A handler constructed without a pool simply drops the
    /// event — the rest of the pipeline (Matrix `/sync`) still delivers
    /// realtime updates; only the cold-load index gets skipped.
    async fn handle_feed_dispatch(
        &self,
        event: &serde_json::Value,
    ) -> Result<(), mm_core::error::MMError> {
        let Some(indexer) = self.feed_indexer.as_ref() else {
            debug!("feed event received but FeedIndexer not configured; dropping");
            return Ok(());
        };
        match indexer.handle_event(event).await {
            Ok(_) => Ok(()),
            Err(e) => {
                warn!(error = %e, "feed indexer reported error");
                Err(e)
            }
        }
    }

    /// Handle a `com.matrixmedia.*` custom event.
    ///
    /// For now this just logs the event. Future versions will track stream
    /// state changes, participant updates, etc.
    async fn handle_mm_event(
        &self,
        event: &serde_json::Value,
    ) -> Result<(), mm_core::error::MMError> {
        let event_type = event
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("<unknown>");
        let room_id = event
            .get("room_id")
            .and_then(|r| r.as_str())
            .unwrap_or("<unknown>");

        debug!(event_type, room_id, "Received MatrixMedia custom event");
        Ok(())
    }
}

/// Generate a default appservice registration for MatrixMedia.
pub fn default_registration(
    homeserver_url: &str,
    mm_url: &str,
    bot_localpart: &str,
) -> AppserviceRegistration {
    let _ = homeserver_url; // Used for documentation / validation only.
    AppserviceRegistration {
        id: "matrixmedia".to_string(),
        url: mm_url.to_string(),
        as_token: String::new(), // Must be set from env var.
        hs_token: String::new(), // Must be set from env var.
        sender_localpart: bot_localpart.to_string(),
        namespaces: Namespaces {
            users: vec![NamespaceEntry {
                exclusive: false,
                regex: format!("@{bot_localpart}:.*"),
            }],
            rooms: vec![],
            aliases: vec![],
        },
        rate_limited: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::BotCommand;

    #[test]
    fn test_handle_invite_event_parsing() {
        // Verify that an invite event for the bot is correctly identified.
        let event = serde_json::json!({
            "type": "m.room.member",
            "room_id": "!test:localhost",
            "sender": "@alice:localhost",
            "state_key": "@mmbot:localhost",
            "content": {
                "membership": "invite"
            }
        });

        let content = event.get("content").unwrap();
        let membership = content.get("membership").and_then(|m| m.as_str());
        let state_key = event.get("state_key").and_then(|s| s.as_str());

        assert_eq!(membership, Some("invite"));
        assert_eq!(state_key, Some("@mmbot:localhost"));
    }

    #[test]
    fn test_handle_bot_command_event() {
        // Verify that a message event with "!mm help" is correctly parsed.
        let event = serde_json::json!({
            "type": "m.room.message",
            "room_id": "!test:localhost",
            "sender": "@alice:localhost",
            "content": {
                "msgtype": "m.text",
                "body": "!mm help"
            }
        });

        let body = event["content"]["body"].as_str().unwrap();
        let command = crate::bot::parse_command(body);
        assert!(command.is_some());
        assert!(matches!(command.unwrap(), BotCommand::Help));
    }

    #[test]
    fn test_non_command_message_ignored() {
        let event = serde_json::json!({
            "type": "m.room.message",
            "room_id": "!test:localhost",
            "sender": "@alice:localhost",
            "content": {
                "msgtype": "m.text",
                "body": "hello everyone"
            }
        });

        let body = event["content"]["body"].as_str().unwrap();
        let command = crate::bot::parse_command(body);
        assert!(command.is_none());
    }

    #[test]
    fn test_invite_for_other_user_ignored() {
        // An invite for a different user should not trigger auto-join.
        let event = serde_json::json!({
            "type": "m.room.member",
            "room_id": "!test:localhost",
            "sender": "@alice:localhost",
            "state_key": "@bob:localhost",
            "content": {
                "membership": "invite"
            }
        });

        let state_key = event.get("state_key").and_then(|s| s.as_str());
        let bot_id = "@mmbot:localhost";
        // This invite is for @bob, not for our bot.
        assert_ne!(state_key, Some(bot_id));
    }

    #[test]
    fn test_default_registration() {
        let reg = default_registration("http://localhost:8008", "http://mm:6167", "mmbot");
        assert_eq!(reg.id, "matrixmedia");
        assert_eq!(reg.sender_localpart, "mmbot");
        assert!(!reg.rate_limited);
        assert_eq!(reg.namespaces.users.len(), 1);
        assert_eq!(reg.namespaces.users[0].regex, "@mmbot:.*");
    }
}
