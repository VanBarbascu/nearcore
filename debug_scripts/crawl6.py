import requests

# Load the visited IPs from a file
visited_ips_file = "visited_ips.txt"
ips_to_query = []

# Read IPs from the file
with open(visited_ips_file, "r") as f:
    ips_to_query = [line.strip() for line in f.readlines()]

# Function to query the status for a given IP and extract earliest_block_height
def query_status(ip):
    peer_rpc_url = f"http://{ip}"  # Assuming RPC port is 3030
    print (peer_rpc_url)
    try:
        status_payload = {
            "jsonrpc": "2.0",
            "id": "dontcare",
            "method": "status",
            "params": {}
        }
        # Query the status endpoint
        response = requests.post(peer_rpc_url, json=status_payload, timeout=0.2)
        status_data = response.json()

        # Extract earliest_block_height
        earliest_block_height = status_data['result']['sync_info']['earliest_block_height']

        return ip, earliest_block_height
    except Exception as e:
        # Return None if the query fails
        return None

# Query status for each IP and save results
output_file = "ip_earliest_block_heights.txt"
with open(output_file, "w") as f:
    for ip in ips_to_query:
        result = query_status(ip)
        if result:
            ip, earliest_block_height = result
            # Save the IP and earliest_block_height to the file
            f.write(f"{ip}, {earliest_block_height}\n")
            print(f"Queried {ip}: earliest_block_height = {earliest_block_height}")

print(f"Results saved to {output_file}.")

