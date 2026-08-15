#!/usr/bin/env bash
# @author kongweiguang
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail

if [[ "$#" -lt 2 ]]; then
  echo 'usage: run-with-timeout.sh <seconds> <command> [args...]' >&2
  exit 64
fi

timeout_seconds="$1"
shift
if ! [[ "$timeout_seconds" =~ ^[1-9][0-9]*$ ]]; then
  echo 'timeout must be a positive integer' >&2
  exit 64
fi

# Use a separate process group so a timed-out Java probe and its direct MCP fixture are terminated
# together; otherwise a blocked stdio reader could leave the fixture alive after the wrapper exits.
/usr/bin/perl -e '
use strict;
use warnings;
use POSIX qw(WIFEXITED WEXITSTATUS WIFSIGNALED WTERMSIG);

my $seconds = shift @ARGV;
my $pid = fork();
die "fork failed: $!\n" unless defined $pid;
if ($pid == 0) {
    setpgrp(0, 0) or die "set process group failed: $!\n";
    exec @ARGV or die "exec failed: $!\n";
}

my $timed_out = 0;
$SIG{ALRM} = sub {
    $timed_out = 1;
    kill "TERM", -$pid;
    sleep 1;
    kill "KILL", -$pid;
};
alarm $seconds;
waitpid($pid, 0);
alarm 0;
if ($timed_out) {
    waitpid($pid, 0);
    exit 124;
}
my $status = $?;
exit WEXITSTATUS($status) if WIFEXITED($status);
exit 128 + WTERMSIG($status) if WIFSIGNALED($status);
exit 125;
' "$timeout_seconds" "$@"
