LD_NEARD="https://s3-us-west-1.amazonaws.com/build.nearprotocol.com/nearcore/Linux1.38.x_with_genesis_init_fix/neard"
NEW_NEARD="https://s3-us-west-1.amazonaws.com/build.nearprotocol.com/nearcore/Linux1.38.x_with_genesis_init_fix/neard"
python3 tests/mocknet/mirror.py \
            --chain-id mainnet \
            --start-height 114882709 \
            --unique-id razvan-114882709 \
    	    $1 \
            --neard-binary-url $OLD_NEARD \
            --neard-upgrade-binary-url $NEW_NEARD \
            --epoch-length 5500 \
            --num-validators 60 \
            --num-seats 30 \
            --genesis-protocol-version 65 \
            --yes
