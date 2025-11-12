#!/usr/bin/env python3
import requests
import json
from datetime import datetime

BLOCKS_PER_EPOCH = 43200
EPOCH_JUMP = 10  # Jump back by this many epochs at a time

def get_block(url, block_id=None, height=None):
    """Get block by ID, height, or latest finalized block"""
    payload = {
        "jsonrpc": "2.0",
        "id": "dontcare",
        "method": "block",
    }
    
    if height is not None:
        payload["params"] = {"block_id": height}
    elif block_id:
        payload["params"] = {"block_id": block_id}
    else:
        payload["params"] = {"finality": "final"}
    
    response = requests.post(url, json=payload)
    result = response.json()
    if "error" in result:
        print(f"  Error: {result['error']}")
        return None
    return result["result"]

def get_protocol_config(url, block_id=None, height=None):
    """Get protocol config at a specific block"""
    payload = {
        "jsonrpc": "2.0",
        "id": "dontcare",
        "method": "EXPERIMENTAL_protocol_config",
    }
    
    if height is not None:
        payload["params"] = {"block_id": height}
    elif block_id:
        payload["params"] = {"block_id": block_id}
    else:
        payload["params"] = {"finality": "final"}
    
    try:
        response = requests.post(url, json=payload)
        result = response.json()
        if "error" in result:
            return None
        return result["result"]
    except Exception as e:
        return None

def get_epoch_start_from_height(url, height):
    """Get the first block of the epoch containing the given height"""
    block = get_block(url, height=height)
    if not block:
        return None
    
    # Get the first block of this epoch using next_epoch_id
    epoch_start_hash = block["header"]["next_epoch_id"]
    epoch_start_block = get_block(url, block_id=epoch_start_hash)
    return epoch_start_block

def find_first_epoch_with_pv_fast(url, start_height, target_pv, epoch_jump):
    """
    Efficiently find the first epoch with target_pv by jumping backwards.
    Returns (epoch_id, height, timestamp, pv) of the first epoch with target_pv.
    """
    print(f"    Fast search for first epoch with PV {target_pv} starting from height {start_height}...")
    
    # First, jump backwards by epoch_jump epochs at a time
    current_height = start_height
    last_known_height_with_target_pv = start_height
    
    jump_iteration = 0
    while jump_iteration < 1000:  # Safety limit
        # Jump back
        jump_back_blocks = BLOCKS_PER_EPOCH * epoch_jump
        target_height = current_height - jump_back_blocks
        
        if target_height < 0:
            print(f"      Would jump to negative height, stopping jumps")
            break
        
        # Get epoch at target height
        epoch_start = get_epoch_start_from_height(url, target_height)
        if not epoch_start:
            print(f"      Could not get epoch at height {target_height}")
            break
        
        epoch_height = epoch_start["header"]["height"]
        pv_config = get_protocol_config(url, height=epoch_height)
        if not pv_config:
            print(f"      Could not get protocol config at height {epoch_height}")
            break
        
        pv = pv_config["protocol_version"]
        print(f"      Jumped to epoch at height {epoch_height}: PV {pv}")
        
        if pv == target_pv:
            # Still have target PV, keep jumping back
            last_known_height_with_target_pv = epoch_height
            current_height = epoch_height
            jump_iteration += 1
        elif pv < target_pv:
            # Found lower PV! Now we need to search epoch-by-epoch between 
            # epoch_height and last_known_height_with_target_pv
            print(f"      Found PV {pv} < {target_pv}, now searching epoch-by-epoch...")
            return find_exact_boundary(url, epoch_height, last_known_height_with_target_pv, target_pv)
        else:
            # pv > target_pv shouldn't happen when going backwards
            print(f"      Unexpected PV {pv} > {target_pv}, stopping")
            break
    
    # If we get here, we either hit genesis or ran out of jumps
    # Do epoch-by-epoch search from current position
    print(f"      Reached jump limit or genesis, searching epoch-by-epoch from height {current_height}...")
    return find_exact_boundary(url, 0, last_known_height_with_target_pv, target_pv)

def find_exact_boundary(url, low_height, high_height, target_pv):
    """
    Search epoch-by-epoch from high_height backwards to find first epoch with target_pv.
    low_height has PV < target_pv (or is 0/genesis).
    high_height has PV == target_pv.
    """
    current_height = high_height
    first_epoch_with_pv = None
    
    for iteration in range(20):  # Max 20 epochs to check
        epoch_start = get_epoch_start_from_height(url, current_height)
        if not epoch_start:
            break
        
        epoch_height = epoch_start["header"]["height"]
        epoch_id = epoch_start["header"]["epoch_id"]
        timestamp = epoch_start["header"]["timestamp"]
        
        # Don't go below low_height
        if epoch_height <= low_height:
            break
        
        pv_config = get_protocol_config(url, height=epoch_height)
        if not pv_config:
            break
        
        pv = pv_config["protocol_version"]
        print(f"        Checking epoch at height {epoch_height}: PV {pv}")
        
        if pv == target_pv:
            # Still in target PV
            first_epoch_with_pv = (epoch_id, epoch_height, timestamp, pv)
            # Go back one more epoch
            if epoch_height <= BLOCKS_PER_EPOCH:
                print(f"        ✓ Reached genesis")
                break
            current_height = epoch_height - 1
        elif pv < target_pv:
            # Found the boundary!
            print(f"        ✓ Found boundary: PV {pv} < {target_pv}")
            break
        else:
            print(f"        Unexpected: PV {pv} > {target_pv}")
            current_height = epoch_height - 1
    
    return first_epoch_with_pv

def find_all_protocol_version_starts(url, max_pvs=None, epoch_jump=10):
    """Find the starting epoch for each protocol version by working backwards"""
    
    print("Finding the starting epoch for each protocol version...\n")
    print(f"Using efficient jumping: {epoch_jump} epochs at a time\n")
    print("Note: Protocol version only increases with time (monotonic)\n")
    
    # Get current block and protocol version
    current_block = get_block(url)
    if not current_block:
        print("Error: Could not fetch current block")
        return
    
    current_height = current_block["header"]["height"]
    current_protocol_config = get_protocol_config(url, height=current_height)
    if not current_protocol_config:
        print("Error: Could not fetch current protocol config")
        return
    
    current_pv = current_protocol_config["protocol_version"]
    print(f"Starting from height {current_height}, Protocol version: {current_pv}\n")
    
    pv_starts = []  # List of (pv, epoch_id, height, timestamp)
    search_height = current_height
    target_pv = current_pv
    
    pv_count = 0
    
    while True:
        if max_pvs and pv_count >= max_pvs:
            print(f"\nReached max protocol versions limit ({max_pvs})")
            break
        
        print(f"\n{'='*80}")
        print(f"[PV {target_pv}] Searching for first epoch with this protocol version...")
        print(f"{'='*80}")
        
        # Find the first epoch with target_pv using fast jumping
        pv_start = find_first_epoch_with_pv_fast(url, search_height, target_pv, epoch_jump)
        
        if not pv_start:
            print(f"  Could not find start of PV {target_pv}")
            break
        
        epoch_id, epoch_height, timestamp, pv = pv_start
        pv_starts.append((pv, epoch_id, epoch_height, timestamp))
        
        timestamp_seconds = int(timestamp) / 1e9
        dt = datetime.fromtimestamp(timestamp_seconds)
        print(f"\n  ✓✓✓ PV {pv} STARTS at epoch height {epoch_height} on {dt}")
        
        # Check if we're at genesis
        if epoch_height <= BLOCKS_PER_EPOCH:
            print(f"\n{'='*80}")
            print(f"✓ Reached genesis epoch")
            print(f"{'='*80}")
            break
        
        # Go back to previous epoch and get its protocol version
        prev_height = epoch_height - 1
        print(f"  → Getting previous epoch (from height {prev_height})...")
        
        prev_epoch_start = get_epoch_start_from_height(url, prev_height)
        if not prev_epoch_start:
            print(f"  Could not get previous epoch start from height {prev_height}")
            break
        
        prev_epoch_height = prev_epoch_start["header"]["height"]
        prev_pv_config = get_protocol_config(url, height=prev_epoch_height)
        
        if not prev_pv_config:
            print(f"  Could not get protocol config at height {prev_epoch_height}")
            break
        
        prev_pv = prev_pv_config["protocol_version"]
        print(f"  → Previous epoch at height {prev_epoch_height} has PV {prev_pv}")
        
        # Since PV only increases, prev_pv MUST be < target_pv
        if prev_pv >= target_pv:
            print(f"  ⚠ Error: Previous PV {prev_pv} >= current PV {target_pv} (violates monotonicity!)")
            break
        
        if prev_pv == target_pv - 1:
            print(f"  → Moving to PV {prev_pv} (sequential)")
        else:
            print(f"  → Moving to PV {prev_pv} (jumped from PV {target_pv}, skipped {target_pv - prev_pv - 1} versions)")
        
        # Update for next iteration - search from the previous epoch
        search_height = prev_epoch_height
        target_pv = prev_pv
        pv_count += 1
    
    # Print summary
    print("\n" + "=" * 170)
    print(f"\n📋 SUMMARY: Found {len(pv_starts)} protocol version starts\n")
    print(f"{'Protocol Ver':<14} {'Epoch ID':<66} {'Start Height':<15} {'Timestamp'}")
    print("=" * 170)
    
    # Print in chronological order (oldest first)
    for pv, epoch_id, height, timestamp in reversed(pv_starts):
        timestamp_seconds = int(timestamp) / 1e9
        dt = datetime.fromtimestamp(timestamp_seconds)
        print(f"{pv:<14} {epoch_id:<66} {height:<15} {dt}")
    
    print("=" * 170)

if __name__ == "__main__":
    import argparse
    
    parser = argparse.ArgumentParser(
        description="Find the starting epoch for each NEAR protocol version (efficient with jumping)"
    )
    parser.add_argument("--url", default="http://localhost:3030", help="RPC URL")
    parser.add_argument("--max_pvs", type=int, help="Maximum number of protocol versions to search")
    parser.add_argument("--chain_id", choices=["mainnet", "testnet"], 
                       help="Use mainnet or testnet archival RPC")
    parser.add_argument("--epoch_jump", type=int, default=10,
                       help="Number of epochs to jump back at a time (default: 10)")
    
    args = parser.parse_args()
    
    if args.chain_id == "mainnet":
        url = "https://archival-rpc.mainnet.near.org"
    elif args.chain_id == "testnet":
        url = "https://archival-rpc.testnet.near.org"
    else:
        url = args.url
    
    find_all_protocol_version_starts(url, args.max_pvs, args.epoch_jump)
