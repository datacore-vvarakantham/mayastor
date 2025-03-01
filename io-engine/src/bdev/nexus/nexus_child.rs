use std::{
    convert::TryFrom,
    fmt::{Debug, Display, Formatter},
    marker::PhantomData,
    sync::atomic::Ordering,
};

use chrono::{DateTime, Utc};
use crossbeam::atomic::AtomicCell;
use nix::errno::Errno;
use parking_lot::Mutex;
use serde::Serialize;
use snafu::{ResultExt, Snafu};
use url::Url;

use super::{nexus_lookup_mut, DrEvent, IOLog, IOLogChannel};

use crate::{
    bdev::{device_create, device_destroy, device_lookup},
    bdev_api::BdevError,
    core::{
        BlockDevice,
        BlockDeviceDescriptor,
        BlockDeviceHandle,
        CoreError,
        DeviceEventSink,
        Reactor,
        Reactors,
        VerboseError,
    },
    persistent_store::PersistentStore,
    rebuild::{RebuildJob, RebuildMap},
};

use crate::{
    bdev::nexus::{
        nexus_bdev::NexusNvmePreemption,
        NexusNvmeParams,
        NvmeReservation,
    },
    core::MayastorEnvironment,
};

use spdk_rs::{
    libspdk::{
        spdk_nvme_registered_ctrlr_extended_data,
        spdk_nvme_reservation_status_extended_data,
    },
    nvme_reservation_acquire_action,
    nvme_reservation_register_action,
    nvme_reservation_register_cptpl,
    DmaError,
};

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ChildError {
    #[snafu(display("Child is permanently faulted"))]
    PermanentlyFaulted {},
    #[snafu(display("Child is faulted, it cannot be reopened"))]
    ChildFaulted {},
    #[snafu(display("Child is being destroyed"))]
    ChildBeingDestroyed {},
    #[snafu(display(
        "Child is smaller than parent {} vs {}",
        child_size,
        parent_size
    ))]
    ChildTooSmall { child_size: u64, parent_size: u64 },
    #[snafu(display("Open child"))]
    OpenChild { source: CoreError },
    #[snafu(display("Claim child"))]
    ClaimChild { source: Errno },
    #[snafu(display("Child is inaccessible"))]
    ChildInaccessible {},
    #[snafu(display("Cannot online child in its current state"))]
    CannotOnlineChild {},
    #[snafu(display("Failed to create a BlockDeviceHandle for child"))]
    HandleCreate { source: CoreError },
    #[snafu(display("Failed to open a BlockDeviceHandle for child"))]
    HandleOpen { source: CoreError },
    #[snafu(display("Failed to allocate DmaBuffer for child"))]
    HandleDmaMalloc { source: DmaError },
    #[snafu(display("Failed to register key for child: {}", source))]
    ResvRegisterKey { source: CoreError },
    #[snafu(display("Failed to acquire reservation for child: {}", source))]
    ResvAcquire { source: CoreError },
    #[snafu(display("Failed to release reservation for child: {}", source))]
    ResvRelease { source: CoreError },
    #[snafu(display(
        "Failed to get reservation report for child: {}",
        source
    ))]
    ResvReport { source: CoreError },
    #[snafu(display("Invalid reservation type for child: {}", resv_type))]
    ResvType { resv_type: u8 },
    #[snafu(display("No reservation holder for child: {}", resv_type,))]
    ResvNoHolder { resv_type: u8 },
    #[snafu(display(
        "Unexpected reservation owner for child: {:?}:{}:{}",
        hostid,
        resv_type,
        resv_key
    ))]
    Holder {
        hostid: [u8; 16usize],
        resv_type: u8,
        resv_key: u64,
    },
    #[snafu(display("Failed to get NVMe host ID: {}", source))]
    NvmeHostId { source: CoreError },
    #[snafu(display("Failed to create a BlockDevice for child {}", child))]
    ChildBdevCreate { child: String, source: BdevError },
}

/// Fault reason.
#[derive(Debug, Serialize, PartialEq, Deserialize, Eq, Copy, Clone)]
pub enum FaultReason {
    /// No particular reason for the child to be in this state.
    /// This is typically the init state.
    Unknown,
    /// Cannot open the underlying block device.
    CantOpen,
    /// Thin-provisioned child failed a write operate because
    /// the underlying logical volume failed to allocate space.
    /// This a recoverable state in case when additional space
    /// can be freed from the logical volume store.
    NoSpace,
    /// The underlying device timed out.
    /// This a recoverable state in case the device can be expected
    /// to come back online.
    TimedOut,
    /// The child has been faulted due to I/O error(s).
    IoError,
    /// The child failed to rebuild successfully.
    RebuildFailed,
    /// Admin command failure.
    AdminCommandFailed,
    /// The child has been temporarily offlined by a client API call.
    Offline,
    /// The child has been permanently offlined by a client API call.
    OfflinePermanent,
}

impl Display for FaultReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "unknown"),
            Self::CantOpen => write!(f, "cannot open"),
            Self::NoSpace => write!(f, "no space"),
            Self::TimedOut => write!(f, "timed out"),
            Self::IoError => write!(f, "I/O error"),
            Self::RebuildFailed => write!(f, "rebuild failed"),
            Self::AdminCommandFailed => write!(f, "admin command failed"),
            Self::Offline => write!(f, "offline"),
            Self::OfflinePermanent => write!(f, "offline permanent"),
        }
    }
}

impl FaultReason {
    /// Determines if the fault reason is recoverable.
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::NoSpace
                | Self::TimedOut
                | Self::IoError
                | Self::Offline
                | Self::AdminCommandFailed
                | Self::RebuildFailed
        )
    }
}

/// State of a nexus child.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
pub enum ChildState {
    /// Child has not been opened, but we are in the process of opening it.
    Init,
    /// Cannot add this block device to the parent as
    /// it is incompatible property-wise.
    ConfigInvalid,
    /// The child is open for I/O.
    Open,
    /// The child has been closed by the nexus.
    Closed,
    /// The child is faulted.
    Faulted(FaultReason),
}

/// State of the child from the client API perspective.
#[derive(Debug, Clone, Copy)]
pub enum ChildStateClient {
    Init,
    ConfigInvalid,
    Open,
    Closed,
    Faulted(FaultReason),
    Faulting(FaultReason),
    OutOfSync,
}

impl Display for ChildState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Init => write!(f, "init"),
            Self::ConfigInvalid => write!(f, "config invalid"),
            Self::Open => write!(f, "open"),
            Self::Closed => write!(f, "closed"),
            Self::Faulted(r) => write!(f, "faulted ({r})"),
        }
    }
}

impl ChildState {
    /// Determines if the state is open-like or open-failed.
    /// A child is such state cannot be opened again.
    pub fn is_open_or_init(&self) -> bool {
        matches!(
            self,
            ChildState::Open | ChildState::Init | ChildState::ConfigInvalid
        )
    }

    /// Determines if a child is in a state that can be onlined (re-opened).
    pub fn is_recoverable(&self) -> bool {
        match self {
            Self::Faulted(reason) => reason.is_recoverable(),
            Self::Closed => true,
            _ => false,
        }
    }

    /// Check if the state is fault due to ENOSPC.
    pub fn is_enospc(&self) -> bool {
        matches!(self, Self::Faulted(FaultReason::NoSpace))
    }
}

/// Synchronization state of a nexus child.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
pub enum ChildSyncState {
    /// Child is fully synced, i.e. can do both read and writes.
    Synced,
    /// Child is out of sync: awaiting for a rebuild or being rebuilt.
    /// Such child can be a part of write I/O path.
    OutOfSync,
}

impl Display for ChildSyncState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Synced => write!(f, "synced"),
            Self::OutOfSync => write!(f, "out-of-sync"),
        }
    }
}

/// State of a child device destroy process.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
pub(crate) enum ChildDestroyState {
    /// The child is not being destroyed.
    None,
    /// The child device is being destroyed.
    Destroying,
}

impl Display for ChildDestroyState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Destroying => write!(f, "destroying"),
        }
    }
}

#[derive(Serialize)]
pub struct NexusChild<'c> {
    /// name of the parent this child belongs too
    parent: String,
    /// current state of the child
    #[serde(skip_serializing)]
    pub state: AtomicCell<ChildState>,
    /// indicates that the child device is ok, but needs to be rebuilt.
    #[serde(skip_serializing)]
    sync_state: AtomicCell<ChildSyncState>,
    /// current state of device destroy process
    #[serde(skip_serializing)]
    destroy_state: AtomicCell<ChildDestroyState>,
    /// last fault timestamp if this child went faulted
    #[serde(skip_serializing)]
    faulted_at: parking_lot::Mutex<Option<DateTime<Utc>>>,
    /// TODO
    #[serde(skip_serializing)]
    remove_channel: (async_channel::Sender<()>, async_channel::Receiver<()>),
    /// Name of the child is the URI used to create it.
    /// Name of the underlying block device can differ from it.
    ///
    /// TODO: we don't rename this field due to possible issues with
    /// TODO: child serialized state.
    name: String,
    /// Underlying block device.
    #[serde(skip_serializing)]
    device: Option<Box<dyn BlockDevice>>,
    /// TODO
    #[serde(skip_serializing)]
    device_descriptor: Option<Box<dyn BlockDeviceDescriptor>>,
    /// I/O log.
    #[serde(skip_serializing)]
    io_log: Mutex<Option<IOLog>>,
    /// TODO
    #[serde(skip_serializing)]
    _c: PhantomData<&'c ()>,
}

impl Debug for NexusChild<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Determine if the child has an I/O log without locking it.
        let has_log = unsafe { (*self.io_log.data_ptr()).is_some() };

        write!(
            f,
            "Child '{name} @ {nexus}' [{st}{dest} {sync}{re}{io_log}]",
            name = self.name,
            nexus = self.parent,
            st = self.state(),
            dest = match self.destroy_state() {
                ChildDestroyState::None => "",
                ChildDestroyState::Destroying => " (destroying)",
            },
            sync = self.sync_state(),
            re = if self.is_rebuilding() {
                "; rebuilding"
            } else {
                ""
            },
            io_log = if has_log { "; I/O log" } else { "" }
        )
    }
}

impl<'c> NexusChild<'c> {
    /// TODO
    fn set_state(&self, state: ChildState) {
        debug!("{self:?}: changing state to '{state}'");
        self.state.store(state);
    }

    /// Unconditionally sets child's state as faulted with the given reason.
    pub(crate) fn set_faulted_state(&self, reason: FaultReason) {
        self.set_state(ChildState::Faulted(reason));
        self.set_fault_timestamp();
    }

    /// Open the child in RW mode and claim the device to be ours. If the child
    /// is already opened by someone else (i.e one of the targets) it will
    /// error out.
    ///
    /// only devices in the closed or Init state can be opened.
    ///
    /// A child can only be opened if:
    ///  - it's not faulted
    ///  - it's not already opened
    ///  - it's not being destroyed
    pub(crate) fn open(
        &mut self,
        parent_size: u64,
        sync_state: ChildSyncState,
    ) -> Result<String, ChildError> {
        info!("{self:?}: opening child device...");

        if self.is_destroying() {
            error!("{self:?}: cannot open: block device is being destroyed");
            return Err(ChildError::ChildBeingDestroyed {});
        }

        // verify the state of the child before we open it
        match self.state() {
            ChildState::Faulted(s) if !s.is_recoverable() => {
                error!("{:?}: cannot open: state is {}", self, self.state());
                return Err(ChildError::ChildFaulted {});
            }
            ChildState::Open => {
                // The child (should) already be open.
                assert!(self.device.is_some());
                assert!(self.device_descriptor.is_some());
                warn!("{:?}: already opened", self);
                return Ok(self.name.clone());
            }
            _ => {}
        }

        let dev = self.device.as_ref().unwrap();

        let child_size = dev.size_in_bytes();
        if parent_size > child_size {
            error!(
                "{:?}: child is too small, parent size: {} child size: {}",
                self, parent_size, child_size
            );

            self.set_state(ChildState::ConfigInvalid);
            return Err(ChildError::ChildTooSmall {
                parent_size,
                child_size,
            });
        }

        let desc = dev.open(true).map_err(|source| {
            self.set_faulted_state(FaultReason::CantOpen);
            ChildError::OpenChild {
                source,
            }
        })?;
        self.device_descriptor = Some(desc);

        self.set_state(ChildState::Open);
        self.set_sync_state(sync_state);

        info!("{:?}: opened successfully", self);
        Ok(self.name.clone())
    }

    /// Returns the state of the child.
    pub fn state(&self) -> ChildState {
        self.state.load()
    }

    /// Returns the state of the child from the client API perspective.
    pub fn state_client(&self) -> ChildStateClient {
        if self.is_opened_unsync() {
            return ChildStateClient::OutOfSync;
        }

        match self.state() {
            ChildState::Init => ChildStateClient::Init,
            ChildState::ConfigInvalid => ChildStateClient::ConfigInvalid,
            ChildState::Open => ChildStateClient::Open,
            ChildState::Closed => ChildStateClient::Closed,
            ChildState::Faulted(r) => {
                // If the child is in the `Faulted` state but the device is
                // still being closed, report it as a transitional `Faulting`
                // state.
                if self.is_destroying() || self.device_descriptor.is_some() {
                    ChildStateClient::Faulting(r)
                } else {
                    ChildStateClient::Faulted(r)
                }
            }
        }
    }

    /// Returns the sync state of the child.
    #[inline]
    pub fn sync_state(&self) -> ChildSyncState {
        self.sync_state.load()
    }

    /// Returns the sync state of the child.
    #[inline]
    pub fn set_sync_state(&self, s: ChildSyncState) {
        self.sync_state.store(s)
    }

    /// Returns the destroy state of the child.
    #[inline]
    pub(crate) fn destroy_state(&self) -> ChildDestroyState {
        self.destroy_state.load()
    }

    /// Sets the destroy state of the child.
    #[inline]
    pub(crate) fn set_destroy_state(&self, s: ChildDestroyState) {
        self.destroy_state.store(s)
    }

    /// Determines if the child is being destroyed.
    #[inline]
    pub(crate) fn is_destroying(&self) -> bool {
        matches!(self.destroy_state.load(), ChildDestroyState::Destroying)
    }

    /// Return the last fault timestamp of the child.
    pub fn fault_timestamp(&self) -> Option<DateTime<Utc>> {
        *self.faulted_at.lock()
    }

    /// Sets the timestamp of this child fault to `now`.
    pub(super) fn set_fault_timestamp(&self) {
        *self.faulted_at.lock() = Some(Utc::now());
    }

    /// Determines if the child is opened.
    #[inline]
    pub fn is_opened(&self) -> bool {
        self.state() == ChildState::Open
    }

    /// Determines if the child is opened but out-of-sync (needs rebuild or
    /// being rebuilt).
    #[inline]
    pub fn is_opened_unsync(&self) -> bool {
        self.state() == ChildState::Open
            && self.sync_state() == ChildSyncState::OutOfSync
    }

    /// Determines if the child is opened and fully synced.
    #[inline]
    pub fn is_healthy(&self) -> bool {
        self.state() == ChildState::Open
            && self.sync_state() == ChildSyncState::Synced
    }

    /// Determines if the child is being rebuilt.
    #[inline]
    pub(crate) fn is_rebuilding(&self) -> bool {
        self.rebuild_job().is_some() && self.is_opened_unsync()
    }

    /// Register an NVMe reservation, specifying a new key
    async fn resv_register(
        &self,
        hdl: &dyn BlockDeviceHandle,
        new_key: u64,
    ) -> Result<(), CoreError> {
        hdl.nvme_resv_register(
            0,
            new_key,
            nvme_reservation_register_action::REPLACE_KEY,
            match MayastorEnvironment::global_or_default().ptpl_dir() {
                Some(_) => nvme_reservation_register_cptpl::PERSIST_POWER_LOSS,
                None => nvme_reservation_register_cptpl::CLEAR_POWER_ON,
            },
        )
        .await?;
        info!("{:?}: registered key {:0x}h", self, new_key);
        Ok(())
    }

    /// Acquire an NVMe reservation
    async fn resv_acquire(
        &self,
        hdl: &dyn BlockDeviceHandle,
        current_key: u64,
        preempt_key: Option<u64>,
        resv_type: NvmeReservation,
    ) -> Result<(), ChildError> {
        let acquire_action = preempt_key
            .map(|_| nvme_reservation_acquire_action::PREEMPT)
            .unwrap_or(nvme_reservation_acquire_action::ACQUIRE);
        let preempt_key = preempt_key.unwrap_or_default();
        if let Err(e) = hdl
            .nvme_resv_acquire(
                current_key,
                preempt_key,
                acquire_action,
                resv_type as u8,
            )
            .await
        {
            return Err(ChildError::ResvAcquire {
                source: e,
            });
        }

        info!(
            "{:?}: acquired reservation type {:x}h, action {:x}h, \
            current key {:0x}h, preempt key {:0x}h",
            self, resv_type as u8, acquire_action, current_key, preempt_key,
        );
        Ok(())
    }

    /// Register an NVMe reservation, specifying a new key
    async fn resv_release(
        &self,
        hdl: &dyn BlockDeviceHandle,
        current_key: u64,
        resv_type: NvmeReservation,
        release_action: u8,
    ) -> Result<(), CoreError> {
        let resv_type = resv_type as u8;
        hdl.nvme_resv_release(current_key, resv_type, release_action)
            .await?;
        info!("{:?}: released key type {:0x}h", self, resv_type);
        Ok(())
    }

    /// Get NVMe reservation holder.
    /// Returns: (key, host id) of the reservation holder.
    async fn resv_holder(
        &self,
        hdl: &dyn BlockDeviceHandle,
    ) -> Result<Option<(u8, u64, [u8; 16])>, ChildError> {
        let mut buffer = hdl.dma_malloc(4096).context(HandleDmaMalloc {})?;
        if let Err(e) = hdl.nvme_resv_report(1, &mut buffer).await {
            return Err(ChildError::ResvReport {
                source: e,
            });
        }

        trace!("{:?}: received reservation report", self);

        let (stext, sl) = buffer.as_slice().split_at(std::mem::size_of::<
            spdk_nvme_reservation_status_extended_data,
        >());
        let (pre, resv_status_ext, post) = unsafe {
            stext.align_to::<spdk_nvme_reservation_status_extended_data>()
        };

        assert!(pre.is_empty());
        assert!(post.is_empty());

        let regctl = resv_status_ext[0].data.regctl;

        info!(
            "reservation status: rtype {}, regctl {}, ptpls {}",
            resv_status_ext[0].data.rtype,
            regctl,
            resv_status_ext[0].data.ptpls,
        );

        let (pre, reg_ctrlr_ext, _post) = unsafe {
            sl.align_to::<spdk_nvme_registered_ctrlr_extended_data>()
        };

        if !pre.is_empty() {
            return Ok(None);
        }

        let mut numctrlr: usize = regctl.into();
        if numctrlr > reg_ctrlr_ext.len() {
            numctrlr = reg_ctrlr_ext.len();
            warn!(
                "Expecting data for {} controllers, received {}",
                regctl, numctrlr
            );
        }

        for (i, c) in reg_ctrlr_ext.iter().enumerate().take(numctrlr) {
            let cntlid = c.cntlid;
            let rkey = c.rkey;
            debug!(
                "ctrlr {}: cntlid {:0x}h, status {}, hostid {:0x?}, \
                rkey {:0x}h",
                i,
                cntlid,
                c.rcsts.status(),
                c.hostid,
                rkey,
            );
            if c.rcsts.status() == 1 {
                return Ok(Some((
                    resv_status_ext[0].data.rtype,
                    rkey,
                    c.hostid,
                )));
            }
        }
        Ok(None)
    }

    /// Check if we're the reservation holder.
    /// # Warning: Ignores bdevs without NVMe reservation support.
    async fn resv_check_holder(
        &self,
        args: &NexusNvmeParams,
    ) -> Result<(), ChildError> {
        let hdl = self.get_io_handle_nonblock().await.context(HandleOpen {})?;

        let mut buffer = hdl.dma_malloc(4096).context(HandleDmaMalloc {})?;
        match hdl.nvme_resv_report(1, &mut buffer).await {
            Err(CoreError::NotSupported {
                ..
            }) => return Ok(()),
            Err(error) => Err(ChildError::ResvReport {
                source: error,
            }),
            Ok(_) => Ok(()),
        }?;

        let (stext, sl) = buffer.as_slice().split_at(std::mem::size_of::<
            spdk_nvme_reservation_status_extended_data,
        >());
        let (pre, resv_status_ext, post) = unsafe {
            stext.align_to::<spdk_nvme_reservation_status_extended_data>()
        };

        assert!(pre.is_empty());
        assert!(post.is_empty());

        let regctl = resv_status_ext[0].data.regctl;

        info!(
            "{:?}: reservation status: rtype {}, regctl {}, ptpls {}",
            self,
            resv_status_ext[0].data.rtype,
            regctl,
            resv_status_ext[0].data.ptpls,
        );

        let shared = |resv_type| {
            matches!(
                resv_type,
                NvmeReservation::ExclusiveAccessAllRegs
                    | NvmeReservation::WriteExclusiveAllRegs
            )
        };

        if args.resv_type as u8 != resv_status_ext[0].data.rtype {
            let rtype =
                NvmeReservation::try_from(resv_status_ext[0].data.rtype)
                    .map_err(|_| ChildError::ResvType {
                        resv_type: resv_status_ext[0].data.rtype,
                    })?;

            // If we're shared, then we don't care which type it is since we're
            // registered...
            if !shared(args.resv_type) || !shared(rtype) {
                return Err(ChildError::ResvType {
                    resv_type: resv_status_ext[0].data.rtype,
                });
            }
        }

        if matches!(
            args.resv_type,
            NvmeReservation::ExclusiveAccessAllRegs
                | NvmeReservation::WriteExclusiveAllRegs
        ) {
            // if we're in "shared" mode, we don't need to know more
            return Ok(());
        }

        let (pre, reg_ctrlr_ext, _post) = unsafe {
            sl.align_to::<spdk_nvme_registered_ctrlr_extended_data>()
        };
        if !pre.is_empty() {
            // todo: why did the previous report return no holder in this
            // scenario?
            return Err(ChildError::ResvNoHolder {
                resv_type: resv_status_ext[0].data.rtype,
            });
        }

        let mut numctrlr: usize = regctl.into();
        if numctrlr > reg_ctrlr_ext.len() {
            numctrlr = reg_ctrlr_ext.len();
            warn!(
                "Expecting data for {} controllers, received {}",
                regctl, numctrlr
            );
        }

        if let Some(owner) = reg_ctrlr_ext
            .iter()
            .take(numctrlr)
            .find(|c| c.rcsts.status() == 1)
        {
            let my_hostid = match hdl.host_id().await {
                Ok(h) => h,
                Err(e) => {
                    return Err(ChildError::NvmeHostId {
                        source: e,
                    });
                }
            };
            if owner.rkey != args.resv_key || owner.hostid != my_hostid {
                return Err(ChildError::Holder {
                    hostid: owner.hostid,
                    resv_type: resv_status_ext[0].data.rtype,
                    resv_key: owner.rkey,
                });
            }
            Ok(())
        } else {
            Err(ChildError::ResvNoHolder {
                resv_type: resv_status_ext[0].data.rtype,
            })
        }
    }

    /// Register an NVMe reservation on the child then acquire or preempt an
    /// existing reservation depending on the specified parameters.
    /// This allows for a "manual" preemption.
    /// # Warning: Ignores bdevs without NVMe reservation support.
    pub(crate) async fn reservation_acquire_argkey(
        &self,
        params: &NexusNvmeParams,
    ) -> Result<(), ChildError> {
        let hdl = self.get_io_handle_nonblock().await.context(HandleOpen {})?;

        let resv_key = params.resv_key;
        if let Err(e) = self.resv_register(&*hdl, resv_key).await {
            return match e {
                CoreError::NotSupported {
                    ..
                } => Ok(()),
                _ => Err(ChildError::ResvRegisterKey {
                    source: e,
                }),
            };
        }

        let preempt_key = params.preempt_key.map(|k| k.get());
        self.resv_acquire(&*hdl, resv_key, preempt_key, params.resv_type)
            .await
            .map_err(|error| {
                warn!(
                    "{:?}: failed to acquire reservation ({:?}): {}",
                    self,
                    params.resv_type,
                    error.verbose()
                );
                error
            })
    }

    /// Register an NVMe reservation on the child.
    /// # Warning: Ignores bdevs without NVMe reservation support.
    pub(crate) async fn reservation_acquire(
        &self,
        params: &NexusNvmeParams,
    ) -> Result<(), ChildError> {
        if std::env::var("NEXUS_NVMF_RESV_ENABLE").is_err() {
            return Ok(());
        }
        if !params.reservations_enabled() {
            return Ok(());
        }

        match params.preempt_policy {
            NexusNvmePreemption::ArgKey => {
                self.reservation_acquire_argkey(params).await?;
            }
            NexusNvmePreemption::Holder => {
                self.reservation_preempt_holder(params).await?;
            }
        }
        self.resv_check_holder(params).await
    }

    /// Register an NVMe reservation on the child and preempt any existing
    /// reservation holder automatically if necessary.
    /// Refer to the NVMe spec for more information:
    /// https://nvmexpress.org/wp-content/uploads/NVMe-NVM-Express-2.0a-2021.07.26-Ratified.pdf
    /// # Warning: Ignores bdevs without NVMe reservation support.
    pub(crate) async fn reservation_preempt_holder(
        &self,
        args: &NexusNvmeParams,
    ) -> Result<(), ChildError> {
        let hdl = self.get_io_handle_nonblock().await.context(HandleOpen {})?;

        // To be able to issue any other commands we must first register.
        if let Err(e) = self.resv_register(&*hdl, args.resv_key).await {
            return match e {
                CoreError::NotSupported {
                    ..
                } => Ok(()),
                _ => Err(ChildError::ResvRegisterKey {
                    source: e,
                }),
            };
        }

        let (rtype, pkey, hostid) = match self.resv_holder(&*hdl).await? {
            Some(existing) => existing,
            None => {
                info!("{:?}: reservation held by NONE", self);
                // Currently there is no reservation holder, so rather than
                // preempt we simply acquire the reservation
                // with our key and type.
                return self
                    .resv_acquire(&*hdl, args.resv_key, None, args.resv_type)
                    .await;
            }
        };

        let my_hostid = match hdl.host_id().await {
            Ok(h) => h,
            Err(e) => {
                return Err(ChildError::NvmeHostId {
                    source: e,
                });
            }
        };
        info!(
            "{:?}::{:?}: reservation held {:0x?} {:0x}h",
            self, my_hostid, hostid, pkey
        );

        let rtype = NvmeReservation::try_from(rtype).map_err(|_| {
            ChildError::ResvType {
                resv_type: rtype,
            }
        })?;
        if rtype == args.resv_type
            && hostid == my_hostid
            && pkey == args.resv_key
        {
            return Ok(());
        }
        if !matches!(
            rtype,
            NvmeReservation::WriteExclusiveAllRegs
                | NvmeReservation::ExclusiveAccessAllRegs
        ) {
            // This is the most straightforward case where we can simply preempt
            // the existing holder with our own key and type.
            self.resv_acquire(&*hdl, args.resv_key, Some(pkey), args.resv_type)
                .await?;
            if !(rtype != args.resv_type && hostid == my_hostid) {
                // When registering a new key with Register Action REPLACE and
                // Ignoring Existing Key, the registration succeeds and the key
                // is replaced but the registration is not changed in the
                // namespace. In this case the report contains the wrong key as
                // the holder so the previous acquire is not sufficient.
                self.resv_acquire(&*hdl, args.resv_key, None, args.resv_type)
                    .await?;
                return Ok(());
            }
            // if we were the previous owner, we've now cleared the
            // registration, so we need to start over.
            self.resv_register(&*hdl, args.resv_key)
                .await
                .map_err(|e| ChildError::ResvRegisterKey {
                    source: e,
                })?;
            self.resv_acquire(&*hdl, args.resv_key, None, args.resv_type)
                .await?;
            return Ok(());
        }

        match args.resv_type {
            NvmeReservation::WriteExclusive
            | NvmeReservation::ExclusiveAccess
            | NvmeReservation::WriteExclusiveRegsOnly
            | NvmeReservation::ExclusiveAccessRegsOnly => {
                // We want to move from a type where everyone has access to a
                // more restricted type so we must first remove
                // all existing registrants.
                // https://nvmexpress.org/wp-content/uploads/NVMe-NVM-Express-2.0a-2021.07.26-Ratified.pdf
                // 8.19.7
                self.resv_release(&*hdl, args.resv_key, rtype, 0)
                    .await
                    .map_err(|e| ChildError::ResvRelease {
                        source: e,
                    })?;
                // And now we can acquire the reservation with our own more
                // restricted reservation type.
                self.resv_acquire(&*hdl, args.resv_key, None, args.resv_type)
                    .await?;
            }
            _ => {
                // Registrants have both R&W access so there is nothing
                // more to do here because we've already registered.
            }
        }

        Ok(())
    }

    /// Closes the child and forces a faulted state.
    pub(crate) async fn close_faulted(&self, reason: FaultReason) {
        self.close().await.ok();
        self.set_faulted_state(reason);
    }

    /// Get URI of this Nexus child.
    pub fn uri(&self) -> &str {
        &self.name
    }

    /// Get UUID of this nexus child, if available.
    pub fn get_uuid(&self) -> Option<String> {
        Self::uuid(self.uri())
    }

    /// Get name of the nexus this child belongs to.
    pub fn nexus_name(&self) -> &str {
        &self.parent
    }

    /// Onlines a previously offlined child.
    /// The child is set out-of-sync so that it will be rebuilt.
    /// TODO: channels need to be updated when block devices are opened.
    pub(crate) async fn online(
        &mut self,
        parent_size: u64,
    ) -> Result<String, ChildError> {
        info!("{:?}: bringing child online", self);

        let state = self.state.load();

        if state.is_open_or_init() {
            warn!(
                "{:?}: child is in {} state and cannot be onlined",
                self, state
            );
            return Err(ChildError::CannotOnlineChild {});
        }

        if self.is_destroying() {
            error!("{self:?}: child is being destroyed and cannot be onlined");
            return Err(ChildError::ChildBeingDestroyed {});
        }

        if !state.is_recoverable() {
            warn!(
                "{:?}: child is permanently faulted and cannot be onlined",
                self
            );
            return Err(ChildError::PermanentlyFaulted {});
        }

        // Re-create the block device as it will have been previously
        // destroyed.
        let name =
            device_create(&self.name).await.context(ChildBdevCreate {
                child: self.name.clone(),
            })?;

        self.device = device_lookup(&name);
        if self.device.is_none() {
            error!(
                "{:?}: failed to find device after successful creation",
                self,
            );
            return Err(ChildError::ChildInaccessible {});
        }

        self.open(parent_size, ChildSyncState::OutOfSync)
    }

    /// Extract a UUID from a URI.
    pub(crate) fn uuid(uri: &str) -> Option<String> {
        let url = Url::parse(uri).expect("Failed to parse URI");
        for pair in url.query_pairs() {
            if pair.0 == "uuid" {
                return Some(pair.1.to_string());
            }
        }
        None
    }

    /// Closes the nexus child.
    pub(crate) async fn close(&self) -> Result<(), BdevError> {
        info!("{self:?}: closing child...");

        if self.destroy_state.compare_exchange(
            ChildDestroyState::None,
            ChildDestroyState::Destroying,
        ) != Ok(ChildDestroyState::None)
        {
            warn!("{self:?}: already being closed");
            return Ok(());
        }

        if self.device.is_none() {
            self.set_destroy_state(ChildDestroyState::None);
            warn!("{self:?}: no block device: appears to be already closed");
            return Ok(());
        }

        // TODO: Check device claiming scheme.
        if self.device_descriptor.is_some() {
            self.device_descriptor.as_ref().unwrap().unclaim();
        }

        // Destruction raises a device removal event.
        info!("{self:?}: destroying block device...");
        match device_destroy(&self.name).await {
            Ok(_) => {
                info!(
                    "{self:?}: block device destroyed, waiting for removal..."
                );

                // Only wait for block device removal if the child has been
                // initialised.
                if self.state.load() != ChildState::Init {
                    self.remove_channel.1.recv().await.ok();
                }

                self.set_destroy_state(ChildDestroyState::None);
                info!("{self:?}: child closed successfully");
                Ok(())
            }
            Err(e) => {
                self.set_destroy_state(ChildDestroyState::None);
                error!("{self:?}: failed to close child: {e}");
                Err(e)
            }
        }
    }

    /// Called in response to a device removal event.
    /// All the necessary teardown should be performed here before the
    /// underlying device is removed.
    ///
    /// Note: The descriptor *must* be dropped for the unplug to complete.
    pub(crate) fn unplug(&mut self) {
        info!("{self:?}: unplugging child...");

        let state = self.state();
        let is_destroying = self.is_destroying();

        // Only drop the device and the device descriptor if the child is being
        // destroyed. For a hot remove event, keep the device and descriptor.
        if is_destroying {
            debug!("{self:?}: dropping block device");
            self.device = None;
        } else {
            debug!("{self:?}: hot remove: keeping block device");
        }

        if matches!(state, ChildState::Open) {
            // Change the state of the child to ensure it is taken out of
            // the I/O path when the nexus is reconfigured.
            // TODO: double-check interaction with rebuild job logic
            self.set_state(ChildState::Closed);
        }

        // Remove the child from the I/O path. If we had an IO error the block
        // device, the channels were already reconfigured so we don't
        // have to do that twice.
        // TODO: Revisit nexus reconfiguration once Nexus has switched to
        // BlockDevice-based children and is able to listen to
        // device-related events directly.
        if state != ChildState::Faulted(FaultReason::IoError) {
            let nexus_name = self.parent.clone();
            Reactor::block_on(async move {
                match nexus_lookup_mut(&nexus_name) {
                    Some(n) => n.reconfigure(DrEvent::ChildUnplug).await,
                    None => error!("Nexus '{nexus_name}' not found"),
                }
            });
        }

        if is_destroying {
            // Dropping the last descriptor results in the device being removed.
            // This must be performed in this function.
            self.device_descriptor.take();
        }

        self.unplug_complete();
        info!("{self:?}: child successfully unplugged");
    }

    /// Signal that the child unplug is complete.
    fn unplug_complete(&self) {
        let sender = self.remove_channel.0.clone();
        let name = self.name.clone();
        Reactors::current().send_future(async move {
            if let Err(e) = sender.send(()).await {
                error!(
                    "Failed to send unplug complete for child '{}': {}",
                    name, e
                );
            }
        });
    }

    /// create a new nexus child
    pub fn new(
        name: String,
        parent: String,
        device: Option<Box<dyn BlockDevice>>,
    ) -> Self {
        // TODO: Remove check for persistent store
        if PersistentStore::enabled() && Self::uuid(&name).is_none() {
            panic!("Child name does not contain a UUID.");
        }

        NexusChild {
            name,
            device,
            parent,
            device_descriptor: None,
            state: AtomicCell::new(ChildState::Init),
            sync_state: AtomicCell::new(ChildSyncState::Synced),
            destroy_state: AtomicCell::new(ChildDestroyState::None),
            faulted_at: parking_lot::Mutex::new(None),
            remove_channel: async_channel::bounded(1),
            io_log: Mutex::new(None),
            _c: Default::default(),
        }
    }

    /// Returns reference to child's block device.
    pub fn get_device(&self) -> Result<&dyn BlockDevice, ChildError> {
        if let Some(ref device) = self.device {
            Ok(&**device)
        } else {
            Err(ChildError::ChildInaccessible {})
        }
    }

    /// TODO
    pub(super) fn remove_rebuild_job(
        &self,
    ) -> Option<std::sync::Arc<RebuildJob>> {
        RebuildJob::remove(&self.name).ok()
    }

    /// Return the rebuild job which is rebuilding this child, if rebuilding.
    pub(crate) fn rebuild_job(&self) -> Option<std::sync::Arc<RebuildJob>> {
        RebuildJob::lookup(&self.name).ok()
    }

    /// Return the rebuild progress on this child, if rebuilding.
    pub async fn get_rebuild_progress(&self) -> i32 {
        match self.rebuild_job() {
            Some(j) => j.stats().await.progress as i32,
            None => -1,
        }
    }

    /// Determine if a child is local to the nexus (i.e. on the same node).
    pub fn is_local(&self) -> Option<bool> {
        match &self.device {
            Some(dev) => {
                // A local child is not exported over nvme.
                let local = dev.driver_name() != "nvme";
                Some(local)
            }
            None => None,
        }
    }

    /// Get I/O handle for the block device associated with this Nexus child.
    pub fn get_io_handle(
        &self,
    ) -> Result<Box<dyn BlockDeviceHandle>, CoreError> {
        if let Some(desc) = self.device_descriptor.as_ref() {
            desc.get_io_handle()
        } else {
            error!("{:?}: child does not have valid descriptor", self);
            Err(CoreError::InvalidDescriptor {
                name: self.name.clone(),
            })
        }
    }

    pub async fn get_io_handle_nonblock(
        &self,
    ) -> Result<Box<dyn BlockDeviceHandle>, CoreError> {
        if let Some(desc) = self.device_descriptor.as_ref() {
            desc.get_io_handle_nonblock().await
        } else {
            error!("{:?}: child does not have valid descriptor", self);
            Err(CoreError::InvalidDescriptor {
                name: self.name.clone(),
            })
        }
    }

    /// TODO
    pub fn get_device_name(&self) -> Option<String> {
        self.device.as_ref().map(|d| d.device_name())
    }

    /// TODO
    pub fn match_device_name(&self, bdev_name: &str) -> bool {
        match self.get_device_name() {
            Some(n) => n == bdev_name,
            None => false,
        }
    }

    /// TODO
    pub(crate) fn set_event_listener(&self, listener: DeviceEventSink) {
        let dev = self
            .get_device()
            .expect("No block device associated with a Nexus child");

        match dev.add_event_listener(listener) {
            Err(err) => {
                error!(
                    ?err,
                    "{:?}: failed to add event for device '{}'",
                    self,
                    dev.device_name()
                )
            }
            _ => {
                debug!(
                    "{:?}: added event listener for device '{}'",
                    self,
                    dev.device_name()
                );
            }
        }
    }

    /// Creates a new I/O log, if none existed.
    /// Returns true if a log has been created or already exists, false if I/O
    /// log is disabled for this child for whatever reason.
    ///
    /// I/O log is never created if the child is not fully synced.
    pub(super) fn start_io_log(&self) -> bool {
        if !super::ENABLE_PARTIAL_REBUILD.load(Ordering::SeqCst) {
            return false;
        }

        if self.sync_state() == ChildSyncState::OutOfSync {
            assert!(self.io_log.lock().is_none());
            return false;
        }

        let mut io_log = self.io_log.lock();

        if io_log.is_none() {
            if let Some(d) = &self.device {
                *io_log = Some(IOLog::new(
                    &d.device_name(),
                    d.num_blocks(),
                    d.block_len(),
                ));

                debug!("{self:?}: started new I/O log: {log:?}", log = *io_log);
            }
        }

        io_log.is_some()
    }

    /// Stops the I/O log and returns a map of segments to be rebuilt.
    pub(super) fn stop_io_log(&self) -> Option<RebuildMap> {
        debug!("{self:?}: stopping I/O log and creating rebuild map");
        self.io_log.lock().take().map(|log| log.finalize())
    }

    /// Returns I/O log channel for the current core.
    pub(super) fn io_log_channel(&self) -> Option<IOLogChannel> {
        self.io_log.lock().as_ref().map(|log| log.current_channel())
    }

    /// Determines if the child has an active I/O log.
    pub(crate) fn has_io_log(&self) -> bool {
        self.io_log.lock().is_some()
    }
}
