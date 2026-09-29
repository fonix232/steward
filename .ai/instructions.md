# Steward: agent instructions

Steward is a free and open-source network controller for OpenWrt 25.12 and later (apk only) that aims at the experience UniFi gives: install it on the router, and the access points, switches and other OpenWrt devices on the network are adopted and managed from one web interface. `steward-controller` (Rust) runs on the router, `steward-agent` (Rust) on every device, `steward-web` is the interface, and `steward` installs all three. This file is the single source of instructions for every AI tool: `AGENTS.md` (Codex), `CLAUDE.md`, `CODEX.md`, `GEMINI.md` and `.github/copilot-instructions.md` are symlinks to it. Edit only `.ai/instructions.md`. Skills live in `.ai/skills` and are linked from `.claude/skills`, `.agents/skills` (Codex, Gemini CLI), `.codex/skills` and `.github/skills`; agents live in `.ai/agents`, linked from `.claude/agents` and `.github/agents`.

## Layout

```
Cargo.toml            the cargo workspace; its version is every package's
crates/proto/         uCentral's messages (JSON-RPC 2.0), shared by agent and controller
crates/tls/           the device channel's TLS: the controller's CA and certificates, the agent's pin
crates/ubus/          a ubus client in Rust (no libubus), and uci::Transaction (rpcd's rollback)
steward-agent/        package: Makefile, crate (main.rs: connection loop; device.rs: what it reports), files/
steward-controller/   package: Makefile, crate (device registry, provisioning), files/
steward-web/          package: the web interface (www/)
steward/              package: installs the other three
steward.mk            shared by the Makefiles: version, release, how the workspace reaches the build
feed/                 the feed's public key and its GitHub Pages page
.github/scripts/      packages.sh (packages, architectures, pkg_inputs), feed-plan.sh, sdk-build.sh, publish.sh
.github/tests/        feed-scripts.sh: feed-plan.sh and publish.sh against a throwaway origin
.github/workflows/    feed.yml (build, publish), test.yml (cargo, shellcheck, the feed scripts)
.ai/                  this file, agents/, skills/, plans/, kanban/ (the board); local/ is untracked
```

## Design rules

- The agent owns only the UCI sections it creates, and marks them. It never rewrites a whole config file or stops a service it didn't start: LuCI, rpcd and the user's own settings keep working on a managed device. TIP's renderer does the opposite (for its example config it would stop uhttpd and rpcd), which is why Steward doesn't use it.
- Every configuration is applied with a rollback. `uci::Transaction` stages the changes in an rpcd session of its own, granted only the configs it names. It then runs `apply {rollback}` and, once the controller is reachable again, `confirm`. rpcd allows one pending rollback per device, so the agent's apply fails while a LuCI Save & Apply is pending: retry, don't force.
- uCentral's protocol and configuration schema (TIP's `wlan-ucentral-schema`) are the wire format; TIP's renderer is a reference for the mapping, not a dependency. Answer `configure` honestly: 0 only when applied as sent, 1 with the substitutions listed, 2 when nothing was applied.
- Neither end of the device channel can hold the other. The controller gives a new connection 10 s for its TLS handshake, 10 s for its WebSocket upgrade and then 10 s for `connect`, serves at most 32 connections that haven't sent `connect`, refuses messages and frames over 1 MiB (a state with 30 stations is about 23 KB), and drops a device that has sent nothing for 3 minutes (agents send their state every minute). What a device sends goes into the log cut to 128 bytes a value, and a longer serial is refused. The agent gives the controller 30 s to take a connection (TCP, TLS and the upgrade), and ends a session that has heard nothing from it for 3 minutes (it pings with every state), so its backoff takes over.
- The device channel is TLS with the controller's own certificate authority (`crates/tls`). An agent pins the CA on first use and afterwards accepts only a controller that chains to it; host names aren't checked, because devices reach the router by IP. Only a missing pin means first use: a pin file that can't be read or holds no certificate is an error, never a reason to pin again. Plain `ws://` exists only behind `--plaintext` (controller) and `--allow-plaintext` (agent), for development. The CA (`/etc/steward/tls/`) and the pin (`/etc/steward-agent/controller-ca.pem`) survive sysupgrade (keep.d): losing either strands every device.
- Crypto runs on `ring` (rcgen, rustls, webpki with `default-features = false`): aws-lc-rs, rustls' default, needs cmake and clang, which the SDK build lacks.
- The device side goes through ubus (`crates/ubus`), not shell commands. Answers keep their key order (serde_json `preserve_order`), because UCI section order matters.
- A UCI option must exist in its consumer (netifd, hostapd's ucode generator in `/usr/share/ucode/wifi/`, fw4). Unknown options are stored and silently dropped. Read state back with the tool's own `show` rather than trusting the UCI that was written.
- Current OpenWrt addresses radios by `phy=` (named in `board.json`, for example `wl0`), not `path=`, and phys are not named `phyN`. Code that finds radios handles both.

## Build and test

- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
- `shellcheck -s sh -x .github/scripts/*.sh .github/tests/*.sh`. CI runs the newest shellcheck, which flags more than older releases: locally, run `docker run --rm -v "$PWD:/mnt" -w /mnt koalaman/shellcheck:latest -s sh -x .github/scripts/*.sh .github/tests/*.sh`.
- The feed's plan and publish scripts: `sh .github/tests/feed-scripts.sh` (docker, seconds).
- One architecture's packages in the official SDK image: `.ai/skills/package-feed` (about 15 minutes under emulation on Apple Silicon).
- On a real device: `.ai/skills/device-testing`. Wire formats and ubus behaviour are settled there, not by reasoning.
- CI failure: `gh run list -R fonix232/steward`, then `gh run view <id> --log-failed`. Don't guess.

## Conventions

- **Tasks live on the board** in `.ai/kanban/`, committed with the work: one Markdown card per task, one folder per column. They're worked through the global `kanban` skill ([fonix232/skills](https://github.com/fonix232/skills/tree/main/kanban)), which also shows the board in a browser. Ticket IDs are `STW-<card number>` (card `009-….md` is `STW-9`).
  - A ticket's life: the agent writes the backlog; the user moves what they want next to Next; the user and the agent agree the sprint and move it to Ready to start; the agent works those tickets one at a time, in parallel where they don't touch the same code, each In progress while it's designed, developed, tested (unit, integration and end-to-end tests written) and verified by hand (on bifrost where it matters).
  - A finished ticket gets the whole verification flow below, then its own commit, which carries its card too: moved to In review, with its Progress written.
  - In review is end-to-end verification by someone other than the author (verifier agents, one per area): the whole suite, every criterion, and edge cases on the happy and unhappy paths. A verified ticket's card moves to Done inside its own commit (amended, or the history rewritten), and main is pushed when the user says so. A ticket that fails review goes back to In progress, with what failed in its Progress.
  - Board upkeep (new cards, reordering, the user's moves) never gets a ticket. It's committed on its own as `Board: <what changed>`, when the user asks.
- One commit per task. The subject starts with the ticket (`STW-9: Encrypted device channel`), and the body says what changed and how it was verified. No Co-Authored-By or other AI attribution trailers. Push only when asked.
- The feed publishes from CI only. gh-pages mirrors main, one commit per push to main, carrying `Source: main@<sha>` for the pushed head. There are no tags or releases. The signing key's private half is the `APK_SIGN_KEY` repository secret (with a copy in `~/.config/steward-feed/`), and its public half is `feed/steward.pem`.
- Every package has the cargo workspace's version. Its release is the date of the last main commit that changed its inputs, so a new file a package is built from goes into `pkg_inputs` in `packages.sh`. A published name-version never changes its bytes: a rebuild gets a new release, and the feed never replaces a published file.
- This repository is public: never commit real IPs, MACs, serials or keys.
  - Placeholders: `192.0.2.N` (management LAN), `198.51.100.N` (other networks), `00:00:5e:00:53:XX` (MACs).
  - A device's serial is its MAC without separators, so redact it from logs as `<serial>`.
  - Before committing, run `git diff --cached -U0 | grep "^+" | grep -nE "192\.168\.|10\.[0-9]+\.[0-9]+\.[0-9]+|([0-9a-f]{2}:){5}[0-9a-f]{2}"`, and check the commit message too.
- Device addresses and other local facts are in `.ai/local/devices.md`. Investigation notes and logs go under `.ai/local/`.
- Never restart, stop or reinstall a live service on a device without the user's yes.

## Skills, agents, plans

- `.ai/skills/package-feed`: packages, architectures, the SDK build on rustup, the gh-pages feed; adding a package or an architecture; debugging CI.
- `.ai/skills/device-testing`: testing on the test AP: static builds, the controller–agent loop on loopback, the ubus and rollback checks, installing SDK packages, cleaning up.
- `.ai/skills/ucentral`: the protocol and the configuration schema: where TIP's specs are, message shapes, `configure` answers, capabilities, and what Spike A found in TIP's renderer.
- `.ai/agents/steward-reviewer.agent.md`: reviews changes for device safety, leaked addresses, protocol honesty and build/feed breakage.
- `.ai/plans/roadmap.md`: what's done, and the order of what comes next.
