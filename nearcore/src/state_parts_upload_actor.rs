use actix::{Actor, Addr, Context};
use actix_rt::ArbiterHandle;

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
    Ok(Some(StatePartsUploadHandle {sync_jobs_actor_addr, arbiter_handle: arbiter.handle() }))
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

pub struct StatePartsUploadActor {
}

impl StatePartsUploadActor {
    pub(crate) const MAILBOX_CAPACITY: usize = 100;
}

impl actix::Actor for StatePartsUploadActor {
    type Context = Context<Self>;
}