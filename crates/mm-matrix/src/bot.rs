use crate::client::HomeserverClient;

/// A parsed bot command.
#[derive(Debug, Clone)]
pub enum BotCommand {
    /// `!mm live [--title "..."]` -- start a live stream.
    Live { title: Option<String> },
    /// `!mm end` -- end the current stream.
    End,
    /// `!mm status` -- show stream status.
    Status,
    /// `!mm help` -- list available commands.
    Help,
}

/// Parse a message body into a bot command, if it matches.
///
/// Returns `None` if the message is not a bot command.
pub fn parse_command(body: &str) -> Option<BotCommand> {
    let body = body.trim();
    if !body.starts_with("!mm ") && body != "!mm" {
        return None;
    }

    let parts: Vec<&str> = body.splitn(3, ' ').collect();
    match parts.get(1).copied() {
        Some("live") => {
            let title = parts.get(2).and_then(|rest| {
                let rest = rest.trim();
                if rest.starts_with("--title") {
                    rest.strip_prefix("--title")
                        .map(|t| t.trim().trim_matches('"').to_string())
                } else {
                    None
                }
            });
            Some(BotCommand::Live { title })
        }
        Some("end") => Some(BotCommand::End),
        Some("status") => Some(BotCommand::Status),
        Some("help") | None => Some(BotCommand::Help),
        Some(_) => Some(BotCommand::Help),
    }
}

/// Executes parsed bot commands against the Matrix homeserver.
///
/// The executor translates `BotCommand` variants into actions: sending state
/// events for stream lifecycle, querying room state, and returning
/// human-readable response text that will be posted as `m.notice`.
///
/// In v1, actual stream management (SFU sessions, participant tracking) will
/// be wired through the stream service. For now, the executor returns stub
/// responses to prove the command pipeline is functional end-to-end.
#[derive(Clone)]
pub struct BotExecutor {
    #[allow(dead_code)]
    hs_client: HomeserverClient,
}

impl BotExecutor {
    /// Create a new bot executor.
    pub fn new(hs_client: HomeserverClient) -> Self {
        Self { hs_client }
    }

    /// Execute a bot command and return the response text.
    ///
    /// The response should be sent as an `m.notice` message to the room.
    pub async fn execute(
        &self,
        _room_id: &str,
        sender: &str,
        command: BotCommand,
    ) -> Result<String, mm_core::error::MMError> {
        match command {
            BotCommand::Live { title } => {
                // Stub: in future this will create an SFU session, publish a
                // com.matrixmedia.stream state event, and register the widget.
                let title_part = title
                    .as_deref()
                    .map(|t| format!(": {t}"))
                    .unwrap_or_default();
                Ok(format!(
                    "Stream started by {sender}{title_part}\n\
                     (Stream management will be available when SFU integration is complete.)"
                ))
            }
            BotCommand::End => {
                // Stub: in future this will tear down the SFU session and
                // clear the com.matrixmedia.stream state event.
                Ok("Stream ended.".to_string())
            }
            BotCommand::Status => {
                // Stub: in future this will query the stream service for
                // active stream info.
                Ok("No active stream in this room.".to_string())
            }
            BotCommand::Help => Ok("MatrixMedia commands:\n\
                     !mm live [--title \"...\"] - Start a stream\n\
                     !mm end - End stream\n\
                     !mm status - Show status\n\
                     !mm help - This message"
                .to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_live_command() {
        let cmd = parse_command("!mm live").unwrap();
        assert!(matches!(cmd, BotCommand::Live { title: None }));
    }

    #[test]
    fn parse_live_with_title() {
        let cmd = parse_command("!mm live --title \"My Stream\"").unwrap();
        assert!(matches!(cmd, BotCommand::Live { title: Some(t) } if t == "My Stream"));
    }

    #[test]
    fn parse_end_command() {
        let cmd = parse_command("!mm end").unwrap();
        assert!(matches!(cmd, BotCommand::End));
    }

    #[test]
    fn parse_status_command() {
        let cmd = parse_command("!mm status").unwrap();
        assert!(matches!(cmd, BotCommand::Status));
    }

    #[test]
    fn parse_help_command() {
        let cmd = parse_command("!mm help").unwrap();
        assert!(matches!(cmd, BotCommand::Help));
    }

    #[test]
    fn not_a_command() {
        assert!(parse_command("hello world").is_none());
        assert!(parse_command("!other command").is_none());
    }

    #[test]
    fn bare_mm_is_help() {
        let cmd = parse_command("!mm").unwrap();
        assert!(matches!(cmd, BotCommand::Help));
    }

    #[test]
    fn unknown_subcommand_is_help() {
        let cmd = parse_command("!mm foobar").unwrap();
        assert!(matches!(cmd, BotCommand::Help));
    }

    #[tokio::test]
    async fn execute_help_returns_commands_list() {
        let client = HomeserverClient::new(
            "http://localhost:8008".to_string(),
            "test-token".to_string(),
            "@mmbot:localhost".to_string(),
        );
        let executor = BotExecutor::new(client);
        let result = executor
            .execute("!test:localhost", "@alice:localhost", BotCommand::Help)
            .await
            .unwrap();

        assert!(result.contains("!mm live"));
        assert!(result.contains("!mm end"));
        assert!(result.contains("!mm status"));
        assert!(result.contains("!mm help"));
    }

    #[tokio::test]
    async fn execute_live_includes_sender() {
        let client = HomeserverClient::new(
            "http://localhost:8008".to_string(),
            "test-token".to_string(),
            "@mmbot:localhost".to_string(),
        );
        let executor = BotExecutor::new(client);
        let result = executor
            .execute(
                "!test:localhost",
                "@alice:localhost",
                BotCommand::Live {
                    title: Some("Test Stream".to_string()),
                },
            )
            .await
            .unwrap();

        assert!(result.contains("@alice:localhost"));
        assert!(result.contains("Test Stream"));
    }

    #[tokio::test]
    async fn execute_status_returns_no_active() {
        let client = HomeserverClient::new(
            "http://localhost:8008".to_string(),
            "test-token".to_string(),
            "@mmbot:localhost".to_string(),
        );
        let executor = BotExecutor::new(client);
        let result = executor
            .execute("!test:localhost", "@alice:localhost", BotCommand::Status)
            .await
            .unwrap();

        assert!(result.contains("No active stream"));
    }

    #[tokio::test]
    async fn execute_end_returns_ended() {
        let client = HomeserverClient::new(
            "http://localhost:8008".to_string(),
            "test-token".to_string(),
            "@mmbot:localhost".to_string(),
        );
        let executor = BotExecutor::new(client);
        let result = executor
            .execute("!test:localhost", "@alice:localhost", BotCommand::End)
            .await
            .unwrap();

        assert!(result.contains("Stream ended"));
    }
}
