// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

use std::fmt;
use std::time::Instant;

use kvproto::import_sstpb::SSTMeta;
use kvproto::metapb;
use kvproto::metapb::RegionEpoch;
use kvproto::pdpb::CheckPolicy;
use kvproto::raft_cmdpb::{RaftCmdRequest, RaftCmdResponse};
use kvproto::raft_serverpb::RaftMessage;
use raft::SnapshotStatus;

use crate::raftstore::store::fsm::apply::TaskRes as ApplyTaskRes;
use crate::raftstore::store::util::KeysInfoFormatter;
use crate::raftstore::store::SnapKey;
use crate::storage::kv::CompactedEvent;
use tikv_util::escape;

use super::RegionSnapshot;

#[derive(Debug, Clone)]
pub struct ReadResponse {
    pub response: RaftCmdResponse,
    pub snapshot: Option<RegionSnapshot>,
}

#[derive(Debug)]
pub struct WriteResponse {
    pub response: RaftCmdResponse,
}

pub type ReadCallback = Box<dyn FnOnce(ReadResponse) + Send>;
pub type WriteCallback = Box<dyn FnOnce(WriteResponse) + Send>;

/// Variants of callbacks for `Msg`.
///  - `Read`: a callbak for read only requests including `StatusRequest`,
///         `GetRequest` and `SnapRequest`
///  - `Write`: a callback for write only requests including `AdminRequest`
///          `PutRequest`, `DeleteRequest` and `DeleteRangeRequest`.
pub enum Callback {
    /// No callback.
    None,
    /// Read callback.
    Read(ReadCallback),
    /// Write callback.
    Write(WriteCallback),
}

impl Callback {
    pub fn invoke_with_response(self, resp: RaftCmdResponse) {
        match self {
            Callback::None => (),
            Callback::Read(read) => {
                let resp = ReadResponse {
                    response: resp,
                    snapshot: None,
                };
                read(resp);
            }
            Callback::Write(write) => {
                let resp = WriteResponse { response: resp };
                write(resp);
            }
        }
    }

    pub fn invoke_read(self, args: ReadResponse) {
        match self {
            Callback::Read(read) => read(args),
            other => panic!("expect Callback::Read(..), got {:?}", other),
        }
    }

    pub fn is_none(&self) -> bool {
        match self {
            Callback::None => true,
            _ => false,
        }
    }
}

impl fmt::Debug for Callback {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Callback::None => write!(fmt, "Callback::None"),
            Callback::Read(_) => write!(fmt, "Callback::Read(..)"),
            Callback::Write(_) => write!(fmt, "Callback::Write(..)"),
        }
    }
}

bitflags! {
    pub struct PeerTicks: u8 {
        const RAFT                   = 0b00000001;
        const RAFT_LOG_GC            = 0b00000010;
        const SPLIT_REGION_CHECK     = 0b00000100;
        const PD_HEARTBEAT           = 0b00001000;
        const CHECK_MERGE            = 0b00010000;
        const CHECK_PEER_STALE_STATE = 0b00100000;
    }
}

impl PeerTicks {
    #[inline]
    pub fn tag(self) -> &'static str {
        match self {
            PeerTicks::RAFT => "raft",
            PeerTicks::RAFT_LOG_GC => "raft_log_gc",
            PeerTicks::SPLIT_REGION_CHECK => "split_region_check",
            PeerTicks::PD_HEARTBEAT => "pd_heartbeat",
            PeerTicks::CHECK_MERGE => "check_merge",
            PeerTicks::CHECK_PEER_STALE_STATE => "check_peer_stale_state",
            _ => unreachable!(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum StoreTick {
    CompactCheck,
    PdStoreHeartbeat,
    SnapGc,
    CompactLockCf,
    ConsistencyCheck,
    CleanupImportSST,
}

impl StoreTick {
    #[inline]
    pub fn tag(self) -> &'static str {
        match self {
            StoreTick::CompactCheck => "compact_check",
            StoreTick::PdStoreHeartbeat => "pd_store_heartbeat",
            StoreTick::SnapGc => "snap_gc",
            StoreTick::CompactLockCf => "compact_lock_cf",
            StoreTick::ConsistencyCheck => "consistency_check",
            StoreTick::CleanupImportSST => "cleanup_import_sst",
        }
    }
}

/// Some significant messages sent to raftstore. Raftstore will dispatch these messages to Raft
/// groups to update some important internal status.
#[derive(Debug, PartialEq)]
pub enum SignificantMsg {
    /// Reports whether the snapshot sending is successful or not.
    SnapshotStatus {
        region_id: u64,
        to_peer_id: u64,
        status: SnapshotStatus,
    },
    StoreUnreachable {
        store_id: u64,
    },
    /// Reports `to_peer_id` is unreachable.
    Unreachable {
        region_id: u64,
        to_peer_id: u64,
    },
}

/// Message that will be sent to a peer.
///
/// These messages are not significant and can be dropped occasionally.
pub enum CasualMessage {
    /// Split the target region into several partitions.
    SplitRegion {
        region_epoch: RegionEpoch,
        // It's an encoded key.
        // TODO: support meta key.
        split_keys: Vec<Vec<u8>>,
        callback: Callback,
    },

    /// Hash result of ComputeHash command.
    ComputeHashResult {
        index: u64,
        hash: Vec<u8>,
    },

    /// Approximate size of target region.
    RegionApproximateSize {
        size: u64,
    },

    /// Approximate key count of target region.
    RegionApproximateKeys {
        keys: u64,
    },
    CompactionDeclinedBytes {
        bytes: u64,
    },
    /// Half split the target region.
    HalfSplitRegion {
        region_epoch: RegionEpoch,
        policy: CheckPolicy,
    },
    /// Result of querying pd whether a region is merged.
    MergeResult {
        target: metapb::Peer,
        stale: bool,
    },
    /// Remove snapshot files in `snaps`.
    GcSnap {
        snaps: Vec<(SnapKey, bool)>,
    },
    /// Clear region size cache.
    ClearRegionSize,
    /// Indicate a target region is overlapped.
    RegionOverlapped,
}

impl fmt::Debug for CasualMessage {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CasualMessage::ComputeHashResult { index, ref hash } => write!(
                fmt,
                "ComputeHashResult [index: {}, hash: {}]",
                index,
                escape(hash)
            ),
            CasualMessage::SplitRegion { ref split_keys, .. } => {
                write!(fmt, "Split region with {}", KeysInfoFormatter(&split_keys))
            }
            CasualMessage::RegionApproximateSize { size } => {
                write!(fmt, "Region's approximate size [size: {:?}]", size)
            }
            CasualMessage::RegionApproximateKeys { keys } => {
                write!(fmt, "Region's approximate keys [keys: {:?}]", keys)
            }
            CasualMessage::CompactionDeclinedBytes { bytes } => {
                write!(fmt, "compaction declined bytes {}", bytes)
            }
            CasualMessage::HalfSplitRegion { .. } => write!(fmt, "Half Split"),
            CasualMessage::MergeResult { target, stale } => write! {
                fmt,
                "target: {:?}, successful: {}",
                target, stale
            },
            CasualMessage::GcSnap { ref snaps } => write! {
                fmt,
                "gc snaps {:?}",
                snaps
            },
            CasualMessage::ClearRegionSize => write! {
                fmt,
                "clear region size"
            },
            CasualMessage::RegionOverlapped => write!(fmt, "RegionOverlapped"),
        }
    }
}

/// Raft command is the command that is expected to be proposed by the
/// leader of the target raft group.
#[derive(Debug)]
pub struct RaftCommand {
    pub send_time: Instant,
    pub request: RaftCmdRequest,
    pub callback: Callback,
}

impl RaftCommand {
    #[inline]
    pub fn new(request: RaftCmdRequest, callback: Callback) -> RaftCommand {
        RaftCommand {
            request,
            callback,
            send_time: Instant::now(),
        }
    }
}

/// Message that can be sent to a peer.
pub enum PeerMsg {
    /// Raft message is the message sent between raft nodes in the same
    /// raft group. Messages need to be redirected to raftstore if target
    /// peer doesn't exist.
    RaftMessage(RaftMessage),
    /// Raft command is the command that is expected to be proposed by the
    /// leader of the target raft group. If it's failed to be sent, callback
    /// usually needs to be called before dropping in case of resource leak.
    RaftCommand(RaftCommand),
    /// Tick is periodical task. If target peer doesn't exist there is a potential
    /// that the raft node will not work anymore.
    Tick(PeerTicks),
    /// Result of applying committed entries. The message can't be lost.
    ApplyRes { res: ApplyTaskRes },
    /// Message that can't be lost but rarely created. If they are lost, real bad
    /// things happen like some peers will be considered dead in the group.
    SignificantMsg(SignificantMsg),
    /// Start the FSM.
    Start,
    /// A message only used to notify a peer.
    Noop,
    /// Message that is not important and can be dropped occasionally.
    CasualMessage(CasualMessage),
    /// Ask region to report a heartbeat to PD.
    HeartbeatPd,
}

impl fmt::Debug for PeerMsg {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeerMsg::RaftMessage(_) => write!(fmt, "Raft Message"),
            PeerMsg::RaftCommand(_) => write!(fmt, "Raft Command"),
            PeerMsg::Tick(tick) => write! {
                fmt,
                "{:?}",
                tick
            },
            PeerMsg::SignificantMsg(msg) => write!(fmt, "{:?}", msg),
            PeerMsg::ApplyRes { res } => write!(fmt, "ApplyRes {:?}", res),
            PeerMsg::Start => write!(fmt, "Startup"),
            PeerMsg::Noop => write!(fmt, "Noop"),
            PeerMsg::CasualMessage(msg) => write!(fmt, "CasualMessage {:?}", msg),
            PeerMsg::HeartbeatPd => write!(fmt, "HeartbeatPd"),
        }
    }
}

pub enum StoreMsg {
    RaftMessage(RaftMessage),
    // For snapshot stats.
    SnapshotStats,

    ValidateSSTResult {
        invalid_ssts: Vec<SSTMeta>,
    },

    // Clear region size and keys for all regions in the range, so we can force them to re-calculate
    // their size later.
    ClearRegionSizeInRange {
        start_key: Vec<u8>,
        end_key: Vec<u8>,
    },
    StoreUnreachable {
        store_id: u64,
    },

    // Compaction finished event
    CompactedEvent(CompactedEvent),
    Tick(StoreTick),
    Start {
        store: metapb::Store,
    },
}

impl fmt::Debug for StoreMsg {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            StoreMsg::RaftMessage(_) => write!(fmt, "Raft Message"),
            StoreMsg::SnapshotStats => write!(fmt, "Snapshot stats"),
            StoreMsg::StoreUnreachable { store_id } => {
                write!(fmt, "Store {}  is unreachable", store_id)
            }
            StoreMsg::CompactedEvent(ref event) => write!(fmt, "CompactedEvent cf {}", event.cf),
            StoreMsg::ValidateSSTResult { .. } => write!(fmt, "Validate SST Result"),
            StoreMsg::ClearRegionSizeInRange {
                ref start_key,
                ref end_key,
            } => write!(
                fmt,
                "Clear Region size in range {:?} to {:?}",
                start_key, end_key
            ),
            StoreMsg::Tick(tick) => write!(fmt, "StoreTick {:?}", tick),
            StoreMsg::Start { ref store } => write!(fmt, "Start store {:?}", store),
        }
    }
}

// TODO: remove this enum and utilize the actual message instead.
#[derive(Debug)]
pub enum Msg {
    PeerMsg(PeerMsg),
    StoreMsg(StoreMsg),
}
