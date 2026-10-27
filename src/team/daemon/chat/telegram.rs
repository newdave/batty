//! Telegram bridge orchestration for the daemon poll loop.

use anyhow::{Result, anyhow, bail};
use tracing::{debug, info, warn};

use super::super::*;
use super::TelegramCommand;

pub(in crate::team::daemon) fn build_telegram_bot(
    team_config: &TeamConfig,
) -> Option<crate::team::telegram::TelegramBot> {
    team_config
        .roles
        .iter()
        .find(|role| {
            role.role_type == RoleType::User && role.channel.as_deref() == Some("telegram")
        })
        .and_then(|role| role.channel_config.as_ref())
        .and_then(crate::team::telegram::TelegramBot::from_config)
}

impl TeamDaemon {
    pub(in crate::team::daemon) fn process_telegram_queue(&mut self) -> Result<()> {
        self.poll_telegram()?;
        self.deliver_user_channel_inbox()
    }

    fn poll_telegram(&mut self) -> Result<()> {
        if self.integrations.telegram_bot.is_none() {
            return Ok(());
        }

        let messages = match self
            .integrations
            .telegram_bot
            .as_mut()
            .expect("checked telegram bot presence")
            .poll_updates()
        {
            Ok(msgs) => {
                if let Some(transition) = self.integrations.telegram_health.record_success() {
                    self.report_integration_health("telegram", transition, "");
                }
                msgs
            }
            Err(error) => {
                debug!(error = %error, "telegram poll failed");
                if let Some(transition) = self.integrations.telegram_health.record_failure() {
                    self.report_integration_health("telegram", transition, &error.to_string());
                }
                return Ok(());
            }
        };

        if messages.is_empty() {
            return Ok(());
        }

        let root = inbox::inboxes_root(&self.config.project_root);
        let targets: Vec<String> = self
            .config
            .team_config
            .roles
            .iter()
            .find(|role| role.role_type == RoleType::User)
            .map(|role| role.talks_to.clone())
            .unwrap_or_default();

        for msg in messages {
            info!(
                from_user = msg.from_user_id,
                text_len = msg.text.len(),
                "telegram inbound"
            );

            if let Some(reply) = self.handle_telegram_command(&msg) {
                if let Some(bot) = self.integrations.telegram_bot.as_ref() {
                    if let Err(error) = bot.send_message(&msg.chat_id.to_string(), &reply) {
                        warn!(chat_id = msg.chat_id, error = %error, "failed to send telegram reply");
                    }
                }
                continue;
            }

            for target in &targets {
                let inbox_msg = inbox::InboxMessage::new_send("human", target, &msg.text);
                if let Err(error) = inbox::deliver_to_inbox(&root, &inbox_msg) {
                    warn!(
                        to = %target,
                        error = %error,
                        "failed to deliver telegram message to inbox"
                    );
                }
            }

            self.record_message_routed("human", "telegram");
        }

        Ok(())
    }

    fn handle_telegram_command(
        &mut self,
        msg: &crate::team::telegram::InboundMessage,
    ) -> Option<String> {
        let command = match parse_telegram_command(&msg.text) {
            Ok(Some(command)) => command,
            Ok(None) => return None,
            Err(error) => return Some(error.to_string()),
        };

        Some(
            match self.execute_chat_command(
                command,
                background_tests::ChatReplyTarget::Telegram {
                    chat_id: msg.chat_id,
                },
            ) {
                Ok(reply) => reply,
                Err(error) => format!("Command failed: {error}"),
            },
        )
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::team::daemon) fn execute_telegram_command(
        &mut self,
        command: TelegramCommand,
    ) -> Result<String> {
        self.execute_chat_command(command, background_tests::ChatReplyTarget::None)
    }

    /// Warn and emit a team event when a chat integration's poll health flips.
    pub(in crate::team::daemon) fn report_integration_health(
        &mut self,
        integration: &str,
        transition: crate::team::integration_health::HealthTransition,
        error: &str,
    ) {
        use crate::team::integration_health::HealthTransition;
        match transition {
            HealthTransition::BecameUnhealthy { failures } => {
                warn!(integration, failures, error, "chat integration unhealthy");
                self.emit_event(TeamEvent::integration_unhealthy(
                    integration,
                    failures,
                    error,
                ));
            }
            HealthTransition::Recovered { failures } => {
                info!(integration, failures, "chat integration recovered");
                self.emit_event(TeamEvent::integration_recovered(integration, failures));
            }
        }
    }

    pub(in crate::team::daemon) fn deliver_user_channel_inbox(&mut self) -> Result<()> {
        let root = inbox::inboxes_root(&self.config.project_root);
        let user_roles: Vec<String> = self
            .config
            .team_config
            .roles
            .iter()
            .filter(|role| role.role_type == RoleType::User)
            .map(|role| role.name.clone())
            .collect();

        for user_name in &user_roles {
            let messages = match inbox::pending_messages(&root, user_name) {
                Ok(msgs) => msgs,
                Err(error) => {
                    debug!(user = %user_name, error = %error, "failed to read user inbox");
                    continue;
                }
            };

            if messages.is_empty() {
                continue;
            }

            for msg in &messages {
                debug!(from = %msg.from, to = %user_name, id = %msg.id, "delivering to user channel");

                let formatted = format!("--- Message from {} ---\n{}", msg.from, msg.body);
                let send_result = match self.delivery.channels.get(user_name) {
                    Some(channel) => channel.send(&formatted),
                    None => {
                        debug!(user = %user_name, "no channel for user role");
                        break;
                    }
                };
                if let Err(error) = send_result {
                    // The channel logs real failures and backs off; leave the rest of
                    // this inbox for a later poll instead of hammering a broken channel.
                    debug!(to = %user_name, error = %error, "user channel send failed; deferring inbox");
                    break;
                }

                info!(from = %msg.from, to = %user_name, id = %msg.id, "delivered to user channel");
                let marked = inbox::mark_delivered(&root, user_name, &msg.id);
                self.record_message_routed(&msg.from, user_name);
                if let Err(error) = marked {
                    // The message was sent but stays pending. Stop here so this
                    // poll does not deliver further messages (and compound the
                    // duplicates); the channel's recent-send dedup suppresses
                    // the resend on the next poll.
                    warn!(user = %user_name, id = %msg.id, error = %error, "failed to mark delivered; stopping delivery for this poll");
                    break;
                }
            }
        }

        Ok(())
    }

    pub(crate) fn automation_sender_for(&self, recipient: &str) -> String {
        let recipient_member = self
            .config
            .members
            .iter()
            .find(|member| member.name == recipient);

        if let Some(member) = recipient_member {
            if let Some(parent) = &member.reports_to {
                return parent.clone();
            }
        }

        if let Some(sender) = &self.config.team_config.automation_sender {
            return sender.clone();
        }

        "daemon".to_string()
    }
}

fn parse_telegram_command(text: &str) -> Result<Option<TelegramCommand>> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return Ok(None);
    }

    let (name, rest) = trimmed
        .split_once(char::is_whitespace)
        .map(|(name, rest)| (name, rest.trim()))
        .unwrap_or((trimmed, ""));

    match name {
        "/status" => {
            if rest.is_empty() {
                Ok(Some(TelegramCommand::Status))
            } else {
                Err(anyhow!("Usage: /status"))
            }
        }
        "/board" => {
            let filter = if rest.is_empty() {
                None
            } else {
                Some(rest.to_string())
            };
            Ok(Some(TelegramCommand::Board { filter }))
        }
        "/logs" => {
            if rest.is_empty() {
                bail!("Usage: /logs <engineer>");
            }
            Ok(Some(TelegramCommand::Logs {
                member: rest.to_string(),
            }))
        }
        "/health" => {
            if rest.is_empty() {
                Ok(Some(TelegramCommand::Health))
            } else {
                Err(anyhow!("Usage: /health"))
            }
        }
        "/assign" => {
            let mut parts = rest
                .splitn(2, char::is_whitespace)
                .filter(|part| !part.is_empty());
            let engineer = parts
                .next()
                .ok_or_else(|| anyhow!("Usage: /assign <engineer> <task>"))?;
            let task = parts.next().unwrap_or("").trim();
            if task.is_empty() {
                bail!("Usage: /assign <engineer> <task>");
            }
            Ok(Some(TelegramCommand::Assign {
                engineer: engineer.to_string(),
                task: task.to_string(),
            }))
        }
        "/kick" => {
            if rest.is_empty() {
                bail!("Usage: /kick <engineer>");
            }
            Ok(Some(TelegramCommand::Kick {
                member: rest.to_string(),
            }))
        }
        "/pause" => {
            if rest.is_empty() {
                Ok(Some(TelegramCommand::Pause))
            } else {
                Err(anyhow!("Usage: /pause"))
            }
        }
        "/resume" => {
            if rest.is_empty() {
                Ok(Some(TelegramCommand::Resume))
            } else {
                Err(anyhow!("Usage: /resume"))
            }
        }
        "/goal" => {
            if rest.is_empty() {
                bail!("Usage: /goal <text>");
            }
            Ok(Some(TelegramCommand::Goal {
                text: rest.to_string(),
            }))
        }
        "/task" => {
            if rest.is_empty() {
                bail!("Usage: /task <title>");
            }
            Ok(Some(TelegramCommand::Task {
                title: rest.to_string(),
            }))
        }
        "/block" => {
            let mut parts = rest
                .splitn(2, char::is_whitespace)
                .filter(|part| !part.is_empty());
            let task_id = parts
                .next()
                .ok_or_else(|| anyhow!("Usage: /block <task> <reason>"))?
                .trim_start_matches('#')
                .parse::<u32>()
                .map_err(|_| anyhow!("Usage: /block <task> <reason>"))?;
            let reason = parts.next().unwrap_or("").trim();
            if reason.is_empty() {
                bail!("Usage: /block <task> <reason>");
            }
            Ok(Some(TelegramCommand::Block {
                task_id,
                reason: reason.to_string(),
            }))
        }
        "/merge" => {
            if rest.is_empty() {
                bail!("Usage: /merge <task>");
            }
            let task_id = rest
                .trim_start_matches('#')
                .parse::<u32>()
                .map_err(|_| anyhow!("Usage: /merge <task>"))?;
            Ok(Some(TelegramCommand::Merge { task_id }))
        }
        "/stop" => Ok(Some(TelegramCommand::Stop {
            confirm: rest == "confirm",
        })),
        "/start" => {
            if rest.is_empty() {
                Ok(Some(TelegramCommand::Start))
            } else {
                Err(anyhow!("Usage: /start"))
            }
        }
        "/help" => {
            if rest.is_empty() {
                Ok(Some(TelegramCommand::Help))
            } else {
                Err(anyhow!("Usage: /help"))
            }
        }
        "/send" => {
            let mut parts = rest
                .splitn(2, char::is_whitespace)
                .filter(|part| !part.is_empty());
            let role = parts
                .next()
                .ok_or_else(|| anyhow!("Usage: /send <role> <message>"))?;
            let message = parts.next().unwrap_or("").trim();
            if message.is_empty() {
                bail!("Usage: /send <role> <message>");
            }
            Ok(Some(TelegramCommand::Send {
                role: role.to_string(),
                message: message.to_string(),
            }))
        }
        _ => Err(anyhow!(
            "Unknown command. Supported: /status, /board, /logs, /health, /assign, /merge, /kick, /pause, /resume, /goal, /task, /block, /stop, /start, /help, /send"
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::super::commands::{
        block_telegram_task, engineer_for_merge_task, render_telegram_board_summary,
    };
    use super::*;
    use crate::team::comms::Channel;
    use crate::team::config::{
        AutomationConfig, BoardConfig, ChannelConfig, OrchestratorPosition, RoleDef, StandupConfig,
        TeamConfig, WorkflowMode, WorkflowPolicy,
    };
    use crate::team::daemon::DaemonConfig;
    use crate::team::errors::DeliveryError;
    use crate::team::events::EventSink;

    use crate::team::hierarchy::MemberInstance;
    use crate::team::test_helpers::daemon_config_with_roles;
    use crate::team::test_support::git_ok;

    struct RecordingChannel {
        messages: Arc<Mutex<Vec<String>>>,
    }

    impl Channel for RecordingChannel {
        fn send(&self, message: &str) -> std::result::Result<(), DeliveryError> {
            self.messages.lock().unwrap().push(message.to_string());
            Ok(())
        }

        fn channel_type(&self) -> &str {
            "test"
        }
    }

    fn backdate_idle_grace(daemon: &mut TeamDaemon, member_name: &str) {
        let grace = daemon.automation_idle_grace_duration() + Duration::from_secs(1);
        daemon
            .idle_started_at
            .insert(member_name.to_string(), Instant::now() - grace);
        if let Some(schedule) = daemon.interventions.nudges.get_mut(member_name) {
            schedule.idle_since = Some(Instant::now() - schedule.interval.max(grace));
        }
    }

    fn write_board_task(project_root: &std::path::Path, file_name: &str, body: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(tasks_dir.join(file_name), body).unwrap();
    }
    #[test]
    fn process_telegram_queue_delivers_pending_user_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                trunk_branch: crate::team::config::default_trunk_branch(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: vec![RoleDef {
                    talks_to: vec!["architect".to_string()],
                    ..RoleDef::test("human", RoleType::User)
                }],
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        })
        .unwrap();
        daemon.delivery.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        let root = inbox::inboxes_root(tmp.path());
        let msg = inbox::InboxMessage::new_send("architect", "human", "Status update");
        inbox::deliver_to_inbox(&root, &msg).unwrap();

        daemon.process_telegram_queue().unwrap();

        assert_eq!(
            sent.lock().unwrap().as_slice(),
            ["--- Message from architect ---\nStatus update"]
        );
        assert!(inbox::pending_messages(&root, "human").unwrap().is_empty());
    }

    #[test]
    fn maybe_fire_nudges_marks_member_working_after_live_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let member = MemberInstance {
            name: "scientist".to_string(),
            role_name: "scientist".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut watchers = HashMap::new();
        let mut scientist_watcher = SessionWatcher::new("%9999999", "scientist", 300, None);
        scientist_watcher.confirm_ready();
        watchers.insert("scientist".to_string(), scientist_watcher);

        // Create a shim handle in Idle state so deliver_message returns LivePane
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "scientist".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        let mut shim_handles = HashMap::new();
        shim_handles.insert("scientist".to_string(), handle);

        let mut daemon = crate::team::test_support::TestDaemonBuilder::new(tmp.path())
            .workflow_policy(WorkflowPolicy::default())
            .automation(AutomationConfig::default())
            .roles(Vec::new())
            .members(vec![member])
            .pane_map(HashMap::from([(
                "scientist".to_string(),
                "%9999999".to_string(),
            )]))
            .states(HashMap::from([(
                "scientist".to_string(),
                MemberState::Idle,
            )]))
            .nudges(HashMap::from([(
                "scientist".to_string(),
                NudgeSchedule {
                    text: "Please make progress.".to_string(),
                    interval: Duration::from_secs(1),
                    idle_since: Some(Instant::now() - Duration::from_secs(5)),
                    fired_this_idle: false,
                    paused: false,
                },
            )]))
            .watchers(watchers)
            .build();
        daemon.shim_handles = shim_handles;
        daemon.event_sink = EventSink::new(&tmp.path().join("events.jsonl")).unwrap();

        backdate_idle_grace(&mut daemon, "scientist");
        daemon.maybe_fire_nudges().unwrap();

        // Shim-managed agents: state driven by shim events, not speculative mark_member_working.
        // mark_member_working is a no-op for shim agents, so state stays Idle and
        // nudge timers are not reset by update_automation_timers_for_state.
        assert_eq!(
            daemon.states.get("scientist"),
            Some(&MemberState::Idle),
            "shim-managed agent state stays Idle; real state comes from shim events"
        );
        let schedule = daemon.interventions.nudges.get("scientist").unwrap();
        // Nudge is NOT paused because mark_member_working is a no-op for shim agents
        assert!(!schedule.paused);
        // idle_since is still set (not cleared) for the same reason
        assert!(schedule.idle_since.is_some());
        // The nudge DID fire (delivered_live was true)
        assert!(schedule.fired_this_idle);
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn maybe_intervene_triage_backlog_marks_member_working_after_live_delivery() {
        let _env_lock = crate::team::test_support::env_lock();
        let session = format!("batty-test-triage-live-delivery-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        crate::tmux::create_session(&session, "cat", &[], "/tmp").unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(Duration::from_millis(150));

        let tmp = tempfile::tempdir().unwrap();
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut watchers = HashMap::new();
        let mut lead_watcher = SessionWatcher::new(&pane_id, "lead", 300, None);
        lead_watcher.confirm_ready();
        watchers.insert("lead".to_string(), lead_watcher);
        let mut daemon = crate::team::test_support::TestDaemonBuilder::new(tmp.path())
            .workflow_policy(WorkflowPolicy::default())
            .automation(AutomationConfig::default())
            .roles(Vec::new())
            .session(session.clone())
            .members(vec![lead, engineer])
            .pane_map(HashMap::from([("lead".to_string(), pane_id.clone())]))
            .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
            .watchers(watchers)
            .build();
        daemon.event_sink = EventSink::new(&tmp.path().join("events.jsonl")).unwrap();

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = super::now_unix();
        let id = inbox::deliver_to_inbox(&root, &result).unwrap();
        inbox::mark_delivered(&root, "lead", &id).unwrap();

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_triage_backlog().unwrap();

        assert_eq!(
            daemon.interventions.triage_interventions.get("lead"),
            Some(&1)
        );
        if daemon.states.get("lead") == Some(&MemberState::Working) {
            let pane = (0..100)
                .find_map(|_| {
                    let pane = tmux::capture_pane(&pane_id).unwrap_or_default();
                    if pane.contains("batty send architect")
                        && pane.contains("next time you become idle")
                    {
                        Some(pane)
                    } else {
                        std::thread::sleep(Duration::from_millis(100));
                        None
                    }
                })
                .unwrap_or_else(|| tmux::capture_pane(&pane_id).unwrap_or_default());
            assert!(pane.contains("Triage backlog detected"));
            assert!(pane.contains("batty send architect"));
            assert!(pane.contains("next time you become idle"));
        } else {
            let pending = inbox::pending_messages(&root, "lead").unwrap();
            assert_eq!(pending.len(), 1);
            assert!(pending[0].body.contains("batty inbox lead"));
        }

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    fn automation_sender_prefers_direct_manager_and_config_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = crate::team::test_support::TestDaemonBuilder::new(tmp.path())
            .workflow_policy(WorkflowPolicy::default())
            .automation(AutomationConfig::default())
            .roles(Vec::new())
            .members(vec![
                MemberInstance {
                    name: "architect".to_string(),
                    role_name: "architect".to_string(),
                    role_type: RoleType::Architect,
                    agent: Some("claude".to_string()),
                    prompt: None,
                    reports_to: None,
                    use_worktrees: false,
                    ..Default::default()
                },
                MemberInstance {
                    name: "lead".to_string(),
                    role_name: "lead".to_string(),
                    role_type: RoleType::Manager,
                    agent: Some("claude".to_string()),
                    prompt: None,
                    reports_to: Some("architect".to_string()),
                    use_worktrees: false,
                    ..Default::default()
                },
                MemberInstance {
                    name: "eng-1".to_string(),
                    role_name: "eng".to_string(),
                    role_type: RoleType::Engineer,
                    agent: Some("codex".to_string()),
                    prompt: None,
                    reports_to: Some("lead".to_string()),
                    use_worktrees: false,
                    ..Default::default()
                },
            ])
            .build();
        daemon.config.team_config.automation_sender = Some("human".to_string());
        daemon.event_sink = EventSink::new(&tmp.path().join("events.jsonl")).unwrap();

        assert_eq!(daemon.automation_sender_for("eng-1"), "lead");
        assert_eq!(daemon.automation_sender_for("lead"), "architect");
        assert_eq!(daemon.automation_sender_for("architect"), "human");

        daemon.config.team_config.automation_sender = None;
        assert_eq!(daemon.automation_sender_for("architect"), "daemon");
    }

    #[test]
    fn daemon_creates_telegram_bot_when_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            talks_to: vec!["architect".to_string()],
            channel: Some("telegram".to_string()),
            channel_config: Some(ChannelConfig {
                target: "12345".to_string(),
                provider: "telegram".to_string(),
                bot_token: Some("test-token-123".to_string()),
                allowed_user_ids: vec![42],
                events_channel_id: None,
                agents_channel_id: None,
                commands_channel_id: None,
                board_channel_id: None,
            }),
            ..RoleDef::test("user", RoleType::User)
        }];

        let config = daemon_config_with_roles(tmp.path(), roles);
        let daemon = TeamDaemon::new(config).unwrap();
        assert!(daemon.integrations.telegram_bot.is_some());
    }

    #[test]
    fn daemon_no_telegram_bot_without_config() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            talks_to: vec!["architect".to_string()],
            ..RoleDef::test("user", RoleType::User)
        }];

        let config = daemon_config_with_roles(tmp.path(), roles);
        let daemon = TeamDaemon::new(config).unwrap();
        assert!(daemon.integrations.telegram_bot.is_none());
    }

    // --- New tests for #255 ---

    #[test]
    fn build_telegram_bot_returns_none_when_no_user_role() {
        let roles = vec![RoleDef {
            agent: Some("claude".to_string()),
            ..RoleDef::test("architect", RoleType::Architect)
        }];
        let tc = crate::team::test_helpers::team_config_with_roles(roles);
        assert!(build_telegram_bot(&tc).is_none());
    }

    #[test]
    fn build_telegram_bot_returns_none_when_user_has_different_channel() {
        let roles = vec![RoleDef {
            talks_to: vec!["architect".to_string()],
            channel: Some("slack".to_string()),
            ..RoleDef::test("human", RoleType::User)
        }];
        let tc = crate::team::test_helpers::team_config_with_roles(roles);
        assert!(build_telegram_bot(&tc).is_none());
    }

    #[test]
    fn build_telegram_bot_returns_none_when_channel_config_missing() {
        let roles = vec![RoleDef {
            talks_to: vec!["architect".to_string()],
            channel: Some("telegram".to_string()),
            ..RoleDef::test("human", RoleType::User)
        }];
        let tc = crate::team::test_helpers::team_config_with_roles(roles);
        assert!(build_telegram_bot(&tc).is_none());
    }

    #[test]
    fn process_telegram_queue_no_pending_messages_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            talks_to: vec!["architect".to_string()],
            ..RoleDef::test("human", RoleType::User)
        }];
        let config = daemon_config_with_roles(tmp.path(), roles);
        let mut daemon = TeamDaemon::new(config).unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.delivery.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        daemon.process_telegram_queue().unwrap();
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn deliver_user_inbox_multiple_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let roles = vec![RoleDef {
            talks_to: vec!["architect".to_string()],
            ..RoleDef::test("human", RoleType::User)
        }];
        let config = daemon_config_with_roles(tmp.path(), roles);
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon.delivery.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        let root = inbox::inboxes_root(tmp.path());
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "human", "First message"),
        )
        .unwrap();
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("manager", "human", "Second message"),
        )
        .unwrap();

        daemon.process_telegram_queue().unwrap();

        let messages = sent.lock().unwrap();
        assert_eq!(messages.len(), 2);
        // Order depends on filesystem listing — check both messages are present
        let combined: String = messages.join("\n");
        assert!(combined.contains("First message"));
        assert!(combined.contains("Second message"));
    }

    #[test]
    fn deliver_user_inbox_no_channel_skips_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            talks_to: vec!["architect".to_string()],
            ..RoleDef::test("human", RoleType::User)
        }];
        let config = daemon_config_with_roles(tmp.path(), roles);
        let mut daemon = TeamDaemon::new(config).unwrap();
        // Intentionally do NOT insert a channel

        let root = inbox::inboxes_root(tmp.path());
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "human", "Test"),
        )
        .unwrap();

        // Should not panic — just skips delivery
        daemon.process_telegram_queue().unwrap();

        // Message should still be pending since it couldn't be delivered
        let pending = inbox::pending_messages(&root, "human").unwrap();
        assert_eq!(pending.len(), 1);
    }

    struct FailingCountingChannel {
        attempts: Arc<Mutex<usize>>,
    }

    impl Channel for FailingCountingChannel {
        fn send(&self, _message: &str) -> std::result::Result<(), DeliveryError> {
            *self.attempts.lock().unwrap() += 1;
            Err(DeliveryError::ChannelSend {
                recipient: "12345".to_string(),
                detail: "Bad Request: chat not found".to_string(),
            })
        }

        fn channel_type(&self) -> &str {
            "test"
        }
    }

    #[test]
    fn deliver_user_inbox_stops_after_first_channel_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            talks_to: vec!["architect".to_string()],
            ..RoleDef::test("human", RoleType::User)
        }];
        let config = daemon_config_with_roles(tmp.path(), roles);
        let mut daemon = TeamDaemon::new(config).unwrap();
        let attempts = Arc::new(Mutex::new(0));
        daemon.delivery.channels.insert(
            "human".to_string(),
            Box::new(FailingCountingChannel {
                attempts: Arc::clone(&attempts),
            }),
        );

        let root = inbox::inboxes_root(tmp.path());
        for body in ["one", "two", "three"] {
            inbox::deliver_to_inbox(
                &root,
                &inbox::InboxMessage::new_send("architect", "human", body),
            )
            .unwrap();
        }

        daemon.process_telegram_queue().unwrap();

        assert_eq!(*attempts.lock().unwrap(), 1);
        let pending = inbox::pending_messages(&root, "human").unwrap();
        assert_eq!(pending.len(), 3);
    }

    #[test]
    fn automation_sender_for_unknown_recipient_uses_config_sender() {
        let tmp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                trunk_branch: crate::team::config::default_trunk_branch(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: Some("boss".to_string()),
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        };
        let daemon = TeamDaemon::new(config).unwrap();
        // "nobody" is not a member → falls through to automation_sender config
        assert_eq!(daemon.automation_sender_for("nobody"), "boss");
    }

    #[test]
    fn automation_sender_for_unknown_recipient_no_config_defaults_to_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                trunk_branch: crate::team::config::default_trunk_branch(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        };
        let daemon = TeamDaemon::new(config).unwrap();
        assert_eq!(daemon.automation_sender_for("nobody"), "daemon");
    }

    #[test]
    fn deliver_user_inbox_marks_messages_delivered() {
        let tmp = tempfile::tempdir().unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let roles = vec![RoleDef {
            talks_to: vec!["architect".to_string()],
            ..RoleDef::test("human", RoleType::User)
        }];
        let config = daemon_config_with_roles(tmp.path(), roles);
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon.delivery.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        let root = inbox::inboxes_root(tmp.path());
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "human", "Test delivery"),
        )
        .unwrap();

        daemon.process_telegram_queue().unwrap();

        // After delivery, no pending messages should remain
        let pending = inbox::pending_messages(&root, "human").unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn deliver_user_inbox_formats_message_with_sender() {
        let tmp = tempfile::tempdir().unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let roles = vec![RoleDef {
            talks_to: vec!["architect".to_string()],
            ..RoleDef::test("human", RoleType::User)
        }];
        let config = daemon_config_with_roles(tmp.path(), roles);
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon.delivery.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        let root = inbox::inboxes_root(tmp.path());
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("engineer-1", "human", "Task done"),
        )
        .unwrap();

        daemon.process_telegram_queue().unwrap();

        let messages = sent.lock().unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].starts_with("--- Message from engineer-1 ---\n"));
        assert!(messages[0].contains("Task done"));
    }

    #[test]
    fn deliver_user_inbox_multiple_users() {
        let tmp = tempfile::tempdir().unwrap();
        let sent_alice = Arc::new(Mutex::new(Vec::new()));
        let sent_bob = Arc::new(Mutex::new(Vec::new()));
        let roles = vec![
            RoleDef {
                talks_to: vec!["architect".to_string()],
                ..RoleDef::test("alice", RoleType::User)
            },
            RoleDef {
                talks_to: vec!["architect".to_string()],
                ..RoleDef::test("bob", RoleType::User)
            },
        ];
        let config = daemon_config_with_roles(tmp.path(), roles);
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon.delivery.channels.insert(
            "alice".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent_alice),
            }),
        );
        daemon.delivery.channels.insert(
            "bob".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent_bob),
            }),
        );

        let root = inbox::inboxes_root(tmp.path());
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "alice", "Hello Alice"),
        )
        .unwrap();
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "bob", "Hello Bob"),
        )
        .unwrap();

        daemon.process_telegram_queue().unwrap();

        assert_eq!(sent_alice.lock().unwrap().len(), 1);
        assert!(sent_alice.lock().unwrap()[0].contains("Hello Alice"));
        assert_eq!(sent_bob.lock().unwrap().len(), 1);
        assert!(sent_bob.lock().unwrap()[0].contains("Hello Bob"));
    }

    #[test]
    fn automation_sender_for_member_with_reports_to_returns_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                trunk_branch: crate::team::config::default_trunk_branch(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: Some("default-sender".to_string()),
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: vec![MemberInstance {
                name: "mgr".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("boss".to_string()),
                use_worktrees: false,
                ..Default::default()
            }],
            pane_map: HashMap::new(),
        };
        let daemon = TeamDaemon::new(config).unwrap();
        assert_eq!(daemon.automation_sender_for("mgr"), "boss");
    }

    #[test]
    fn parse_telegram_commands_supports_phone_friendly_forms() {
        assert_eq!(
            parse_telegram_command("/status").unwrap(),
            Some(TelegramCommand::Status)
        );
        assert_eq!(
            parse_telegram_command("/board").unwrap(),
            Some(TelegramCommand::Board { filter: None })
        );
        assert_eq!(
            parse_telegram_command("/board review").unwrap(),
            Some(TelegramCommand::Board {
                filter: Some("review".to_string()),
            })
        );
        assert_eq!(
            parse_telegram_command("/logs eng-1").unwrap(),
            Some(TelegramCommand::Logs {
                member: "eng-1".to_string(),
            })
        );
        assert_eq!(
            parse_telegram_command("/health").unwrap(),
            Some(TelegramCommand::Health)
        );
        assert_eq!(
            parse_telegram_command("/pause").unwrap(),
            Some(TelegramCommand::Pause)
        );
        assert_eq!(
            parse_telegram_command("/resume").unwrap(),
            Some(TelegramCommand::Resume)
        );
        assert_eq!(
            parse_telegram_command("/task Add remote control").unwrap(),
            Some(TelegramCommand::Task {
                title: "Add remote control".to_string(),
            })
        );
        assert_eq!(
            parse_telegram_command("/block 41 waiting on CI").unwrap(),
            Some(TelegramCommand::Block {
                task_id: 41,
                reason: "waiting on CI".to_string(),
            })
        );
        assert_eq!(
            parse_telegram_command("/stop confirm").unwrap(),
            Some(TelegramCommand::Stop { confirm: true })
        );
        assert_eq!(
            parse_telegram_command("/help").unwrap(),
            Some(TelegramCommand::Help)
        );
        assert_eq!(
            parse_telegram_command("/assign eng-1 Fix flaky test").unwrap(),
            Some(TelegramCommand::Assign {
                engineer: "eng-1".to_string(),
                task: "Fix flaky test".to_string(),
            })
        );
        assert_eq!(
            parse_telegram_command("/merge #41").unwrap(),
            Some(TelegramCommand::Merge { task_id: 41 })
        );
        assert_eq!(
            parse_telegram_command("/send architect Need review on task 41").unwrap(),
            Some(TelegramCommand::Send {
                role: "architect".to_string(),
                message: "Need review on task 41".to_string(),
            })
        );
    }

    #[test]
    fn parse_telegram_command_rejects_invalid_usage() {
        assert_eq!(parse_telegram_command("hello").unwrap(), None);
        assert!(parse_telegram_command("/assign eng-1").is_err());
        assert!(parse_telegram_command("/logs").is_err());
        assert!(parse_telegram_command("/merge nope").is_err());
        assert!(parse_telegram_command("/goal").is_err());
        assert!(parse_telegram_command("/task").is_err());
        assert!(parse_telegram_command("/block 41").is_err());
        assert!(parse_telegram_command("/send architect").is_err());
        assert!(parse_telegram_command("/unknown").is_err());
    }

    #[test]
    fn telegram_assign_command_delivers_assignment_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                talks_to: vec!["eng".to_string()],
                ..RoleDef::test("human", RoleType::User)
            },
            RoleDef {
                agent: Some("codex".to_string()),
                ..RoleDef::test("eng", RoleType::Engineer)
            },
        ];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![
            MemberInstance {
                name: "architect".to_string(),
                role_name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng".to_string(),
                role_name: "eng".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: None,
                use_worktrees: false,
            },
        ];
        let mut daemon = TeamDaemon::new(config).unwrap();

        let reply = daemon
            .execute_telegram_command(TelegramCommand::Assign {
                engineer: "eng".to_string(),
                task: "Fix flaky test".to_string(),
            })
            .unwrap();

        assert!(reply.contains("Assigned eng"));
        let pending = inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "eng").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Fix flaky test"));
    }

    #[test]
    fn telegram_assign_command_rejects_busy_engineer() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            agent: Some("codex".to_string()),
            ..RoleDef::test("eng", RoleType::Engineer)
        }];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![MemberInstance {
            name: "eng".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        }];
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon
            .states
            .insert("eng".to_string(), MemberState::Working);

        let error = daemon
            .execute_telegram_command(TelegramCommand::Assign {
                engineer: "eng".to_string(),
                task: "Fix flaky test".to_string(),
            })
            .unwrap_err();

        assert!(error.to_string().contains("not idle"));
    }

    #[test]
    fn telegram_assign_command_rejects_blocked_task() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            agent: Some("codex".to_string()),
            ..RoleDef::test("eng", RoleType::Engineer)
        }];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![MemberInstance {
            name: "eng".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
        }];
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon.states.insert("eng".to_string(), MemberState::Idle);
        write_board_task(
            tmp.path(),
            "task-41.md",
            "---\nid: 41\ntitle: Waiting task\nstatus: todo\npriority: high\nblocked_on: waiting for auth\nclass: standard\n---\n",
        );

        let error = daemon
            .execute_telegram_command(TelegramCommand::Assign {
                engineer: "eng".to_string(),
                task: "41".to_string(),
            })
            .unwrap_err();

        assert!(error.to_string().contains("blocked"));
    }

    #[test]
    fn telegram_send_command_delivers_message_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                talks_to: vec!["architect".to_string()],
                ..RoleDef::test("human", RoleType::User)
            },
            RoleDef {
                agent: Some("claude".to_string()),
                ..RoleDef::test("architect", RoleType::Architect)
            },
        ];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![
            MemberInstance {
                name: "architect".to_string(),
                role_name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng".to_string(),
                role_name: "eng".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: None,
                use_worktrees: false,
            },
        ];
        let mut daemon = TeamDaemon::new(config).unwrap();

        let reply = daemon
            .execute_telegram_command(TelegramCommand::Send {
                role: "architect".to_string(),
                message: "Need a quick review".to_string(),
            })
            .unwrap();

        assert!(reply.contains("Sent to architect"));
        let pending =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "architect").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, "Need a quick review");
    }

    #[test]
    fn render_telegram_board_summary_reports_counts_and_queues() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "task-11.md",
            "---\nid: 11\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
        );
        write_board_task(
            tmp.path(),
            "task-12.md",
            "---\nid: 12\ntitle: Review task\nstatus: review\npriority: medium\nclaimed_by: eng-2\nclass: standard\n---\n",
        );
        write_board_task(
            tmp.path(),
            "task-13.md",
            "---\nid: 13\ntitle: Todo task\nstatus: todo\npriority: low\nclass: standard\n---\n",
        );

        let summary = render_telegram_board_summary(tmp.path(), None).unwrap();
        assert!(summary.contains("in-progress=1"));
        assert!(summary.contains("review=1"));
        assert!(summary.contains("todo=1"));
        assert!(summary.contains("#11 Active task (eng-1)"));
        assert!(summary.contains("#12 Review task (eng-2)"));
    }

    #[test]
    fn engineer_for_merge_task_reads_claimed_engineer() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "task-41.md",
            "---\nid: 41\ntitle: Merge me\nstatus: review\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
        );

        assert_eq!(engineer_for_merge_task(tmp.path(), 41).unwrap(), "eng-1");
    }

    #[test]
    fn telegram_merge_command_rejects_non_review_task() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "task-41.md",
            "---\nid: 41\ntitle: Merge me\nstatus: todo\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
        );
        let mut daemon = TeamDaemon::new(daemon_config_with_roles(tmp.path(), Vec::new())).unwrap();

        let error = daemon
            .execute_telegram_command(TelegramCommand::Merge { task_id: 41 })
            .unwrap_err();

        assert!(error.to_string().contains("not in review"));
    }

    #[test]
    fn telegram_merge_command_verifies_in_background() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "task-41.md",
            "---\nid: 41\ntitle: Merge me\nstatus: review\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
        );
        let mut daemon = TeamDaemon::new(daemon_config_with_roles(tmp.path(), Vec::new())).unwrap();

        let reply = daemon
            .execute_telegram_command(TelegramCommand::Merge { task_id: 41 })
            .unwrap();
        assert!(reply.contains("Running verification"), "{reply}");
        assert_eq!(daemon.verification.background_tests.chat_merges.len(), 1);

        let again = daemon
            .execute_telegram_command(TelegramCommand::Merge { task_id: 41 })
            .unwrap();
        assert!(again.contains("already being verified"), "{again}");
        assert_eq!(daemon.verification.background_tests.chat_merges.len(), 1);

        // The engineer worktree does not exist, so verification fails and the
        // job is drained without merging.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !daemon.verification.background_tests.chat_merges.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "chat merge never finished"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
            daemon.poll_chat_merge_jobs();
        }
        let task = crate::task::Task::from_file(
            &crate::team::task_cmd::find_task_path(&daemon.board_dir(), 41).unwrap(),
        )
        .unwrap();
        assert_eq!(task.status, "review");
    }

    #[test]
    fn pause_and_resume_commands_toggle_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TeamDaemon::new(daemon_config_with_roles(tmp.path(), Vec::new())).unwrap();

        assert_eq!(
            daemon
                .execute_telegram_command(TelegramCommand::Pause)
                .unwrap(),
            "Automation paused."
        );
        assert!(crate::team::pause_marker_path(tmp.path()).exists());

        assert_eq!(
            daemon
                .execute_telegram_command(TelegramCommand::Resume)
                .unwrap(),
            "Automation resumed."
        );
        assert!(!crate::team::pause_marker_path(tmp.path()).exists());
    }

    #[test]
    fn telegram_kick_command_autosaves_dirty_worktree_before_restart() {
        let tmp = tempfile::tempdir().unwrap();
        git_ok(tmp.path(), &["init"]);
        git_ok(tmp.path(), &["config", "user.email", "test@example.com"]);
        git_ok(tmp.path(), &["config", "user.name", "Test User"]);
        std::fs::write(tmp.path().join("README.md"), "root\n").unwrap();
        git_ok(tmp.path(), &["add", "README.md"]);
        git_ok(tmp.path(), &["commit", "-m", "init"]);

        let worktree_dir = tmp.path().join(".batty").join("worktrees").join("eng-1");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        git_ok(&worktree_dir, &["init"]);
        git_ok(&worktree_dir, &["config", "user.email", "test@example.com"]);
        git_ok(&worktree_dir, &["config", "user.name", "Test User"]);
        std::fs::write(worktree_dir.join("README.md"), "worktree\n").unwrap();
        git_ok(&worktree_dir, &["add", "README.md"]);
        git_ok(&worktree_dir, &["commit", "-m", "init"]);
        std::fs::write(worktree_dir.join("README.md"), "dirty\n").unwrap();

        let roles = vec![RoleDef {
            agent: Some("codex".to_string()),
            use_worktrees: true,
            ..RoleDef::test("eng", RoleType::Engineer)
        }];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: true,
        }];
        let mut daemon = TeamDaemon::new(config).unwrap();

        let error = daemon
            .execute_telegram_command(TelegramCommand::Kick {
                member: "eng-1".to_string(),
            })
            .unwrap_err();
        assert!(error.to_string().contains("No pane registered"));

        let status = Command::new("git")
            .args(["status", "--short"])
            .current_dir(&worktree_dir)
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(String::from_utf8_lossy(&status.stdout).trim().is_empty());
    }

    #[test]
    fn block_command_updates_task_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(
            tmp.path(),
            "task-91.md",
            "---\nid: 91\ntitle: Needs dependency\nstatus: todo\npriority: medium\nclass: standard\n---\n",
        );

        let reply = block_telegram_task(tmp.path(), 91, "waiting for auth").unwrap();
        assert!(reply.contains("Blocked #91"));

        let task_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("task-91.md");
        let content = std::fs::read_to_string(task_path).unwrap();
        assert!(content.contains("blocked_on: waiting for auth"));
    }

    #[test]
    fn status_summary_reports_member_and_board_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                talks_to: vec!["architect".to_string(), "eng".to_string()],
                ..RoleDef::test("human", RoleType::User)
            },
            RoleDef {
                agent: Some("claude".to_string()),
                ..RoleDef::test("architect", RoleType::Architect)
            },
            RoleDef {
                agent: Some("codex".to_string()),
                ..RoleDef::test("eng", RoleType::Engineer)
            },
        ];
        let mut config = daemon_config_with_roles(tmp.path(), roles);
        config.members = vec![
            MemberInstance {
                name: "architect".to_string(),
                role_name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng".to_string(),
                role_name: "eng".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: None,
                use_worktrees: false,
            },
        ];
        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon
            .states
            .insert("architect".to_string(), MemberState::Working);
        daemon.states.insert("eng".to_string(), MemberState::Idle);
        let root = inbox::inboxes_root(tmp.path());
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("human", "architect", "Need input"),
        )
        .unwrap();
        write_board_task(
            tmp.path(),
            "task-50.md",
            "---\nid: 50\ntitle: In progress\nstatus: in-progress\npriority: high\nclaimed_by: eng\nclass: standard\n---\n",
        );
        write_board_task(
            tmp.path(),
            "task-51.md",
            "---\nid: 51\ntitle: In review\nstatus: review\npriority: medium\nclaimed_by: eng\nclass: standard\n---\n",
        );

        let summary = daemon.render_telegram_status_summary();
        assert!(summary.contains("Members: idle=1, working=1"));
        assert!(summary.contains("Inbox: 1"));
        assert!(summary.contains("Board: active=1, review=1"));
    }

    #[test]
    fn report_integration_health_emits_unhealthy_and_recovered_events() {
        use crate::team::integration_health::HealthTransition;

        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        daemon.report_integration_health(
            "telegram",
            HealthTransition::BecameUnhealthy { failures: 5 },
            "timed out",
        );
        daemon.report_integration_health(
            "telegram",
            HealthTransition::Recovered { failures: 6 },
            "",
        );

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(tmp.path())).unwrap();
        let unhealthy = events
            .iter()
            .find(|event| event.event == "integration_unhealthy")
            .expect("unhealthy event");
        assert_eq!(unhealthy.step.as_deref(), Some("telegram"));
        assert_eq!(unhealthy.error.as_deref(), Some("timed out"));
        assert!(
            events
                .iter()
                .any(|event| event.event == "integration_recovered")
        );
    }
}
