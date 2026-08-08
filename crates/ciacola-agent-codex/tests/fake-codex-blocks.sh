#!/usr/bin/env bash
set -eu

pid_file=$1
printf '%s\n' "$$" > "$pid_file"
sleep 30
