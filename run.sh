#!/bin/bash

# tiny wrapper around `gocrash`, primarily to set the GO environment variables
# that we want to use

set -o xtrace
set -o errexit

dir="$(dirname "${BASH_SOURCE[0]}")"
cd "$dir"

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

cargo run -- "$@"
