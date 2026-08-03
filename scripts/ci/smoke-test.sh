#!/usr/bin/env bash

: ${OXIDE_DESKTOP_RUNS:=1}
: ${OXIDE_DESKTOP_LEAK_TEST:=0}
: ${OXIDE_DESKTOP_EXPECT_RETURNCODE:=0}
: ${OXIDE_DESKTOP_VERBOSE:=0}

if ! test -x "$1/oxide-desktop"; then
	echo "$1/oxide-desktop not found"
	exit 1
fi

args=(
	"$1/oxide-desktop"
	-C scripts/ci
	-d
)

export XDG_RUNTIME_DIR=$(mktemp -d)
export WLR_BACKENDS=headless

gdb_run() {
	# Not using -Db_sanitize=address,undefined
	# because it slows down the usual execution
	# way too much and spams pages over pages
	# of irrelevant memory-still-in-use logs
	# for external libraries.
	#
	# Not using coredumps either because they
	# are a pain to setup on GH actions and
	# just running oxide-desktop again is a lot faster
	# anyway.

	gdb --batch                       \
		--return-child-result     \
		-ex run                   \
		-ex 'bt full'             \
		-ex 'echo \n'             \
		-ex 'echo "Local vars:\n' \
		-ex 'info locals'         \
		-ex 'echo \n'             \
		-ex 'set listsize 50'     \
		-ex list                  \
		--args "${args[@]}"
	return $?
}

echo "Running with OXIDE_DESKTOP_RUNS=$OXIDE_DESKTOP_RUNS"

if test "$OXIDE_DESKTOP_LEAK_TEST" != "0"; then
	LSAN_OPTIONS=suppressions=scripts/asan_leak_suppressions "${args[@]}"
	exit $?
fi

ret=0
for((i=1; i<=OXIDE_DESKTOP_RUNS; i++)); do
	printf "Starting run %2s\n" $i
	output=$(gdb_run 2>&1)
	ret=$?
	if test $ret -ne $OXIDE_DESKTOP_EXPECT_RETURNCODE; then
		echo "Crash encountered:"
		echo "------------------"
		echo "$output"
		break
	elif test $OXIDE_DESKTOP_VERBOSE -eq 1; then
		echo "------------------"
		echo "$output"
	fi
done

echo "oxide-desktop terminated with return code $ret"
if test $ret -eq $OXIDE_DESKTOP_EXPECT_RETURNCODE; then
	exit 0;
else
	exit 1;
fi
