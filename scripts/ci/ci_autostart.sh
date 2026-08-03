#!/usr/bin/env bash
exec 1>&2

if test -z "$OXIDE_DESKTOP_PID"; then
	echo "OXIDE_DESKTOP_PID not set" >&2
	exit 1
fi

echo "Running with pid $OXIDE_DESKTOP_PID"

# Runtime tests
echo "Executing foot"
foot sh -c 'sleep 1; exit'
echo "Foot exited with $?"

echo "Killing oxide-desktop"
kill -s TERM $OXIDE_DESKTOP_PID
