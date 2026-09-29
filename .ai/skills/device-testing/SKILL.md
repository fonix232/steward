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

Expect the controller to log its CA's fingerprint. The agent should log `pinned the controller's CA` on the first run and `matches the pin` afterwards, then `connected`, then `state` with load, memory and uptime. To check that the pin holds, start a controller with a fresh `--state-dir`: the agent must refuse it ("not the controller this device was pinned to"). An emptied pin file must stop it too ("holds no certificate"), not pin again, and so must a removed pin with the credential kept ("no pinned controller"): the agent doesn't connect at all. Give the controller `--control /tmp/stw/control.sock`, and the command-line client the same flag. Manual runs don't pass `--adopt-local` (the init script does), so a loopback agent stays pending as a remote one would. With it, only a loopback connection reporting the device's own serial is adopted by itself; any other serial stays pending. A new device is pending: `./steward-controller --control /tmp/stw/control.sock devices` lists it, and nothing is written to `controller/devices.json` for it. Past 64 pending devices, the oldest one that isn't connected is dropped, so devices that connect and hang up don't push out one that stays. When all 64 are connected, the oldest is dropped and its connection closed with it ("too many devices waiting for adoption"), so the controller's open file descriptors stay bounded. `adopt <serial>` delivers its credential (the agent logs `adopted`, and `/tmp/stw/agent/credential` is 0600). The next connection presents it; with the credential file altered, it's refused. To exercise `configure`, write `{"uuid": 5, "interfaces": []}` to `controller/configs/<serial>.json` and restart the agent: only an adopted device is sent it, and the agent answers with its honest status. Kill both and remove `/tmp/stw`.

Without `--controller`, the agent tries `wss://<default gateway>:15002`. With nothing listening there, expect `Connection refused` retries.

## Applying configurations

**Dry run first**: build the `render-check` example (`cargo build --release -p steward-render --example render-check` in the static-build container), copy it and a configuration to the device, and run `render-check config.json`. It renders against the device's `wireless`, `network` and board.json, with each radio's HT modes from `iwinfo info` (it prints them), stages the result in an rpcd session of its own, prints the staged changes (keys redacted), the rejections and the substitutions, then drops the session: nothing is applied. Take `md5sum` of both configs before and check them after, with nothing in `uci changes`.

A configuration whose values match the device's current ones applies nothing, so the no-op path is safe to run any time. The agent tells by reading the staged configs back through its session (`Transaction::get`) and comparing them with what it read before, because rpcd stages a change for a list set to the value it has (an owned VLAN's `ports`, say). Anything that changes wireless reloads the radios and drops the AP's clients for a few seconds; a network change (a new VLAN) reloads the bridge. Both are restarts of a live service: ask the user first.

"Nothing to change" also needs no apply pending on the device: a LuCI Save & Apply, or the agent's own that it couldn't roll back, may revert what the configuration matched. A pending apply makes the configuration busy, retried like a refused apply. The agent asks with `Transaction::pending`: rpcd's `confirm` from a fresh session, which has applied nothing. In rpcd's `uci.c` (`rpc_uci_confirm`), confirm answers NoData (`ubus` prints "No response") when nothing is pending and PermissionDenied when another session applied, both before touching anything; only the applying session's confirm confirms. The side with nothing pending is safe to check any time, with the call itself or the CLI:

    sid=$(ubus call session create '{"timeout": 30}' | jsonfilter -e '@.ubus_rpc_session')
    ubus call uci confirm "{\"ubus_rpc_session\": \"$sid\"}"      # No response: nothing pending
    ubus call session destroy "{\"ubus_rpc_session\": \"$sid\"}"

The pending side needs an apply, which the rollback check below makes on its throwaway config.

The live test (done 2026-09-29, with the user's yes):
1. **Confirm:** add a test SSID; the agent applies and confirms, and it's on air (`iw dev`).
2. **Rollback:** change it and kill the controller right after it sends; `running.json` records `rolled_back`. On 2026-09-29 rpcd's timer reverted it after 60 s. Since the second review the agent rolls it back itself (`uci rollback` from its session) once its tries fail, by 40 s after the apply, before it answers 2.
3. **No loop:** restart the controller; it re-sends that uuid, and the agent refuses it.
4. **Cleanup:** an empty configuration removes the SSID.

Finish by checking that `/etc/config/wireless` equals a copy taken before (`cmp`), with nothing in `uci changes`.

## The API

Run the controller with `--web-listen 127.0.0.1:8443` (loopback only) and use `curl -sk` on the device. Signing in needs an OpenWrt account whose password you know, with rpcd's access group `steward`. Don't use root's; add temporary rpcd logins and remove them afterwards (rpcd reads logins from UCI and the groups from `/usr/share/rpcd/acl.d/` on every sign-in, so nothing restarts). Take `md5sum /etc/config/rpcd` before, and compare it after:

    sec=$(uci add rpcd login); uci set rpcd.$sec.username=stwtest
    uci set rpcd.$sec.password="$(uhttpd -m 'a-test-password')"
    uci add_list rpcd.$sec.read='*'; uci add_list rpcd.$sec.write='*'; uci commit rpcd
    ...
    uci delete rpcd.$sec; uci commit rpcd

Add two more the same way: one with only `read 'steward'`, and one with only a group that doesn't exist.

Without the package, rpcd knows no `steward` group, so every sign-in gets 403, root's included. Put the package's ACL file in place for the test (`steward-controller/files/steward-controller.acl.json` as `/usr/share/rpcd/acl.d/steward-controller.json`) and remove it afterwards. To touch nothing outside `/tmp`, sign in through rpcd instead and grant the session what rpcd gives a login whose lists match the group, then hand its token to the API (JSON arguments in a script file on the device, as below):

    ubus call session login '{"username":"stwtest","password":"a-test-password"}'
    ubus call session grant '{"ubus_rpc_session":"<token>","scope":"access-group","objects":[["steward","read"]]}'

`ubus call session access '{"ubus_rpc_session":"<token>","scope":"access-group"}'` lists a session's groups.

Check these:
- a wrong password and a missing token get 401; a login without the group gets 403, and its rpcd session is gone;
- rpcd's unauthenticated session (`Bearer 00000000000000000000000000000000`) gets 401 on every route;
- `/api/devices` lists the pending agent, without `credential_sha256`;
- with `read 'steward'` only, the GETs and the event stream work, while `adopt`, `forget` and `PUT .../config` get 403, and logout works;
- `POST .../adopt` adopts it;
- `PUT .../config` returns a uuid, and the agent receives it;
- `curl -N /api/events` shows `adopted`, `configuration` and `answer`;
- after `POST /api/logout`, the token gets 401, and an open `curl -N /api/events` ends within 30 s;
- the cookie (`-H "Cookie: steward_session=<token>"`) is set with `Path=/api`, and GETs work with it alone; a change by cookie gets 403 without `-H 'X-Steward: 1'` and works with it, while a bearer token needs no header;
- a change with a foreign `Origin` (`https://evil.example`, or the router's own address without the port, LuCI's) or with `Sec-Fetch-Site: same-site` gets 403, whatever token it carries, and an `Origin` of `https://` and the Host curl sent works;
- a connection that sends nothing (`nc`) is closed after 10 s, as are one that finished its TLS handshake (`openssl s_client`) without a request, and an idle one 10 s after its answer; with 40 idle ones open, a request waits until the first 32 are closed. Watch closes in `netstat -tn`: `s_client` doesn't exit when the server closes, and BusyBox `sleep` takes whole seconds;
- run under `ulimit -n 20`, 40 idle connections make accept fail (EMFILE): `api accept` is logged once a second, not in a loop;
- a sign-in or a `PUT .../config` whose body trickles in (a `Content-Length` larger than what's sent, through `openssl s_client`) gets 408 10 s after its headers, and a sign-in body over 4 KiB gets 413 without rpcd being asked.

## ubus and rpcd checks

- **The client against the CLI**: build the `ubus-call` example (`cargo build --release --example ubus-call` in the container above). Compare its output with `ubus call` for the same object, method and arguments, as JSON, in order: `uci get`, `network.interface dump`, `network.device status`, `luci-rpc getHostHints`. Pass JSON arguments through a script file, because ssh strips the quotes.
- **Rollback**: `touch /etc/config/steward_test`, run the `rollback-check` example with `steward_test`, and expect:
  - an unconfirmed apply reverted within 14 s
  - another session's apply refused while that one is pending (PermissionDenied), which the agent retries
  - `Transaction::pending` true while it's pending, false after the revert
  - an apply rolled back by its own session: reverted at once, nothing pending
  - a confirmed apply kept
  - `network` refused (PermissionDenied)
  - a list set to the value it has: a change staged, but it reads back the same through the session
  - a new value staged: the session reads it, the config doesn't

  Then remove `/etc/config/steward_test`. No service watches that config, so nothing restarts.

## SDK-built packages

    apk add --allow-untrusted ./steward-agent-*.apk ./steward-controller-*.apk
    steward-agent --version; ldd /usr/sbin/steward-agent     # ld-musl-aarch64 and libgcc_s only
    apk del steward-agent steward-controller

The packages' post-install enables their init scripts, but `option enabled '0'` keeps them from starting. Removal takes the UCI config and the feed list with it, but leaves `/etc/steward/` (each device's configuration), which the controller's init script creates when the controller starts, and `/etc/steward-agent/`, which the agent writes (its pinned CA and credential). That's on purpose: it's the controller's and the agent's data, not the package's. So "removes cleanly" means nothing else is left: check afterwards that `/etc/apk/repositories.d/` and `/etc/config/` have no `steward` entries, and remove `/etc/steward/` and `/etc/steward-agent/` by hand if the test started the controller or the agent.
