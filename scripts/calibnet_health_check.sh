#!/usr/bin/env bash

set -e

FOREST_PATH="forest"
FOREST_CLI_PATH="forest-cli"

TMP_DIR=$(mktemp --directory)
SNAPSHOT_DIRECTORY=$TMP_DIR/snapshots
LOG_DIRECTORY=$TMP_DIR/logs

usage() {
  echo "Usage: $0 <PRELOADED_WALLET_STRING>"
  exit 1
}

if [ -z "$1" ]
  then
    usage
fi

function cleanup {
  $FOREST_CLI_PATH shutdown --force

  timeout 10s sh -c "while pkill -0 forest 2>/dev/null; do sleep 1; done"
}
trap cleanup EXIT

echo "$1" > preloaded_wallet.key

echo "Fetching params"
$FOREST_CLI_PATH fetch-params --keys
echo "Downloading snapshot"
$FOREST_CLI_PATH --chain calibnet snapshot fetch --aria2 -s "$SNAPSHOT_DIRECTORY"

echo "Importing snapshot and running Forest"
$FOREST_PATH --chain calibnet --encrypt-keystore false --halt-after-import --height=-200 --import-snapshot "$SNAPSHOT_DIRECTORY"/*.car
echo "Checking DB stats"
$FOREST_CLI_PATH --chain calibnet db stats
echo "Running forest in detached mode"
$FOREST_PATH --chain calibnet --encrypt-keystore false --log-dir "$LOG_DIRECTORY" --detach --save-token ./admin_token --track-peak-rss

echo "Validating checkpoint tipset hashes"
$FOREST_CLI_PATH chain validate-tipset-checkpoints

echo "Waiting for sync and check health"
timeout 30m $FOREST_CLI_PATH --chain calibnet sync wait && $FOREST_CLI_PATH --chain calibnet db stats

# Admin token used when interacting with wallet
ADMIN_TOKEN=$(cat admin_token)
# Set environment variable
export FULLNODE_API_INFO="$ADMIN_TOKEN:/ip4/127.0.0.1/tcp/1234/http"

echo "Running database garbage collection"
du -hS ~/.local/share/forest/calibnet
$FOREST_CLI_PATH --chain calibnet db gc
du -hS ~/.local/share/forest/calibnet

echo "Exporting snapshot"
$FOREST_CLI_PATH --chain calibnet snapshot export

echo "Verifing snapshot checksum"
sha256sum -c ./*.sha256sum

echo "Testing js console"
$FOREST_CLI_PATH attach --exec 'showPeers()'

echo "Validating as mainnet snapshot"
set +e
$FOREST_CLI_PATH --chain mainnet snapshot validate "$SNAPSHOT_DIRECTORY"/*.car --force && \
{
    echo "mainnet snapshot validation with calibnet snapshot should fail";
    exit 1;
}
set -e

echo "Validating as calibnet snapshot"
$FOREST_CLI_PATH --chain calibnet snapshot validate "$SNAPSHOT_DIRECTORY"/*.car --force

echo "Print forest log files"
ls -hl "$LOG_DIRECTORY"
cat "$LOG_DIRECTORY"/*

echo "Wallet tests"

# The following steps does basic wallet handling tests.

# Amount to send to
FIL_AMT=500

echo "Importing preloaded wallet key"
$FOREST_CLI_PATH --chain calibnet wallet import preloaded_wallet.key

# The preloaded address
ADDR_ONE=$($FOREST_CLI_PATH --chain calibnet wallet list | tail -1 | cut -d ' ' -f1)

sleep 5s

echo "Exporting key"
$FOREST_CLI_PATH --chain calibnet wallet export "$ADDR_ONE" > preloaded_wallet.test.key
if ! cmp -s preloaded_wallet.key preloaded_wallet.test.key; then
    echo ".key files should match"
    exit 1
fi

echo "Fetching metrics"
wget -O metrics.log http://localhost:6116/metrics

sleep 5s

# Show balances
echo "Listing wallet balances"
$FOREST_CLI_PATH --chain calibnet wallet list

echo "Creating a new address to send FIL to"
ADDR_TWO=$($FOREST_CLI_PATH --chain calibnet wallet new)
echo "$ADDR_TWO"
$FOREST_CLI_PATH --chain calibnet wallet set-default "$ADDR_ONE"

echo "Listing wallet balances"
$FOREST_CLI_PATH --chain calibnet wallet list

echo "Sending FIL to the above address"
$FOREST_CLI_PATH --chain calibnet send "$ADDR_TWO" "$FIL_AMT"

echo "Checking balance of $ADDR_TWO..."

ADDR_TWO_BALANCE=0
i=0
while [[ $i != 10 && $ADDR_TWO_BALANCE == 0 ]]; do
  i=$((i+1))
  
  echo "Checking balance $i/10"
  sleep 30s
  ADDR_TWO_BALANCE=$($FOREST_CLI_PATH --chain calibnet wallet balance "$ADDR_TWO")
done

# wallet list should contain address two with transfered FIL amount
$FOREST_CLI_PATH --chain calibnet wallet list

if [ "$ADDR_TWO_BALANCE" != "$FIL_AMT" ]; then
  echo "FIL amount should match"
  exit 1
fi

echo "Get and print metrics and logs and stop forest"
wget -O metrics.log http://localhost:6116/metrics

echo "--- Forest STDOUT ---"; cat forest.out
echo "--- Forest STDERR ---"; cat forest.err
echo "--- Forest Prometheus metrics ---"; cat metrics.log
