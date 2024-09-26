import requests

# Replace with your Near Protocol node RPC URL
rpc_url = "https://rpc.mainnet.near.org"

# Initialize sets for visited and backlog peers
visited_peers = set()
backlog_peers = set()

# List to store matching peers
matching_peers = []

# Add initial node to the backlog
backlog_peers.add(rpc_url)

# Function to check peer's status
def check_peer_status(peer_rpc_url):
    try:
        status_payload = {
            "jsonrpc": "2.0",
            "id": "dontcare",
            "method": "status",
            "params": {}
        }
        # Call the status method for the peer with a 0.1 second timeout
        response = requests.post(peer_rpc_url, json=status_payload, timeout=0.2)
        status_data = response.json()
        
        # Extract relevant data
        genesis_hash = status_data['result']['genesis_hash']
        earliest_block_hash = status_data['result']['sync_info']['earliest_block_hash']
        
        # Check if earliest_block_hash equals genesis_hash
        return earliest_block_hash == genesis_hash, status_data['result']['node_public_key']
    except Exception as e:
        print(f"Failed to contact {peer_rpc_url}: {e}")
        return False, None

# Function to get network_info of the peer and extract new active peers
def get_peer_network_info(peer_rpc_url):
    try:
        network_info_payload = {
            "jsonrpc": "2.0",
            "id": "dontcare",
            "method": "network_info",
            "params": {}
        }
        # Call the network_info method for the peer with a 0.1 second timeout
        response = requests.post(peer_rpc_url, json=network_info_payload, timeout=0.2)
        network_info_data = response.json()
        
        # Extract active peers and add new ones to the backlog
        new_peers = []
        active_peers = network_info_data['result']['active_peers']
        for peer in active_peers:
            peer_ip = peer['addr'].split(':')[0]
            peer_rpc_url = f"http://{peer_ip}:3030"  # Assuming RPC port is 3030
            
            # Add peer to backlog if it's not visited
            if peer_rpc_url not in visited_peers and peer_rpc_url not in backlog_peers:
                backlog_peers.add(peer_rpc_url)
                new_peers.append((peer['id'], peer_ip))  # Store both peer_id and IP
        
        return new_peers
    except Exception as e:
        print(f"Failed to retrieve network_info from {peer_rpc_url}: {e}")
        return []

# Main loop to process the backlog of peers
while backlog_peers:
    # Pop a peer from the backlog
    current_peer = backlog_peers.pop()
    
    # Mark peer as visited
    visited_peers.add(current_peer)
    
    print(f"Querying {current_peer}...")

    # Check if this peer's earliest_block_hash matches the genesis_hash
    status_matches, node_public_key = check_peer_status(current_peer)
    
    if status_matches and node_public_key:
        # If the current peer matches, save it in matching_peers
        matching_peers.append(f"{node_public_key}@{current_peer.split('//')[1]}")

    # Get the peer's network_info and find new peers to query
    new_peers = get_peer_network_info(current_peer)
    
    for peer_id, peer_ip in new_peers:
        print(f"Discovered new peer {peer_id}@{peer_ip} from {current_peer}.")

# Final output: print only matching hosts by peer_id@ip
print("\nMatching peers:")
for peer in matching_peers:
    print(peer)

