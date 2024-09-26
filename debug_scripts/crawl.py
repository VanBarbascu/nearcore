import requests

# Replace with your Near Protocol node RPC URL
rpc_url = "https://rpc.mainnet.near.org"

# Request payload for the network_info method
network_info_payload = {
    "jsonrpc": "2.0",
    "id": "dontcare",
    "method": "network_info",
    "params": {}
}

# Get active peers from the network_info API
network_info_response = requests.post(rpc_url, json=network_info_payload)
network_info_data = network_info_response.json()

# Extract active peers
active_peers = network_info_data['result']['active_peers']

# Prepare a map to store peer data
peers_status_map = {}

# Function to check status of each peer
def check_peer_status(peer_rpc_url):
    try:
        status_payload = {
            "jsonrpc": "2.0",
            "id": "dontcare",
            "method": "status",
            "params": {}
        }
        # Call the status method for each peer
        response = requests.post(peer_rpc_url, json=status_payload, timeout=0.1)
        status_data = response.json()
        
        # Extract relevant data
        genesis_hash = status_data['result']['genesis_hash']
        earliest_block_hash = status_data['result']['sync_info']['earliest_block_hash']
        
        # Check if earliest_block_hash equals genesis_hash
        if earliest_block_hash == genesis_hash:
            return True
        else:
            return False
    except Exception as e:
        print(f"Failed to contact {peer_rpc_url}: {e}")
        return False

# Iterate through active peers and call status on each
for peer in active_peers:
    peer_ip = peer['addr'].split(':')[0]
    peer_rpc_url = f"http://{peer_ip}:3030"  # Assuming the RPC port is 3030 for each peer
    
    # Save peer status in the map
    peers_status_map[peer['addr']] = check_peer_status(peer_rpc_url)

# Output results
for peer, status in peers_status_map.items():
    if status:
        print(f"Peer {peer} has earliest_block_hash matching the genesis hash.")
    else:
        print(f"Peer {peer} does not match the genesis hash.")

