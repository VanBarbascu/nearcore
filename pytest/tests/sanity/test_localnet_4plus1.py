#!/usr/bin/env python3
# Creates a localnet with 4 active validators (test0-test3) and 1 non-validator (test4)
# Node 4 is NOT started initially - it only starts before staking
# Stakes for the non-validator (test4), ensures it becomes a validator
# This setup is useful for testing validator staking mechanics and epoch transitions

import sys, time, pathlib

sys.path.append(str(pathlib.Path(__file__).resolve().parents[2] / 'lib'))

from cluster import init_cluster, spin_up_node, load_config
from configured_logger import logger
from transaction import sign_staking_tx
import utils

TIMEOUT = 300  # Increased timeout for state sync + staking
EPOCHS_TO_WAIT = 3
EPOCH_LENGTH = 20
TARGET_HEIGHT = EPOCHS_TO_WAIT * EPOCH_LENGTH

# Initialize cluster with 4 validators + 1 observer (test4)
config = load_config()
near_root, node_dirs = init_cluster(
    4, 1, 1, config,
    [["epoch_length", EPOCH_LENGTH], ["block_producer_kickout_threshold", 40]], {
        0: {"tracked_shards_config": "NoShards", "consensus": {"block_fetch_horizon": 5}},
        1: {"tracked_shards_config": "NoShards", "consensus": {"block_fetch_horizon": 5}},
        2: {"tracked_shards_config": "NoShards", "consensus": {"block_fetch_horizon": 5}},
        3: {"tracked_shards_config": "NoShards", "consensus": {"block_fetch_horizon": 5}},
        4: {"tracked_shards_config": "NoShards", "consensus": {"block_fetch_horizon": 5}},
    })

logger.info("=" * 80)
logger.info("Localnet Setup: 4 Active Validators + 1 Non-Validator")
logger.info("=" * 80)
logger.info("Genesis created with 5 accounts: test0-test4")
logger.info("Starting only nodes 0-3 (validators), node 4 will start later")
logger.info("")
logger.info("Node directories:")
for i, node_dir in enumerate(node_dirs):
    logger.info(f"  node{i} (test{i}): {node_dir}")
logger.info("=" * 80)

# Start only nodes 0-3 (NOT node 4)
nodes = []

# Start node 0 as boot node
logger.info("Starting node 0 as BOOT NODE")
boot_node = spin_up_node(config, near_root, node_dirs[0], ordinal=0, boot_node=None, single_node=False)
nodes.append(boot_node)

# Start nodes 1-3
for i in range(1, 4):
    logger.info(f"Starting node {i}")
    node = spin_up_node(config, near_root, node_dirs[i], ordinal=i, boot_node=boot_node, single_node=False)
    nodes.append(node)

# Reserve slot for node 4 (will be started later)
nodes.append(None)

logger.info("")
logger.info("Initial 4 nodes started. Node 4 (test4) NOT started yet.")
logger.info("")

def get_validators():
    """Returns the set of current validator account IDs"""
    return set([x['account_id'] for x in nodes[0].get_status()['validators']])

def get_validator_info():
    """Get detailed validator information from RPC"""
    return nodes[0].json_rpc('validators', [None])

# Wait for 3 epochs (60 blocks with epoch_length=20)
# This ensures we're past block_fetch_horizon so node 4 will need to state sync
logger.info(f"Waiting for {EPOCHS_TO_WAIT} epochs ({TARGET_HEIGHT} blocks) before starting node 4...")
logger.info("This ensures node 4 will sync via headers, not full state download")

for height, _ in utils.poll_blocks(nodes[0], timeout=TIMEOUT):
    if height >= TARGET_HEIGHT:
        logger.info(f"✓ Reached height {height}")
        break

# NOW start node 4 (before sending stake transaction)
logger.info("")
logger.info("=" * 80)
logger.info("STARTING NODE 4 (test4) - it will sync headers only")
logger.info("=" * 80)
node4 = spin_up_node(config, near_root, node_dirs[4], ordinal=4, boot_node=boot_node, single_node=False)
nodes[4] = node4
logger.info(f"Node 4 started, waiting for it to sync...")
time.sleep(5)  # Give it a moment to start syncing

# Check initial validator state
initial_validators = get_validators()
logger.info(f"Current validators: {sorted(initial_validators)}")
assert 'test4' not in initial_validators, "test4 should not be a validator initially"

# Get initial validator info from RPC
validator_info_before = get_validator_info()
logger.info(f"Validator info before staking:")
logger.info(f"  Current validators: {validator_info_before['result']['current_validators']}")
logger.info(f"  Next validators: {validator_info_before['result']['next_validators']}")

# Get latest block hash for transaction
hash_ = nodes[4].get_latest_block().hash_bytes

# Send stake transaction from test4 (node index 4)
stake_amount = 100000000000000000000000000000000  # 100M NEAR
logger.info("")
logger.info("=" * 80)
logger.info(f"SENDING STAKE TRANSACTION for test4: {stake_amount} yoctoNEAR")
logger.info("=" * 80)

# Get nonce from access key
try:
    access_key_response = nodes[0].get_access_key_list("test4")
    nonce = 0
    for key in access_key_response['result']['keys']:
        nonce = max(nonce, key['access_key']['nonce'])
    current_nonce = nonce + 1
    logger.info(f"Using nonce {current_nonce} for test4 stake transaction")
except Exception as e:
    # Fallback to nonce 1 if we can't get it
    logger.warning(f"Could not get nonce from access key: {e}, using nonce 1")
    current_nonce = 1

tx = sign_staking_tx(
    nodes[4].signer_key,     # test4's signer key
    nodes[4].validator_key,  # test4's validator key
    stake_amount,
    current_nonce,           # Use correct nonce
    hash_
)
tx_result = nodes[0].send_tx(tx)
logger.info(f"Stake transaction sent: {tx_result}")

# Wait a few blocks for the transaction to be processed
logger.info("Waiting for transaction to be included in a block...")
time.sleep(5)

# Check validator info after staking
validator_info_after = get_validator_info()
logger.info(f"Validator info after staking:")
logger.info(f"  Current proposals: {[v['account_id'] for v in validator_info_after['result']['current_proposals']]}")

# Check if test4 is in proposals
test4_in_proposals = any(v['account_id'] == 'test4' for v in validator_info_after['result']['current_proposals'])
if test4_in_proposals:
    logger.info("✓ test4 found in current_proposals - stake transaction was accepted!")
else:
    logger.warning("⚠️  test4 NOT found in current_proposals - checking if already in next_validators")
    test4_in_next = any(v['account_id'] == 'test4' for v in validator_info_after['result']['next_validators'])
    if test4_in_next:
        logger.info("✓ test4 found in next_validators")
    else:
        logger.error("❌ test4 NOT found in proposals or next_validators - stake transaction may have failed")

# Wait for test4 to become a validator
logger.info("")
logger.info("=" * 80)
logger.info("Waiting for test4 to join the active validator set...")
logger.info("=" * 80)

last_log_height = 0
for height, _ in utils.poll_blocks(nodes[0], timeout=TIMEOUT):
    # Log progress every epoch
    if height - last_log_height >= EPOCH_LENGTH:
        current_validators = get_validators()
        validator_info = get_validator_info()
        proposals = [v['account_id'] for v in validator_info['result']['current_proposals']]
        next_vals = [v['account_id'] for v in validator_info['result']['next_validators']]

        logger.info(f"Height {height}:")
        logger.info(f"  Current validators: {sorted(current_validators)}")
        logger.info(f"  Proposals: {proposals}")
        logger.info(f"  Next validators: {next_vals}")
        last_log_height = height

    if 'test4' in get_validators():
        logger.info("")
        logger.info("=" * 80)
        logger.info(f"✓✓✓ SUCCESS! test4 joined validator set at height {height} ✓✓✓")
        logger.info("=" * 80)

        # Wait a few more blocks to give test4 a chance to produce
        logger.info("Waiting a few more blocks for test4 to start producing...")
        time.sleep(10)
        height = nodes[0].get_latest_block().height

        final_validators = get_validators()
        logger.info(f"Final validators: {sorted(final_validators)}")

        final_validator_info = get_validator_info()
        logger.info(f"Final validator info:")
        logger.info(f"  Current validators: {[v['account_id'] for v in final_validator_info['result']['current_validators']]}")

        # Find test4 in the validator info and check if it's producing
        test4_info = None
        for v in final_validator_info['result']['current_validators']:
            if v['account_id'] == 'test4':
                test4_info = v
                break

        if test4_info:
            logger.info(f"")
            logger.info(f"test4 production stats:")
            logger.info(f"  Blocks: {test4_info['num_produced_blocks']}/{test4_info['num_expected_blocks']}")
            logger.info(f"  Chunks: {test4_info['num_produced_chunks']}/{test4_info['num_expected_chunks']}")
            logger.info(f"  Endorsements: {test4_info['num_produced_endorsements']}/{test4_info['num_expected_endorsements']}")

            if test4_info['num_produced_blocks'] > 0:
                logger.info(f"✓ test4 is producing blocks!")
            else:
                logger.warning(f"⚠️ test4 has NOT produced any blocks yet (may need more time)")

            if test4_info['num_produced_chunks'] > 0:
                logger.info(f"✓ test4 is producing chunks!")
            else:
                logger.warning(f"⚠️ test4 has NOT produced any chunks yet")
        else:
            logger.error(f"❌ Could not find test4 in validator info")

        break
else:
    logger.error("❌ TIMEOUT: test4 did not join the validator set")
    sys.exit(1)

logger.info("")
logger.info("Test completed successfully!")
