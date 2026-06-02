// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause
//

pub use context::{
    CompletedMigrationContext, DowntimeContext, MemoryMigrationContext, MigrationContextError,
    OngoingMigrationContext,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::protocol::MemoryRangeTable;

mod bitpos_iterator;
mod context;
pub mod protocol;

#[derive(Error, Debug)]
pub enum UffdError {
    #[error("Snapshot ranges are not page-aligned")]
    UnalignedRanges,

    #[error("Failed to create userfaultfd")]
    Create(#[source] std::io::Error),

    #[error("Cannot translate GPA {gpa:#x} to host address")]
    GpaTranslation { gpa: u64 },

    #[error("Failed to register region at {addr:#x}+{len:#x}")]
    Register {
        addr: u64,
        len: u64,
        #[source]
        source: std::io::Error,
    },

    #[error("Region at {addr:#x}+{len:#x} missing COPY/WAKE support")]
    MissingIoctlSupport { addr: u64, len: u64 },

    #[error("Failed to spawn handler thread")]
    SpawnThread(#[source] std::io::Error),

    #[error("Handler terminated before startup completed")]
    HandlerStartup,

    #[error("Handler failed after startup")]
    HandlerFailed(#[source] std::io::Error),
}

#[derive(Error, Debug)]
pub enum PausableError {
    #[error("Failed to pause migratable component: {0}")]
    Pause(String),

    #[error("Failed to resume migratable component: {0}")]
    Resume(String),

    #[error("Lifecycle operation skipped for disconnected component {0}")]
    DeviceDisconnected(String),
}

#[derive(Error, Debug)]
pub enum SnapshotError {
    #[error("Failed to serialize snapshot state")]
    Serialize(#[source] serde_json::Error),

    #[error("Failed to snapshot migratable component: {0}")]
    Snapshot(String),
}

impl SnapshotError {
    pub fn snapshot(source: impl std::fmt::Display) -> Self {
        Self::Snapshot(source.to_string())
    }
}

#[derive(Error, Debug)]
pub enum RestoreError {
    #[error("Failed to deserialize snapshot state")]
    Deserialize(#[source] serde_json::Error),

    #[error("Missing snapshot data")]
    MissingSnapshotData,

    #[error("On-demand restore failed")]
    OnDemandRestore(#[source] UffdError),

    #[error("Failed to restore migratable component: {0}")]
    Restore(String),
}

impl RestoreError {
    pub fn restore(source: impl std::fmt::Display) -> Self {
        Self::Restore(source.to_string())
    }
}

impl PausableError {
    pub fn pause(source: impl std::fmt::Display) -> Self {
        Self::Pause(source.to_string())
    }

    pub fn resume(source: impl std::fmt::Display) -> Self {
        Self::Resume(source.to_string())
    }
}

#[derive(Error, Debug)]
pub enum MigrationProtocolError {
    #[error("Socket error")]
    Socket(#[source] std::io::Error),
}

#[derive(Error, Debug)]
pub enum MigrationLifecycleError {
    #[error("Failed to start dirty logging for migratable component: {0}")]
    StartDirtyLog(String),

    #[error("Failed to stop dirty logging for migratable component: {0}")]
    StopDirtyLog(String),

    #[error("Failed to retrieve dirty ranges for migratable component: {0}")]
    DirtyLog(String),

    #[error("Failed to start migration for migratable component: {0}")]
    StartMigration(String),

    #[error("Failed to complete migration for migratable component: {0}")]
    CompleteMigration(String),

    #[error("Missing guest memory")]
    MissingGuestMemory,

    #[error("Missing guest memory region at {gpa:#x}")]
    MissingGuestMemoryRegion { gpa: u64 },
}

impl MigrationLifecycleError {
    pub fn start_dirty_log(source: impl std::fmt::Display) -> Self {
        Self::StartDirtyLog(source.to_string())
    }

    pub fn stop_dirty_log(source: impl std::fmt::Display) -> Self {
        Self::StopDirtyLog(source.to_string())
    }

    pub fn dirty_log(source: impl std::fmt::Display) -> Self {
        Self::DirtyLog(source.to_string())
    }

    pub fn start_migration(source: impl std::fmt::Display) -> Self {
        Self::StartMigration(source.to_string())
    }

    pub fn complete_migration(source: impl std::fmt::Display) -> Self {
        Self::CompleteMigration(source.to_string())
    }
}

#[derive(Error, Debug)]
pub enum MigrationSendError {
    #[error(transparent)]
    Pausable(#[from] PausableError),

    #[error(transparent)]
    Snapshot(#[from] SnapshotError),

    #[error(transparent)]
    Restore(#[from] RestoreError),

    #[error(transparent)]
    Lifecycle(#[from] MigrationLifecycleError),

    #[error(transparent)]
    Protocol(#[from] MigrationProtocolError),

    #[error("Failed to send migratable component snapshot: {0}")]
    Send(String),

    #[error("Failed to release a disk lock: {0}")]
    Unlock(String),

    #[error("Receiver rejected VM migration config")]
    ConfigRejected,

    #[error("Receiver rejected VM migration state")]
    StateRejected,

    #[error("Receiver rejected migration memory")]
    MemoryRejected,

    #[error("Receiver rejected migration start")]
    StartRejected,

    #[error("Receiver rejected migration completion")]
    CompletionRejected,
}

impl MigrationSendError {
    pub fn send(source: impl std::fmt::Display) -> Self {
        Self::Send(source.to_string())
    }

    pub fn unlock(source: impl std::fmt::Display) -> Self {
        Self::Unlock(source.to_string())
    }
}

#[derive(Error, Debug)]
pub enum MigrationReceiveError {
    #[error(transparent)]
    Pausable(#[from] PausableError),

    #[error(transparent)]
    Snapshot(#[from] SnapshotError),

    #[error(transparent)]
    Restore(#[from] RestoreError),

    #[error(transparent)]
    Lifecycle(#[from] MigrationLifecycleError),

    #[error(transparent)]
    Protocol(#[from] MigrationProtocolError),

    #[error("Failed to receive migratable component snapshot: {0}")]
    Receive(String),
}

impl MigrationReceiveError {
    pub fn receive(source: impl std::fmt::Display) -> Self {
        Self::Receive(source.to_string())
    }
}

/// A Pausable component can be paused and resumed.
pub trait Pausable {
    /// Pause the component.
    fn pause(&mut self) -> std::result::Result<(), PausableError> {
        Ok(())
    }

    /// Resume the component.
    fn resume(&mut self) -> std::result::Result<(), PausableError> {
        Ok(())
    }
}

/// A Snapshottable component snapshot section.
///
/// Migratable component can split their migration snapshot into
/// separate sections.
/// Splitting a component migration data into different sections
/// allows for easier and forward compatible extensions.
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct SnapshotData {
    state: String,
}

impl SnapshotData {
    /// Generate the state data from the snapshot data
    pub fn to_state<'a, T>(&'a self) -> Result<T, RestoreError>
    where
        T: Deserialize<'a>,
    {
        serde_json::from_str(&self.state).map_err(RestoreError::Deserialize)
    }

    /// Create from state that can be serialized
    pub fn new_from_state<T>(state: &T) -> Result<Self, SnapshotError>
    where
        T: Serialize,
    {
        let state = serde_json::to_string(state).map_err(SnapshotError::Serialize)?;

        Ok(SnapshotData { state })
    }
}

/// Data structure to describe snapshot data
///
/// A Snapshottable component's snapshot is a tree of snapshots, where leaves
/// contain the snapshot data. Nodes of this tree track all their children
/// through the snapshots field, which is basically their sub-components.
/// Leaves will typically have an empty snapshots map, while nodes usually
/// carry an empty snapshot_data.
///
/// For example, a device manager snapshot is the composition of all its
/// devices snapshots. The device manager Snapshot would have no snapshot_data
/// but one Snapshot child per tracked device. Then each device's Snapshot
/// would carry an empty snapshots map but a map of SnapshotData, i.e.
/// the actual device snapshot data.
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Snapshot {
    /// The Snapshottable component snapshots.
    pub snapshots: std::collections::BTreeMap<String, Snapshot>,

    /// The Snapshottable component's snapshot data.
    /// A map of snapshot sections, indexed by the section ids.
    pub snapshot_data: Option<SnapshotData>,
}

impl Snapshot {
    pub fn from_data(data: SnapshotData) -> Self {
        Snapshot {
            snapshot_data: Some(data),
            ..Default::default()
        }
    }

    /// Create from state that can be serialized
    pub fn new_from_state<T>(state: &T) -> Result<Self, SnapshotError>
    where
        T: Serialize,
    {
        Ok(Snapshot::from_data(SnapshotData::new_from_state(state)?))
    }

    /// Add a sub-component's Snapshot to the Snapshot.
    pub fn add_snapshot(&mut self, id: String, snapshot: Snapshot) {
        self.snapshots.insert(id, snapshot);
    }

    /// Generate the state data from the snapshot
    pub fn to_state<'a, T>(&'a self) -> Result<T, RestoreError>
    where
        T: Deserialize<'a>,
    {
        self.snapshot_data
            .as_ref()
            .ok_or(RestoreError::MissingSnapshotData)?
            .to_state()
    }
}

pub fn snapshot_from_id<'a>(snapshot: Option<&'a Snapshot>, id: &str) -> Option<&'a Snapshot> {
    snapshot.and_then(|s| s.snapshots.get(id))
}

pub fn state_from_id<'a, T>(s: Option<&'a Snapshot>, id: &str) -> Result<Option<T>, RestoreError>
where
    T: Deserialize<'a>,
{
    if let Some(s) = s.as_ref() {
        s.snapshots.get(id).map(|s| s.to_state()).transpose()
    } else {
        Ok(None)
    }
}

/// A snapshottable component can be snapshotted.
pub trait Snapshottable: Pausable {
    /// The snapshottable component id.
    fn id(&self) -> String {
        String::new()
    }

    /// Take a component snapshot.
    fn snapshot(&mut self) -> std::result::Result<Snapshot, SnapshotError> {
        Ok(Snapshot::default())
    }
}

/// A transportable component can be sent or receive to a specific URL.
///
/// This trait is meant to be used for component that have custom
/// transport handlers.
pub trait Transportable: Pausable + Snapshottable {
    /// Send a component snapshot.
    ///
    /// # Arguments
    ///
    /// * `snapshot` - The migratable component snapshot to send.
    /// * `destination_url` - The destination URL to send the snapshot to. This
    ///   could be an HTTP endpoint, a TCP address or a local file.
    fn send(
        &self,
        _snapshot: &Snapshot,
        _destination_url: &str,
    ) -> std::result::Result<(), MigrationSendError> {
        Ok(())
    }

    /// Receive a component snapshot.
    ///
    /// # Arguments
    ///
    /// * `source_url` - The source URL to fetch the snapshot from. This could be an HTTP
    ///   endpoint, a TCP address or a local file.
    fn recv(&self, _source_url: &str) -> std::result::Result<Snapshot, MigrationReceiveError> {
        Ok(Snapshot::default())
    }
}

/// Trait to define shared behaviors of components that can be migrated
///
/// Examples are device, CPU, RAM, etc.
/// All migratable components are paused before being snapshotted, and then
/// eventually resumed. Thus any Migratable component must be both Pausable
/// and Snapshottable.
/// Moreover a migratable component can be transported to a remote or local
/// destination and thus must be Transportable.
pub trait Migratable: Send + Pausable + Snapshottable + Transportable {
    fn start_dirty_log(&mut self) -> std::result::Result<(), MigrationLifecycleError> {
        Ok(())
    }

    fn stop_dirty_log(&mut self) -> std::result::Result<(), MigrationLifecycleError> {
        Ok(())
    }

    fn dirty_log(&mut self) -> std::result::Result<MemoryRangeTable, MigrationLifecycleError> {
        Ok(MemoryRangeTable::default())
    }

    fn start_migration(&mut self) -> std::result::Result<(), MigrationLifecycleError> {
        Ok(())
    }

    fn complete_migration(&mut self) -> std::result::Result<(), MigrationLifecycleError> {
        Ok(())
    }
}
