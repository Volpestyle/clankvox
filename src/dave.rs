use std::collections::HashMap;
use std::num::NonZeroU16;
use std::time::Instant;

use anyhow::{Context, Result};
use davey::errors::{DecryptError, DecryptorDecryptError};
use davey::{DaveSession, MediaType, ProposalsOperationType};
use tracing::{debug, info, warn};

/// Maximum consecutive DAVE decrypt failures before triggering session recovery.
/// Node version uses 200 to give clients enough time to transition.
pub const FAILURE_TOLERANCE: u32 = 200;

/// Passthrough expiry (seconds) for pending downgrades (`protocol_version` → 0).
const PASSTHROUGH_DOWNGRADE_EXPIRY: u32 = 24;

/// Passthrough expiry (seconds) for upgrades (`protocol_version` 0 → N).
const PASSTHROUGH_TRANSITION_EXPIRY: u32 = 10;

/// Maximum seconds to wait for OP22 (Execute Transition) after OP21 prepares a pv=0
/// downgrade. If OP22 hasn't arrived after this timeout, auto-execute the downgrade
/// so the bot can hear unencrypted audio from peers who have already transitioned.
const PENDING_DOWNGRADE_AUTO_EXECUTE_SECS: u64 = 3;

pub struct DaveManager {
    session: DaveSession,
    ready: bool,
    user_id: u64,
    channel_id: u64,
    protocol_version: u16,
    /// Pending transitions: `transition_id` → `protocol_version`.
    /// Populated by OP21 (prepare transition) and OP29/OP30 (commit/welcome).
    /// Consumed by OP22 (execute transition).
    pending_transitions: HashMap<u16, u16>,
    last_transition_id: u16,
    consecutive_failures: u32,
    reinitializing: bool,
    /// Timestamp of when a pv=0 downgrade transition was prepared (OP22).
    /// Used to auto-execute the transition if OP22 never arrives.
    pending_downgrade_since: Option<Instant>,
}

/// Serialized commit+welcome ready to send as OP28 binary.
pub struct CommitResponse {
    pub data: Vec<u8>,
}

/// Recovery action: reinit produced a new key package that must be sent to Discord.
pub struct RecoveryAction {
    pub transition_id: u16,
    pub key_package: Vec<u8>,
}

impl DaveManager {
    pub fn new(protocol_version: u16, user_id: u64, channel_id: u64) -> Result<(Self, Vec<u8>)> {
        let pv =
            NonZeroU16::new(protocol_version).context("DAVE protocol version must be non-zero")?;

        let mut session = DaveSession::new(pv, user_id, channel_id, None)
            .map_err(|e| anyhow::anyhow!("DaveSession::new failed: {e:?}"))?;

        info!(
            "DAVE session created: protocol_version={} user_id={} channel_id={}",
            protocol_version, user_id, channel_id
        );

        let pkg = session
            .create_key_package()
            .map_err(|e| anyhow::anyhow!("create_key_package: {e:?}"))?;

        Ok((
            Self {
                session,
                ready: false,
                user_id,
                channel_id,
                protocol_version,
                pending_transitions: HashMap::new(),
                last_transition_id: 0,
                consecutive_failures: 0,
                reinitializing: false,
                pending_downgrade_since: None,
            },
            pkg,
        ))
    }

    pub fn set_external_sender(&mut self, data: &[u8]) -> Result<()> {
        self.session
            .set_external_sender(data)
            .map_err(|e| anyhow::anyhow!("set_external_sender: {e:?}"))?;
        debug!("DAVE: external sender set ({} bytes)", data.len());
        Ok(())
    }

    pub fn process_proposals(
        &mut self,
        op_type: ProposalsOperationType,
        proposals: &[u8],
        user_ids: Option<&[u64]>,
    ) -> Result<Option<CommitResponse>> {
        let result = self
            .session
            .process_proposals(op_type, proposals, user_ids)
            .map_err(|e| anyhow::anyhow!("process_proposals: {e:?}"))?;

        debug!(
            "DAVE: proposals processed ({} bytes), has_commit={}",
            proposals.len(),
            result.is_some()
        );

        Ok(result.map(|cw| {
            let mut data = cw.commit;
            if let Some(welcome) = cw.welcome {
                data.extend_from_slice(&welcome);
            }
            CommitResponse { data }
        }))
    }

    pub fn process_welcome(&mut self, data: &[u8]) -> Result<()> {
        self.session
            .process_welcome(data)
            .map_err(|e| anyhow::anyhow!("process_welcome: {e:?}"))?;
        self.ready = self.session.is_ready();
        if self.ready {
            info!("DAVE: session ready after welcome");
        }
        Ok(())
    }

    pub fn process_commit(&mut self, data: &[u8]) -> Result<()> {
        self.session
            .process_commit(data)
            .map_err(|e| anyhow::anyhow!("process_commit: {e:?}"))?;
        self.ready = self.session.is_ready();
        if self.ready {
            info!("DAVE: session ready after commit");
        }
        Ok(())
    }

    pub fn encrypt_opus(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        if !self.ready || self.protocol_version == 0 {
            return Ok(frame.to_vec());
        }
        self.session
            .encrypt_opus(frame)
            .map(std::borrow::Cow::into_owned)
            .map_err(|e| anyhow::anyhow!("encrypt_opus: {e:?}"))
    }

    pub fn decrypt(&mut self, sender_user_id: u64, frame: &[u8]) -> Result<Vec<u8>> {
        if !self.ready {
            return Ok(frame.to_vec());
        }
        if self.protocol_version == 0 {
            // DAVE disabled (downgrade executed). Frames should be plain Opus, but
            // peers still transitioning may send DAVE-encrypted frames with keys from
            // their old MLS group. Try decrypt; if the frame is unencrypted (no magic
            // marker), pass it through. If it's encrypted with unknown keys, drop it —
            // passing raw DAVE frames to Opus produces garbage.
            return match self
                .session
                .decrypt(sender_user_id, MediaType::AUDIO, frame)
            {
                Ok(decrypted) => {
                    self.consecutive_failures = 0;
                    Ok(decrypted)
                }
                Err(DecryptError::DecryptionFailed(
                    DecryptorDecryptError::UnencryptedWhenPassthroughDisabled,
                )) => {
                    // Frame has no DAVE magic marker — it's plain Opus. Pass through.
                    Ok(frame.to_vec())
                }
                Err(_) => {
                    // Frame is DAVE-encrypted with keys we don't have. Drop it.
                    Err(anyhow::anyhow!(
                        "decrypt: encrypted with unknown keys (pv=0 transition)"
                    ))
                }
            };
        }
        match self
            .session
            .decrypt(sender_user_id, MediaType::AUDIO, frame)
        {
            Ok(decrypted) => {
                self.consecutive_failures = 0;
                Ok(decrypted)
            }
            Err(DecryptError::DecryptionFailed(
                DecryptorDecryptError::UnencryptedWhenPassthroughDisabled,
            )) => {
                // Some live sessions can briefly deliver plaintext Opus even while
                // protocol_version remains non-zero. Treat these frames as passthrough
                // instead of dropping the user audio stream.
                self.consecutive_failures = 0;
                Ok(frame.to_vec())
            }
            Err(e) => Err(anyhow::anyhow!("decrypt: {e:?}")),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    // --- Transition management (matches discord.js DAVESession) ---

    /// Handle OP21 (prepare transition). Returns `true` if the caller should send OP23.
    pub fn prepare_transition(&mut self, transition_id: u16, protocol_version: u16) -> bool {
        info!(
            "DAVE: prepare transition id={} pv={}",
            transition_id, protocol_version
        );
        self.pending_transitions
            .insert(transition_id, protocol_version);

        if transition_id == 0 {
            // Transition 0 is (re)initialization — execute immediately
            self.execute_transition(transition_id);
            false
        } else {
            if protocol_version == 0 {
                // Downgrade pending — set passthrough mode so mixed encrypted/plain
                // frames are tolerated during the transition window.
                self.session
                    .set_passthrough_mode(true, Some(PASSTHROUGH_DOWNGRADE_EXPIRY));
                self.pending_downgrade_since = Some(Instant::now());
            }
            true // caller should send OP23 DaveTransitionReady
        }
    }

    /// Handle OP22 (execute transition). Returns `true` if transition was executed.
    pub fn execute_transition(&mut self, transition_id: u16) -> bool {
        if let Some(new_pv) = self.pending_transitions.remove(&transition_id) {
            let old_pv = self.protocol_version;
            self.protocol_version = new_pv;

            if old_pv != new_pv && new_pv == 0 {
                info!("DAVE: session downgraded (v{} -> v0)", old_pv);
                self.pending_downgrade_since = None;
            } else if transition_id > 0 && new_pv > 0 {
                self.session
                    .set_passthrough_mode(true, Some(PASSTHROUGH_TRANSITION_EXPIRY));
                info!("DAVE: session upgraded (v{} -> v{})", old_pv, new_pv);
            }

            self.reinitializing = false;
            self.last_transition_id = transition_id;
            info!(
                "DAVE: transition executed (v{} -> v{}, id={})",
                old_pv, new_pv, transition_id
            );
            true
        } else {
            warn!(
                "DAVE: execute transition but no pending transition for id={}",
                transition_id
            );
            false
        }
    }

    /// Store a pending transition after successful commit/welcome processing.
    pub fn store_pending_transition(&mut self, transition_id: u16) {
        // Transition 0 is immediate initialization; it is not tracked in pending transitions.
        // Keeping id=0 pending blocks failure recovery and diverges from discord.js behavior.
        if transition_id == 0 {
            self.reinitializing = false;
            self.last_transition_id = 0;
            return;
        }

        self.pending_transitions
            .insert(transition_id, self.protocol_version);
    }

    /// Track a decryption failure. Returns `true` if recovery should be triggered.
    /// Failures during reinitialization or pending transitions are NOT counted
    /// (matches discord.js behavior).
    ///
    /// NOTE: Recovery (OP31 + OP26) causes Discord to close the voice WS with
    /// code 4006 ("Session is no longer valid"). discord.js handles this by
    /// auto-rejoining the voice channel, but our subprocess doesn't support
    /// reconnect yet. So we log the threshold but do NOT trigger recovery to
    /// keep the session alive. Undecryptable packets (from users with stale
    /// MLS group keys) are silently dropped.
    pub fn track_decrypt_failure(&mut self) -> bool {
        if self.reinitializing || !self.pending_transitions.is_empty() {
            return false;
        }
        self.consecutive_failures += 1;
        if self.consecutive_failures == FAILURE_TOLERANCE + 1 {
            warn!(
                "DAVE: {} consecutive decrypt failures exceeded tolerance ({}); \
                 suppressing OP31 recovery to avoid 4006 disconnect",
                self.consecutive_failures, FAILURE_TOLERANCE
            );
        }
        // Return false to suppress recovery — the session stays alive and
        // undecryptable packets are dropped. If the remote user eventually
        // re-keys (e.g. via a new epoch), decryption will resume.
        false
    }

    /// Reinitialize the DAVE session from scratch. Returns a `RecoveryAction`
    /// containing the `transition_id` to invalidate (OP31) and a new key package (OP26).
    pub fn reinit(&mut self) -> Result<RecoveryAction> {
        let transition_id = self.last_transition_id;
        info!(
            "DAVE: reinitializing session (invalidating transition {})",
            transition_id
        );

        let pv = NonZeroU16::new(self.protocol_version)
            .context("DAVE protocol version must be non-zero for reinit")?;

        let new_session = DaveSession::new(pv, self.user_id, self.channel_id, None)
            .map_err(|e| anyhow::anyhow!("DaveSession reinit: {e:?}"))?;

        self.session = new_session;
        let pkg = self
            .session
            .create_key_package()
            .map_err(|e| anyhow::anyhow!("reinit create_key_package: {e:?}"))?;

        self.ready = false;
        self.reinitializing = true;
        self.consecutive_failures = 0;
        self.pending_transitions.clear();
        self.pending_downgrade_since = None;

        info!(
            "DAVE: session reinitialized, new key package ({} bytes)",
            pkg.len()
        );

        Ok(RecoveryAction {
            transition_id,
            key_package: pkg,
        })
    }

    /// Check if a pending pv=0 downgrade has timed out waiting for OP22.
    /// If so, auto-execute it so the bot can hear unencrypted audio.
    /// Returns the `transition_id` that was auto-executed, if any.
    pub fn maybe_auto_execute_downgrade(&mut self) -> Option<u16> {
        let since = self.pending_downgrade_since?;
        if since.elapsed().as_secs() < PENDING_DOWNGRADE_AUTO_EXECUTE_SECS {
            return None;
        }

        // Find the pending pv=0 transition
        let tid = self
            .pending_transitions
            .iter()
            .find(|&(_, &pv)| pv == 0)
            .map(|(&tid, _)| tid);

        if let Some(tid) = tid {
            warn!(
                "DAVE: auto-executing pv=0 downgrade (transition_id={}) after {}s without OP23",
                tid,
                since.elapsed().as_secs()
            );
            self.execute_transition(tid);
            Some(tid)
        } else {
            // Pending transitions changed — clear the timestamp
            self.pending_downgrade_since = None;
            None
        }
    }

    pub fn has_pending_transition_id(&self, transition_id: u16) -> bool {
        self.pending_transitions.contains_key(&transition_id)
    }

    pub fn can_passthrough(&self, user_id: u64) -> bool {
        self.session.can_passthrough(user_id)
    }

    pub fn user_id(&self) -> u64 {
        self.user_id
    }

    pub fn known_user_ids(&self) -> Vec<u64> {
        self.session.get_user_ids().unwrap_or_default()
    }

    pub fn protocol_version(&self) -> u16 {
        self.protocol_version
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{DaveManager, PENDING_DOWNGRADE_AUTO_EXECUTE_SECS};

    fn new_manager() -> DaveManager {
        DaveManager::new(1, 42, 24)
            .expect("manager should initialize")
            .0
    }

    #[test]
    fn transition_zero_executes_immediately() {
        let mut manager = new_manager();

        assert!(!manager.prepare_transition(0, 0));
        assert_eq!(manager.protocol_version(), 0);
        assert!(!manager.has_pending_transition_id(0));
    }

    #[test]
    fn transition_zero_is_not_tracked_as_pending() {
        let mut manager = new_manager();

        manager.store_pending_transition(0);

        assert!(!manager.has_pending_transition_id(0));
        assert_eq!(manager.last_transition_id, 0);
        assert!(!manager.reinitializing);
    }

    #[test]
    fn decrypt_failures_are_suppressed_while_transition_is_pending() {
        let mut manager = new_manager();

        assert!(manager.prepare_transition(7, 1));
        assert!(!manager.track_decrypt_failure());
        assert_eq!(manager.consecutive_failures, 0);
    }

    #[test]
    fn pending_downgrade_auto_executes_after_timeout() {
        let mut manager = new_manager();

        assert!(manager.prepare_transition(9, 0));
        manager.pending_downgrade_since =
            Some(Instant::now() - Duration::from_secs(PENDING_DOWNGRADE_AUTO_EXECUTE_SECS + 1));

        assert_eq!(manager.maybe_auto_execute_downgrade(), Some(9));
        assert_eq!(manager.protocol_version(), 0);
        assert!(!manager.has_pending_transition_id(9));
        assert!(manager.pending_downgrade_since.is_none());
    }
}
