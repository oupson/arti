#!/bin/sh
#
# binary_size.sh: Build arti with a given set of options, and
# dump the binary size in a json format.

set -eu

ORIGDIR=$(pwd)
TMPDIR=$(mktemp -d -t arti_binsize.XXXXXX)
trap 'cd "$ORIGDIR" && rm -rf "$TMPDIR"' 0

RUST_TARGET=$(rustc -vV | sed -n 's|host: ||p')

cd "$(dirname "$0")/.."

echo "{"
echo "  \"date\": \"$(date -u -Iseconds)\","
echo "  \"head\": \"$(git rev-parse HEAD)\","
echo "  \"default_target\": \"$RUST_TARGET\","
echo "  \"options\": \"$*\","

cargo build --release "$@"

cp ./target/release/arti "$TMPDIR"
cd "$TMPDIR"

strip --strip-debug arti
gzip -9 -k arti
xz -9 -k arti

du -sb arti arti.gz arti.xz | sed -e 's/\(\S*\)\s\(\S*\)/  "\2": \1,/;'
echo "}"
