#!/bin/bash
# Test unmount functionality

source "$(dirname "$0")/test_helper.sh"

test_unmount_main() {
    setup
    do_mount

    # Unmount main
    do_unmount

    # Should be unmounted
    assert "! mountpoint -q '$TEST_MNT'" "Mount point unmounted"
}

test_unmount_does_not_commit_single_branch() {
    setup
    do_mount
    do_create "unmount_test" "main"

    echo "branch content" > "$TEST_MNT/branch_file.txt"

    # Unmount without committing the branch.
    do_unmount

    # Should be unmounted
    assert "! mountpoint -q '$TEST_MNT'" "Mount point unmounted"

    # No changes to base
    assert_file_not_exists "$TEST_BASE/branch_file.txt" "No changes to base"
}

test_unmount_preserves_all_branches() {
    setup
    do_mount

    # Create nested branches
    do_create "parent_branch" "main"
    echo "parent content" > "$TEST_MNT/parent_file.txt"

    do_create "child_branch" "parent_branch"
    echo "child content" > "$TEST_MNT/child_file.txt"

    # Unmount — daemon exits, but branch stores are durable and should be
    # available again on the next mount for review/commit/abort.
    do_unmount

    # Should be unmounted
    assert "! mountpoint -q '$TEST_MNT'" "Mount point unmounted"

    # Remount - daemon restarts and reloads the durable branch store.
    do_mount

    assert_branch_exists "main" "Main branch exists after remount"
    assert_branch_exists "parent_branch" "Parent branch preserved on unmount"
    assert_branch_exists "child_branch" "Child branch preserved on unmount"

    # No changes to base (nothing was committed)
    assert_file_not_exists "$TEST_BASE/parent_file.txt" "No parent file in base"
    assert_file_not_exists "$TEST_BASE/child_file.txt" "No child file in base"

    do_unmount
}

test_unmount_cleanup() {
    setup
    do_mount
    do_create "cleanup_test" "main"

    # Create some files
    echo "test" > "$TEST_MNT/test.txt"

    # Check that branches directory exists before unmount
    local branches_dir="$TEST_STORAGE/branches"
    assert "[[ -d '$branches_dir' ]]" "Branches directory exists before unmount"

    # Should have branches (main + cleanup_test)
    local branch_count_before
    branch_count_before=$(ls "$branches_dir" 2>/dev/null | wc -l)
    assert "[[ $branch_count_before -gt 0 ]]" "Branches exist before unmount"

    # Unmount — daemon exits when last mount removed
    do_unmount

    # After daemon restart, durable branch metadata remains for review.
    do_mount
    local branch_count_after
    branch_count_after=$(ls "$branches_dir" 2>/dev/null | wc -l)
    assert "[[ $branch_count_after -eq $branch_count_before ]]" "Branches preserved after remount"
    assert_branch_exists "cleanup_test" "cleanup_test branch preserved after remount"
    do_unmount
}

# Run tests
run_test "Unmount Main" test_unmount_main
run_test "Unmount Does Not Commit Single Branch" test_unmount_does_not_commit_single_branch
run_test "Unmount Preserves All Branches" test_unmount_preserves_all_branches
run_test "Unmount Cleanup" test_unmount_cleanup

print_summary
