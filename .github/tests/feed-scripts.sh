#!/bin/sh
# Exercises feed-plan.sh and publish.sh end to end against a throwaway origin:
# which packages each kind of change rebuilds, and for which architectures,
# that a published file never changes its bytes, what gh-pages holds
# afterwards, and the history cases (a re-run, a rewritten main, unrelated
# histories, a gh-pages that moved since the plan, a pull request). No SDK
# and no real remote: the build step makes stand-in packages with apk mkpkg,
# and a stand-in for docker indexes and signs them with a throwaway key.
#
#   sh .github/tests/feed-scripts.sh       (from the repository's top; needs docker)
#
# It tests the working tree as it is, uncommitted changes included, in an
# alpine container (which installs git, dash and openssl from Alpine's
# mirrors). CI runs it too (test.yml).
set -eu

if [ -z "${FEED_TEST_INNER:-}" ]; then
	top=$(git rev-parse --show-toplevel)
	tmp=$(mktemp -d)
	trap 'rm -rf "$tmp"' EXIT
	(cd "$top" && git ls-files -z | tar --null -T - -cf "$tmp/src.tar")
	docker run --rm -e FEED_TEST_INNER=1 -v "$tmp/src.tar:/src.tar:ro" \
		-v "$top/.github/tests/feed-scripts.sh:/feed-scripts.sh:ro" alpine:latest sh /feed-scripts.sh
	exit
fi

# Inside the container from here on.
apk add -q git dash coreutils findutils openssl >/dev/null
T=/t
mkdir -p "$T/bin" "$T/keys"
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.invalid
export GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.invalid
git config --global init.defaultBranch main
git config --global advice.detachedHead false
git init -q --bare "$T/origin.git"
git init -q "$T/repo"
cd "$T/repo"
tar -xf /src.tar
git remote add origin "$T/origin.git"
openssl ecparam -name prime256v1 -genkey -noout -out "$T/sign.key" 2>/dev/null
openssl ec -in "$T/sign.key" -pubout -out "$T/keys/test.pem" 2>/dev/null
APK_SIGN_KEY=$(cat "$T/sign.key")
export APK_SIGN_KEY

# publish.sh runs `docker run ... <sdk image> sh -c '... apk mkndx ...'`:
# this does the same with the container's own apk.
cat > "$T/bin/docker" << 'EOF'
#!/bin/sh
set -eu
apkdir="" keydir=""
while [ $# -gt 0 ]; do
	case $1 in
	run | --rm) shift ;;
	-e) export "${2?}"; shift 2 ;;
	-v)
		case $2 in *:/apk) apkdir=${2%:/apk} ;; *:/keys:ro) keydir=${2%:/keys:ro} ;; esac
		shift 2
		;;
	*) break ;;
	esac
done
for a in $REINDEX; do
	ls "$apkdir/$a"/*.apk > /dev/null 2>&1 || continue
	cd "$apkdir/$a" && rm -f packages.adb
	apk mkndx --allow-untrusted --sign "$keydir/apk-private.pem" --output packages.adb ./*.apk > /dev/null
done
EOF
chmod +x "$T/bin/docker"
PATH=$T/bin:$PATH

fails=0
check() { # <what> <expected> <actual>
	if [ "$2" = "$3" ]; then
		echo "ok   $1"
	else
		echo "FAIL $1"
		echo "       expected: $2"
		echo "       got:      $3"
		fails=$((fails + 1))
	fi
}

N=0
commit() { # <message>: commit everything, dated an hour after the last, and push to main
	N=$((N + 1))
	d="$((1790812800 + N * 3600)) +0000"
	git add -A
	GIT_AUTHOR_DATE=$d GIT_COMMITTER_DATE=$d git commit -q --allow-empty -m "$1"
	git push -q -f origin HEAD:refs/heads/main 2> /dev/null
}

# A stand-in for sdk-build.sh: one package, numbered as steward.mk numbers it.
# Its contents differ on every build, as a rebuilt binary's may.
fake_build() { # <arch> <package>
	v=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' Cargo.toml)
	# shellcheck disable=SC1091,SC2046
	r=$(. .github/scripts/packages.sh && TZ=UTC git log -1 --first-parent --format=%cd \
		--date=format-local:%Y%m%d%H%M -- $(pkg_inputs "$2"))
	case $2 in steward | steward-web) arch=noarch ;; *) arch=$1 ;; esac
	f=$T/files/$1/$2
	rm -rf "$f" && mkdir -p "$f/usr/share/stw" "$T/new/$1"
	od -An -N16 -tx1 /dev/urandom > "$f/usr/share/stw/$2"
	apk mkpkg -I "name:$2" -I "version:$v-r$r" -I "arch:$arch" -F "$f" -o "$T/new/$1/$2-$v-r$r.apk"
}

plan() { # [feed-plan.sh environment...]: plan main@HEAD
	env "$@" dash .github/scripts/feed-plan.sh "$(git rev-parse HEAD)" > "$T/plan.out" 2> "$T/plan.err"
	planned=$(sed -n 's/^packages=//p' "$T/plan.out")
	base=$(sed -n 's/^base=//p' "$T/plan.out")
	tip=$(sed -n 's/^tip=//p' "$T/plan.out")
	# The build matrix, one "<arch>:<packages>" line per job.
	matrix=$(sed -n 's/^targets=//p' "$T/plan.out" | grep -o '"arch":"[^"]*","sdk":"[^"]*","packages":"[^"]*"' |
		sed 's/^"arch":"\([^"]*\)","sdk":"[^"]*","packages":"\([^"]*\)"$/\1:\2/')
	arches=$(printf '%s\n' "$matrix" | sed -n 's/:.*//p' | tr '\n' ' ')
}
job() { printf '%s\n' "$matrix" | sed -n "s/^$1://p"; } # <arch>: the packages its job builds

publish() { # build what the plan chose, per architecture, then publish.sh as feed.yml runs it
	rm -rf "$T/new"
	for a in $arches; do for p in $(job "$a"); do fake_build "$a" "$p"; done; done
	st=0
	BASE=$base TIP=$tip dash .github/scripts/publish.sh "$(git rev-parse HEAD)" "$T/new" 2> "$T/publish.err" || st=$?
	[ "$st" = 0 ] || cat "$T/publish.err" >&2
	git fetch -q origin "+refs/heads/gh-pages:refs/remotes/origin/gh-pages"
	return "$st"
}

run() { plan && publish; }

pages=refs/remotes/origin/gh-pages
tree() { git ls-tree -r --name-only "$pages"; }
count() { tree | grep -c "^apk/$1/$2-[0-9]" || true; }
newest() { tree | grep "^apk/$1/$2-[0-9]" | sort -V | tail -n 1; }
blob() { git rev-parse "$pages:$1"; }
source_of() { git log -1 --format=%B "$1" | sed -n 's/^Source: main@//p'; }
# shellcheck disable=SC1091 # the throwaway repository's copy
all_arches() { (. .github/scripts/packages.sh && for t in $TARGETS; do printf '%s ' "${t%%:*}"; done); }
release_of() { # <commit>: its date as a release number
	TZ=UTC git log -1 --format=%cd --date=format-local:%Y%m%d%H%M "$1"
}
indexes_ok() { # every repository's index is signed by the test key and lists its packages
	for a in $(all_arches); do
		git show "$pages:apk/$a/packages.adb" > "$T/idx.adb"
		apk --keys-dir "$T/keys" verify "$T/idx.adb" > /dev/null 2>&1 || { echo "$a: bad signature"; return; }
		n=$(apk adbdump "$T/idx.adb" | grep -c '^  - name:')
		[ "$n" = "$(tree | grep -c "^apk/$a/.*\.apk$")" ] || { echo "$a: index lists $n"; return; }
	done
	echo ok
}
ALL="steward steward-agent steward-controller steward-web"

echo "== 1. first run: no gh-pages yet"
commit "initial"
c1=$(git rev-parse HEAD)
run
check "builds every package" "$ALL" "$planned"
check "no base to publish onto" "" "$base"
check "gh-pages is one root commit" "1" "$(git rev-list --count "$pages")"
check "its Source line names main@HEAD" "$c1" "$(source_of "$pages")"
for a in $(all_arches); do
	check "apk/$a holds the 4 packages" "4" "$(tree | grep -c "^apk/$a/.*\.apk$")"
done
check "every index signed and complete" "ok" "$(indexes_ok)"
check "steward.pem, index.html and .nojekyll at the top" "3" "$(tree | grep -cxE 'steward\.pem|index\.html|\.nojekyll')"
check "agent's release is the commit's date" "apk/aarch64_cortex-a53/steward-agent-0.1.0-r$(release_of "$c1").apk" \
	"$(newest aarch64_cortex-a53 steward-agent)"

echo "== 2. a change no package is built from"
idx=$(blob apk/aarch64_cortex-a53/packages.adb)
prev=$(git rev-parse "$pages")
echo "docs" >> README.md
commit "docs only"
run
check "builds nothing" "" "$planned"
check "publishes onto the tip" "$prev" "$base"
check "still gets its own gh-pages commit" "$prev" "$(git rev-parse "$pages^")"
check "whose Source line names it" "$(git rev-parse HEAD)" "$(source_of "$pages")"
check "the index is kept byte for byte (not re-signed)" "$idx" "$(blob apk/aarch64_cortex-a53/packages.adb)"

echo "== 3. a change to a crate"
echo "// test" >> crates/proto/src/lib.rs
commit "crates"
c3=$(git rev-parse HEAD)
run
check "builds agent and controller" "steward-agent steward-controller" "$planned"
check "for every architecture" "$(all_arches)" "$arches"
check "two agent builds kept" "2" "$(count mipsel_24kc steward-agent)"
check "steward-web not rebuilt" "1" "$(count mipsel_24kc steward-web)"
check "the new agent's release is this commit's date" \
	"apk/mipsel_24kc/steward-agent-0.1.0-r$(release_of "$c3").apk" "$(newest mipsel_24kc steward-agent)"
check "every index signed and complete" "ok" "$(indexes_ok)"

echo "== 4. steward-web's own files"
echo "<!-- test -->" >> steward-web/www/index.html
commit "web"
run
check "builds steward-web only" "steward-web" "$planned"

echo "== 5. the feed's key (the agent carries it)"
echo "" >> feed/steward.pem
commit "key"
run
check "builds steward-agent only" "steward-agent" "$planned"

echo "== 6. packages.sh alone (the lists, not how anything is built)"
echo "# test" >> .github/scripts/packages.sh
commit "packages.sh comment"
run
check "builds nothing" "" "$planned"

echo "== 7. sdk-build.sh (how every package is built)"
old=$(newest x86_64 steward-agent)
oldblob=$(blob "$old")
echo "# test" >> .github/scripts/sdk-build.sh
commit "sdk-build.sh"
run
check "builds every package" "$ALL" "$planned"
check "for every architecture" "$(all_arches)" "$arches"
check "a rebuilt package gets a new version (no file name reused for other bytes)" \
	"new name" "$([ "$(newest x86_64 steward-agent)" != "$old" ] && echo "new name" || echo "same name $old, blob $oldblob -> $(blob "$old")")"

echo "== 8. the last KEEP=5 builds are kept"
for i in 1 2 3 4 5 6; do
	echo "<!-- $i -->" >> steward-web/www/index.html
	commit "web $i"
	run
done
check "5 steward-web builds in each repository" "5 5 5 5" \
	"$(for a in $(all_arches); do printf '%s ' "$(count "$a" steward-web)"; done | sed 's/ $//')"
check "the newest is this commit's" "apk/arm_cortex-a7_neon-vfpv4/steward-web-0.1.0-r$(release_of HEAD).apk" \
	"$(newest arm_cortex-a7_neon-vfpv4 steward-web)"
check "every index signed and complete" "ok" "$(indexes_ok)"

echo "== 9. a new architecture"
before=$(blob "$(newest aarch64_cortex-a53 steward-agent)")
sed -i 's/^TARGETS="/TARGETS="aarch64_generic:armsr-armv8 /' .github/scripts/packages.sh
commit "new architecture"
run
check "builds every package" "$ALL" "$planned"
check "for the new architecture only" "aarch64_generic " "$arches"
check "which gets all 4" "$ALL" "$(job aarch64_generic)"
check "says why" "4" "$(grep -c 'build for aarch64_generic only (not in the feed there)' "$T/plan.err")"
check "the new repository holds the 4 packages" "4" "$(tree | grep -c '^apk/aarch64_generic/.*\.apk$')"
check "the other architectures are not rebuilt (their bytes stay)" "$before" \
	"$(blob "$(newest aarch64_cortex-a53 steward-agent)")"
check "every index signed and complete" "ok" "$(indexes_ok)"

echo "== 10. an architecture dropped"
sed -i 's/ x86_64:x86-64//' .github/scripts/packages.sh
commit "drop x86_64"
run
check "builds nothing" "" "$planned"
check "apk/x86_64 removed" "0" "$(tree | grep -c '^apk/x86_64/' || true)"
check "every index signed and complete" "ok" "$(indexes_ok)"

echo "== 11. the workflow re-run for the same commit"
echo "// re-run" >> crates/proto/src/lib.rs
commit "crates, then re-run"
run
agent=$(newest mipsel_24kc steward-agent)
agentblob=$(blob "$agent")
n=$(git rev-list --count "$pages")
prev=$(git rev-parse "$pages^")
run
check "publishes onto the commit before" "$prev" "$base"
check "replaces the tip rather than adding one" "$n" "$(git rev-list --count "$pages")"
check "one gh-pages commit for it" "1" \
	"$(git log --format=%B "$pages" | grep -c "^Source: main@$(git rev-parse HEAD)$")"
check "builds agent and controller again" "steward-agent steward-controller" "$planned"
check "but publishes the bytes the replaced commit published" "$agentblob" "$(blob "$agent")"
check "says so" "8" "$(grep -c '^publish: restored .*(the new build of it is dropped)$' "$T/publish.err")"
check "every index signed and complete" "ok" "$(indexes_ok)"

echo "== 12. a gh-pages commit without a Source line (made by hand)"
git clone -q "$T/origin.git" "$T/other"
(cd "$T/other" && git checkout -q gh-pages && echo x > by-hand && git add by-hand &&
	git commit -q -m "by hand" && git push -q origin gh-pages)
mirror=$(git rev-parse "$pages")
echo "docs" >> README.md
commit "after a hand commit"
run
check "skipped: publishes onto the mirror commit below it" "$mirror" "$base"
check "the hand commit is dropped" "0" "$(tree | grep -c '^by-hand$' || true)"

echo "== 13. main rewritten"
git reset -q --hard "$c3"
echo "// rewritten" >> crates/proto/src/lib.rs
commit "rewritten"
run
check "goes back to the mirror of the last shared commit" "$c3" "$(source_of "$base")"
check "says so" "1" "$(grep -c 'main was rewritten' "$T/plan.err")"
check "builds what changed since it" "steward-agent steward-controller" "$planned"
check "gh-pages: the new commit on top of it" "$base" "$(git rev-parse "$pages^")"
check "every index signed and complete" "ok" "$(indexes_ok)"

echo "== 14. main with no history in common"
git checkout -q --orphan fresh
commit "unrelated"
run
check "starts over: builds everything" "$ALL" "$planned"
check "onto nothing" "" "$base"
check "gh-pages is one root commit again" "1" "$(git rev-list --count "$pages")"

echo "== 15. gh-pages moved after the plan"
echo "docs" >> README.md
commit "race"
plan
(cd "$T/other" && git fetch -q origin && git checkout -q -B gh-pages origin/gh-pages &&
	git commit -q --allow-empty -m "someone else" && git push -q origin gh-pages)
if publish > /dev/null 2>&1; then rc=0; else rc=$?; fi
check "the push is refused" "refused" "$([ "$rc" != 0 ] && echo refused || echo "pushed")"
check "gh-pages keeps the other commit" "someone else" \
	"$(git ls-remote "$T/origin.git" refs/heads/gh-pages | cut -f1 | xargs git log -1 --format=%s)"

echo "== 16. a pull request"
git fetch -q origin
echo "// pr" >> crates/ubus/src/lib.rs
git add -A && git commit -q -m "pr"
plan BASE_SHA="$(git rev-parse HEAD^)"
check "compares with the base branch" "steward-agent steward-controller" "$planned"
check "publishes nothing (no base, no tip)" " " "$base $tip"
git reset -q --hard HEAD^

echo "== 17. rebuild requested"
before=$(git ls-tree -r "$pages" apk/)
plan REBUILD=true
check "builds every package" "$ALL" "$planned"
check "for every architecture" "$(all_arches)" "$arches"
publish
check "publishes none of it: apk/ stays byte for byte, indexes included" "$before" "$(git ls-tree -r "$pages" apk/)"
check "says so" "16" "$(grep -c '^publish: kept .* as published (the new build of it is dropped)$' "$T/publish.err")"
check "its gh-pages commit names it" "$(git rev-parse HEAD)" "$(source_of "$pages")"

echo "== 18. a rewritten main whose dates go back"
before=$(git rev-parse "$pages")
echo "// backdated" >> crates/proto/src/lib.rs
git add -A
d="1790812800 +0000" # before every commit above, so before every published build
GIT_AUTHOR_DATE=$d GIT_COMMITTER_DATE=$d git commit -q -m "backdated"
git push -q -f origin HEAD:refs/heads/main 2> /dev/null
if dash .github/scripts/feed-plan.sh "$(git rev-parse HEAD)" > "$T/plan.out" 2> "$T/plan.err"; then rc=0; else rc=$?; fi
check "the plan refuses it" "refused" "$([ "$rc" != 0 ] && echo refused || echo planned)"
check "naming each older build" "$(($(all_arches | wc -w) * 2))" \
	"$(grep -c '^plan: steward-\(agent\|controller\) for .* would be .*, older than the published ' "$T/plan.err")"
check "gh-pages is left alone" "$before" "$(git ls-remote "$T/origin.git" refs/heads/gh-pages | cut -f1)"

echo
if [ "$fails" = 0 ]; then echo "all passed"; else echo "$fails failed"; exit 1; fi
