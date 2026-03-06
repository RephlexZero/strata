#!/usr/bin/env bash
# scripts/band-lock.sh — Persistent LTE band lock for Huawei HiLink modems
#
# Locks a modem to a specific LTE band via the HiLink HTTP API.
# Run as a cron job or systemd timer to survive reconnects/reboots.
#
# Usage:
#   ./scripts/band-lock.sh                          # Lock modem at 192.168.8.1 to Band 8
#   ./scripts/band-lock.sh 192.168.9.1 7FFFFFFFFFFFFFFF  # Unlock all bands
#
# Band values (hex bitmask):
#   Band  1 (2100 MHz): 1
#   Band  3 (1800 MHz): 4
#   Band  7 (2600 MHz): 40
#   Band  8 (900 MHz):  80
#   Band 20 (800 MHz):  80000
#   All bands:          7FFFFFFFFFFFFFFF
#
# To run every 60s via cron:
#   * * * * * /path/to/scripts/band-lock.sh >> /var/log/band-lock.log 2>&1

set -euo pipefail

MODEM_IP="${1:-192.168.8.1}"
LTE_BAND="${2:-80}"  # Band 8 (900 MHz) by default

BASE_URL="http://${MODEM_IP}"

# Step 1: Get a session token (required for authenticated API calls)
TOKEN_RESPONSE=$(curl -s "${BASE_URL}/api/webserver/SesTokInfo" 2>/dev/null || true)
if [ -z "$TOKEN_RESPONSE" ]; then
    echo "$(date -Iseconds) ERROR: Cannot reach modem at ${MODEM_IP}"
    exit 1
fi

SESSION_ID=$(echo "$TOKEN_RESPONSE" | grep -oP '(?<=<SesInfo>).*(?=</SesInfo>)' || true)
TOKEN=$(echo "$TOKEN_RESPONSE" | grep -oP '(?<=<TokInfo>).*(?=</TokInfo>)' || true)

if [ -z "$TOKEN" ]; then
    echo "$(date -Iseconds) ERROR: Failed to get session token from ${MODEM_IP}"
    exit 1
fi

# Step 2: Check current band setting
CURRENT=$(curl -s "${BASE_URL}/api/net/net-mode" \
    -H "Cookie: ${SESSION_ID}" \
    -H "__RequestVerificationToken: ${TOKEN}" 2>/dev/null || true)

CURRENT_BAND=$(echo "$CURRENT" | grep -oP '(?<=<LTEBand>).*(?=</LTEBand>)' || true)

if [ "$CURRENT_BAND" = "$LTE_BAND" ]; then
    echo "$(date -Iseconds) OK: Band already locked to ${LTE_BAND} on ${MODEM_IP}"
    exit 0
fi

echo "$(date -Iseconds) INFO: Band is ${CURRENT_BAND:-unknown}, locking to ${LTE_BAND} on ${MODEM_IP}"

# Step 3: Re-fetch token (the GET above consumed it)
TOKEN_RESPONSE=$(curl -s "${BASE_URL}/api/webserver/SesTokInfo" 2>/dev/null)
SESSION_ID=$(echo "$TOKEN_RESPONSE" | grep -oP '(?<=<SesInfo>).*(?=</SesInfo>)')
TOKEN=$(echo "$TOKEN_RESPONSE" | grep -oP '(?<=<TokInfo>).*(?=</TokInfo>)')

# Step 4: Set band lock
RESULT=$(curl -s "${BASE_URL}/api/net/net-mode" \
    -X POST \
    -H "Cookie: ${SESSION_ID}" \
    -H "__RequestVerificationToken: ${TOKEN}" \
    -H "Content-Type: application/xml" \
    -d "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<request>
<NetworkMode>03</NetworkMode>
<NetworkBand>3FFFFFFF</NetworkBand>
<LTEBand>${LTE_BAND}</LTEBand>
</request>" 2>/dev/null)

if echo "$RESULT" | grep -q "<response>OK</response>"; then
    echo "$(date -Iseconds) OK: Locked to band ${LTE_BAND} on ${MODEM_IP}"
else
    echo "$(date -Iseconds) ERROR: Failed to set band on ${MODEM_IP}: ${RESULT}"
    exit 1
fi
