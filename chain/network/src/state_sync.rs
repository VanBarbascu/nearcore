use actix::Message;
use near_primitives::{hash::CryptoHash, types::ShardId, network::PeerId};

use crate::network_protocol::KnownStateRequestMsg;

#[derive(Debug)]
pub struct StatePartRequest {
    pub shard_id: ShardId,
    pub sync_hash: CryptoHash,
    pub part_id: u64,
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub enum StateSyncRequestFromNetwork {
    KnownStatePartsRequest { request: KnownStateRequestMsg, peer_id: PeerId },
    StatePartRequest { request: StatePartRequest, peer_id: PeerId },
}
