// hostapd-check.uc <ssids.json>: what OpenWrt's own wifi scripts write into hostapd's
// configuration for the SSIDs `render-check --hostapd` planned. Run with `ucode -S`: it
// validates each SSID's options as netifd does (aliases, types, defaults) and runs the AP
// generator (`wifi.ap`), then prints the lines. Nothing is applied and nothing restarts.
//
// Output is the generator's, less the lines every AP has. Secrets are redacted, and the files
// the generator writes (/var/run/hostapd-check<n>.*) are removed.
'use strict';

import { validate } from 'wifi.validate';
import * as ap from 'wifi.ap';
import { dump_config, flush_config } from 'wifi.common';
import * as fs from 'fs';

const COMMON = [ 'interface', 'bss', 'bssid', 'ctrl_interface', 'bridge', 'snoop_iface', 'ssid2', 'wmm_enabled', 'dtim_period', 'start_disabled' ];
const SECRET = /(secret|passphrase|wpa_psk|_kh|password)/;

for (let i, ssid in json(fs.readfile(ARGV[0]))) {
	let config = { ...ssid.options, ifname: 'check' + i, macaddr: '00:00:5e:00:53:0' + i };
	for (let k in [ 'device', 'mode', 'network', 'steward', 'disabled' ])
		delete config[k];
	validate('iface', config);

	flush_config();
	ap.generate(i, { phy: 'check', phy_suffix: '', config: { band: ssid.band } }, config, [], [], {});

	printf('== %s (%s)\n', ssid.options.ssid, ssid.band);
	for (let line in split(dump_config(), '\n')) {
		let kv = split(line, '=', 2), key = kv[0], value = kv[1];
		if (!length(line) || substr(line, 0, 1) == '#' || key in COMMON)
			continue;
		if (match(key, SECRET))
			value = '<redacted>';
		// "<address> <secret>": hostapd refuses the line without its secret, so say so.
		if (key == 'radius_das_client') {
			let parts = split(value, ' ');
			value = parts[0] + (length(parts) > 1 ? ' <redacted>' : ' <NO SECRET: hostapd refuses this line>');
		}
		printf('%s=%s\n', key, value);
	}
	// The generator writes these next to hostapd's own; the PSK files hold the key.
	for (let ext in [ 'vlan', 'psk', 'sae', 'maclist' ])
		fs.unlink(`/var/run/hostapd-check${i}.${ext}`);
}
