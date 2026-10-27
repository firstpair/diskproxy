#!/bin/zsh
set -eu
cd "${0:A:h}/.."
config=''
if (( $# )); then
  [[ $# == 2 && $1 == --config && -f $2 ]] || {
    print -u2 'Usage: scripts/install.sh [--config CONFIG.json]'; exit 1
  }
  config=$2
fi
cargo build --release --locked
mkdir -p "$HOME/bin" "$HOME/.config/diskproxy"
# Atomic replacement leaves an already-running server's executable intact.
new_binary=$(mktemp "$HOME/bin/.diskproxy-server.XXXXXX")
trap 'rm -f -- "$new_binary"' EXIT
install -m 755 target/release/diskproxy-server "$new_binary"
mv -f "$new_binary" "$HOME/bin/diskproxy-server"
for command in diskproxy pmp-card pmp-ingest-proxy; do
  new_binary=$(mktemp "$HOME/bin/.$command.XXXXXX")
  install -m 755 "bin/$command" "$new_binary"
  mv -f "$new_binary" "$HOME/bin/$command"
done
if [[ ! -f $HOME/.config/diskproxy/pmp.json ]]; then
  install -m 600 configs/pmp.json "$HOME/.config/diskproxy/pmp.json"
fi
if [[ -n $config && ! -f $HOME/.config/diskproxy/config.json ]]; then
  install -m 600 "$config" "$HOME/.config/diskproxy/config.json"
fi
print 'Installed diskproxy, diskproxy-server, and PMP compatibility commands in ~/bin.'
print 'Existing config files and card profiles were preserved.'
