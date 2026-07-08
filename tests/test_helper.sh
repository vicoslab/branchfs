#!/bin/bash
# Test helper functions for branchfs tests
# Source this file in test scripts: source "$(dirname "$0")/test_helper.sh"

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Test counters
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

# Get the project root directory
PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BRANCHFS="$PROJECT_ROOT/target/release/branchfs"

# Create unique test directories for this test script. Each setup() call gets
# its own base/storage/mount triple under this root because BranchFS branch
# stores are intentionally durable across unmount/remount. Reusing one storage
# directory for every test made branch names and uncommitted deltas leak between
# test cases.
TEST_ID="$$_$(date +%s)"
TEST_ROOT="/tmp/branchfs_test_$TEST_ID"
TEST_SEQ=0
TEST_BASE=""
TEST_STORAGE=""
TEST_MNT=""

# Track if we've set up
SETUP_DONE=0

# Set up test environment
setup() {
    TEST_SEQ=$((TEST_SEQ + 1))
    TEST_BASE="$TEST_ROOT/base_$TEST_SEQ"
    TEST_STORAGE="$TEST_ROOT/storage_$TEST_SEQ"
    TEST_MNT="$TEST_ROOT/mnt_$TEST_SEQ"

    # Create test directories
    mkdir -p "$TEST_BASE"
    mkdir -p "$TEST_STORAGE"
    mkdir -p "$TEST_MNT"

    # Create some initial files in base
    echo "base content" > "$TEST_BASE/file1.txt"
    echo "another file" > "$TEST_BASE/file2.txt"
    mkdir -p "$TEST_BASE/subdir"
    echo "nested file" > "$TEST_BASE/subdir/nested.txt"

    SETUP_DONE=1
    echo -e "${GREEN}Test environment set up${NC}"
    echo "  BASE:    $TEST_BASE"
    echo "  STORAGE: $TEST_STORAGE"
    echo "  MNT:     $TEST_MNT"
}

# Clean up test environment
cleanup() {
    echo -e "${YELLOW}Cleaning up...${NC}"

    # Try to unmount any per-test mount that survived a failed test.
    for mnt in "$TEST_ROOT"/mnt_*; do
        [[ -e "$mnt" ]] || continue
        if mountpoint -q "$mnt" 2>/dev/null; then
            fusermount3 -u "$mnt" 2>/dev/null || fusermount -u "$mnt" 2>/dev/null || true
            sleep 0.5
        fi
    done

    # Kill any daemon that might be running with a per-test storage directory.
    for socket in "$TEST_ROOT"/storage_*/daemon.sock; do
        [[ -S "$socket" ]] || continue
        echo '{"cmd":"shutdown"}' | nc -U "$socket" 2>/dev/null || true
        sleep 0.5
    done

    # Remove all test directories for this script.
    rm -rf "$TEST_ROOT" 2>/dev/null || true

    echo -e "${GREEN}Cleanup complete${NC}"
}

# Ensure cleanup on exit
trap cleanup EXIT

# Mount the filesystem
do_mount() {
    local extra_args=()
    if [[ "$(id -u)" == "0" ]]; then
        extra_args+=(--passthrough)
    fi
    "$BRANCHFS" mount --base "$TEST_BASE" --storage "$TEST_STORAGE" "${extra_args[@]}" "$TEST_MNT"
    sleep 0.5  # Give FUSE time to initialize
}

# Unmount the filesystem
do_unmount() {
    "$BRANCHFS" unmount "$TEST_MNT" --storage "$TEST_STORAGE"
    sleep 0.3
}

# Create a branch (always switches to it)
# Usage: do_create <name> [parent]
# - name: branch name
# - parent: parent branch (default: main)
do_create() {
    local name="$1"
    local parent="${2:-main}"

    "$BRANCHFS" create "$name" "$TEST_MNT" -p "$parent" --storage "$TEST_STORAGE"
    sleep 0.3
}

# Commit changes
do_commit() {
    "$BRANCHFS" commit "$TEST_MNT" --storage "$TEST_STORAGE"
}

# Abort changes
do_abort() {
    "$BRANCHFS" abort "$TEST_MNT" --storage "$TEST_STORAGE"
}

# Switch to a branch by writing to the ctl file and notifying daemon
do_switch() {
    local name="$1"
    echo -n "switch:${name}" > "$TEST_MNT/.branchfs_ctl"
    # Notify daemon of the switch so get_mount_branch() returns correct data
    local canon_mnt
    canon_mnt="$(readlink -f "$TEST_MNT")"
    python3 -c "
import socket, json, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
msg = json.dumps({'cmd': 'notify_switch', 'mountpoint': sys.argv[2], 'branch': sys.argv[3]}) + chr(10)
s.sendall(msg.encode())
s.recv(4096)
s.close()
" "$TEST_STORAGE/daemon.sock" "$canon_mnt" "$name" 2>/dev/null || true
}

# List branches
do_list() {
    "$BRANCHFS" list --storage "$TEST_STORAGE"
}

# Assert that a condition is true
assert() {
    local condition="$1"
    local message="$2"

    TESTS_RUN=$((TESTS_RUN + 1))

    if eval "$condition"; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}✓${NC} $message"
        return 0
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}✗${NC} $message"
        echo -e "    ${RED}Condition failed: $condition${NC}"
        return 1
    fi
}

# Assert that two values are equal
assert_eq() {
    local actual="$1"
    local expected="$2"
    local message="$3"

    TESTS_RUN=$((TESTS_RUN + 1))

    if [[ "$actual" == "$expected" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}✓${NC} $message"
        return 0
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}✗${NC} $message"
        echo -e "    ${RED}Expected: $expected${NC}"
        echo -e "    ${RED}Actual:   $actual${NC}"
        return 1
    fi
}

# Assert that a file exists
assert_file_exists() {
    local file="$1"
    local message="${2:-File $file exists}"
    assert "[[ -f '$file' ]]" "$message"
}

# Assert that a file does not exist
assert_file_not_exists() {
    local file="$1"
    local message="${2:-File $file does not exist}"
    assert "[[ ! -f '$file' ]]" "$message"
}

# Assert that a file contains specific content
assert_file_contains() {
    local file="$1"
    local expected="$2"
    local message="${3:-File $file contains expected content}"

    if [[ -f "$file" ]]; then
        local actual
        actual=$(cat "$file")
        assert_eq "$actual" "$expected" "$message"
    else
        TESTS_RUN=$((TESTS_RUN + 1))
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}✗${NC} $message"
        echo -e "    ${RED}File does not exist: $file${NC}"
        return 1
    fi
}

# Assert that mount is on a specific branch (check via listing)
assert_branch_exists() {
    local branch="$1"
    local message="${2:-Branch $branch exists}"

    local output
    output=$(do_list 2>&1)
    assert "[[ '$output' == *'$branch'* ]]" "$message"
}

# Assert that a branch does not exist
assert_branch_not_exists() {
    local branch="$1"
    local message="${2:-Branch $branch does not exist}"

    local output
    output=$(do_list 2>&1)
    assert "[[ '$output' != *'$branch'* ]]" "$message"
}

# Print test summary
print_summary() {
    echo ""
    echo "=================================="
    echo "Test Summary"
    echo "=================================="
    echo -e "Total:  $TESTS_RUN"
    echo -e "Passed: ${GREEN}$TESTS_PASSED${NC}"
    echo -e "Failed: ${RED}$TESTS_FAILED${NC}"
    echo "=================================="

    if [[ $TESTS_FAILED -gt 0 ]]; then
        return 1
    fi
    return 0
}

# Run a test function
run_test() {
    local test_name="$1"
    local test_func="$2"

    echo ""
    echo -e "${YELLOW}Running: $test_name${NC}"
    echo "-----------------------------------"

    if $test_func; then
        echo -e "${GREEN}$test_name: PASSED${NC}"
    else
        echo -e "${RED}$test_name: FAILED${NC}"
    fi
}
