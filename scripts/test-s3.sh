#!/bin/sh
# Starts an S3-compatible server (versitygw, in Docker) with a bucket for the tests that need S3, and prints the
# KIWI_NEST_TEST_S3 to set.
#
#   scripts/test-s3.sh [PORT]     default: 9000
#
# Running it again with the server up does nothing. `scripts/test-s3.sh stop` removes it. In GitHub Actions
# (GITHUB_ENV set), the variable is also set for the following steps.
set -eu

name=kiwi-nest-test-s3
port=${1:-9000}
if [ "$port" = stop ]; then
  docker rm -f "$name" >/dev/null 2>&1 || true
  exit 0
fi
access=kntest
secret=kntest-secret
bucket=kn-test

if [ -z "$(docker ps -q --filter "name=^${name}\$")" ]; then
  docker run -d --rm --name "$name" -p "127.0.0.1:$port:7070" \
    -e ROOT_ACCESS_KEY="$access" -e ROOT_SECRET_KEY="$secret" \
    --tmpfs /data versity/versitygw:latest posix /data >/dev/null
  # Make the bucket once the server answers (the server checks the signature, so any S3 client will do; this
  # uses one in a container, so nothing needs installing).
  for _ in $(seq 1 30); do
    if docker run --rm --network host -e AWS_ACCESS_KEY_ID="$access" -e AWS_SECRET_ACCESS_KEY="$secret" \
      -e AWS_DEFAULT_REGION=us-east-1 amazon/aws-cli:latest --endpoint-url "http://127.0.0.1:$port" \
      s3 mb "s3://$bucket" >/dev/null 2>&1; then
      break
    fi
    sleep 1
  done
fi

value="http://127.0.0.1:$port $bucket $access $secret"
echo "KIWI_NEST_TEST_S3=$value"
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "KIWI_NEST_TEST_S3=$value" >>"$GITHUB_ENV"
fi
