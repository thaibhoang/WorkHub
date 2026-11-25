#!/bin/sh
# Print FizzBuzz from 1 to $1 (default 15).
n=${1:-15}
i=1
while [ "$i" -le "$n" ]; do
  if [ $((i % 3)) -eq 0 ]; then
    echo Fizz
  elif [ $((i % 5)) -eq 0 ]; then
    echo Buzz
  elif [ $((i % 15)) -eq 0 ]; then
    echo FizzBuzz
  else
    echo "$i"
  fi
  i=$((i + 1))
done
