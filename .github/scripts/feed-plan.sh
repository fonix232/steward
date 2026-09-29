#!/bin/sh
# Which packages a feed run builds, for which architectures, and which
# gh-pages commit it publishes onto.
#
#   feed-plan.sh <sha>     prints packages=, targets=, base=, tip= and source= lines
#
# gh-pages mirrors main (publish.sh): each of its commits is the whole feed
# after one feed run on main (a push), named after the pushed head in a
# "Source: main@<sha>" line.
# The base is the newest of them whose source is still an ancestor of <sha>
# -- the tip on an ordinary push, an older one after main was rewritten,
# none at all when the two histories share nothing (gh-pages then starts over).
# A package whose inputs (pkg_inputs in packages.sh, the build script
# included) changed since the base's source is built for every architecture,
# and gets a new release. One that an architecture's repository in the base
# lacks (a new architecture) is built for that architecture only. The rest are
# published again as they are: a published name-version never changes its
# bytes. targets= is the build matrix, as JSON, one entry per architecture
# with something to build, with its packages; packages= is every package
# built.
#
# A build is never older than the base's newest of that package: a release
# is a commit date (steward.mk), so it goes down only when main was
# rewritten with older dates, and apk wouldn't upgrade to it. The plan fails
# instead.
#
# BASE_SHA set (a pull request): compare with it instead, and publish nothing.
# REBUILD=true: build every package for every architecture (publish.sh keeps
# the ones already published as they are).
set -eu
SHA=$1
BRANCH=${FEED_BRANCH:-gh-pages}
REMOTE=${FEED_REMOTE:-origin}
# shellcheck source=.github/scripts/packages.sh
. "$(dirname "$0")/packages.sh"

log() { printf 'plan: %s\n' "$*" >&2; }
short() { printf '%.7s' "$1"; }

base="" tip="" src=""
if [ -n "${BASE_SHA:-}" ]; then
	src=$BASE_SHA
	log "pull request: comparing with $(short "$src")"
elif git fetch --quiet "$REMOTE" "+refs/heads/$BRANCH:refs/remotes/$REMOTE/$BRANCH" 2>/dev/null; then
	tip=$(git rev-parse "refs/remotes/$REMOTE/$BRANCH")
	for c in $(git rev-list "$tip"); do
		s=$(git log -1 --format=%B "$c" | sed -n 's/^Source: main@\([0-9a-f]\{40\}\)$/\1/p' | head -n 1)
		[ -n "$s" ] || continue                 # not a mirror commit
		[ "$s" = "$SHA" ] && continue           # published before: replace it
		if git cat-file -e "$s^{commit}" 2>/dev/null && git merge-base --is-ancestor "$s" "$SHA"; then
			base=$c src=$s
			break
		fi
	done
	if [ -z "$base" ]; then
		log "$BRANCH shares no history with main@$(short "$SHA"): starting it over"
	elif [ "$base" = "$tip" ]; then
		log "$BRANCH at $(short "$tip") is main@$(short "$src")"
	else
		log "main was rewritten: $BRANCH goes back to $(short "$base") (main@$(short "$src"))"
	fi
else
	log "no $BRANCH yet: starting it"
fi

# The architectures whose repository in the base lacks package $1.
missing() {
	for t in $TARGETS; do
		a=${t%%:*}
		git ls-tree --name-only "$base" "apk/$a/" | grep -q "^apk/$a/$1-[0-9]" || printf '%s ' "$a"
	done
}

# every: the packages built for every architecture. some: <package>@<arch>,
# a package built for that architecture only.
every="" some=""
for p in $PKGS; do
	why="" inputs=$(pkg_inputs "$p")
	# shellcheck disable=SC2086 # $inputs is a list of paths
	if [ "${REBUILD:-false}" = true ]; then
		why="rebuild requested"
	elif [ -z "$src" ]; then
		why="nothing published to compare with"
	elif ! git diff --quiet "$src" "$SHA" -- $inputs; then
		why="changed"
	fi
	lacking=""
	[ -n "$why" ] || [ -n "${BASE_SHA:-}" ] || lacking=$(missing "$p")
	if [ -n "$why" ]; then
		every="$every $p"
		log "$p: build ($why)"
	elif [ -n "$lacking" ]; then
		for a in $lacking; do some="$some $p@$a"; done
		log "$p: build for ${lacking% } only (not in the feed there)"
	else
		log "$p: unchanged"
	fi
done

# The base's newest build of package $1 for architecture $2 (<version>-r<release>).
published() {
	git ls-tree --name-only "$base" "apk/$2/" | sed -n "s|^apk/$2/$1-\([0-9].*\)\.apk\$|\1|p" | sort -V | tail -n 1
}
version=$(git show "$SHA:Cargo.toml" | sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p')
older=""
for p in $PKGS; do
	[ -n "$base" ] || break
	# The release steward.mk gives it at $SHA.
	# shellcheck disable=SC2046 # pkg_inputs is a list of paths
	new=$version-r$(TZ=UTC git log -1 --first-parent --format=%cd --date=format-local:%Y%m%d%H%M "$SHA" -- $(pkg_inputs "$p"))
	for t in $TARGETS; do
		a=${t%%:*}
		case " $every $some " in *" $p "* | *" $p@$a "*) ;; *) continue ;; esac
		old=$(published "$p" "$a")
		[ -n "$old" ] || continue
		if [ "$(printf '%s\n%s\n' "$old" "$new" | sort -V | tail -n 1)" != "$new" ]; then
			log "$p for $a would be $new, older than the published $old"
			older=1
		fi
	done
done
if [ -n "$older" ]; then
	log "refusing to publish older builds: main's commit dates go back (a rewrite that kept older dates?)"
	exit 1
fi

build="" targets=""
for p in $PKGS; do
	case " $every $some " in *" $p "* | *" $p@"*) build="$build $p" ;; esac
done
for t in $TARGETS; do
	a=${t%%:*} pkgs=""
	for p in $PKGS; do
		case " $every $some " in *" $p "* | *" $p@$a "*) pkgs="$pkgs $p" ;; esac
	done
	[ -z "$pkgs" ] || targets="$targets$(printf '{"arch":"%s","sdk":"%s","packages":"%s"},' "$a" "${t#*:}" "${pkgs# }")"
done
echo "packages=${build# }"
echo "targets=[${targets%,}]"
echo "base=$base"
echo "tip=$tip"
echo "source=$src"
