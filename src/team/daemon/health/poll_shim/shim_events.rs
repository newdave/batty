//! Per-variant handlers for shim [`Event`]s. `handle_shim_event` in the parent
//! module is a thin dispatcher; each handler preserves the exact ordering of
//! side effects of the original single-match implementation.

use std::time::Instant;

use anyhow::Result;
use tracing::{debug, info, warn};

use super::*;

/// Fields of `Event::ContextWarning` consumed by the handler.
pub(super) struct ContextWarningStats {
    pub(super) model: Option<String>,
    pub(super) output_bytes: u64,
    pub(super) uptime_secs: u64,
    pub(super) used_tokens: u64,
    pub(super) context_limit_tokens: u64,
    pub(super) usage_pct: u8,
}

impl TeamDaemon {
    /// Record the thread identity reported by a freshly started Codex session.
    pub(super) fn on_session_started(
        &mut self,
        member_name: &str,
        session_id: String,
    ) -> Result<()> {
        if uuid::Uuid::parse_str(&session_id).is_err() {
            return self.handle_shim_event(
                member_name,
                Event::ConfigurationRequired {
                    message: "Codex reported an invalid thread identity".into(),
                    code: Some("invalid_thread_id".into()),
                },
            );
        }
        let identities = load_launch_state(&self.config.project_root);
        if identities.iter().any(|(other, identity)| {
            other != member_name
                && identity.agent == "codex-cli"
                && identity.session_id.as_deref() == Some(&session_id)
        }) {
            return self.handle_shim_event(
                member_name,
                Event::ConfigurationRequired {
                    message: "Codex thread identity is already owned by another member".into(),
                    code: Some("duplicate_thread_id".into()),
                },
            );
        }
        if let Some(watcher) = self.watchers.get_mut(member_name) {
            watcher.set_session_id(Some(session_id.clone()));
        }
        self.persist_member_session_id(member_name, &session_id)?;
        Ok(())
    }

    /// Shim finished starting (or reconnecting): flip to idle and drain queued messages.
    pub(super) fn on_ready(&mut self, member_name: &str) -> Result<()> {
        let _ = append_shim_event_log(&self.config.project_root, member_name, "<- ready");
        info!(member = member_name, "shim agent ready");
        self.health.context_pressure.clear_member(member_name);

        // Only preserve Working state if the *shim handle* was Working
        // (meaning work was mid-flight when the SDK connection dropped).
        // Do NOT use self.states (persisted daemon state) here — after a
        // full daemon restart the agent is freshly spawned and not working
        // on anything, even if the persisted state said Working.
        let preserve_working = self
            .shim_handles
            .get(member_name)
            .map(|handle| handle.state == ShimState::Working)
            .unwrap_or(false);

        if preserve_working {
            debug!(
                member = member_name,
                "shim ready received while work already in flight; preserving working state"
            );
        } else {
            if let Some(handle) = self.shim_handles.get_mut(member_name) {
                handle.apply_state_change(ShimState::Idle);
                handle.clear_in_flight_messages();
            }
            self.states
                .insert(member_name.to_string(), MemberState::Idle);
            self.update_automation_timers_for_state(member_name, MemberState::Idle);
        }

        // Drain pending messages AFTER the state flip so that
        // deliver_message → handle.is_ready() sees Idle. Otherwise
        // the drain re-defers every message it just pulled out,
        // and they rot until `expire_stale_pending_messages` fires
        // (600s later) and kicks them to inbox fallback.
        if self.delivery.pending_queue.contains_key(member_name) {
            if let Err(error) = self.drain_pending_queue(member_name) {
                warn!(
                    member = member_name,
                    error = %error,
                    "failed to drain pending queue after shim ready"
                );
            }
        }

        self.maybe_persist_member_session_id(member_name);
        Ok(())
    }

    /// Shim classifier reported a state transition.
    pub(super) fn on_state_changed(
        &mut self,
        member_name: &str,
        from: ShimState,
        to: ShimState,
        summary: String,
    ) -> Result<()> {
        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!("<- state {from} -> {to}: {summary}"),
        );
        debug!(
            member = member_name,
            from = %from,
            to = %to,
            "shim state change"
        );

        if let Some(handle) = self.shim_handles.get_mut(member_name) {
            handle.apply_state_change(to);
        }

        // Drain pending messages on Idle transition. Previously this
        // was only done on Event::Ready and on force-idle timeout, so
        // a shim that went Working→Idle naturally would leave its
        // pending queue stuck until the 600s expiry fallback fired.
        if to == ShimState::Idle && self.delivery.pending_queue.contains_key(member_name) {
            if let Err(error) = self.drain_pending_queue(member_name) {
                warn!(
                    member = member_name,
                    error = %error,
                    "failed to drain pending queue after state change to idle"
                );
            }
        }

        let member_state = match to {
            ShimState::Idle => MemberState::Idle,
            ShimState::Working => MemberState::Working,
            ShimState::Starting => MemberState::Working,
            ShimState::Dead | ShimState::ContextExhausted => MemberState::Working,
        };

        let prev_state = self.states.get(member_name).copied();
        if prev_state != Some(member_state) {
            self.states.insert(member_name.to_string(), member_state);
            self.update_automation_timers_for_state(member_name, member_state);
            // #690: track continuous Working windows so a fresh
            // Idle→Working transition does not get killed for
            // lifetime-zero-output on a long-idle shim.
            if member_state == MemberState::Working {
                self.health
                    .working_since
                    .entry(member_name.to_string())
                    .or_insert_with(Instant::now);
            } else {
                self.health.working_since.remove(member_name);
            }
        }
        if member_state != MemberState::Working {
            self.health.context_pressure.mark_not_working(member_name);
        }

        self.maybe_persist_member_session_id(member_name);

        // Handle terminal states
        if to == ShimState::Dead {
            warn!(member = member_name, "shim reports agent died");
            self.emit_event(TeamEvent::pane_death(member_name));
            self.record_orchestrator_action(format!(
                "lifecycle: shim reported death for {} — {}",
                member_name, summary
            ));
        } else if to == ShimState::ContextExhausted {
            let task_id = self.active_task_id(member_name);
            warn!(
                member = member_name,
                task_id, "shim reports context exhaustion"
            );
            self.record_context_exhausted(member_name, task_id, None);
            self.record_orchestrator_action(format!(
                "lifecycle: shim reported context exhaustion for {} — {}",
                member_name, summary
            ));
        }
        Ok(())
    }

    /// Shim acknowledged delivery of a tracked message.
    pub(super) fn on_message_delivered(&mut self, member_name: &str, id: String) -> Result<()> {
        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!("<- message_delivered id={id}"),
        );
        if let Some(handle) = self.shim_handles.get_mut(member_name) {
            handle.record_activity();
        }
        if let Some(pending) = self.delivery.pending_shim_acks.remove(&id) {
            let inbox_root = inbox::inboxes_root(&self.config.project_root);
            for message in pending.inbox_messages {
                if let Err(error) =
                    inbox::mark_delivered(&inbox_root, &pending.recipient, &message.id)
                {
                    warn!(
                        member = pending.recipient.as_str(),
                        id = message.id.as_str(),
                        error = %error,
                        "failed to mark shim-acked inbox message delivered"
                    );
                } else {
                    self.record_message_routed(&message.from, &pending.recipient);
                    self.record_notification_delivery_sample(
                        &message.from,
                        &pending.recipient,
                        message.age_secs,
                        message.sample_kind,
                    );
                }
            }
            self.record_orchestrator_action(format!(
                "delivery: ack recipient={} message_id={id}",
                pending.recipient
            ));
        }
        debug!(
            member = member_name,
            message_id = id,
            "shim delivered message"
        );
        Ok(())
    }

    /// Agent finished a turn: run planning or engineer completion flows.
    pub(super) fn on_completion(
        &mut self,
        member_name: &str,
        message_id: Option<String>,
        response: String,
        last_lines: String,
    ) -> Result<()> {
        let summary = if response.trim().is_empty() {
            last_lines.lines().last().unwrap_or("").trim().to_string()
        } else {
            response.lines().next().unwrap_or("").trim().to_string()
        };
        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!("<- completion: {summary}"),
        );
        info!(
            member = member_name,
            response_len = response.len(),
            "shim reports completion"
        );

        if let Some(handle) = self.shim_handles.get_mut(member_name) {
            handle.apply_state_change(ShimState::Idle);
            handle.complete_in_flight_message(message_id.as_deref());
        }
        self.states
            .insert(member_name.to_string(), MemberState::Idle);
        self.update_automation_timers_for_state(member_name, MemberState::Idle);
        self.maybe_persist_member_session_id(member_name);

        if self.handle_stalled_mid_turn_completion(member_name, &response)? {
            return Ok(());
        }

        let is_architect = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .is_some_and(|member| member.role_type == RoleType::Architect);
        if is_architect && self.scheduler.planning_cycle_active {
            if let Err(error) = self.handle_planning_response(&response) {
                warn!(
                    member = member_name,
                    error = %error,
                    "planning response handling failed"
                );
            }
            return Ok(());
        }

        let zero_diff_completion = self.maybe_track_zero_diff_completion(member_name)?;

        // Trigger engineer completion flow if there's an active task
        if self.active_task_id(member_name).is_some() {
            if let Err(error) = merge::handle_engineer_completion(self, member_name) {
                warn!(
                    member = member_name,
                    error = %error,
                    "shim completion handling failed"
                );
            }
        }
        if let Some((task_id, count)) = zero_diff_completion {
            self.maybe_recover_zero_diff_completion_loop(member_name, task_id, count)?;
        }

        let _ = last_lines; // Available for logging if needed
        Ok(())
    }

    /// Agent process died: respawn, park, or escalate.
    pub(super) fn on_died(
        &mut self,
        member_name: &str,
        exit_code: Option<i32>,
        last_lines: String,
    ) -> Result<()> {
        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!(
                "<- died exit={exit_code:?}: {}",
                last_lines.lines().last().unwrap_or("").trim()
            ),
        );
        warn!(member = member_name, exit_code, "shim agent process died");

        if let Some(handle) = self.shim_handles.get_mut(member_name) {
            handle.apply_state_change(ShimState::Dead);
        }

        self.emit_event(TeamEvent::pane_death(member_name));
        self.record_orchestrator_action(format!(
            "lifecycle: shim process died for {} (exit_code={:?}): {}",
            member_name, exit_code, last_lines
        ));

        if self
            .health
            .member_backend_parked(&self.config.project_root, member_name)
        {
            // Backend is parked (quota exhausted or auth required).
            // Respawning the shim would just burn retries against the
            // same non-recoverable condition, which is exactly the
            // failure mode we observed in the field (140x auth
            // retries against a dead refresh token). Record the
            // crash and wait for an operator to unpark the member.
            self.record_member_crashed(member_name, false);
            info!(
                member = member_name,
                "skipping shim respawn — backend parked; waiting for operator"
            );
            self.record_orchestrator_action(format!(
                "lifecycle: skipped shim respawn for {member_name} — backend parked"
            ));
        } else if self.config.team_config.auto_respawn_on_crash {
            self.record_member_crashed(member_name, true);
            let respawn = if self.should_cold_respawn_codex_member(member_name, &last_lines) {
                self.handle_shim_cold_respawn(member_name, "missing saved Codex session")
            } else {
                self.handle_shim_crash_respawn(member_name)
            };
            if let Err(error) = respawn {
                warn!(
                    member = member_name,
                    error = %error,
                    "shim crash respawn failed"
                );
            }
        } else {
            self.record_member_crashed(member_name, false);
            self.escalate_shim_crash(member_name, exit_code);
        }
        Ok(())
    }

    /// Shim reports the agent exhausted its context window.
    pub(super) fn on_context_exhausted(
        &mut self,
        member_name: &str,
        message: String,
        last_lines: String,
    ) -> Result<()> {
        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!("<- context_exhausted: {message}"),
        );
        let task_id = self.active_task_id(member_name);
        warn!(
            member = member_name,
            task_id,
            message = message.as_str(),
            "shim reports context exhaustion"
        );

        // Send Shutdown to the exhausted shim before respawning
        if let Some(handle) = self.shim_handles.get_mut(member_name) {
            handle.apply_state_change(ShimState::ContextExhausted);
            if let Err(error) =
                handle.send_shutdown_with_reason(5, ShutdownReason::ContextExhausted)
            {
                debug!(
                    member = member_name,
                    error = %error,
                    "failed to send shutdown to context-exhausted shim"
                );
            }
        }

        self.record_context_exhausted(member_name, task_id, None);
        self.record_orchestrator_action(format!(
            "lifecycle: shim context exhausted for {}: {} — {}",
            member_name, message, last_lines
        ));

        if let Err(error) = self.handle_context_exhaustion(member_name) {
            warn!(
                member = member_name,
                error = %error,
                "shim context exhaustion restart failed"
            );
        }
        Ok(())
    }

    /// Shim warns the agent is nearing its context limit.
    pub(super) fn on_context_approaching(
        &mut self,
        member_name: &str,
        message: String,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!(
                "<- context_approaching: {message} (input_tokens={input_tokens}, output_tokens={output_tokens})"
            ),
        );
        warn!(
            member = member_name,
            input_tokens,
            output_tokens,
            "shim reports context approaching limit — proactive handoff"
        );
        self.record_orchestrator_action(format!(
            "health: proactive context warning for {} — {} (tokens: {}/{})",
            member_name, message, input_tokens, output_tokens
        ));
        self.handle_context_pressure_warning(member_name, 0, 0, 80)?;
        Ok(())
    }

    /// Heartbeat reply; deliberately silent.
    pub(super) fn on_pong(&mut self, member_name: &str) -> Result<()> {
        if let Some(handle) = self.shim_handles.get_mut(member_name) {
            handle.record_pong();
        }
        Ok(())
    }

    /// Periodic output/usage stats; deliberately kept out of the event log.
    pub(super) fn on_session_stats(
        &mut self,
        member_name: &str,
        output_bytes: u64,
        uptime_secs: u64,
        context_usage_pct: Option<u8>,
    ) -> Result<()> {
        if let Some(handle) = self.shim_handles.get_mut(member_name) {
            handle.record_output_bytes(output_bytes);
        }
        self.handle_context_pressure_stats(
            member_name,
            output_bytes,
            uptime_secs,
            context_usage_pct,
        )?;
        Ok(())
    }

    /// Shim reports proactive context pressure.
    pub(super) fn on_context_warning(
        &mut self,
        member_name: &str,
        stats: ContextWarningStats,
    ) -> Result<()> {
        let ContextWarningStats {
            model,
            output_bytes,
            uptime_secs,
            used_tokens,
            context_limit_tokens,
            usage_pct,
        } = stats;
        if let Some(handle) = self.shim_handles.get_mut(member_name) {
            handle.record_output_bytes(output_bytes);
        }
        let model_label = model
            .as_deref()
            .map(|value| format!(" model={value}"))
            .unwrap_or_default();
        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!(
                "<- context_warning usage_pct={usage_pct} used_tokens={used_tokens} limit_tokens={context_limit_tokens}{model_label}"
            ),
        );
        warn!(
            member = member_name,
            model = model.as_deref().unwrap_or("unknown"),
            output_bytes,
            uptime_secs,
            used_tokens,
            context_limit_tokens,
            usage_pct,
            "shim reports proactive context pressure"
        );
        self.record_orchestrator_action(format!(
            "health: proactive context warning for {} at {}% of {} tokens{}",
            member_name, usage_pct, context_limit_tokens, model_label
        ));
        self.handle_context_pressure_warning(member_name, output_bytes, uptime_secs, usage_pct)?;
        Ok(())
    }

    /// Shim emitted a warning; surfaces genuine stalls to the manager.
    pub(super) fn on_warning(
        &mut self,
        member_name: &str,
        message: String,
        idle_secs: Option<u64>,
    ) -> Result<()> {
        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!("<- warning idle_secs={idle_secs:?}: {message}"),
        );

        // "message queued while agent working" is normal queue behavior,
        // not a stall. Only surface genuine stall warnings to the manager.
        if message.contains("message queued while agent working") {
            info!(
                member = member_name,
                message = message.as_str(),
                "shim warning: message queued (not a stall)"
            );
        } else {
            info!(
                member = member_name,
                idle_secs,
                message = message.as_str(),
                "shim warning: potential stall"
            );
            self.surface_shim_stall_warning(member_name, &message, idle_secs);
        }
        Ok(())
    }

    /// Shim could not deliver a message.
    pub(super) fn on_delivery_failed(
        &mut self,
        member_name: &str,
        id: String,
        reason: String,
    ) -> Result<()> {
        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!("<- delivery_failed id={id}: {reason}"),
        );
        if let Some(handle) = self.shim_handles.get_mut(member_name) {
            handle.record_activity();
            if handle.remove_in_flight_message(&id) {
                handle.apply_state_change(ShimState::Idle);
            }
        }
        if let Some(pending) = self.delivery.pending_shim_acks.remove(&id) {
            if pending.inbox_messages.is_empty() {
                self.record_failed_delivery_with_id(
                    &pending.recipient,
                    &pending.from,
                    &pending.body,
                    Some(id.clone()),
                );
            }
            self.record_orchestrator_action(format!(
                "delivery: failure recipient={} message_id={} reason={}",
                pending.recipient, id, reason
            ));
        }
        self.record_delivery_failed_with_details(
            member_name,
            "shim",
            reason.as_str(),
            Some(&format!("message_id={id}")),
        );
        warn!(
            member = member_name,
            message_id = id,
            reason = reason.as_str(),
            "shim failed to deliver message"
        );
        Ok(())
    }

    /// Shim reported a command error.
    pub(super) fn on_error(
        &mut self,
        member_name: &str,
        command: String,
        reason: String,
    ) -> Result<()> {
        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!("<- error {command}: {reason}"),
        );

        // Quota exhaustion: mark backend unhealthy and stop dispatching.
        // Don't restart the agent — it will just hit the same error.
        if command == "QuotaExhausted" {
            warn!(
                member = member_name,
                reason = reason.as_str(),
                "backend quota exhausted — pausing agent"
            );
            self.health.backend.insert(
                member_name.to_string(),
                crate::agent::BackendHealth::QuotaExhausted,
            );
            self.record_orchestrator_action(format!(
                "quota: {member_name} backend quota exhausted — {reason}"
            ));
            self.emit_event(crate::team::events::TeamEvent::backend_quota_exhausted(
                member_name,
                &reason,
            ));
            return Ok(());
        }

        self.health.context_pressure.record_failure(member_name);
        if self.should_cold_respawn_codex_member(member_name, &reason) {
            self.record_member_crashed(member_name, true);
            if let Err(error) =
                self.handle_shim_cold_respawn(member_name, "missing saved Codex session")
            {
                warn!(
                    member = member_name,
                    error = %error,
                    "cold shim respawn after resume failure failed"
                );
            }
            return Ok(());
        }
        debug!(member = member_name, command, reason, "shim error");
        Ok(())
    }

    /// Backend quota is blocked until a retry deadline.
    pub(super) fn on_quota_blocked(
        &mut self,
        member_name: &str,
        message: String,
        retry_at_epoch_secs: Option<u64>,
        retry_at_label: Option<String>,
    ) -> Result<()> {
        let label = retry_at_label.as_deref().unwrap_or("unknown");
        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!("<- quota_blocked until={label}: {message}"),
        );
        warn!(
            member = member_name,
            retry_at = label,
            "backend quota blocked — pausing dispatch"
        );
        self.health.backend.insert(
            member_name.to_string(),
            crate::agent::BackendHealth::QuotaExhausted,
        );
        // Record the retry_at deadline so that backend-health transition
        // checks, dispatch selection, and aging alerts keep the member
        // parked until the quota window elapses (#674 defect 1).
        // A successful poll_shim ping is NOT evidence of quota recovery.
        if let Some(retry_at) = retry_at_epoch_secs {
            self.health
                .quota_retry_at
                .insert(member_name.to_string(), retry_at);
        }
        self.record_orchestrator_action(format!(
            "quota: {member_name} backend blocked until {label} — {message}"
        ));
        self.emit_event(crate::team::events::TeamEvent::backend_quota_exhausted(
            member_name,
            &message,
        ));
        Ok(())
    }

    /// Backend needs operator configuration repair.
    pub(super) fn on_configuration_required(
        &mut self,
        member_name: &str,
        code: Option<String>,
    ) -> Result<()> {
        self.health.backend.insert(
            member_name.to_string(),
            crate::agent::BackendHealth::ConfigurationRequired,
        );
        // Persist only a fixed diagnostic: provider messages may contain
        // request details and must not enter this durable operator marker.
        let directory = self
            .config
            .project_root
            .join(".batty/backend-configuration");
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(format!("{member_name}.json"));
        std::fs::write(
            &path,
            "{\"reason\":\"backend configuration requires repair\"}\n",
        )?;

        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!(
                "<- configuration_required: {}",
                code.as_deref().unwrap_or("model_configuration")
            ),
        );
        self.record_orchestrator_action(format!(
            "configuration: {member_name} requires operator repair before retry ({})",
            code.as_deref().unwrap_or("model_configuration")
        ));
        Ok(())
    }

    /// Backend needs the operator to re-login.
    pub(super) fn on_auth_required(&mut self, member_name: &str, message: String) -> Result<()> {
        let _ = append_shim_event_log(
            &self.config.project_root,
            member_name,
            &format!("<- auth_required: {message}"),
        );
        warn!(
            member = member_name,
            "backend auth required — pausing dispatch until operator re-logs in"
        );
        // AuthRequired is non-recoverable without human action. Park
        // the member and stop any respawn attempts until an operator
        // resolves the credential and manually clears the state.
        self.health.backend.insert(
            member_name.to_string(),
            crate::agent::BackendHealth::AuthRequired,
        );
        self.record_orchestrator_action(format!(
            "auth: {member_name} backend requires re-login — {message}"
        ));
        self.emit_event(crate::team::events::TeamEvent::backend_auth_required(
            member_name,
            &message,
        ));
        Ok(())
    }
}
