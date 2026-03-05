use near_o11y::metrics::{
    IntCounter, IntCounterVec, try_create_int_counter, try_create_int_counter_vec,
};
use std::sync::LazyLock;

pub static TRANSACTIONS_SENT: LazyLock<IntCounterVec> = LazyLock::new(|| {
    try_create_int_counter_vec(
        "near_mirror_transactions_sent",
        "Total number of transactions sent",
        &["status"],
    )
    .unwrap()
});

pub static TRANSACTIONS_INCLUDED: LazyLock<IntCounter> = LazyLock::new(|| {
    try_create_int_counter(
        "near_mirror_transactions_included",
        "Total number of transactions sent that made it on-chain",
    )
    .unwrap()
});

pub static TRANSACTIONS_REQUEUED: LazyLock<IntCounter> = LazyLock::new(|| {
    try_create_int_counter(
        "near_mirror_transactions_requeued",
        "Total number of transactions re-queued due to awaiting nonce",
    )
    .unwrap()
});

pub static NONCES_FORCE_RESOLVED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    try_create_int_counter_vec(
        "near_mirror_nonces_force_resolved",
        "Nonces resolved by actively querying the target chain",
        &["status"],
    )
    .unwrap()
});

pub fn log_summary() {
    let sent_ok = TRANSACTIONS_SENT.with_label_values(&["ok"]).get();
    let sent_invalid = TRANSACTIONS_SENT.with_label_values(&["invalid"]).get();
    let sent_other_error = TRANSACTIONS_SENT.with_label_values(&["other_error"]).get();
    let sent_awaiting = TRANSACTIONS_SENT.with_label_values(&["awaiting_nonce"]).get();
    let included = TRANSACTIONS_INCLUDED.get();
    let requeued = TRANSACTIONS_REQUEUED.get();
    let resolved_ok = NONCES_FORCE_RESOLVED.with_label_values(&["success"]).get();
    let resolved_fail = NONCES_FORCE_RESOLVED.with_label_values(&["failed"]).get();

    tracing::info!(
        target: "mirror",
        sent_ok,
        sent_invalid,
        sent_other_error,
        sent_awaiting,
        included,
        requeued,
        resolved_ok,
        resolved_fail,
        "mirror metrics summary",
    );
}
