#!/bin/bash

# tiny wrapper around `gocrash`, primarily to set the GO environment variables
# that we want to use

set -o xtrace
set -o errexit
set -o noclobber

dir="$(dirname "${BASH_SOURCE[0]}")"
cd "$dir"

# Use our pid as a unique id.
id=$$

# Create output directory and temporary directories.
mkdir -p output

# On Linux, add "$PWD/bin-linux" to PATH.
# export PATH=$PATH:$PWD/bin-linux

# Turn on extra run-time checks for use of unsafe.Pointer.
#export GO_GCFLAGS="-d=checkptr"

# Dump core (with SIGABORT) on fatal runtime errors and SIGSEGV.
export GOTRACEBACK=crash

# See https://pkg.go.dev/runtime#hdr-Environment_Variables.
# "GODEBUG=clobberfree" causes GC'd objects to be filled with known contents to
# aid debugging memory issues.
export GODEBUG=clobberfree

# Turn off GC altogether.
# export GOGC=off

exec nohup cargo run -- "$@" > output/$id 2>&1
