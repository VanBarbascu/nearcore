import requests
import argparse

def fetch_validator_data(url, block_height):
    payload = {
        "method": "validators",
        "params": {"block_id": block_height},
        "id": 123,
        "jsonrpc": "2.0"
    }
    
    response = requests.post(url, json=payload)
    
    if response.status_code == 200:
        data = response.json()
        
        # Extract epoch height
        epoch_height = data.get("result", {}).get("epoch_height")
        print(f"Epoch Height: {epoch_height}")
        
        # Extract and display each validator's information
        for validator in data.get("result", {}).get("current_validators", []):
            account_id = validator.get("account_id")
            blocks = (validator.get("num_produced_blocks", 0) / validator.get("num_expected_blocks", 1) * 100) \
                     if validator.get("num_expected_blocks", 0) > 0 else 0
            chunks = (validator.get("num_produced_chunks", 0) / validator.get("num_expected_chunks", 1) * 100) \
                     if validator.get("num_expected_chunks", 0) > 0 else 0
            endorsements = (validator.get("num_produced_endorsements", 0) / validator.get("num_expected_endorsements", 1) * 100) \
                           if validator.get("num_expected_endorsements", 0) > 0 else 0

            # Print the validator information
            print(f"{account_id}, {blocks:.2f}, {chunks:.2f}, {endorsements:.2f}")
    else:
        print("Failed to retrieve data:", response.status_code)

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Fetch validator data for a specified block height from a given URL.")
    parser.add_argument("url", type=str, help="The URL of the NEAR RPC endpoint.")
    parser.add_argument("block_height", type=int, help="The block height to query for validator data.")
    
    args = parser.parse_args()
    fetch_validator_data(args.url, args.block_height)

