#!/usr/bin/env bash
# Light each Agent Key a distinct color so physical keys can be matched to
# slot IDs. Usage:
#   identify.sh          # all six at once, with a printed legend
#   identify.sh step     # one key at a time (2s each), announcing each slot
#   identify.sh off      # clear all lights
set -u

SOCK="${MICROD_SOCKET:-$HOME/.cache/microd/microd.sock}"

send() {
  printf '%s\n' "$1" | /usr/bin/nc -U "$SOCK" -w 2 >/dev/null
}

light_one() { # slot color
  send "{\"cmd\":\"lights\",\"lights\":[{\"slot\":$1,\"color\":$2,\"effect\":\"solid\",\"speed\":50}]}"
}

clear_all() {
  send '{"cmd":"clear"}'
}

# slot -> color name + 24-bit value
names=(red yellow green cyan blue magenta)
colors=(16711680 16776960 65280 65535 255 16711935)

case "${1:-all}" in
  off)
    clear_all
    echo "lights cleared"
    ;;
  step)
    clear_all
    for i in 0 1 2 3 4 5; do
      echo "AG0$i = ${names[$i]} (lit now)"
      light_one "$i" "${colors[$i]}"
      sleep 2
      send "{\"cmd\":\"lights\",\"lights\":[{\"slot\":$i,\"color\":0,\"effect\":\"off\",\"speed\":50}]}"
    done
    echo "done"
    ;;
  *)
    lights=""
    for i in 0 1 2 3 4 5; do
      lights+="{\"slot\":$i,\"color\":${colors[$i]},\"effect\":\"solid\",\"speed\":50},"
    done
    send "{\"cmd\":\"lights\",\"lights\":[${lights%,}]}"
    echo "legend:"
    for i in 0 1 2 3 4 5; do
      echo "  AG0$i = ${names[$i]}"
    done
    echo "(run 'identify.sh off' to clear)"
    ;;
esac
