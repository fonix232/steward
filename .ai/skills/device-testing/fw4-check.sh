#!/bin/sh
# fw4-check.sh <dir>: what fw4 would make of the firewall config `stage-export` wrote to <dir>,
# offline. It runs fw4's own code in print mode, with a copy of fw4.uc whose UCI cursor reads
# <dir>, writes the ruleset to <dir>/fw4.nft and fw4's warnings to <dir>/fw4.warn, and checks
# the ruleset with `nft -c`, as `fw4 check` does. Nothing is loaded, and print mode writes no
# state file.
DIR=$1
W=$DIR/fw4
mkdir -p "$W/lib" "$W/fw" "$W/save"
sed "s|uci.cursor()|uci.cursor(\"$DIR\", \"$W/save\")|" /usr/share/ucode/fw4.uc > "$W/lib/fw4.uc"
if ! grep -q "uci.cursor(\"$DIR\"" "$W/lib/fw4.uc"; then
	echo "fw4.uc has no uci.cursor() to point at $DIR: fw4 changed" >&2
	exit 1
fi
cp /usr/share/firewall4/main.uc "$W/fw/main.uc"
[ -e "$W/fw/templates" ] || ln -s /usr/share/firewall4/templates "$W/fw/templates"
if ! ACTION=print utpl -L "$W/lib/*.uc" -S "$W/fw/main.uc" > "$DIR/fw4.nft" 2> "$DIR/fw4.warn"; then
	cat "$DIR/fw4.warn" >&2
	exit 1
fi
nft -c -f "$DIR/fw4.nft" && echo "nft -c: the ruleset passes"
