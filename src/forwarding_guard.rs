//! Shared Windows IPv4 forwarding protection for one explicitly selected physical uplink.
//!
//! The caller owns the core lifecycle/execution lease. Recovery and restoration may only
//! run after that caller has confirmed that no core still depends on these settings.
//! No request-supplied journal paths or automatic outbound-interface fallback are accepted.

use anyhow::Result;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterfaceUnavailableReason {
    Missing,
    Disconnected,
}

#[cfg(test)]
mod tests {
    use super::state::{Adapter, Backend, Phase, Record, State, Target};
    use anyhow::{Result, bail};
    use std::collections::HashMap;

    #[derive(Default)]
    enum NextChange {
        #[default]
        Normal,
        CompetingWriter,
        AppliedThenError,
    }

    #[derive(Default)]
    struct Fake {
        forwarding: HashMap<[u8; 16], bool>,
        session: Option<String>,
        journal: Vec<Record>,
        archived: Vec<Record>,
        saves: usize,
        fail_save: Option<usize>,
        fail_archive: bool,
        permission_denied: bool,
        next_change: NextChange,
        writes: usize,
    }

    fn target(name: &str) -> Target {
        Target {
            name: name.to_owned(),
            guid: [if name == "A" { 1 } else { 2 }; 16],
        }
    }

    impl Backend for Fake {
        fn adapter(&mut self, name: &str) -> Result<Adapter> {
            self.inspect(&target(name))?
                .ok_or_else(|| anyhow::anyhow!("missing interface"))
        }
        fn inspect(&mut self, target: &Target) -> Result<Option<Adapter>> {
            Ok(self.forwarding.get(&target.guid).map(|forwarding| Adapter {
                target: target.clone(),
                forwarding: *forwarding,
                hardware: true,
                ethernet_or_wifi: true,
                up: true,
                uplink: true,
            }))
        }
        fn session(&mut self) -> Result<String> {
            Ok(self.session.get_or_insert_with(|| "same-boot".to_owned()).clone())
        }
        fn same_session(&mut self, session: &str) -> Result<bool> {
            Ok(self.session.as_deref() == Some(session))
        }
        fn can_change(&mut self) -> Result<()> {
            if self.permission_denied {
                bail!("administrator permission required");
            }
            Ok(())
        }
        fn change(&mut self, target: &Target, expected: bool, value: bool) -> Result<bool> {
            self.can_change()?;
            let current = self
                .forwarding
                .get_mut(&target.guid)
                .ok_or_else(|| anyhow::anyhow!("missing interface"))?;
            let next = std::mem::take(&mut self.next_change);
            if matches!(next, NextChange::CompetingWriter) {
                *current = value;
                return Ok(false);
            }
            if *current != expected {
                return Ok(false);
            }
            assert!(self.journal.iter().any(|record| record.target.guid == target.guid));
            *current = value;
            self.writes += 1;
            if matches!(next, NextChange::AppliedThenError) {
                bail!("read-back failed");
            }
            Ok(true)
        }
        fn save(&mut self, records: &[Record]) -> Result<()> {
            self.saves += 1;
            if self.fail_save == Some(self.saves) {
                bail!("disk full");
            }
            self.journal = records.to_vec();
            Ok(())
        }
        fn archive(&mut self, records: &[Record]) -> Result<()> {
            if self.fail_archive {
                bail!("disk full");
            }
            self.archived.extend_from_slice(records);
            Ok(())
        }
    }

    fn setup() -> (State, Fake) {
        (
            State::default(),
            Fake {
                forwarding: [(target("A").guid, true), (target("B").guid, true)].into(),
                ..Default::default()
            },
        )
    }

    fn restarted(backend: &Fake) -> State {
        let mut state = State::default();
        state.records = backend.journal.clone();
        state
    }

    #[test]
    fn one_transition_handles_switch_rollback_commit_and_confirmed_stop() -> Result<()> {
        let (mut state, mut backend) = setup();
        let first = state.prepare(&mut backend, Some("A"))?;
        state.commit(&mut backend, first)?;
        let next = state.prepare(&mut backend, Some("B"))?;
        state.rollback(&mut backend, next)?;
        assert!(!backend.forwarding[&target("A").guid]);
        assert!(backend.forwarding[&target("B").guid]);
        let next = state.prepare(&mut backend, Some("B"))?;
        state.commit(&mut backend, next)?;
        assert!(backend.forwarding[&target("A").guid]);
        assert!(!backend.forwarding[&target("B").guid]);
        state.restore(&mut backend)?;
        assert!(state.idle());
        assert!(backend.journal.is_empty());
        assert!(backend.forwarding[&target("B").guid]);
        Ok(())
    }

    #[test]
    fn durable_intent_failure_never_changes_the_os() {
        let (mut state, mut backend) = setup();
        backend.fail_save = Some(1);
        assert!(state.prepare(&mut backend, Some("A")).is_err());
        assert_eq!(backend.writes, 0);
        assert!(state.idle());
        assert!(backend.journal.is_empty());
    }

    #[test]
    fn compare_failed_and_cleanup_failed_never_claim_another_writers_value() -> Result<()> {
        for failed_save in [2, 3] {
            let (mut state, mut backend) = setup();
            backend.next_change = NextChange::CompetingWriter;
            backend.fail_save = Some(failed_save);
            assert!(state.prepare(&mut backend, Some("A")).is_err());
            assert_eq!(backend.writes, 0);
            assert!(!backend.forwarding[&target("A").guid]);
            let mut restart = restarted(&backend);
            backend.fail_save = None;
            let recovery = restart.recover(&mut backend);
            if failed_save == 2 {
                // Prepared on disk is ambiguous, not permission to restore another writer.
                assert!(recovery.is_err());
            } else {
                // Durable Retired can be safely cleaned without any adapter mutation.
                recovery?;
                assert!(restart.idle());
            }
            assert_eq!(backend.writes, 0);
            assert!(!backend.forwarding[&target("A").guid]);
        }
        Ok(())
    }

    #[test]
    fn completed_write_without_durable_proof_is_ambiguous_after_restart() -> Result<()> {
        let (mut state, mut backend) = setup();
        backend.fail_save = Some(2);
        assert!(state.prepare(&mut backend, Some("A")).is_err());
        assert_eq!(state.records[0].phase, Phase::Applied);
        assert_eq!(backend.journal[0].phase, Phase::Prepared);
        let mut restart = restarted(&backend);
        backend.fail_save = None;
        assert!(restart.recover(&mut backend).is_err());
        assert_eq!(backend.writes, 1);
        // The live owner still has direct proof, and can restore after a confirmed stop.
        state.restore(&mut backend)?;
        assert!(backend.forwarding[&target("A").guid]);
        Ok(())
    }

    #[test]
    fn uncertain_native_write_never_blindly_restores_on_recovery() -> Result<()> {
        let (mut state, mut backend) = setup();
        backend.next_change = NextChange::AppliedThenError;
        assert!(state.prepare(&mut backend, Some("A")).is_err());
        assert_eq!(backend.journal[0].phase, Phase::Prepared);
        let mut restart = restarted(&backend);
        assert!(restart.recover(&mut backend).is_err());
        assert_eq!(backend.writes, 1);
        assert!(!backend.forwarding[&target("A").guid]);
        Ok(())
    }

    #[test]
    fn restore_cleanup_failure_cannot_replay_an_old_applied_record() -> Result<()> {
        let (mut state, mut backend) = setup();
        let id = state.prepare(&mut backend, Some("A"))?;
        state.commit(&mut backend, id)?;
        backend.fail_save = Some(4); // Prepared, Applied, Restoring, then failed Retired.
        assert!(state.restore(&mut backend).is_err());
        assert!(backend.forwarding[&target("A").guid]);
        assert_eq!(backend.journal[0].phase, Phase::Restoring);
        backend.forwarding.insert(target("A").guid, false); // Later external writer.
        let writes = backend.writes;
        let mut restart = restarted(&backend);
        backend.fail_save = None;
        assert!(restart.recover(&mut backend).is_err());
        assert_eq!(backend.writes, writes);
        assert!(!backend.forwarding[&target("A").guid]);
        Ok(())
    }

    #[test]
    fn expired_boot_retirement_needs_no_adapter_and_preserves_records_on_disk_failure() -> Result<()> {
        for failed_archive in [true, false] {
            let (mut state, mut backend) = setup();
            let id = state.prepare(&mut backend, Some("A"))?;
            state.commit(&mut backend, id)?;
            let mut restart = restarted(&backend);
            backend.session = None;
            backend.forwarding.clear();
            backend.fail_archive = failed_archive;
            if !failed_archive {
                backend.fail_save = Some(3);
            }
            assert!(restart.retire_stale_records(&mut backend).is_err());
            assert_eq!(restart.records.len(), 1);
            assert_eq!(backend.journal.len(), 1);
            backend.fail_archive = false;
            backend.fail_save = None;
            restart.retire_stale_records(&mut backend)?;
            assert!(restart.idle());
            assert!(backend.journal.is_empty());
            assert_eq!(backend.writes, 1);
        }
        Ok(())
    }

    #[test]
    fn preflight_and_retained_start_do_not_restore_a_live_core_guard() -> Result<()> {
        let (mut state, mut backend) = setup();
        let id = state.prepare(&mut backend, Some("A"))?;
        state.retain(id)?;
        let writes = backend.writes;
        state.preflight(&mut backend, Some("A"))?;
        state.check(&mut backend)?;
        assert_eq!(backend.writes, writes);
        assert!(!backend.forwarding[&target("A").guid]);
        state.restore(&mut backend)?;
        assert!(state.idle());
        Ok(())
    }

    #[test]
    fn legacy_intent_defaults_to_ambiguous_not_applied() -> Result<()> {
        let record: Record = serde_json::from_value(serde_json::json!({
            "target": {"name": "A", "guid": [1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1]},
            "original_forwarding": true,
            "session": "same-boot"
        }))?;
        assert_eq!(record.phase, Phase::Prepared);
        Ok(())
    }

    #[test]
    fn confirmed_stop_recovers_a_cancelled_start_transaction() -> Result<()> {
        let (mut state, mut backend) = setup();
        let id = state.prepare(&mut backend, Some("A"))?;
        // Dropped start future never reached commit, retain, or rollback.
        assert!(state.restore(&mut backend).is_err());
        state.recover(&mut backend)?;
        assert!(state.idle());
        assert!(backend.forwarding[&target("A").guid]);
        assert!(state.commit(&mut backend, id).is_err());
        Ok(())
    }

    #[test]
    fn restore_intent_save_failure_keeps_the_live_owners_applied_proof() -> Result<()> {
        let (mut state, mut backend) = setup();
        let id = state.prepare(&mut backend, Some("A"))?;
        state.commit(&mut backend, id)?;
        backend.fail_save = Some(3);
        assert!(state.restore(&mut backend).is_err());
        assert_eq!(state.records[0].phase, Phase::Applied);
        assert_eq!(backend.journal[0].phase, Phase::Applied);
        assert_eq!(backend.writes, 1);
        backend.fail_save = None;
        state.restore(&mut backend)?;
        assert!(backend.forwarding[&target("A").guid]);
        assert!(state.idle());
        Ok(())
    }

    #[test]
    fn permission_denied_recovery_preserves_applied_proof_for_an_elevated_retry() -> Result<()> {
        let (mut state, mut backend) = setup();
        let id = state.prepare(&mut backend, Some("A"))?;
        state.commit(&mut backend, id)?;
        let mut restart = restarted(&backend);
        backend.permission_denied = true;
        assert!(restart.recover(&mut backend).is_err());
        assert_eq!(restart.records[0].phase, Phase::Applied);
        assert_eq!(backend.journal[0].phase, Phase::Applied);
        assert_eq!(backend.saves, 2);
        assert_eq!(backend.writes, 1);
        assert!(!backend.forwarding[&target("A").guid]);

        let mut elevated = restarted(&backend);
        backend.permission_denied = false;
        elevated.recover(&mut backend)?;
        assert!(elevated.idle());
        assert!(backend.journal.is_empty());
        assert!(backend.forwarding[&target("A").guid]);
        assert_eq!(backend.writes, 2);
        Ok(())
    }
}

#[derive(Debug)]
pub struct InterfaceUnavailable {
    pub name: String,
    pub reason: InterfaceUnavailableReason,
}

impl std::fmt::Display for InterfaceUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason {
            InterfaceUnavailableReason::Missing => {
                write!(formatter, "The selected TUN interface was not found: {}", self.name)
            }
            InterfaceUnavailableReason::Disconnected => write!(
                formatter,
                "The selected TUN interface is not a connected IPv4 uplink: {}",
                self.name
            ),
        }
    }
}

impl std::error::Error for InterfaceUnavailable {}

#[cfg(any(all(windows, not(feature = "test")), test))]
mod state {
    use super::{InterfaceUnavailable, InterfaceUnavailableReason};
    use anyhow::{Result, bail};
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub(super) struct Target {
        pub name: String,
        pub guid: [u8; 16],
    }

    pub(super) struct Adapter {
        pub target: Target,
        pub forwarding: bool,
        pub hardware: bool,
        pub ethernet_or_wifi: bool,
        pub up: bool,
        pub uplink: bool,
    }

    impl Adapter {
        pub fn validate(&self) -> Result<()> {
            if !self.hardware || !self.ethernet_or_wifi {
                bail!(
                    "TUN compatibility protection requires a physical Ethernet or Wi-Fi interface: {}",
                    self.target.name
                );
            }
            if !self.up || !self.uplink {
                return Err(InterfaceUnavailable {
                    name: self.target.name.clone(),
                    reason: InterfaceUnavailableReason::Disconnected,
                }
                .into());
            }
            Ok(())
        }
    }

    /// A durable intent is not proof of a completed OS write.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub(super) enum Phase {
        /// Also the safe interpretation of legacy version-one intent records.
        #[default]
        Prepared,
        Applied,
        Restoring,
        /// No longer owns an OS change, even if durable cleanup must be retried.
        Retired,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub(super) struct Record {
        pub target: Target,
        pub original_forwarding: bool,
        pub session: String,
        #[serde(default)]
        pub phase: Phase,
    }

    pub(super) trait Backend {
        fn adapter(&mut self, name: &str) -> Result<Adapter>;
        fn inspect(&mut self, target: &Target) -> Result<Option<Adapter>>;
        fn session(&mut self) -> Result<String>;
        fn same_session(&mut self, session: &str) -> Result<bool>;
        fn can_change(&mut self) -> Result<()>;
        fn change(&mut self, target: &Target, expected: bool, value: bool) -> Result<bool>;
        fn save(&mut self, records: &[Record]) -> Result<()>;
        fn archive(&mut self, records: &[Record]) -> Result<()>;
    }

    struct Pending {
        id: u64,
        target: Option<Target>,
        added: Option<Target>,
    }

    #[derive(Default)]
    pub(super) struct State {
        pub records: Vec<Record>,
        pub active: Option<Target>,
        pending: Option<Pending>,
        next_id: u64,
    }

    impl State {
        fn validate_name(name: &str) -> Result<()> {
            if name.trim().is_empty() || name.contains('\0') {
                bail!("TUN compatibility protection requires an explicitly selected outbound interface");
            }
            Ok(())
        }

        pub fn preflight(&self, backend: &mut impl Backend, name: Option<&str>) -> Result<()> {
            let Some(name) = name else { return Ok(()) };
            if self.pending.is_some() {
                bail!("A TUN compatibility protection transaction is already active");
            }
            Self::validate_name(name)?;
            let adapter = backend.adapter(name)?;
            adapter.validate()?;
            if adapter.forwarding
                && self
                    .records
                    .iter()
                    .any(|record| record.target.guid == adapter.target.guid && record.phase == Phase::Applied)
            {
                bail!("IPv4 forwarding on {name} changed outside TUN compatibility protection");
            }
            if adapter.forwarding || !self.records.is_empty() {
                backend.can_change()?;
            }
            Ok(())
        }

        pub fn prepare(&mut self, backend: &mut impl Backend, name: Option<&str>) -> Result<u64> {
            if self.pending.is_some() {
                bail!("A TUN compatibility protection transaction is already active");
            }
            if name.is_some() {
                self.restore_except(backend, self.active.clone().as_ref())?;
            }
            let mut added = None;
            let target = match name {
                Some(name) => {
                    Self::validate_name(name)?;
                    let adapter = backend.adapter(name)?;
                    adapter.validate()?;
                    let owned = self
                        .records
                        .iter()
                        .find(|record| record.target.guid == adapter.target.guid);
                    if owned.is_some_and(|record| record.phase != Phase::Applied) {
                        bail!("Forwarding ownership on {name} is unresolved; protection cannot be applied");
                    }
                    if owned.is_some() && adapter.forwarding {
                        bail!("IPv4 forwarding on {name} changed outside TUN compatibility protection");
                    }
                    if adapter.forwarding {
                        backend.can_change()?;
                        let record = Record {
                            target: adapter.target.clone(),
                            original_forwarding: true,
                            session: backend.session()?,
                            phase: Phase::Prepared,
                        };
                        let mut records = self.records.clone();
                        records.push(record);
                        // Never perform the OS write before the restoration intent is durable.
                        backend.save(&records)?;
                        self.records = records;
                        let index = self.records.len() - 1;
                        match backend.change(&adapter.target, true, false) {
                            Ok(true) => {
                                // Keep in-memory proof if committing the completed write fails.
                                self.records[index].phase = Phase::Applied;
                                backend.save(&self.records)?;
                                added = Some(adapter.target.clone());
                            }
                            Ok(false) => {
                                // A competing writer won. Its value must never become ours to restore.
                                self.records[index].phase = Phase::Retired;
                                backend.save(&self.records)?;
                                self.remove_record(backend, index)?;
                            }
                            Err(error) => {
                                // The backend may have completed the write before reporting an error.
                                // An ambiguous intent remains pending; do not guess during rollback.
                                return Err(error.context("TUN forwarding write outcome is unresolved"));
                            }
                        }
                    }
                    Some(adapter.target)
                }
                None => None,
            };
            self.next_id = self
                .next_id
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("TUN guard transaction counter exhausted"))?;
            self.pending = Some(Pending {
                id: self.next_id,
                target,
                added,
            });
            Ok(self.next_id)
        }

        fn take_pending(&mut self, id: u64) -> Result<Pending> {
            if self.pending.as_ref().is_none_or(|pending| pending.id != id) {
                bail!("TUN compatibility protection transaction is no longer current");
            }
            self.pending
                .take()
                .ok_or_else(|| anyhow::anyhow!("TUN compatibility transaction disappeared"))
        }

        pub fn commit(&mut self, backend: &mut impl Backend, id: u64) -> Result<()> {
            self.active = self.take_pending(id)?.target;
            // Once the new core accepted its config, cleanup failure cannot undo its guard.
            self.restore_except(backend, self.active.clone().as_ref())
        }

        pub fn rollback(&mut self, backend: &mut impl Backend, id: u64) -> Result<()> {
            if let Some(target) = self.take_pending(id)?.added {
                self.restore_one(backend, &target)?;
            }
            Ok(())
        }

        pub fn retain(&mut self, id: u64) -> Result<()> {
            self.active = self.take_pending(id)?.target;
            Ok(())
        }

        fn remove_record(&mut self, backend: &mut impl Backend, index: usize) -> Result<()> {
            let mut records = self.records.clone();
            records.remove(index);
            backend.save(&records)?;
            self.records = records;
            Ok(())
        }

        fn restore_one(&mut self, backend: &mut impl Backend, target: &Target) -> Result<()> {
            let Some(index) = self.records.iter().position(|record| record.target.guid == target.guid) else {
                return Ok(());
            };
            let record = self.records[index].clone();
            if record.phase == Phase::Retired {
                return self.remove_record(backend, index);
            }
            let current = backend.inspect(&record.target)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "Forwarding restoration is pending because interface {} is unavailable",
                    target.name
                )
            })?;
            if !current.hardware || !current.ethernet_or_wifi {
                bail!("The protected interface no longer identifies the original physical uplink");
            }
            if current.forwarding == record.original_forwarding {
                // Already restored or changed externally: no OS mutation is needed.
                self.records[index].phase = Phase::Retired;
                backend.save(&self.records)?;
                return self.remove_record(backend, index);
            }
            if record.phase != Phase::Applied {
                bail!(
                    "Forwarding ownership on {} is ambiguous after an interrupted write; no network setting was restored",
                    target.name
                );
            }
            backend.can_change()?;
            // Write this phase before restoring. A restart must not replay an old applied
            // record after a successful restore whose journal cleanup failed.
            let mut records = self.records.clone();
            records[index].phase = Phase::Restoring;
            backend.save(&records)?;
            self.records = records;
            backend.change(&record.target, false, record.original_forwarding)?;
            self.records[index].phase = Phase::Retired;
            backend.save(&self.records)?;
            self.remove_record(backend, index)
        }

        fn restore_except(&mut self, backend: &mut impl Backend, keep: Option<&Target>) -> Result<()> {
            let targets = self
                .records
                .iter()
                .filter(|record| keep.is_none_or(|keep| keep.guid != record.target.guid))
                .map(|record| record.target.clone())
                .collect::<Vec<_>>();
            let mut errors = Vec::new();
            for target in targets {
                if let Err(error) = self.restore_one(backend, &target) {
                    errors.push(format!("{error:#}"));
                }
            }
            if !errors.is_empty() {
                bail!("{}", errors.join("; "));
            }
            Ok(())
        }

        pub fn restore(&mut self, backend: &mut impl Backend) -> Result<()> {
            if self.pending.is_some() {
                bail!("Cannot restore forwarding during a TUN compatibility transaction");
            }
            self.active = None;
            self.restore_except(backend, None)
        }

        pub fn restore_confirmed_stop(&mut self, backend: &mut impl Backend) -> Result<()> {
            // A cancelled start may leave an in-process transaction without a token
            // holder. The lifecycle owner has now confirmed that its core is stopped.
            self.pending = None;
            self.restore(backend)
        }

        pub fn retire_stale_records(&mut self, backend: &mut impl Backend) -> Result<()> {
            let mut stale = Vec::new();
            let mut retained = Vec::new();
            for record in &self.records {
                if record.phase == Phase::Retired {
                    // Durable cleanup markers do not own an OS setting and need no
                    // machine-wide mutation lock, adapter lookup, or stopped core.
                    continue;
                }
                if backend.same_session(&record.session)? {
                    retained.push(record.clone());
                } else {
                    stale.push(record.clone());
                }
            }
            if !stale.is_empty() {
                // ActiveStore may contain legitimate settings from a new Windows boot.
                backend.archive(&stale)?;
            }
            if retained.len() != self.records.len() {
                backend.save(&retained)?;
                self.records = retained;
            }
            Ok(())
        }

        #[cfg(test)]
        pub fn recover(&mut self, backend: &mut impl Backend) -> Result<()> {
            self.retire_stale_records(backend)?;
            self.restore_confirmed_stop(backend)
        }

        pub fn check(&self, backend: &mut impl Backend) -> Result<()> {
            let Some(target) = &self.active else { return Ok(()) };
            let adapter = backend
                .inspect(target)?
                .ok_or_else(|| anyhow::anyhow!("Protected TUN uplink {} is unavailable", target.name))?;
            adapter.validate()?;
            if adapter.target.name != target.name {
                bail!("Protected TUN uplink {} was renamed; select it again", target.name);
            }
            if adapter.forwarding {
                bail!(
                    "IPv4 forwarding on {} was enabled outside TUN compatibility protection",
                    target.name
                );
            }
            Ok(())
        }

        pub fn idle(&self) -> bool {
            self.active.is_none() && self.pending.is_none() && self.records.is_empty()
        }
    }
}

#[cfg(all(windows, not(feature = "test")))]
mod native {
    use super::state::{Adapter, Backend, Record, State, Target};
    use super::{InterfaceUnavailable, InterfaceUnavailableReason};
    use anyhow::{Context as _, Result, bail};
    use serde::{Deserialize, Serialize};
    use std::{
        fs::{self, File, OpenOptions},
        io::{ErrorKind, Write as _},
        os::windows::{
            fs::OpenOptionsExt as _,
            io::{FromRawHandle as _, OwnedHandle},
        },
        path::{Path, PathBuf},
    };
    use windows_sys::{
        Win32::{
            Foundation::{
                ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER,
                ERROR_NOT_FOUND, GetLastError,
            },
            NetworkManagement::{
                IpHelper::{
                    ConvertInterfaceAliasToLuid, ConvertInterfaceGuidToLuid, FreeMibTable, GetIfEntry2, GetIfTable2,
                    GetIpForwardTable2, GetIpInterfaceEntry, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211, MIB_IF_ROW2,
                    MIB_IF_TABLE2, MIB_IPFORWARD_TABLE2, MIB_IPINTERFACE_ROW, SetIpInterfaceEntry,
                },
                Ndis::{IfOperStatusUp, NET_LUID_LH},
            },
            Networking::WinSock::AF_INET,
            Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
            System::Com::CoCreateGuid,
            System::Registry::{
                HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, KEY_WRITE, REG_CREATED_NEW_KEY,
                REG_OPTION_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW,
                RegSetValueExW,
            },
            System::Threading::CreateEventW,
            UI::Shell::IsUserAnAdmin,
        },
        core::GUID,
    };
    const SESSION_KEY: &str = r"SOFTWARE\ClashVergeRevTunGuardSession-";
    pub(super) const JOURNAL_FILE: &str = "tun-compatibility-guard.json";

    #[derive(Serialize, Deserialize)]
    struct Journal {
        version: u8,
        records: Vec<Record>,
    }

    pub(super) struct NativeBackend {
        path: PathBuf,
    }

    fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt as _;
        value.encode_wide().chain(Some(0)).collect()
    }

    fn identity(guid: GUID) -> [u8; 16] {
        let mut bytes = [0; 16];
        bytes[..4].copy_from_slice(&guid.data1.to_be_bytes());
        bytes[4..6].copy_from_slice(&guid.data2.to_be_bytes());
        bytes[6..8].copy_from_slice(&guid.data3.to_be_bytes());
        bytes[8..].copy_from_slice(&guid.data4);
        bytes
    }

    const fn guid(bytes: [u8; 16]) -> GUID {
        let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = bytes;
        GUID {
            data1: u32::from_be_bytes([a, b, c, d]),
            data2: u16::from_be_bytes([e, f]),
            data3: u16::from_be_bytes([g, h]),
            data4: [i, j, k, l, m, n, o, p],
        }
    }

    fn check_code(code: u32, operation: &str) -> Result<()> {
        if code == 0 {
            return Ok(());
        }
        if code == ERROR_ACCESS_DENIED {
            bail!("TUN compatibility protection requires an elevated lifecycle owner; {operation} was denied");
        }
        Err(std::io::Error::from_raw_os_error(code as i32))
            .with_context(|| format!("TUN compatibility protection: {operation}"))
    }

    fn require_admin() -> Result<()> {
        // SAFETY: IsUserAnAdmin inspects the effective token without changing it.
        if unsafe { IsUserAnAdmin() } == 0 {
            bail!("TUN compatibility protection requires an elevated lifecycle owner");
        }
        Ok(())
    }

    fn unique_token() -> Result<String> {
        let mut value = GUID::default();
        // SAFETY: value receives a newly generated GUID; no COM initialization is required.
        let result = unsafe { CoCreateGuid(&mut value) };
        if result < 0 {
            bail!("Unable to generate a TUN guard session identifier: HRESULT {result:#x}");
        }
        Ok(identity(value).iter().map(|byte| format!("{byte:02x}")).collect())
    }

    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: this is a successfully opened owned registry key.
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn session_key(token: &str) -> Vec<u16> {
        wide(std::ffi::OsStr::new(&format!("{SESSION_KEY}{token}")))
    }

    fn ip_row(luid: NET_LUID_LH) -> Result<MIB_IPINTERFACE_ROW> {
        let mut row = MIB_IPINTERFACE_ROW {
            Family: AF_INET,
            InterfaceLuid: luid,
            ..Default::default()
        };
        // SAFETY: row is a writable IPv4 query initialized with a valid interface LUID.
        check_code(unsafe { GetIpInterfaceEntry(&mut row) }, "query IPv4 interface")?;
        Ok(row)
    }

    fn is_uplink(luid: NET_LUID_LH) -> Result<bool> {
        let mut table = std::ptr::null_mut();
        // SAFETY: table receives a system-allocated route table, freed below.
        check_code(unsafe { GetIpForwardTable2(AF_INET, &mut table) }, "query IPv4 routes")?;
        struct Routes(*mut MIB_IPFORWARD_TABLE2);
        impl Drop for Routes {
            fn drop(&mut self) {
                // SAFETY: this is the allocation returned by GetIpForwardTable2.
                unsafe { FreeMibTable(self.0.cast()) };
            }
        }
        let table = Routes(table);
        if table.0.is_null() {
            bail!("Windows returned no IPv4 route table");
        }
        // SAFETY: the allocation contains NumEntries rows; union Value and AF_INET NextHop are valid.
        let result = unsafe {
            std::slice::from_raw_parts((*table.0).Table.as_ptr(), (*table.0).NumEntries as usize)
                .iter()
                .any(|route| {
                    route.InterfaceLuid.Value == luid.Value
                        && route.DestinationPrefix.PrefixLength == 0
                        && !route.Loopback
                        && route.ValidLifetime != 0
                        && route.NextHop.si_family == AF_INET
                        && route.NextHop.Ipv4.sin_addr.S_un.S_addr != 0
                })
        };
        Ok(result)
    }

    fn inspect_luid(luid: NET_LUID_LH) -> Result<Adapter> {
        let mut row = MIB_IF_ROW2 {
            InterfaceLuid: luid,
            ..Default::default()
        };
        // SAFETY: row is writable and identifies a system interface by LUID.
        check_code(unsafe { GetIfEntry2(&mut row) }, "query network interface")?;
        let name_end = row
            .Alias
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(row.Alias.len());
        let name = String::from_utf16(&row.Alias[..name_end]).context("network interface alias is invalid UTF-16")?;
        let ipv4 = ip_row(luid)?;
        let flags = row.InterfaceAndOperStatusFlags._bitfield;
        Ok(Adapter {
            target: Target {
                name,
                guid: identity(row.InterfaceGuid),
            },
            forwarding: ipv4.ForwardingEnabled,
            hardware: flags & 1 != 0 && flags & (2 | 128) == 0,
            ethernet_or_wifi: matches!(row.Type, IF_TYPE_ETHERNET_CSMACD | IF_TYPE_IEEE80211),
            up: row.OperStatus == IfOperStatusUp && ipv4.Connected,
            uplink: is_uplink(luid)?,
        })
    }

    fn interface_alias_missing(name: &str) -> Result<bool> {
        let mut table = std::ptr::null_mut();
        // SAFETY: table receives a system-allocated interface table, freed below.
        check_code(unsafe { GetIfTable2(&mut table) }, "query network interfaces")?;
        struct Interfaces(*mut MIB_IF_TABLE2);
        impl Drop for Interfaces {
            fn drop(&mut self) {
                // SAFETY: this is the allocation returned by GetIfTable2.
                unsafe { FreeMibTable(self.0.cast()) };
            }
        }
        let table = Interfaces(table);
        if table.0.is_null() {
            bail!("Windows returned no network interface table");
        }
        // SAFETY: the allocation contains NumEntries aligned MIB_IF_ROW2 entries.
        let rows = unsafe { std::slice::from_raw_parts((*table.0).Table.as_ptr(), (*table.0).NumEntries as usize) };
        let folded_name = name.to_lowercase();
        for row in rows {
            let end = row
                .Alias
                .iter()
                .position(|value| *value == 0)
                .unwrap_or(row.Alias.len());
            let alias = String::from_utf16(&row.Alias[..end]).context("network interface alias is invalid UTF-16")?;
            if alias == name {
                return Ok(false);
            }
            if alias.to_lowercase() == folded_name {
                bail!("The locked interface name must exactly match its Windows alias");
            }
        }
        Ok(true)
    }

    fn target_luid(target: &Target) -> Result<Option<NET_LUID_LH>> {
        let mut luid = NET_LUID_LH::default();
        // SAFETY: target contains a complete GUID and luid is writable.
        let code = unsafe { ConvertInterfaceGuidToLuid(&guid(target.guid), &mut luid) };
        if code == ERROR_FILE_NOT_FOUND || code == ERROR_NOT_FOUND {
            return Ok(None);
        }
        check_code(code, "resolve protected interface GUID")?;
        Ok(Some(luid))
    }

    fn atomic_write(path: &Path, journal: &Journal) -> Result<()> {
        let parent = path.parent().context("TUN guard journal has no parent")?;
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".tun-guard-{}.tmp", unique_token()?));
        let result = (|| {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&temporary)?;
            file.write_all(&serde_json::to_vec(journal)?)?;
            file.sync_all()?;
            drop(file);
            // SAFETY: both paths are NUL-terminated; replace is atomic on the same filesystem.
            if unsafe {
                MoveFileExW(
                    wide(temporary.as_os_str()).as_ptr(),
                    wide(path.as_os_str()).as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error()).context("publish TUN guard journal");
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    impl Backend for NativeBackend {
        fn adapter(&mut self, name: &str) -> Result<Adapter> {
            let mut luid = NET_LUID_LH::default();
            // SAFETY: name is validated against embedded NUL before this call and luid is writable.
            let code = unsafe { ConvertInterfaceAliasToLuid(wide(std::ffi::OsStr::new(name)).as_ptr(), &mut luid) };
            if matches!(code, ERROR_FILE_NOT_FOUND | ERROR_NOT_FOUND | ERROR_INVALID_PARAMETER)
                && interface_alias_missing(name)?
            {
                return Err(InterfaceUnavailable {
                    name: name.to_owned(),
                    reason: InterfaceUnavailableReason::Missing,
                }
                .into());
            }
            check_code(code, "resolve locked interface")?;
            let adapter = inspect_luid(luid)?;
            if adapter.target.name != name {
                bail!("The locked interface name must exactly match its Windows alias");
            }
            Ok(adapter)
        }

        fn inspect(&mut self, target: &Target) -> Result<Option<Adapter>> {
            let adapter = target_luid(target)?.map(inspect_luid).transpose()?;
            if adapter
                .as_ref()
                .is_some_and(|adapter| adapter.target.guid != target.guid)
            {
                bail!("The protected interface identity changed during lookup");
            }
            Ok(adapter)
        }

        fn session(&mut self) -> Result<String> {
            require_admin()?;
            let token = unique_token()?;
            let mut raw = std::ptr::null_mut();
            let mut disposition = 0;
            // SAFETY: all output pointers are writable and the key path is NUL-terminated.
            check_code(
                unsafe {
                    RegCreateKeyExW(
                        HKEY_LOCAL_MACHINE,
                        session_key(&token).as_ptr(),
                        0,
                        std::ptr::null(),
                        REG_OPTION_VOLATILE,
                        KEY_READ | KEY_WRITE | KEY_WOW64_64KEY,
                        std::ptr::null(),
                        &mut raw,
                        &mut disposition,
                    )
                },
                "create temporary Windows boot-session key",
            )?;
            let key = Key(raw);
            // Volatility is ignored for existing keys; never adopt one.
            if disposition != REG_CREATED_NEW_KEY {
                bail!("The temporary TUN guard session key already exists; no network settings were changed");
            }
            let value = wide(std::ffi::OsStr::new(&token));
            // SAFETY: the UTF-16 value includes its terminator and is valid for this byte length.
            check_code(
                unsafe {
                    RegSetValueExW(
                        key.0,
                        wide(std::ffi::OsStr::new("Token")).as_ptr(),
                        0,
                        REG_SZ,
                        value.as_ptr().cast(),
                        (value.len() * 2) as u32,
                    )
                },
                "record temporary Windows boot-session token",
            )?;
            Ok(token)
        }

        fn same_session(&mut self, session: &str) -> Result<bool> {
            let mut raw = std::ptr::null_mut();
            // SAFETY: the key path is NUL-terminated and raw is writable.
            let code = unsafe {
                RegOpenKeyExW(
                    HKEY_LOCAL_MACHINE,
                    session_key(session).as_ptr(),
                    0,
                    KEY_READ | KEY_WOW64_64KEY,
                    &mut raw,
                )
            };
            if code == ERROR_FILE_NOT_FOUND {
                return Ok(false);
            }
            check_code(code, "open Windows boot-session key")?;
            let key = Key(raw);
            let mut kind = 0;
            let mut buffer = [0u16; 65];
            let mut size = std::mem::size_of_val(&buffer) as u32;
            // SAFETY: the supplied buffer has size writable bytes; registry type is checked below.
            check_code(
                unsafe {
                    RegQueryValueExW(
                        key.0,
                        wide(std::ffi::OsStr::new("Token")).as_ptr(),
                        std::ptr::null(),
                        &mut kind,
                        buffer.as_mut_ptr().cast(),
                        &mut size,
                    )
                },
                "read Windows boot-session token",
            )?;
            if kind != REG_SZ || size == 0 || !size.is_multiple_of(2) || size as usize > std::mem::size_of_val(&buffer)
            {
                bail!("The Windows boot-session token is invalid");
            }
            let length = size as usize / 2;
            if buffer[length - 1] != 0 {
                bail!("The Windows boot-session token is not terminated");
            }
            let value = String::from_utf16(&buffer[..length - 1])
                .context("The Windows boot-session token is invalid UTF-16")?;
            Ok(value == session)
        }

        fn can_change(&mut self) -> Result<()> {
            require_admin()
        }

        fn change(&mut self, target: &Target, expected: bool, value: bool) -> Result<bool> {
            let luid = target_luid(target)?
                .context("Protected interface is unavailable; forwarding restoration remains pending")?;
            let adapter = inspect_luid(luid)?;
            if adapter.target.guid != target.guid || !adapter.hardware || !adapter.ethernet_or_wifi {
                bail!("The protected interface no longer identifies the original physical uplink");
            }
            if !value {
                adapter.validate()?;
            }
            let mut row = ip_row(luid)?;
            if row.ForwardingEnabled != expected {
                return Ok(false);
            }
            require_admin()?;
            row.ForwardingEnabled = value;
            // Required by SetIpInterfaceEntry for IPv4, even when querying an existing row.
            row.SitePrefixLength = 0;
            // SAFETY: only ForwardingEnabled is changed in the freshly queried IPv4 row.
            check_code(
                unsafe { SetIpInterfaceEntry(&mut row) },
                "temporarily set IPv4 forwarding",
            )?;
            if ip_row(luid)?.ForwardingEnabled != value {
                bail!(
                    "IPv4 forwarding on {} did not remain in the requested state",
                    target.name
                );
            }
            Ok(true)
        }

        fn save(&mut self, records: &[Record]) -> Result<()> {
            if records.is_empty() {
                match fs::remove_file(&self.path) {
                    Ok(()) => return Ok(()),
                    Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
                    Err(error) => return Err(error).context("remove restored TUN guard journal"),
                }
            }
            atomic_write(
                &self.path,
                &Journal {
                    version: 2,
                    records: records.to_vec(),
                },
            )
            .context("save TUN guard journal before changing network settings")
        }

        fn archive(&mut self, records: &[Record]) -> Result<()> {
            let archive = self
                .path
                .with_file_name(format!("tun-compatibility-unrestored-{}.json", unique_token()?));
            atomic_write(
                &archive,
                &Journal {
                    version: 2,
                    records: records.to_vec(),
                },
            )?;
            Ok(())
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum RecoveryState {
        Unloaded,
        Pending,
        Ready,
    }

    pub(super) struct Runtime {
        state: State,
        backend: NativeBackend,
        recovery: RecoveryState,
        lock: Option<OwnedHandle>,
        file_lock: Option<File>,
    }

    impl Runtime {
        pub(super) fn new(root: PathBuf) -> Self {
            Self {
                state: State::default(),
                backend: NativeBackend {
                    path: root.join(JOURNAL_FILE),
                },
                recovery: RecoveryState::Unloaded,
                lock: None,
                file_lock: None,
            }
        }

        fn claim_journal(&mut self) -> Result<()> {
            if self.file_lock.is_some() {
                return Ok(());
            }
            if !self.backend.path.is_absolute() {
                bail!("TUN guard journal root must be an absolute trusted runtime directory");
            }
            let parent = self.backend.path.parent().context("TUN guard journal has no parent")?;
            fs::create_dir_all(parent)?;
            self.file_lock = Some(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .share_mode(0)
                    .open(parent.join(".tun-compatibility-guard.lock"))
                    .context("TUN protection journal is owned by another lifecycle owner")?,
            );
            Ok(())
        }

        fn claim_mutation(&mut self) -> Result<()> {
            self.claim_journal()?;
            if self.lock.is_some() {
                return Ok(());
            }
            let name = wide(std::ffi::OsStr::new(r"Global\ClashVergeRevTunCompatibilityGuard"));
            // SAFETY: event name is fixed and NUL-terminated.
            let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
            // SAFETY: read the native error state immediately after creation.
            let last_error = unsafe { GetLastError() };
            if handle.is_null() {
                return Err(std::io::Error::from_raw_os_error(last_error as i32))
                    .context("another instance or account may own TUN compatibility protection");
            }
            // SAFETY: CreateEventW returned an owned handle.
            let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
            if last_error == ERROR_ALREADY_EXISTS {
                bail!("Another lifecycle owner already holds TUN compatibility protection");
            }
            self.lock = Some(handle);
            Ok(())
        }

        fn release_if_idle(&mut self) {
            if self.state.idle() {
                self.file_lock = None;
                self.lock = None;
            }
        }

        /// Absence is only a no-op observation, never permission to write without locking.
        fn absent_idle(&mut self) -> Result<bool> {
            if self.state.idle() && self.lock.is_none() && !self.backend.path.try_exists()? {
                self.recovery = RecoveryState::Ready;
                self.release_if_idle();
                return Ok(true);
            }
            Ok(false)
        }

        fn load_locked(&mut self) -> Result<()> {
            // Reading and expiring a journal never needs the machine-wide mutation lock.
            self.claim_journal()?;
            if self.recovery != RecoveryState::Unloaded {
                return Ok(());
            }
            let metadata = match fs::metadata(&self.backend.path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    self.recovery = RecoveryState::Ready;
                    return Ok(());
                }
                Err(error) => return Err(error).context("inspect TUN forwarding journal"),
            };
            if !metadata.is_file() || metadata.len() > 1_048_576 {
                bail!("TUN forwarding journal is not a bounded regular file");
            }
            let bytes = fs::read(&self.backend.path).context("read TUN forwarding journal")?;
            let mut journal: Journal = serde_json::from_slice(&bytes)
                .context("TUN guard journal is invalid; no network settings were changed")?;
            if !matches!(journal.version, 1 | 2) || journal.records.len() > 32 {
                bail!("TUN guard journal version or record count is unsupported");
            }
            let mut identities = std::collections::HashSet::new();
            for record in &mut journal.records {
                if !record.original_forwarding
                    || record.target.guid == [0; 16]
                    || record.target.name.is_empty()
                    || record.target.name.len() > 1024
                    || record.target.name.contains('\0')
                    || !matches!(record.session.len(), 21 | 32)
                    || !record
                        .session
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                    || !identities.insert(record.target.guid)
                {
                    bail!("TUN guard journal contains an unsupported restoration record");
                }
                if journal.version == 1 {
                    // The old schema persisted intent only, not proof of a successful OS write.
                    record.phase = super::state::Phase::Prepared;
                }
            }
            self.state.records = journal.records;
            self.recovery = if self.state.idle() {
                RecoveryState::Ready
            } else {
                RecoveryState::Pending
            };
            Ok(())
        }

        pub(super) fn preflight(&mut self, name: Option<&str>) -> Result<()> {
            // A service may legitimately own a live protected core. Read-only target
            // validation must not reject its journal before lifecycle takeover stops it.
            // The subsequent recover/prepare phase enforces durable journal ownership.
            self.state.preflight(&mut self.backend, name)
        }

        pub(super) fn prepare(&mut self, name: Option<&str>) -> Result<Option<u64>> {
            if name.is_none() && self.absent_idle()? {
                return Ok(None);
            }
            if self.state.idle() && self.lock.is_none() {
                // Always reread after acquiring ownership, including after an absent-file fast path.
                self.recovery = RecoveryState::Unloaded;
            }
            self.load_locked()?;
            if self.recovery != RecoveryState::Ready {
                bail!("TUN forwarding journal requires confirmed-core-stop recovery before protection can start");
            }
            self.claim_mutation()?;
            let result = self.state.prepare(&mut self.backend, name).map(Some);
            self.release_if_idle();
            result
        }

        pub(super) fn finish(&mut self, id: u64, commit: bool) -> Result<()> {
            let result = if commit {
                self.state.commit(&mut self.backend, id)
            } else {
                self.state.rollback(&mut self.backend, id)
            };
            self.release_if_idle();
            result
        }

        pub(super) fn retain(&mut self, id: u64) -> Result<()> {
            self.state.retain(id)
        }

        pub(super) fn prepare_recovery(&mut self) -> Result<bool> {
            if self.absent_idle()? {
                return Ok(false);
            }
            if self.state.idle() && self.lock.is_none() {
                self.recovery = RecoveryState::Unloaded;
            }
            self.load_locked()?;
            if self.recovery == RecoveryState::Ready {
                self.release_if_idle();
                return Ok(false);
            }
            self.state.retire_stale_records(&mut self.backend)?;
            if self.state.idle() {
                self.recovery = RecoveryState::Ready;
                self.release_if_idle();
                return Ok(false);
            }
            Ok(true)
        }

        pub(super) fn recover(&mut self) -> Result<()> {
            if self.absent_idle()? {
                return Ok(());
            }
            if self.state.idle() && self.lock.is_none() {
                self.recovery = RecoveryState::Unloaded;
            }
            self.load_locked()?;
            self.state.retire_stale_records(&mut self.backend)?;
            if !self.state.records.is_empty() {
                self.claim_mutation()?;
            }
            self.state.restore_confirmed_stop(&mut self.backend)?;
            self.recovery = RecoveryState::Ready;
            self.release_if_idle();
            Ok(())
        }

        pub(super) fn restore(&mut self) -> Result<()> {
            self.recover()
        }

        pub(super) fn check(&mut self) -> Result<()> {
            if self.absent_idle()? {
                return Ok(());
            }
            if self.recovery != RecoveryState::Ready {
                bail!("TUN forwarding restoration remains pending");
            }
            self.state.check(&mut self.backend)
        }

        pub(super) fn has_owned_guard(&self) -> bool {
            self.lock.is_some() && !self.state.idle()
        }
    }
}

/// Shared guard implementation; callers serialize it with their core lifecycle.
/// The root must be a locally selected trusted directory, never an IPC request path.
/// This type deliberately does not stop processes, acquire execution leases, or
/// restore settings on Drop: only the lifecycle owner can confirm safe restoration.
pub struct GuardRuntime {
    #[cfg(all(windows, not(feature = "test")))]
    native: native::Runtime,
}

impl GuardRuntime {
    pub fn new(root: PathBuf) -> Self {
        #[cfg(all(windows, not(feature = "test")))]
        {
            Self {
                native: native::Runtime::new(root),
            }
        }
        #[cfg(any(not(windows), feature = "test"))]
        {
            let _ = root;
            Self {}
        }
    }

    /// Read-only validation before stopping a currently managed core.
    pub fn preflight(&mut self, name: Option<&str>) -> Result<()> {
        #[cfg(all(windows, not(feature = "test")))]
        {
            self.native.preflight(name)
        }
        #[cfg(any(not(windows), feature = "test"))]
        {
            if name.is_some() {
                anyhow::bail!("Native Windows forwarding protection is unavailable in this build");
            }
            Ok(())
        }
    }

    /// Prepare protection before starting a core, while holding execution exclusion.
    pub fn prepare(&mut self, name: Option<&str>) -> Result<Option<u64>> {
        #[cfg(all(windows, not(feature = "test")))]
        {
            self.native.prepare(name)
        }
        #[cfg(any(not(windows), feature = "test"))]
        {
            if name.is_some() {
                anyhow::bail!("Native Windows forwarding protection is unavailable in this build");
            }
            Ok(None)
        }
    }

    /// Commit after readiness, or roll back only after a confirmed failed spawn/stop.
    pub fn finish(&mut self, id: u64, commit: bool) -> Result<()> {
        #[cfg(all(windows, not(feature = "test")))]
        {
            self.native.finish(id, commit)
        }
        #[cfg(any(not(windows), feature = "test"))]
        {
            let _ = (id, commit);
            Ok(())
        }
    }

    /// Preserve protection when core termination cannot be confirmed.
    pub fn retain(&mut self, id: u64) -> Result<()> {
        #[cfg(all(windows, not(feature = "test")))]
        {
            self.native.retain(id)
        }
        #[cfg(any(not(windows), feature = "test"))]
        {
            let _ = id;
            Ok(())
        }
    }

    /// Archive expired boot records without altering any network setting.
    pub fn prepare_recovery(&mut self) -> Result<bool> {
        #[cfg(all(windows, not(feature = "test")))]
        {
            self.native.prepare_recovery()
        }
        #[cfg(any(not(windows), feature = "test"))]
        {
            Ok(false)
        }
    }

    /// Requires execution exclusion and confirmed absence of the prior core.
    pub fn recover(&mut self) -> Result<()> {
        #[cfg(all(windows, not(feature = "test")))]
        {
            self.native.recover()
        }
        #[cfg(any(not(windows), feature = "test"))]
        {
            Ok(())
        }
    }

    /// Restore after a confirmed stop; no journal is a no-op even if another owner runs.
    pub fn restore(&mut self) -> Result<()> {
        #[cfg(all(windows, not(feature = "test")))]
        {
            self.native.restore()
        }
        #[cfg(any(not(windows), feature = "test"))]
        {
            Ok(())
        }
    }

    /// Audit only; never repair an externally changed interface or choose another one.
    pub fn check(&mut self) -> Result<()> {
        #[cfg(all(windows, not(feature = "test")))]
        {
            self.native.check()
        }
        #[cfg(any(not(windows), feature = "test"))]
        {
            Ok(())
        }
    }

    pub fn has_owned_guard(&self) -> bool {
        #[cfg(all(windows, not(feature = "test")))]
        {
            self.native.has_owned_guard()
        }
        #[cfg(any(not(windows), feature = "test"))]
        {
            false
        }
    }
}
