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
    /// `!mm setup` -- post payment onboarding link.
    Setup,
    /// `!mm donate <amount> [message]` -- start a donation checkout.
    Donate {
        amount: u32,
        message: Option<String>,
    },
    /// `!mm tiers` -- show available subscription tiers.
    Tiers,
    /// `!mm subscribe` -- post a subscribe link.
    Subscribe,
    /// `!mm gate <tier_level>` -- set content gate (host only).
    Gate { level: u32 },
    /// `!mm ungate` -- remove content gate (host only).
    Ungate,
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
        Some("setup") => Some(BotCommand::Setup),
        Some("donate") => {
            // Parse: !mm donate <amount> [message]
            let rest = parts.get(2).map(|s| s.trim()).unwrap_or("");
            if rest.is_empty() {
                return Some(BotCommand::Help);
            }
            // Split into amount and optional message.
            let (amount_str, message) = match rest.split_once(' ') {
                Some((a, m)) => (a.trim(), Some(m.trim().to_string())),
                None => (rest, None),
            };
            // Strip leading '$' if present.
            let amount_str = amount_str.strip_prefix('$').unwrap_or(amount_str);
            match amount_str.parse::<u32>() {
                Ok(0) => Some(BotCommand::Help),
                Ok(amount) => Some(BotCommand::Donate { amount, message }),
                Err(_) => Some(BotCommand::Help),
            }
        }
        Some("tiers") => Some(BotCommand::Tiers),
        Some("subscribe") => Some(BotCommand::Subscribe),
        Some("gate") => {
            let rest = parts.get(2).map(|s| s.trim()).unwrap_or("");
            if rest.is_empty() {
                return Some(BotCommand::Help);
            }
            // Take the first token as the level.
            let level_str = rest.split_whitespace().next().unwrap_or("");
            match level_str.parse::<u32>() {
                Ok(level) if (1..=5).contains(&level) => Some(BotCommand::Gate { level }),
                _ => Some(BotCommand::Help),
            }
        }
        Some("ungate") => Some(BotCommand::Ungate),
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
    _hs_client: HomeserverClient,
}

impl BotExecutor {
    /// Create a new bot executor.
    pub fn new(hs_client: HomeserverClient) -> Self {
        Self {
            _hs_client: hs_client,
        }
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
            BotCommand::Setup => {
                // Stub: the actual onboarding URL will be provided by
                // mm-payment at runtime once the monetization service is
                // wired in.
                Ok("Set up payments for your streams: \
                     <onboarding URL will be provided by the payment service>"
                    .to_string())
            }
            BotCommand::Donate { amount, message } => {
                // Stub: the actual checkout URL will be provided by
                // mm-payment at runtime.
                let msg_part = message
                    .as_deref()
                    .map(|m| format!(" with message: \"{m}\""))
                    .unwrap_or_default();
                Ok(format!(
                    "Donate ${amount} to the stream host{msg_part}: \
                     <checkout URL will be provided by the payment service>"
                ))
            }
            BotCommand::Tiers => {
                // Stub: in future this will query the room's
                // com.matrixmedia.subscription_tiers state event and
                // display the tiers to the user.
                Ok("No subscription tiers configured in this room. \
                    The stream host can set them up via the dashboard."
                    .to_string())
            }
            BotCommand::Subscribe => {
                // Stub: the actual subscribe URL will be generated by
                // mm-payment at runtime.
                Ok("Subscribe to this creator: \
                     <subscribe URL will be provided by the payment service>"
                    .to_string())
            }
            BotCommand::Gate { level } => {
                // Stub: in future this will send a
                // com.matrixmedia.content_gate state event. Host-only
                // permission check will be done by the caller.
                Ok(format!(
                    "Content gate set to Tier {level}. \
                     Only subscribers at Tier {level} or above can view the stream."
                ))
            }
            BotCommand::Ungate => {
                // Stub: in future this will clear the
                // com.matrixmedia.content_gate state event.
                Ok("Content gate removed. Stream is now open to all viewers.".to_string())
            }
            BotCommand::Help => Ok("MatrixMedia commands:\n\
                     !mm live [--title \"...\"] - Start a stream\n\
                     !mm end - End stream\n\
                     !mm status - Show status\n\
                     !mm setup - Set up payments\n\
                     !mm donate <amount> [message] - Donate to streamer\n\
                     !mm tiers - Show subscription tiers\n\
                     !mm subscribe - Get subscribe link\n\
                     !mm gate <1-5> - Set content gate (host only)\n\
                     !mm ungate - Remove content gate (host only)\n\
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

    #[test]
    fn parse_setup_command() {
        let cmd = parse_command("!mm setup").unwrap();
        assert!(matches!(cmd, BotCommand::Setup));
    }

    #[test]
    fn parse_donate_command() {
        let cmd = parse_command("!mm donate 5").unwrap();
        assert!(matches!(
            cmd,
            BotCommand::Donate {
                amount: 5,
                message: None
            }
        ));
    }

    #[test]
    fn parse_donate_with_dollar_sign() {
        let cmd = parse_command("!mm donate $10").unwrap();
        assert!(matches!(
            cmd,
            BotCommand::Donate {
                amount: 10,
                message: None
            }
        ));
    }

    #[test]
    fn parse_donate_with_message() {
        let cmd = parse_command("!mm donate 5 Great stream!").unwrap();
        assert!(
            matches!(cmd, BotCommand::Donate { amount: 5, message: Some(m) } if m == "Great stream!")
        );
    }

    #[test]
    fn parse_donate_no_amount_is_help() {
        let cmd = parse_command("!mm donate").unwrap();
        assert!(matches!(cmd, BotCommand::Help));
    }

    #[test]
    fn parse_donate_invalid_amount_is_help() {
        let cmd = parse_command("!mm donate abc").unwrap();
        assert!(matches!(cmd, BotCommand::Help));
    }

    #[test]
    fn parse_donate_zero_is_help() {
        let cmd = parse_command("!mm donate 0").unwrap();
        assert!(matches!(cmd, BotCommand::Help));
    }

    #[tokio::test]
    async fn execute_setup_returns_placeholder() {
        let client = HomeserverClient::new(
            "http://localhost:8008".to_string(),
            "test-token".to_string(),
            "@mmbot:localhost".to_string(),
        );
        let executor = BotExecutor::new(client);
        let result = executor
            .execute("!test:localhost", "@alice:localhost", BotCommand::Setup)
            .await
            .unwrap();

        assert!(result.contains("Set up payments"));
    }

    #[tokio::test]
    async fn execute_donate_returns_placeholder() {
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
                BotCommand::Donate {
                    amount: 10,
                    message: Some("Keep it up!".to_string()),
                },
            )
            .await
            .unwrap();

        assert!(result.contains("Donate $10"));
        assert!(result.contains("Keep it up!"));
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

    // ---------------------------------------------------------------
    // Subscription / gate command parsing tests
    // ---------------------------------------------------------------

    #[test]
    fn parse_tiers_command() {
        let cmd = parse_command("!mm tiers").unwrap();
        assert!(matches!(cmd, BotCommand::Tiers));
    }

    #[test]
    fn parse_subscribe_command() {
        let cmd = parse_command("!mm subscribe").unwrap();
        assert!(matches!(cmd, BotCommand::Subscribe));
    }

    #[test]
    fn parse_gate_command() {
        let cmd = parse_command("!mm gate 2").unwrap();
        assert!(matches!(cmd, BotCommand::Gate { level: 2 }));
    }

    #[test]
    fn parse_gate_command_level_5() {
        let cmd = parse_command("!mm gate 5").unwrap();
        assert!(matches!(cmd, BotCommand::Gate { level: 5 }));
    }

    #[test]
    fn parse_gate_no_level_is_help() {
        let cmd = parse_command("!mm gate").unwrap();
        assert!(matches!(cmd, BotCommand::Help));
    }

    #[test]
    fn parse_gate_invalid_level_is_help() {
        let cmd = parse_command("!mm gate abc").unwrap();
        assert!(matches!(cmd, BotCommand::Help));
    }

    #[test]
    fn parse_gate_level_zero_is_help() {
        let cmd = parse_command("!mm gate 0").unwrap();
        assert!(matches!(cmd, BotCommand::Help));
    }

    #[test]
    fn parse_gate_level_too_high_is_help() {
        let cmd = parse_command("!mm gate 6").unwrap();
        assert!(matches!(cmd, BotCommand::Help));
    }

    #[test]
    fn parse_ungate_command() {
        let cmd = parse_command("!mm ungate").unwrap();
        assert!(matches!(cmd, BotCommand::Ungate));
    }

    // ---------------------------------------------------------------
    // Subscription / gate executor tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn execute_tiers_returns_placeholder() {
        let client = HomeserverClient::new(
            "http://localhost:8008".to_string(),
            "test-token".to_string(),
            "@mmbot:localhost".to_string(),
        );
        let executor = BotExecutor::new(client);
        let result = executor
            .execute("!test:localhost", "@alice:localhost", BotCommand::Tiers)
            .await
            .unwrap();

        assert!(result.contains("No subscription tiers"));
    }

    #[tokio::test]
    async fn execute_subscribe_returns_placeholder() {
        let client = HomeserverClient::new(
            "http://localhost:8008".to_string(),
            "test-token".to_string(),
            "@mmbot:localhost".to_string(),
        );
        let executor = BotExecutor::new(client);
        let result = executor
            .execute("!test:localhost", "@alice:localhost", BotCommand::Subscribe)
            .await
            .unwrap();

        assert!(result.contains("Subscribe to this creator"));
    }

    #[tokio::test]
    async fn execute_gate_returns_confirmation() {
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
                BotCommand::Gate { level: 3 },
            )
            .await
            .unwrap();

        assert!(result.contains("Content gate set to Tier 3"));
        assert!(result.contains("Tier 3 or above"));
    }

    #[tokio::test]
    async fn execute_ungate_returns_confirmation() {
        let client = HomeserverClient::new(
            "http://localhost:8008".to_string(),
            "test-token".to_string(),
            "@mmbot:localhost".to_string(),
        );
        let executor = BotExecutor::new(client);
        let result = executor
            .execute("!test:localhost", "@alice:localhost", BotCommand::Ungate)
            .await
            .unwrap();

        assert!(result.contains("Content gate removed"));
        assert!(result.contains("open to all viewers"));
    }

    #[tokio::test]
    async fn execute_help_includes_new_commands() {
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

        assert!(result.contains("!mm tiers"));
        assert!(result.contains("!mm subscribe"));
        assert!(result.contains("!mm gate"));
        assert!(result.contains("!mm ungate"));
    }
}
