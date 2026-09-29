#!/bin/sh
# Publish the feed as of main@<sha> onto gh-pages (after fonix232/CV's
# scripts/publish.sh, as openUF does).
#
#   publish.sh <main-sha> [<dir of newly built packages, one directory per architecture>]
#
# gh-pages is a parallel history of main: one commit per feed run on main
# (a push), the whole feed as of the pushed head, its body naming it
# ("Source: main@<sha>"). BASE is the gh-pages commit to publish onto and TIP
# the branch head, both as feed-plan.sh found them: BASE is behind TIP when
# main was rewritten (the commits after it are dropped), and empty to start
# the branch over. The push is refused if gh-pages moved since the plan.
#
# The feed is one apk repository per package architecture, apk/<arch>/. The
# packages not rebuilt are carried over from BASE as they were published;
# the last KEEP builds of each stay, so an earlier one can be reinstalled
# with `apk add steward-agent=<version>`. An architecture no longer in
# TARGETS (packages.sh) is dropped. Each changed repository's index is
# signed with APK_SIGN_KEY (an ECDSA private key, PEM) in the OpenWrt SDK
# image.
#
# A published name-version never changes its bytes: a device, a cache or a
# mirror that has the file would no longer match the index. A new build of a
# file already in BASE is dropped for the published one (REBUILD, a change
# within the same minute). So is a new build of a file an earlier gh-pages
# commit published, which this publish replaces (a re-run for the same main
# commit, a rewritten main): the file comes back from gh-pages' history.
set -eu
SHA=$1 NEW=${2:-}
BRANCH=${FEED_BRANCH:-gh-pages}
REMOTE=${FEED_REMOTE:-origin}
KEEP=${KEEP:-5}
BASE=${BASE:-} TIP=${TIP:-}
SDK_IMAGE=${SDK_IMAGE:-openwrt/sdk:x86-64-openwrt-25.12}
# shellcheck source=.github/scripts/packages.sh
. "$(dirname "$0")/packages.sh"

log() { printf 'publish: %s\n' "$*" >&2; }
short() { printf '%.7s' "$1"; }

git cat-file -e "$SHA^{commit}" || { log "unknown commit $SHA"; exit 2; }
[ -z "$TIP" ] || git cat-file -e "$TIP^{commit}" || { log "unknown commit $TIP (fetch $BRANCH)"; exit 2; }
wt=$(mktemp -d) keys=$(mktemp -d)
trap 'rm -rf "$keys"; git worktree remove --force "$wt" 2>/dev/null || true
	git branch -q -D "$BRANCH-publish" 2>/dev/null || true' EXIT

if [ -n "$BASE" ]; then
	git worktree add --quiet --detach "$wt" "$BASE"
	log "onto $(short "$BASE")"
else
	git worktree add --quiet --detach "$wt" "$SHA"
	git -C "$wt" checkout --quiet --orphan "$BRANCH-publish"
	git -C "$wt" rm -rqf --cached .
	find "$wt" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
	log "starting $BRANCH over"
fi
mkdir -p "$wt/apk"

arches=$(for t in $TARGETS; do printf '%s ' "${t%%:*}"; done)
for d in "$wt"/apk/*/; do
	[ -d "$d" ] || continue
	a=$(basename "$d")
	case " $arches" in *" $a "*) ;; *) rm -rf "$d"; log "dropped apk/$a" ;; esac
done

# The gh-pages commit that last published apk/<arch>/<file> $1, if any: the
# last one that added or changed it (older gh-pages history has overwrites).
published() {
	[ -z "$TIP" ] || git log -1 --format=%H --diff-filter=AM "$TIP" -- "apk/$1"
}

reindex=""
for a in $arches; do
	mkdir -p "$wt/apk/$a"
	[ -f "$wt/apk/$a/packages.adb" ] || reindex="$reindex $a"
	added=""
	if [ -n "$NEW" ] && ls "$NEW/$a"/*.apk >/dev/null 2>&1; then
		for f in "$NEW/$a"/*.apk; do
			n=$(basename "$f")
			if [ -e "$wt/apk/$a/$n" ]; then
				log "kept $a/$n as published (the new build of it is dropped)"
				continue
			fi
			c=$(published "$a/$n")
			if [ -n "$c" ]; then
				git cat-file blob "$c:apk/$a/$n" > "$wt/apk/$a/$n"
				log "restored $a/$n from $(short "$c"), which published it (the new build of it is dropped)"
			else
				cp "$f" "$wt/apk/$a/"
			fi
			added=1
		done
	fi
	if [ -n "$added" ]; then
		case " $reindex " in *" $a "*) ;; *) reindex="$reindex $a" ;; esac
		for p in $PKGS; do
			find "$wt/apk/$a" -maxdepth 1 -name "$p-[0-9]*.apk" -exec basename {} \; | sort -V \
				| awk -v keep="$KEEP" '{ f[NR] = $0 } END { for (i = 1; i <= NR - keep; i++) print f[i] }' \
				| while read -r f; do rm -f "$wt/apk/$a/$f"; log "retired $a/$f"; done
		done
	fi
done

# Re-signed only when a repository's packages changed: an unchanged one keeps
# its index byte for byte.
reindex=${reindex# }
if [ -n "$reindex" ]; then
	(umask 077; printf '%s\n' "$APK_SIGN_KEY" > "$keys/apk-private.pem")
	chmod -R a+rX "$keys" && chmod -R a+rwX "$wt/apk"
	docker run --rm -e REINDEX="$reindex" -v "$wt/apk:/apk" -v "$keys:/keys:ro" "$SDK_IMAGE" sh -c '
		set -e
		cd /builder && { [ -f rules.mk ] || ./setup.sh >/dev/null; }
		for a in $REINDEX; do
			ls /apk/$a/*.apk >/dev/null 2>&1 || continue
			cd /apk/$a && rm -f packages.adb
			/builder/staging_dir/host/bin/apk mkndx --allow-untrusted \
				--sign /keys/apk-private.pem --output packages.adb *.apk
		done'
	for a in $reindex; do
		log "indexed $a: $(find "$wt/apk/$a" -maxdepth 1 -name '*.apk' -exec basename {} \; | sort | tr '\n' ' ')"
	done
fi
cp feed/steward.pem feed/index.html "$wt/"
touch "$wt/.nojekyll"

subject=$(git log -1 --format=%s "$SHA")
git -C "$wt" add -A
git -C "$wt" commit --quiet --allow-empty \
	-m "Publish main@$(short "$SHA"): $subject" \
	-m "Source: main@$SHA"
# An empty TIP means the branch must not exist yet.
git -C "$wt" push --quiet --force-with-lease="refs/heads/$BRANCH:$TIP" "$REMOTE" "HEAD:refs/heads/$BRANCH"
log "pushed $(short "$(git -C "$wt" rev-parse HEAD)") to $BRANCH (main@$(short "$SHA"))"
