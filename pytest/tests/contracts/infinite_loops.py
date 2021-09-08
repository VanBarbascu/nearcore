# Spins up four nodes, deploy an smart contract to one node,
# Call a smart contract method in another node
import sys, time
import base58
import concurrent.futures
import requests

sys.path.append('lib')
from cluster import start_cluster
from transaction import sign_deploy_contract_tx, sign_function_call_tx
from utils import load_test_contract

nodes = start_cluster(
    4, 0, 1, None,
    [["epoch_length", 10], ["block_producer_kickout_threshold", 80],
     ["transaction_validity_period", 10000]], {})

status = nodes[0].get_status()
hash_ = status['sync_info']['latest_block_hash']
hash_ = base58.b58decode(hash_.encode('utf8'))
tx = sign_deploy_contract_tx(nodes[0].signer_key, load_test_contract(), 10,
                             hash_)
nodes[0].send_tx(tx)

time.sleep(3)


# send num_tx function calls from node[i]'s account
def send_transactions(i, num_tx):
    print("Sending Transactions {}".format(i))
    status2 = nodes[i].get_status()
    hash_2 = status2['sync_info']['latest_block_hash']
    hash_2 = base58.b58decode(hash_2.encode('utf8'))
    nonce = 20
    for _ in range(num_tx):
        tx = sign_function_call_tx(nodes[i].signer_key,
                                   nodes[0].signer_key.account_id,
                                   'loop_forever', [], 300000000000, 0, nonce,
                                   hash_2)
        nonce += 1
        res = nodes[i].send_tx_and_wait(tx, 20)
        assert 'result' in res, res
        assert res['result']['status']['Failure']['ActionError']['kind'][
            'FunctionCallError'][
                'ExecutionError'] == 'Exceeded the prepaid gas.', "result: {}".format(
                    res)


with concurrent.futures.ThreadPoolExecutor() as executor:
    futures = []
    for i in range(len(nodes)):
        futures.append(executor.submit(send_transactions, i, 20))
    for future in concurrent.futures.as_completed(futures):
        future.result()
