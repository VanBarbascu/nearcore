# Forknet traffic from a cloud archive

<!-- cspell:ignore castream forknets coldline -->

Design notes for `mirror run --source-cloud-archive` and the forknet tooling
around it. The user-facing description is in
[tools/mirror/README.md](../../tools/mirror/README.md), section "Source chain
modes".

## Problem

The forknet traffic generator is `neard mirror run --source-home ~/.near/source`
on the traffic VM, replaying mainnet transactions onto the forked chain. Its
source chain is a copy of a mainnet RPC backup (`backup-mainnet-rpc-*`, 1000 GB
disk, 724 GB store) baked into the forknet image at image-creation time. Three
things follow:

- The backup holds about five epochs above its GC tail, so traffic runs out
  after roughly four epochs and the forknet has to be reset.
- The fork height is usually below the backup's tail (castream: fork at
  212641759, first complete epoch in the store at 212644334), so the first
  blocks after the fork are never replayed.
- Every forknet carries a 1 TB disk whose only reader is the mirror.

Goal: the mirror streams its source blocks from the cloud archive bucket as
they are written, so traffic never runs out and the traffic disk goes away.

## Cloud archive recap

Archival nodes with `cloud_archival.writer` upload the chain in 32-block
windows, zstd (level 3), under `chain_id=<id>/archive/`: `metadata/{config,
block_head, shard_head/<shard>}`, `epochs/epoch_id=<id>/epoch_data` (keyed by
the previous epoch's last block), `blocks/batch_id=<n>/data` (headers, blocks,
transactions, receipts), and `shards/shard_id=<s>/batch_id=<n>/data` (chunks,
outcomes, chunk extras, incoming/outgoing receipts, state changes). The writer
uploads a window's block batch and moves `block_head`, then the shard batches
and the shard heads. Once per epoch it also writes a state snapshot
(`epoch_height=<h>/epoch_id=<id>/shard_id=<s>/state_part_*`). Readers rebuild a
local store from the bucket: `cloud-archive bootstrap` walks a height range
(the historical reader), `follow` keeps pulling new windows (the recent reader
loop, polling every `cloud_archival.reader.polling_interval`, 5 s by default)
and saves a reader head once a whole window is in. The reader store carries
blocks, the height index, chunks, transactions, receipts, outcomes, chunk
extras and epoch light client blocks; State is reconstructed from the epoch
snapshot plus the per-batch state changes, unless `--skip-state` leaves it out.

## Design

### (a) `mirror run --source-cloud-archive` (tools/mirror/src/cloud_archive.rs)

`--source-home` is a cloud archive reader home (`archive: true`, `AllShards`,
`cloud_archival.location` and `.reader`). The mirror opens its store once with
`nearcore::open_storage`, which also builds the bucket client, and shares the
one `Store` handle between the embedded recent reader loop (a tokio task) and
its own `offline::ChainAccess`, built with `from_parts` on that handle. Nothing
else may open the store (RocksDB lock), which is why the reader is embedded
rather than run as a second process.

- Bootstrap. The store opens where the mirror goes on: one above the last
  source height in the mirror DB, or the target chain's genesis height on a
  first run. A home without a reader head is bootstrapped by `bootstrap_range`
  from there up `SOURCE_READ_AHEAD` (3200) blocks, or to the bucket head if that
  is lower. If the bucket has no block below that height, a binary search over
  the batch windows finds the first archived block and the bootstrap opens one
  above it; the mirror skips the gap on its own. A missing blob and a failed
  read are the same error to the bucket client, so the search reads a window
  three times before it counts it as missing. A bucket whose head is below the
  start is polled until it gets there. Failed bucket reads are retried a
  polling interval apart, up to five in a row. `bootstrap_range` saves the
  reader head after every window, so a bootstrap that fails part way (the
  writer moves `block_head` before the shard batches, so the top windows can
  lack a shard batch for a while) is taken on by the reader loop, which waits
  for each shard head. A home with a reader head (earlier run, or a manual
  `cloud-archive bootstrap`) is followed from it, unless that head is more than
  `SOURCE_READ_AHEAD` below the start: `open` then fails and asks for the
  store to be removed, instead of copying everything in between.
- Read-ahead bound. The reader shares a `PullLimit` with the mirror's
  `ChainAccess` and opens no window while its head is at or above it. The
  mirror raises the limit to `height + SOURCE_READ_AHEAD` on every
  `get_next_block_height(height)` and in `init`. Without it the reader copies
  everything up to the bucket head: for a fork at 220587532 on the mainnet
  bucket (head 224.98M on 2026-11-17) that is about 4.4M blocks, about 54 hours
  of bootstrap and 1.5 TB of reader store before the first transaction.
- Head gating. The recent reader writes a window's block rows and height
  index before its shard batches, and moves the reader head, and the chain
  heads with it (`save_reader_position`), only after both. The height index
  above the reader head can therefore name blocks whose chunks are not in yet:
  `get_next_block_height` answers `Unknown` above the reader head
  (`ChainAccess::complete_head`), so the mirror never queues such a height.
- First block. `init` takes the first height whose block has its new chunks in
  the store. The block a bootstrap anchors on has only its header and block
  rows, since the bootstrap copies what lies above it, so a store opened on the
  bucket's first archived block does not hand the mirror an empty block.
- Waiting at the head. `init` re-reads the head every 500 ms instead of
  returning the end of the chain, logs one "waiting for the source store"
  line, and bails out when the shutdown token is cancelled. Without
  `--stop-height` the run never ends on its own. With it, the blocks before the
  stop height still get the create-account lookahead
  (`ChainAccess::source_grows`), since the bucket goes on past it.
- Shutdown. SIGINT/SIGTERM cancels the token: the mirror drains sent
  transactions (30 s cap), then `ReaderFollower::stop` stops the reader loop
  once (its interrupt handle asserts against a second stop), aborts and awaits
  the monitor task so its `Store` clone is gone before the caller's, and
  reports the loop's result. A reader loop failure (its startup checks)
  cancels the token itself so the mirror stops; the reader's error is then the
  one `run` returns and the mirror's own is logged.
- Metrics. `near_mirror_source_reader_head` and
  `near_mirror_source_bucket_head` gauges; a progress line every 16 batches.

### (b) `--source-skip-state` and the default-extra-key fallback

A bucket without state snapshots (one filled by `cloud-archive backfill`, a
separate change) is bootstrapped and followed without State (`CloudArchivalRecentReader::with_skip_state` skips the
state anchors and the per-batch state application). The mirror's only trie
query is `get_full_access_keys`, used by `push_extra_tx` to pick the signer of
the extra `AddKey`/`CreateAccount` transactions it sends for keys and accounts
created by function calls. The offline `ChainAccess` opened with
`--source-skip-state` maps the `QueryError::InternalError` a trie
`StorageError` produces to `ChainError::NoState`, and `push_extra_tx` treats
that variant like the no-keys case: a warning and the default extra key. On any
other source (online, or an offline store that is supposed to hold state) the
same error aborts the run as it always did. What is lost: those extra transactions are
signed with the default extra key rather than a key mapped from the account's
own, so where that key is not valid for the account the function-call-created
keys are not mirrored (4 warnings in the first 29 minutes on castream). With a
bucket written by the real writer fleet the snapshots are there, the flag is
dropped and the fallback is never taken.

### (c) Runner and tooling

- `mirror.py start-traffic --source-cloud-archive --source-home <path>
  [--source-skip-state]` reaches the neard-runner's `start` as `StartParams`;
  the runner keeps them as `traffic_source` in `data.json` and turns them into
  the mirror flags. A start without `--source-*` flags reuses the saved source;
  one with any of them replaces it all (a change forces a restart).
  `--source-rpc-backup` goes back to `~/.near/source`, the default. Both
  `mirror.py` and the runner reject `--source-skip-state` without
  `--source-cloud-archive`, and `--source-cloud-archive` without
  `--source-home`, which `neard mirror run` would fail on.
- `forknet_scenarios/test_cloud_archive_stream.py` (`TestCloudArchiveStream`):
  10 CP, all hosts in europe-west4 (the bucket's region), epoch 14500, fork at
  220587532, no archival or state dumper. The binary comes from
  `--neard-binary-url` and has to have `--source-cloud-archive`.

## A forknet run with the new mode

1. The writer fleet (or `cloud-archive backfill`) fills the bucket from the
   fork height up.
2. The traffic VM gets a reader home (`genesis.json`, `node_key.json`,
   `config.json` naming the bucket) and a service account or key file that can
   read it.
3. `mirror.py start-traffic --source-cloud-archive --source-skip-state
   --source-home <reader home>`; the runner starts `neard mirror run` with the
   flags.
4. The mirror opens the reader store, bootstraps it from where the mirror goes
   on (the target genesis height on a first run, or the first archived block)
   up the read-ahead, starts the reader loop, and the in-process target node
   syncs the forked chain.
5. The mirror sends each source block's transactions, one source block per
   target block, up to the reader head; the reader pulls new windows as the
   mirror moves the pull limit up, and as the bucket head moves once the
   mirror is near it; the mirror waits at the head when it catches up.
6. `stop-traffic` or a restart: drain, stop the reader once, close the store.
   A later start follows from the saved reader head without a bootstrap.

## Verified on the castream forknet (2026-10-02)

Bucket `gs://nearone-mocknet-cloud-archive-castream`, filled by
`cloud-archive backfill` from the traffic host's RPC backup (3000 blocks, then
extended twice, by 6000 and 3000). Mirror runs with
`--source-cloud-archive --source-skip-state` on a fresh reader home:

- Bootstrap of 94 batches (212644335..212647333) in 68 s, about 1.4 s per
  batch; the reader store was 963 MB.
- Steady state: about 1.7 source blocks/s, 455-510 transactions sent per
  minute (480/min is the reference), 99.3-99.95 % of the sent ones included.
- Waiting at the head for two hours: reader head = bucket head, counters flat.
- Pick-up after a bucket extension: first new block sent 0.8 s after the
  heads moved, 5-8 s on the third extension (5 s poll plus the window);
  the reader pulls about 23-25 blocks/s.
- Restart on the same home: no bootstrap, resumed from the mirror db height;
  SIGINT drains and exits with the stores closed, 7 s when nothing is pending
  and 30 s (the drain cap) when stuck re-forwarded transactions never land.
- Bugs found by the run, fixed and folded into the series: the mirror read
  each new window before its chunks landed, `ChunkMissing` on every shard and about
  100 blocks mirrored without transactions (now it reads up to the reader
  head, verified at the catch-up point of the third extension: 0
  `ChunkMissing`); a double reader stop panicked SIGINT in dev-release.

## Checked against the mainnet bucket (2026-11-17)

`gs://mainnet-nearone-cloud-archive` (writer fleet on neard 2.14.1, live since
2026-10-16): block and shard batches for all 10 shards from 220204320 with no
gaps, state snapshots every 20 epochs. Reader home
with the production reader's config (`archive`, `save_trie_changes`,
`AllShards`, `cloud_archival.location.GCS.bucket`, `cloud_archival.reader = {}`)
and `--source-skip-state`, fork height 220587532 (image
`mirror-*-mainnet-220587532`):

- Bootstrap of 3200 blocks (220587532..220590751) in 68 s, then the reader
  stopped at the limit with the bucket head at 224987839.
- 6500 blocks read through `CloudArchiveSource::open` and the mirror's
  `ChainAccess`, the way the chain tracker reads them: 10 chunks in every
  block, 19663 transactions, 122270 receipts, 12517 sampled transaction
  outcomes all found. The reader pulled about 15 blocks/s from outside
  europe-west4.
- With the mirror idle and the reader head above the limit, the reader pulled
  nothing for 90 s.
- Reader store: 1.9 GB for 9396 blocks, about 200 KB per block.

## Run on a 10-CP forknet (2026-11-18)

`TestCloudArchiveStream` in europe-west4 (10 chunk producers on
n2d-standard-32), fork at 220587532, traffic binary built from this branch,
reader home with the production reader's config, bucket read as
`mocknet-traffic`:

- Bootstrap of 3200 blocks in 75 s. The reader then stayed about 3.4k blocks
  above the mirror.
- Default batch interval: 8.2 transactions/s sent and included, 2.1 source
  blocks/s, the forknet at 2.2 blocks/s. No error lines.
- Stepping `--batch-interval-millis` down:

  | interval | sent/s | included/s | source blocks/s | forknet blocks/s |
  |----------|--------|------------|-----------------|------------------|
  | 200 ms   | 12.5   | 12.6       | 4.9             | 2.3              |
  | 100 ms   | 26.7   | 24.0       | 9.7             | 1.0 (one sample) |
  | 50 ms    | 63.3   | 63.2       | 18.9            | 2.3              |
  | 20 ms    | 100.5  | 99.9       | 25.4            | 2.28 (min 1.7)   |

  At 20 ms the mirror ran 50-90 blocks under the reader head: the reader's
  pull rate, about 25 blocks/s, is the cap, not the forknet. Chunks were about
  2 % full of gas (the busiest at 109 of 1000 Tgas).

## Cost and sizing

About 75 KB per archived mainnet block (late 2026): a 1.9 MB block batch and
0.4-0.6 MB of shard batches per 32-block window, 11 objects. Objects move to
Coldline after a day, so a forknet pays $0.02 per GiB retrieved and $0.01 per
1000 GETs: about $5 per million source blocks ($1.5 retrieval, $3.4 GETs),
about $1 per day at the default batch interval and about $11 at 25 blocks/s.
The bucket has no requester-pays, so its project is billed. Egress to a VM in
the bucket's region (europe-west4) is free. The reader store grows about
190 KB per block (4.7 GB at 24.5k blocks), against the 724 GB backup store.

## Limitations and follow-ups

- The mainnet bucket has state snapshots only every 20 epochs, and a reader
  with state would load every shard's full state and replay up to 20 epochs of
  changes. Forknets therefore keep `--source-skip-state` and the
  default-extra-key fallback.
- The recent reader has TODOs for shards a resharding adds above its head.
- `near_mirror_source_bucket_head` is refreshed on every poll of the monitor
  (one small GET per polling interval); the progress line keeps its cadence.
- While waiting at the head the mirror re-forwards stuck transactions every
  5 s (`intents-relayer.near`, `NotEnoughBalance`): pre-existing, noisy.
- The traffic VM reads the bucket as `mocknet-traffic@mocknet-nearone`, which
  has `objectViewer` on the mainnet bucket (infra-ops). The mainnet reader SA
  cannot be attached: `mainnet-nearone` enforces
  `iam.disableCrossProjectServiceAccountUsage`.
- Image creation can drop the traffic disk and the traffic stage once forknets
  default to the bucket.
- The writer moves `block_head` before the shard batches; a reader that gates
  on the bucket head alone sees the same race the mirror hit on the reader
  store. Either the writer writes shard batches first, or readers gate on
  their own reader head, which the mirror now does.
- The reader store is never pruned. The read-ahead bounds how far it runs
  ahead of the mirror, not what it keeps behind: at about 190 KB per block it
  grows about 35 GB per day at the default batch interval and about 400 GB
  per day at 25 blocks/s. Until there is a GC of the heights the mirror has
  sent, removing the reader home's `data` directory and restarting the traffic
  bootstraps it again at the mirror's height.
- The recent reader pulls about 25 blocks/s: per 32-block window it reads the
  block head, the block batch, each shard's head (each a listing) and each
  shard batch, one after another. Fetching the shard heads and batches in
  parallel, and the next window while the current one is written, would lift
  the cap that the TPS sweep above hit.
- The reader loop runs as a task on the mirror's tokio runtime and does its
  blob decompression and RocksDB writes there. `cloud-archive follow` runs it
  on a runtime of its own, which the mirror could do too.
- `CloudArchiveSource::open` repeats the reader-home setup of
  `ReaderHandles::open` in tools/cloud-archive and of `offline::ChainAccess::new`;
  a shared helper would keep them from drifting.
- A home whose reader head is above the mirror's start but which lacks the
  blocks below it (one bootstrapped for a higher fork) is not detected: the
  mirror skips the heights the store does not hold.

## Reviewer guide

Commits, oldest first:

1. feat(mirror): let the recent reader follow a store bootstrapped without
   state
2. feat(mirror): fall back to the default extra key when the source store has
   no state
3. feat(mirror): stream the source chain from a cloud archive bucket
4. feat(mocknet): start the traffic generator from a cloud archive source
5. feat(mocknet): forknet scenario for the cloud archive traffic source
6. docs(cloud-archive): design note for the forknet cloud archive traffic
   source

Start with `tools/mirror/src/cloud_archive.rs` (open, bootstrap, follower),
then the `offline.rs` diff (`from_parts`, `complete_head`, the wait loop in
`init`), then the `lib.rs` fallback and the runner changes in
`neard_runner.py`.
