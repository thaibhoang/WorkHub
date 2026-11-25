#!/bin/sh
# Hidden check for the greet-name case (runs in a checkout of the task branch).
set -e
test -x greet.sh || { echo "greet.sh is not executable"; exit 1; }
out=$(./greet.sh); [ "$out" = "Hello, world!" ] || { echo "no argument: got '$out'"; exit 1; }
out=$(./greet.sh Ada); [ "$out" = "Hello, Ada!" ] || { echo "with Ada: got '$out'"; exit 1; }
out=$(./greet.sh "Grace Hopper"); [ "$out" = "Hello, Grace Hopper!" ] || { echo "with a space: got '$out'"; exit 1; }
echo ok
