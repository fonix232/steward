---
name: device-testing
description: Testing Steward on a real OpenWrt device (the test AP, bifrost). Covers static aarch64 builds, the controller–agent loop on loopback, the ubus client and rollback checks, installing SDK-built packages, and cleaning up. Use before claiming anything about ubus, rpcd, UCI, netifd or hostapd behaviour, and before pushing agent changes.
---

# Testing on a device

The test AP is **bifrost**: a Linksys E8450 (mediatek/mt7622, package architecture `aarch64_cortex-a53`) running OpenWrt SNAPSHOT, reached as root over SSH. Its address and the other devices are in `.ai/local/devices.md`, which is not tracked. Test on bifrost only. The other APs and the router need the user's yes.

## Rules

- Never restart, stop or reinstall a live service without the user's yes. Everything below runs from `/tmp` and touches nothing that's running.
- Leave nothing behind: remove the binaries and `/tmp` directories, any throwaway config (`/etc/config/steward_test`), and any installed Steward packages.
- Logs contain the device's serial (its MAC). Redact it (`sed "s/$serial/<serial>/g"`) before showing or committing anything.
- The device has no `scp`/`sftp` server by default. Copy with `cat file | ssh root@<ap> 'cat > /tmp/file'`.

## Static builds (quick)

A static aarch64 build runs on the device without its libraries, and builds natively on Apple Silicon:

    docker run --rm --platform linux/arm64 -v "$PWD:/src" -w /src \
        -v steward-cargo-registry:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/src/target-musl \
        rust:alpine sh -c 'apk add -q musl-dev && cargo build --release -p steward-agent -p steward-controller'

The output is in `target-musl/release/` (ignored by git).

## The controller–agent loop

Run both ends on the device, the controller on loopback, so nothing is exposed:

    cd /tmp/stw
    ./steward-controller --listen 127.0.0.1:15002 --state-dir /tmp/stw/controller > ctl.log 2>&1 &
    ./steward-agent --controller wss://127.0.0.1:15002 --state-dir /tmp/stw/agent > agent.log 2>&1 &

Expect the controller to log its CA's fingerprint. The agent should log `pinned the controller's CA` on the first run and `matches the pin` afterwards, then `connected`, then `state` with load, memory and uptime. To check that the pin holds, start a controller with a fresh `--state-dir`: the agent must refuse it ("not the controller this device was pinned to"). An emptied pin file must stop it too ("holds no certificate"), not pin again, and so must a removed pin with the credential kept ("no pinned controller"): the agent doesn't connect at all. Give the controller `--control /tmp/stw/control.sock`, and the command-line client the same flag. A new device is pending: `./steward-controller --control /tmp/stw/control.sock devices` lists it, and nothing is written to `controller/devices.json` for it. Past 64 pending devices, the oldest one that isn't connected is dropped, so devices that connect and hang up don't push out one that stays. When all 64 are connected, the oldest is dropped and its connection closed with it ("too many devices waiting for adoption"), so the controller's open file descriptors stay bounded. `adopt <serial>` delivers its credential (the agent logs `adopted`, and `/tmp/stw/agent/credential` is 0600). The next connection presents it; with the credential file altered, it's refused. To exercise `configure`, write `{"uuid": 5, "interfaces": []}` to `controller/configs/<serial>.json` and restart the agent: only an adopted device is sent it, and the agent answers with its honest status. Kill both and remove `/tmp/stw`.

Without `--controller`, the agent tries `wss://<default gateway>:15002`. With nothing listening there, expect `Connection refused` retries.

## ubus and rpcd checks

- **The client against the CLI**: build the `ubus-call` example (`cargo build --release --example ubus-call` in the container above). Compare its output with `ubus call` for the same object, method and arguments, as JSON, in order: `uci get`, `network.interface dump`, `network.device status`, `luci-rpc getHostHints`. Pass JSON arguments through a script file, because ssh strips the quotes.
- **Rollback**: `touch /etc/config/steward_test`, run the `rollback-check` example with `steward_test`, and expect:
  - an unconfirmed apply reverted within 14 s
  - a confirmed apply kept
  - `network` refused (PermissionDenied)

  Then remove `/etc/config/steward_test`. No service watches that config, so nothing restarts.

## SDK-built packages

    apk add --allow-untrusted ./steward-agent-*.apk ./steward-controller-*.apk
    steward-agent --version; ldd /usr/sbin/steward-agent     # ld-musl-aarch64 and libgcc_s only
    apk del steward-agent steward-controller

The packages' post-install enables their init scripts, but `option enabled '0'` keeps them from starting. Removal takes the UCI config and the feed list with it, but leaves `/etc/steward/` (each device's configuration), which the controller's init script creates when the controller starts, and `/etc/steward-agent/`, which the agent writes (its pinned CA and credential). That's on purpose: it's the controller's and the agent's data, not the package's. So "removes cleanly" means nothing else is left: check afterwards that `/etc/apk/repositories.d/` and `/etc/config/` have no `steward` entries, and remove `/etc/steward/` and `/etc/steward-agent/` by hand if the test started the controller or the agent.
