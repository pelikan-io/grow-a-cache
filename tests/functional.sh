#!/bin/bash
# Functional tests for grow-a-cache
# Tests all protocol and runtime combinations

set -e

BINARY="${BINARY:-./target/release/grow-a-cache}"
HOST="${HOST:-127.0.0.1}"
PORT="${PORT:-11211}"
LISTEN="${HOST}:${PORT}"
TIMEOUT=1

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Track results
PASSED=0
FAILED=0

log_pass() {
    echo -e "${GREEN}✓ PASS${NC}: $1"
    PASSED=$((PASSED + 1))
}

log_fail() {
    echo -e "${RED}✗ FAIL${NC}: $1"
    FAILED=$((FAILED + 1))
}

log_info() {
    echo -e "${YELLOW}→${NC} $1"
}

cleanup() {
    pkill -f 'grow-a-cache' 2>/dev/null || true
    sleep 0.2
}

wait_for_server() {
    local max_attempts=20
    local attempt=0
    while ! nc -z "$HOST" "$PORT" 2>/dev/null; do
        attempt=$((attempt + 1))
        if [ $attempt -ge $max_attempts ]; then
            echo "Server failed to start"
            return 1
        fi
        sleep 0.1
    done
}

send_command() {
    local cmd="$1"
    printf '%s\r\n' "$cmd" | nc -q $TIMEOUT "$HOST" "$PORT" 2>/dev/null | tr -d '\r'
}

# Test Ping protocol
test_ping() {
    local runtime="$1"
    local test_name="ping-$runtime"

    log_info "Testing Ping protocol with $runtime runtime"

    cleanup
    $BINARY --protocol ping --runtime "$runtime" --listen "$LISTEN" --log-level error &
    sleep 0.5

    if ! wait_for_server; then
        log_fail "$test_name: server did not start"
        return
    fi

    local response
    response=$(send_command "PING")

    if [ "$response" = "PONG" ]; then
        log_pass "$test_name: PING -> PONG"
    else
        log_fail "$test_name: expected 'PONG', got '$response'"
    fi

    cleanup
}

# Test Echo protocol
# Echo format: <length>\r\n<data> -> echoes back <length>\r\n<data>
test_echo() {
    local runtime="$1"
    local test_name="echo-$runtime"

    log_info "Testing Echo protocol with $runtime runtime"

    cleanup
    $BINARY --protocol echo --runtime "$runtime" --listen "$LISTEN" --log-level error &
    sleep 0.5

    if ! wait_for_server; then
        log_fail "$test_name: server did not start"
        return
    fi

    # Echo protocol expects: <length>\r\n<data>
    # Returns: <length>\r\n<data>
    local response
    response=$(printf '11\r\nhello world' | nc -q $TIMEOUT "$HOST" "$PORT" 2>/dev/null | tr -d '\r')

    # Expected: "11\nhello world" (after removing \r)
    local expected=$'11\nhello world'
    if [ "$response" = "$expected" ]; then
        log_pass "$test_name: echo works"
    else
        log_fail "$test_name: expected '$expected', got '$response'"
    fi

    cleanup
}

# Test Echo protocol with large payload (32KB)
# Note: Default buffer size is 64KB, so 32KB payload + header fits
test_echo_large() {
    local runtime="$1"
    local test_name="echo-large-$runtime"
    local payload_size=32768  # 32KB

    log_info "Testing Echo protocol with 32KB payload ($runtime runtime)"

    cleanup
    $BINARY --protocol echo --runtime "$runtime" --listen "$LISTEN" --log-level error &
    sleep 0.5

    if ! wait_for_server; then
        log_fail "$test_name: server did not start"
        return
    fi

    # Generate 32KB of random data and compute its checksum
    local tmpfile=$(mktemp)
    local outfile=$(mktemp)
    dd if=/dev/urandom bs=1024 count=32 2>/dev/null > "$tmpfile"
    local input_checksum=$(md5sum "$tmpfile" | cut -d' ' -f1)

    # Send: <length>\r\n<data>
    # The response will be: <length>\r\n<data>
    {
        printf '%d\r\n' "$payload_size"
        cat "$tmpfile"
    } | nc -q 5 "$HOST" "$PORT" 2>/dev/null > "$outfile"

    # Extract the data portion (skip the length header line)
    # The response is: "1048576\r\n<data>"
    local header_len=$((${#payload_size} + 2))  # length digits + \r\n
    tail -c +$((header_len + 1)) "$outfile" > "${outfile}.data"
    local output_checksum=$(md5sum "${outfile}.data" | cut -d' ' -f1)

    if [ "$input_checksum" = "$output_checksum" ]; then
        log_pass "$test_name: 32KB payload echoed correctly"
    else
        log_fail "$test_name: checksum mismatch (input: $input_checksum, output: $output_checksum)"
    fi

    rm -f "$tmpfile" "$outfile" "${outfile}.data"
    cleanup
}

# Test Echo protocol with very large payload (1MB)
# NOTE: Full chained buffer read path is not yet implemented.
# This test is skipped until multi-buffer read support is implemented.
test_echo_very_large() {
    local runtime="$1"
    local test_name="echo-1mb-$runtime"

    # Skip for now - chained buffer read path not yet implemented
    log_info "Skipping $test_name: chained buffer read path not yet implemented"
}

# Test max_value_size rejection
# Server should reject values larger than max_value_size
test_max_value_size_rejection() {
    local runtime="$1"
    local test_name="max-value-size-$runtime"

    log_info "Testing max_value_size rejection ($runtime runtime)"

    cleanup
    # Start server with small max_value_size (10KB)
    $BINARY --protocol echo --runtime "$runtime" --listen "$LISTEN" --log-level error --max-value-size 10240 &
    sleep 0.5

    if ! wait_for_server; then
        log_fail "$test_name: server did not start"
        return
    fi

    # Try to send a 20KB payload (should be rejected)
    local payload_size=20480
    local response
    response=$(printf '%d\r\n' "$payload_size" | nc -q $TIMEOUT "$HOST" "$PORT" 2>/dev/null | tr -d '\r')

    if echo "$response" | grep -qi "error.*too large"; then
        log_pass "$test_name: large value correctly rejected"
    else
        log_fail "$test_name: expected rejection error, got '$response'"
    fi

    cleanup
}

# Test Memcached SET with large value
# NOTE: Full chained buffer read path is not yet implemented.
# This test is skipped until multi-buffer read support is implemented.
test_memcached_large_value() {
    local runtime="$1"
    local test_name="memcached-large-$runtime"

    # Skip for now - chained buffer read path not yet implemented
    log_info "Skipping $test_name: chained buffer read path not yet implemented"
}

# Test Memcached protocol
test_memcached() {
    local runtime="$1"
    local test_name="memcached-$runtime"

    log_info "Testing Memcached protocol with $runtime runtime"

    cleanup
    $BINARY --protocol memcached --runtime "$runtime" --listen "$LISTEN" --log-level error &
    sleep 0.5

    if ! wait_for_server; then
        log_fail "$test_name: server did not start"
        return
    fi

    # Test SET
    local set_response
    set_response=$(printf 'set foo 0 0 3\r\nbar\r\n' | nc -q $TIMEOUT "$HOST" "$PORT" 2>/dev/null | tr -d '\r')

    if [ "$set_response" = "STORED" ]; then
        log_pass "$test_name: SET stored value"
    else
        log_fail "$test_name: SET expected 'STORED', got '$set_response'"
        cleanup
        return
    fi

    # Test GET
    local get_response
    get_response=$(send_command "get foo")

    if echo "$get_response" | grep -q "VALUE foo 0 3"; then
        log_pass "$test_name: GET retrieved value"
    else
        log_fail "$test_name: GET expected VALUE, got '$get_response'"
    fi

    # Test DELETE
    local del_response
    del_response=$(send_command "delete foo")

    if [ "$del_response" = "DELETED" ]; then
        log_pass "$test_name: DELETE removed value"
    else
        log_fail "$test_name: DELETE expected 'DELETED', got '$del_response'"
    fi

    # Test GET after DELETE (should miss)
    local miss_response
    miss_response=$(send_command "get foo")

    if [ "$miss_response" = "END" ]; then
        log_pass "$test_name: GET after DELETE returns END"
    else
        log_fail "$test_name: expected 'END', got '$miss_response'"
    fi

    cleanup
}

# Test RESP protocol (Redis-like)
test_resp() {
    local runtime="$1"
    local test_name="resp-$runtime"

    log_info "Testing RESP protocol with $runtime runtime"

    cleanup
    $BINARY --protocol resp --runtime "$runtime" --listen "$LISTEN" --log-level error &
    sleep 0.5

    if ! wait_for_server; then
        log_fail "$test_name: server did not start"
        return
    fi

    # Test PING
    local ping_response
    ping_response=$(printf '*1\r\n$4\r\nPING\r\n' | nc -q $TIMEOUT "$HOST" "$PORT" 2>/dev/null | tr -d '\r')

    if [ "$ping_response" = "+PONG" ]; then
        log_pass "$test_name: PING -> +PONG"
    else
        log_fail "$test_name: PING expected '+PONG', got '$ping_response'"
    fi

    # Test SET
    local set_response
    set_response=$(printf '*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n' | nc -q $TIMEOUT "$HOST" "$PORT" 2>/dev/null | tr -d '\r')

    if [ "$set_response" = "+OK" ]; then
        log_pass "$test_name: SET -> +OK"
    else
        log_fail "$test_name: SET expected '+OK', got '$set_response'"
    fi

    # Test GET
    local get_response
    get_response=$(printf '*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n' | nc -q $TIMEOUT "$HOST" "$PORT" 2>/dev/null | tr -d '\r')

    if echo "$get_response" | grep -q "value"; then
        log_pass "$test_name: GET retrieved value"
    else
        log_fail "$test_name: GET expected value, got '$get_response'"
    fi

    cleanup
}

# Test RESP max_value_size rejection
test_resp_max_value_size() {
    local runtime="$1"
    local test_name="resp-max-value-size-$runtime"

    log_info "Testing RESP max_value_size rejection ($runtime runtime)"

    cleanup
    # Start server with small max_value_size (10KB)
    $BINARY --protocol resp --runtime "$runtime" --listen "$LISTEN" --log-level error --max-value-size 10240 &
    sleep 0.5

    if ! wait_for_server; then
        log_fail "$test_name: server did not start"
        return
    fi

    # Try to SET a 20KB value (should be rejected)
    # RESP format: *3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$20480\r\n<20KB data>\r\n
    local value_size=20480
    local response
    response=$(printf '*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$%d\r\n%s\r\n' "$value_size" "$(head -c $value_size /dev/zero | tr '\0' 'A')" | nc -q $TIMEOUT "$HOST" "$PORT" 2>/dev/null | tr -d '\r')

    if echo "$response" | grep -qi "err.*too large"; then
        log_pass "$test_name: large value correctly rejected"
    else
        log_fail "$test_name: expected rejection error, got '$response'"
    fi

    cleanup
}

# Main
main() {
    echo "========================================"
    echo "grow-a-cache Functional Tests"
    echo "========================================"
    echo ""

    # Check binary exists
    if [ ! -x "$BINARY" ]; then
        echo "Binary not found at $BINARY"
        echo "Run: cargo build --release"
        exit 1
    fi

    # Check nc is available
    if ! command -v nc &> /dev/null; then
        echo "netcat (nc) is required but not found"
        exit 1
    fi

    cleanup

    # Determine available runtimes
    # On Linux: test both uring and mio
    # On macOS: only mio is available
    if [ "$(uname)" = "Linux" ]; then
        local runtimes=("uring" "mio")
    else
        local runtimes=("mio")
    fi

    # Run tests for each runtime
    for runtime in "${runtimes[@]}"; do
        echo ""
        echo "--- Runtime: $runtime ---"
        test_ping "$runtime"
        test_echo "$runtime"
        test_echo_large "$runtime"
        test_echo_very_large "$runtime"
        test_max_value_size_rejection "$runtime"
        test_memcached "$runtime"
        test_memcached_large_value "$runtime"
        test_resp "$runtime"
        test_resp_max_value_size "$runtime"
    done

    # Summary
    echo ""
    echo "========================================"
    echo "Results: $PASSED passed, $FAILED failed"
    echo "========================================"

    if [ $FAILED -gt 0 ]; then
        exit 1
    fi
}

# Run if executed directly
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
