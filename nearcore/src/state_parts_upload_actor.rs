use actix::{Actor, Addr, Context, Handler, Message};
use actix_rt::ArbiterHandle;
use near_o11y::{handler_debug_span, WithSpanContext};

use near_network::state_sync::StateSyncRequestFromNetwork;
use near_network::types::{KnownShardsInfo, StateResponseInfo, StateResponseInfoV1};
use near_primitives::{hash::CryptoHash, syncing::ShardStateSyncResponseV1, types::ShardId};

const POISONED_LOCK_ERR: &str = "The lock was poisoned.";

pub(crate) fn spawn_state_sync_parts_upload() -> anyhow::Result<Option<StatePartsUploadHandle>> {
    let arbiter = actix_rt::Arbiter::new();
    arbiter.handle().stop();
    let sync_jobs_actor_addr = StatePartsUploadActor::start_in_arbiter(
        &arbiter.handle(),
        move |ctx: &mut Context<StatePartsUploadActor>| {
            ctx.set_mailbox_capacity(StatePartsUploadActor::MAILBOX_CAPACITY);
            StatePartsUploadActor {}
        },
    );
    Ok(Some(StatePartsUploadHandle { sync_jobs_actor_addr, arbiter_handle: arbiter.handle() }))
}

pub struct StatePartsUploadHandle {
    pub sync_jobs_actor_addr: Addr<StatePartsUploadActor>,
    arbiter_handle: ArbiterHandle,
}

impl StatePartsUploadHandle {
    pub fn stop(&self) {
        self.arbiter_handle.stop();
    }
}

// Actor Logic

pub struct StatePartsUploadActor {}

impl StatePartsUploadActor {
    pub(crate) const MAILBOX_CAPACITY: usize = 100;
}

impl actix::Actor for StatePartsUploadActor {
    type Context = Context<Self>;
}

/// Response to state request.
#[derive(Message)]
#[rtype(result = "()")]
pub struct StatePartResponse(pub Box<StateResponseInfo>, pub Box<KnownShardsInfo>);


#[derive(Message)]
#[rtype(result = "Option<KnownShardsInfo>")]
pub struct KnownShardsRequest {
    pub sync_hash: CryptoHash,
}

impl Handler<StateSyncRequestFromNetwork> for StatePartsUploadActor {
    type Result = ();

    fn handle(&mut self, msg: StateSyncRequestFromNetwork, _ctx: &mut Context<Self>) {
        match msg {
            StateSyncRequestFromNetwork::KnownStatePartsRequest { request, peer_id } => todo!(),
            StateSyncRequestFromNetwork::StatePartRequest { request, peer_id } => todo!(),
        }
    }
}