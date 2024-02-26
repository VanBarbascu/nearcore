#!/usr/bin/env python3
# Spins up one validating node, one dumping node and one node to monitor the state parts dumped.
# Start all nodes.
# Before the end of epoch stop the dumping node.
# In the next epoch check the number of state sync files reported by the monitor. Should be 0
# Start the dumping node.
# In the following epoch check the number of state sync files reported.
# Should be non 0

import pathlib
import sys
import re
from itertools import islice, takewhile

sys.path.append(str(pathlib.Path(__file__).resolve().parents[2] / 'lib'))

from utils import poll_blocks, poll_epochs
from cluster import init_cluster, spin_up_node, load_config
import state_sync_lib
from configured_logger import logger

EPOCH_LENGTH = 50
NUM_SHARDS = 4


def get_dump_check_metrics(target):
    metrics = target.get_metrics()
    metrics = [line.split(' ')
               for line in str(metrics).strip().split('\\n')
               if re.search('state_sync_dump_check.*{', line)]
    metrics = {metric: int(val) for metric, val in metrics}
    return metrics


def main():
    node_config_dump, node_config = state_sync_lib.get_state_sync_configs_pair(
    )
    config = load_config()
    near_root, node_dirs = init_cluster(1, 2, 4, config,
                                        [["epoch_length", EPOCH_LENGTH]], {
                                            0: node_config,
                                            1: node_config_dump,
                                            2: node_config
                                        })

    boot_node = spin_up_node(config, near_root, node_dirs[0], 0)
    logger.info('Started boot_node')
    dump_node = spin_up_node(config,
                             near_root,
                             node_dirs[1],
                             1,
                             boot_node=boot_node)
    dump_check = spin_up_node(config,
                              near_root,
                              node_dirs[2],
                              2,
                              boot_node=boot_node)
    dump_check.kill()
    chain_id = boot_node.get_status()['chain_id']
    dump_folder = node_config_dump["state_sync"]["dump"]["location"]["Filesystem"]["root_dir"]
    rpc_address, rpc_port = boot_node.rpc_addr()
    local_rpc_address, local_rpc_port = dump_check.rpc_addr()
    cmd = dump_check.get_command_for_subprogram(
        ('state-parts-dump-check',
         '--chain-id',
         chain_id,
         '--root-dir',
         dump_folder,
         'loop-check',
         '--rpc-server-addr',
         f"http://{rpc_address}:{rpc_port}",
         '--prometheus-addr',
         f"{local_rpc_address}:{local_rpc_port}",
         '--interval',
         '2'),
        dump_check.near_root,
        dump_check.node_dir,
        dump_check.binary_name)
    dump_check.run_cmd(cmd=cmd)

    logger.info('Started nodes')

    # Close to the end of the epoch
    takewhile(lambda height: height < EPOCH_LENGTH - 5, poll_blocks(boot_node))
    # kill dumper node so that it can't dump state.
    node_height = dump_node.get_latest_block().height
    logger.info(f'Dump_node is @{node_height}')
    dump_node.kill()
    logger.info(f'Killed dump_node')

    # wait until epoch 1 starts
    takewhile(
        lambda height: height < 1,
        poll_epochs(
            boot_node,
            epoch_length=EPOCH_LENGTH))
    # wait for 5 more blocks
    islice(poll_blocks(boot_node), 5)
    # Check the dumped stats
    metrics = get_dump_check_metrics(dump_check)
    assert sum([val for metric, val in metrics.items() if 'state_sync_dump_check_process_is_up' in metric]
               ) == NUM_SHARDS, f"Dumper process missing for some shards. {metrics}"
    assert sum([val for metric, val in metrics.items() if 'state_sync_dump_check_num_parts_dumped' in metric]
               ) == 0, f"No node was supposed to dump parts. {metrics}"
    assert sum([val for metric, val in metrics.items() if 'state_sync_dump_check_num_header_dumped' in metric]
               ) == 0, f"No node was supposed to dump headers. {metrics}"

    # wait for 10 more blocks.
    islice(poll_blocks(boot_node), 10)
    # Start state dumper node to keep up with the network and dump state next
    # epoch.
    logger.info(f'Starting dump_node')
    dump_node.start(boot_node=boot_node)

    # wait for the next epoch
    takewhile(
        lambda height: height < 2,
        poll_epochs(
            boot_node,
            epoch_length=EPOCH_LENGTH))
    islice(poll_blocks(boot_node), 10)
    # State should have been dumped and reported as dumped.
    metrics = get_dump_check_metrics(dump_check)
    assert sum([val for metric, val in metrics.items() if 'state_sync_dump_check_num_parts_dumped' in metric]
               ) >= NUM_SHARDS, f"Some parts are missing. {metrics}"
    assert sum([val for metric, val in metrics.items() if 'state_sync_dump_check_num_header_dumped' in metric]
               ) == NUM_SHARDS, f"Some headers are missing. {metrics}"


if __name__ == "__main__":
    main()
