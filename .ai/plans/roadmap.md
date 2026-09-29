# Roadmap

## Done

- Packages (`steward`, `steward-agent`, `steward-controller`, `steward-web`) and the signed per-architecture feed on gh-pages: aarch64_cortex-a53, arm_cortex-a7_neon-vfpv4, mipsel_24kc, x86_64.
- `crates/proto`: uCentral's messages.
- `crates/ubus`: a ubus client, and `uci::Transaction` (an rpcd session with rollback and confirm). Checked on bifrost.
- The connection loop:
  - the agent: connect, state every minute, reconnects with backoff, and finds the controller on the default gateway
  - the controller: device registry, provisioning a stored configuration by uuid

## Next, in order

1. **TLS and adoption.**
   - Done: the controller creates its own CA on first start and serves `wss://`; agents pin it on first use.
   - Done: a new device waits as pending; adopting it (`steward-controller adopt`) hands it a credential that later connections present. The web interface will adopt through the same controller operations.
2. **The configuration renderer.**
   - Render a subset of uCentral's schema into sections the agent owns and marks: radios and SSIDs (done, `crates/render`), VLANs, ports.
   - Apply it through `uci::Transaction`, and confirm once the controller answers again.
   - Persist the running uuid, and answer `configure` 0, 1 or 2 honestly.
3. **Controller storage and API**: devices, configurations and state persisted on the router. A REST and WebSocket API for the interface.
4. **steward-web**: devices, adoption, clients, and configuration editing.
5. **State and topology**:
   - clients (hostapd over ubus)
   - LLDP neighbours (lldpd)
   - per-device and per-client traffic (nlbwmon or conntrack accounting)
   - the topology built from them
6. **IDS/IPS**: Suricata, which the OpenWrt feeds lack (package it), with alerts reported to the controller.
7. **More devices and features**:
   - switches (DSA VLANs, PoE)
   - router features (firewall, DHCP, WireGuard)
   - firmware upgrades (owut / attended sysupgrade)
