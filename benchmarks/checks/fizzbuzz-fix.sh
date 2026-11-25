#!/bin/sh
# Hidden check for the fizzbuzz-fix case.
set -e
expected="1 2 Fizz 4 Buzz Fizz 7 8 Fizz Buzz 11 Fizz 13 14 FizzBuzz 16 17 Fizz 19 Buzz Fizz 22 23 Fizz Buzz 26 Fizz 28 29 FizzBuzz"
got=$(sh fizzbuzz.sh 30 | tr '\n' ' ' | sed 's/ $//')
[ "$got" = "$expected" ] || { echo "expected: $expected"; echo "got:      $got"; exit 1; }
got=$(sh fizzbuzz.sh | wc -l | tr -d ' ')
[ "$got" = 15 ] || { echo "default should print 15 lines, got $got"; exit 1; }
echo ok
