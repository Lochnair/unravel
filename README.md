# unRAVEL

If your internet connection changes speed throughout the day, finding a good bandwidth setting for CAKE can be a bit of a pain. Set it too high and latency goes up when the connection slows down. Set it too low and you're giving up bandwidth when there's more available. unRAVEL watches traffic load and measures delay to remote hosts (reflectors), then adjusts the upload and download shapers as conditions change.

RAVEL stands for "Rate Adaptation Via Event-driven Logic". OpenWrt is the main target, though unRAVEL also runs on other Linux systems with CAKE configured.

This project started as a Rust port of [sqm-autorate](https://github.com/sqm-autorate/sqm-autorate), the original Lua project.

## Before you start

You'll need a Linux router with CAKE already configured on the interfaces you want to control. On OpenWrt, you can set this up through SQM. unRAVEL changes the bandwidth of an existing root CAKE qdisc, so your SQM setup still handles things like overhead compensation and traffic redirection to the download IFB.

Check the interfaces before starting (replace these names with yours):

```sh
tc qdisc show dev wan
tc qdisc show dev ifb4wan
```

Both should show CAKE at the root if you're controlling both directions. Run unRAVEL as root, since it needs access to raw ICMP sockets and traffic control. Stop any other autorate program controlling the same qdiscs.

## Installation

### OpenWrt package

On OpenWrt 25.12, you can install unRAVEL from [my APK feed](https://openwrt.lochnair.net). Run this as root on your router; it reads your package architecture from `/etc/openwrt_release`:

```sh
. /etc/openwrt_release
wget -O /etc/apk/keys/lochnair-apk.pem https://openwrt.lochnair.net/keys/lochnair-apk.pem
echo "https://openwrt.lochnair.net/25.12/${DISTRIB_ARCH}/packages.adb" >> /etc/apk/repositories.d/customfeeds.list
apk update
apk add unravel
```

The package includes the binary, reflector inventory, UCI configuration and init script. Edit `/etc/config/unravel` before starting the service.

The feed only publishes APK packages. For older OpenWrt releases using opkg, build an `.ipk` with the SDK below.

### Building with the OpenWrt SDK

Use an [SDK](https://openwrt.org/docs/guide-developer/toolchain/using_the_sdk) matching your OpenWrt release and target, with Rust package support available. The package builds use the [OpenWrt Rust SDK images](https://github.com/Lochnair/openwrt-sdk-rust).

From the extracted SDK directory, add the [package feed](https://github.com/Lochnair/openwrt-feeds):

```sh
[ -f feeds.conf ] || cp feeds.conf.default feeds.conf
echo 'src-git lochnair https://github.com/Lochnair/openwrt-feeds.git' >> feeds.conf
./scripts/feeds update -a
./scripts/feeds install unravel
make menuconfig
```

Select `unravel` under Utilities, save the configuration, then build:

```sh
make package/unravel/compile V=s
```

The resulting package is under `bin/packages/`. Copy it to `/tmp` on your router. For an `.ipk`, install it with:

```sh
opkg update
opkg install /tmp/unravel_*.ipk
```

For a manually built `.apk`, use:

```sh
apk add --allow-untrusted /tmp/unravel*.apk
```

### Building with Cargo

For other Linux systems, use a current stable Rust toolchain. The default build reads TOML and UCI configuration files, supports IRTT and bundles Lua 5.1. It also includes ubus status support. On Debian or Ubuntu, install the build dependencies first.

```sh
sudo apt install build-essential
```

Then clone and build:

```sh
git clone https://github.com/Lochnair/unravel.git
cd unravel
cargo build --release --locked
sudo install -m 755 target/release/unravel /usr/sbin/unravel
sudo mkdir -p /etc/unravel
sudo install -m 644 reflectors.csv /etc/unravel/reflectors.csv
```

For a TOML-only build with IRTT support, run `cargo build --release --locked --no-default-features --features toml,irtt`.

## Configuration

On OpenWrt, edit `/etc/config/unravel`. If you use `uci set`, run `uci commit unravel` before restarting the service so it reads the updated file. If you're upgrading an existing installation, start with the [migration guide](MIGRATION.md).

For an instance named `wan`, unRAVEL reads its settings from UCI first, then applies `/etc/unravel/wan.toml` on builds with TOML support. Environment variables override both, for example `UNRAVEL__NETWORK__UPLOAD_INTERFACE=wan`.

### UCI

Set your interface names and bandwidths in `/etc/config/unravel`.

```uci
config instance 'wan'
    option enabled '1'
    option dry_run '0'
    option download_interface 'ifb4wan'
    option upload_interface 'wan'
    option download_base_kbits '50000'
    option upload_base_kbits '10000'
    option download_min_percent '20'
    option upload_min_percent '20'

config output
    option instance 'wan'
    option log_level 'info'
```

The bandwidth values are in kbit/s, so this example uses 50 Mbit/s download and 10 Mbit/s upload. Set the base rates to what you'd normally configure in SQM. These are reference rates for the controller, and it can increase the shapers above them when load and delay allow it.

The minimum is a percentage of the base rate, 20% by default. With these values the shapers can go down to 10 Mbit/s download and 2 Mbit/s upload. Choose minima that leave you with a usable connection when capacity drops.

To cap either shaper, set `download_max_percent` or `upload_max_percent` to at least 100. For example, 150 caps the shaper at 1.5 times its base rate. The caps are off by default and apply to every control law.

### TOML

For a manual install, put the equivalent settings for `wan` in `/etc/unravel/wan.toml`.

```toml
dry_run = false

[network]
download_interface = "ifb4wan"
upload_interface = "wan"
download_base_kbits = 50000
upload_base_kbits = 10000
download_min_percent = 20
upload_min_percent = 20

[output]
log_level = "info"
```

### Controlling one direction

If you only want to control upload, set `option download_interface 'none'` in UCI, or `download_interface = "none"` in TOML. You can then omit `download_base_kbits` and `download_min_percent`. The same applies to `upload_interface` if you only want download shaping. At least one direction must be enabled.

### Running more than one WAN

Give each WAN a named `config instance` section with the interfaces and base rates for that link. Use ASCII letters, digits and underscores in instance names. The OpenWrt init script starts each enabled instance. Set `option enabled '0'` to skip it.

This example adds a second WAN with its own SQMA settings and reflector inventory.

```uci
config instance 'wan2'
    option download_interface 'ifb4wan2'
    option upload_interface 'wan2'
    option download_base_kbits '30000'
    option upload_base_kbits '5000'

config law
    option instance 'wan2'
    option type 'sqma'
    option download_delay_ms '20'

config reflectors
    option instance 'wan2'
    list files '/etc/unravel/wan2-reflectors.csv'
    list countries 'NO'

config bootstrap_reflector
    option instance 'wan2'
    option host 'global.irtt.lochnair.net'
    list protocols 'irtt'
```

Each subordinate section needs an `option instance` naming its owner. It can appear anywhere in the file and can be anonymous. Each instance can have one `law`, `reflectors`, `output` and `observability` section. Missing sections and options use built-in defaults. Use as many `reflector` and `bootstrap_reflector` sections as needed for each instance. Different instances can independently point to the same CSV or Lua file.

Invalid instance names, duplicate section names, unknown section types and missing or invalid ownership links make the UCI package invalid. Duplicate singleton sections and invalid options within owned sections prevent the owning instance from starting.

On Linux, set `measurement_bind_device` on an instance if you need to bind its probes to a particular interface. To start the second WAN manually, run `/usr/sbin/unravel --instance wan2`. With TOML support, its override file is `/etc/unravel/wan2.toml`.

## Running it

To try it without changing CAKE's rates, set `option dry_run '1'` in the UCI instance section, set `dry_run = true` before any TOML table headers, or set `UNRAVEL__DRY_RUN=true`. Dry run still needs the configured interfaces and CAKE qdiscs. It sends probes and SQMA still writes its CSV files if statistics are enabled. Turn off `dry_run` when you're ready to let unRAVEL change the rates.

After configuring the OpenWrt package, start the service and enable it at boot:

```sh
/etc/init.d/unravel start
/etc/init.d/unravel enable
```

To try it in the foreground, stop the service first, then run the binary:

```sh
/etc/init.d/unravel stop
/usr/sbin/unravel --instance wan
```

For a manual install on another Linux system, run `sudo /usr/sbin/unravel --instance wan`. Every invocation requires `--instance`. A manual binary install doesn't include a service definition.

unRAVEL starts by setting the enabled shapers to their minimum rates. Once it's running, try a download or upload and watch the rates with `tc -s qdisc show dev wan` or the same command for the download interface.

Ctrl+C or SIGTERM stops the program and attempts to restore the enabled shapers to your configured base rates. When running through the OpenWrt service, use `/etc/init.d/unravel stop` to stop it.

### Checking status on OpenWrt

With the `ubus` build feature enabled, you can check the current rates, the law's delay readings and the reflectors without stopping the service. The status object is `unravel.<instance>`.

```sh
ubus call unravel.wan status
ubus call unravel.wan2 status
```

```json
{
	"version": "0.6.0-rc1",
	"instance": "wan",
	"law": "sqma",
	"dry_run": false,
	"uptime": 3605,
	"reflectors": 5,
	"download": {
		"interface": "ifb4wan",
		"base_kbps": 100000,
		"min_kbps": 20000,
		"rate_kbps": 87341,
		"traffic_kbps": 41210,
		"delay_us": 2140,
		"delay_threshold_us": 15000
	},
	"upload": {
		"interface": "wan",
		"base_kbps": 20000,
		"min_kbps": 4000,
		"rate_kbps": 18922,
		"traffic_kbps": 1530,
		"delay_us": 640,
		"delay_threshold_us": 15000
	}
}
```

`uptime` is in seconds, and `reflectors` counts the active reflectors that are ready to probe. Each enabled shaper has its own entry, with `max_kbps` included if you've set a cap. Once unRAVEL has set a rate, `rate_kbps` shows that last rate (simulated in dry run). `traffic_kbps` shows the current sample when one is available. `delay_us` is the delay above baseline that the law last acted on, in microseconds, and `delay_threshold_us` is the threshold it compares that delay to. Both appear once the law reports them, and both built-in laws do.

`history` returns the last `window` seconds (ten minutes) of each direction, one value every `interval` seconds, oldest first. `rate_kbps` is the rate at the end of each interval, while `traffic_kbps` and `delay_us` are averages over it. A `null` marks an interval without a value.

```sh
ubus call unravel.wan history
```

```json
{
	"interval": 2,
	"window": 600,
	"download": {
		"rate_kbps": [ 87341, 88120, 90004 ],
		"traffic_kbps": [ 41210, 52977, 61340 ],
		"delay_us": [ 2140, 1980, 3310 ]
	},
	"upload": { "...": "..." }
}
```

`reflectors` lists every reflector being probed, active ones first. The figures cover the reflector's last 20 probes: loss, the latest RTT and the average and minimum RTT, jitter (the mean change between replies), and how long ago the last reply came. `one_way` is the state of the last one-way measurement, `usable` or why it was rejected.

```sh
ubus call unravel.wan reflectors
```

```json
{
	"reflectors": [
		{
			"reflector": "109.224.229.159:2112",
			"protocol": "irtt",
			"role": "active",
			"ready": true,
			"probes": 20,
			"loss_pct": 0,
			"rtt_us": 32176,
			"rtt_avg_us": 32711,
			"rtt_min_us": 31643,
			"jitter_us": 1372,
			"last_reply_ms": 84,
			"one_way": "usable"
		}
	]
}
```

If the ubus socket is missing at startup, unRAVEL logs a message and carries on. For LuCI and other rpcd clients, the `unravel` read permission covers all three methods on every instance. [rpcd ACL](dist/openwrt/unravel.rpcd.json)

## Control laws

SQMA is the default. If you want to change its delay thresholds or sampling interval, put the options in the instance's `config law` with `option type 'sqma'` on OpenWrt, or in `[law.sqma]` in TOML. [OpenWrt example config](dist/openwrt/unravel.config)

If you want to keep SQMA's learned safe rates across a service restart, set `option save_history '1'` in that UCI section or `save_history = true` in `[law.sqma]`. SQMA writes `sqma/history.txt` under the instance's output directory. It restores history up to six hours old if the base rate and history size still match. Use a persistent `output.folder` if you want the history to survive a reboot.

### Using a Lua script

If you want to try your own rate control, select the `script` law in a build with Lua support. The default Cargo build includes it. The script gets probe results and traffic samples and decides when to change the rates. The example sets the shapers to their base rates and leaves them there. [examples/laws/minimal.lua](examples/laws/minimal.lua)

If your script works out a delay signal, call `ctx:report_delay(direction, delay_ms, threshold_ms)` whenever it decides, so `ubus call unravel.wan status` and the LuCI status page can show it. The threshold is optional. The cake-autorate law reports its average delay and its delay threshold.

Copy the script to `/etc/unravel/minimal.lua` and set `config law` as follows.

```uci
config law
    option instance 'wan'
    option type 'script'
    option path '/etc/unravel/minimal.lua'
```

In TOML, set `name = "script"` under `[law]` and put the script path under `[law.script]`.

```toml
[law]
name = "script"

[law.script]
path = "/etc/unravel/minimal.lua"
```

### Trying the cake-autorate law

The experimental `cake.lua` script uses cake-autorate's rate control and defaults. unRAVEL still picks and replaces the reflectors. You'll need a build with Lua support.

The example uses `/usr/share/unravel/laws/cake.lua`. If your package hasn't installed it there, copy it from `laws/cake.lua` in the release archive or from the source tree. [dist/laws/cake.lua](dist/laws/cake.lua)

Add a maximum for each direction you shape to your existing `config instance` section. Adjust these percentages to suit your link.

```uci
    option download_max_percent '400'
    option upload_max_percent '175'
```

Then replace the SQMA `config law` section with this one. You can put tuning options such as `download_delay_ms` and `high_load_level` in the same section. Try it with `dry_run` on first.

```uci
config law
    option instance 'wan'
    option type 'script'
    option path '/usr/share/unravel/laws/cake.lua'
```

At startup the script logs the largest packet size it will use for each direction. On Linux, unRAVEL works this out from the interface MTU and CAKE's overhead and ATM or PTM framing. At low rates, sending a packet takes longer, so the script raises its delay thresholds and measures load over a longer window. If it can't get the packet size, it uses 1500 bytes. Set `download_wire_packet_bytes` or `upload_wire_packet_bytes` in the law section if you need to override it.

## Reflectors and measurement protocols

unRAVEL selects reflectors and protocols automatically, preferring IRTT and ICMP timestamps so it can measure upload and download delay separately. If there aren't enough healthy directional reflectors, it can fall back to ICMP echo and switch back when they recover. Echo uses round-trip measurements, so it can't separate upload and download delay.

unRAVEL starts with the built-in bootstrap hosts by default. After that, the reflector manager tries hosts from the combined bootstrap and configured inventory, replacing failed reflectors and looking for better ones. By default it uses up to five active reflectors and probes three candidates in the background, each every three seconds. SQMA uses the active set for rate control.

You can change these with `num_reflectors`, `num_candidates` and `candidate_period` under `reflectors` (`config reflectors` in UCI). `candidate_period` is in seconds. Set `num_candidates` to `0` to keep the startup set fixed and turn off protocol switching. With multiple active reflectors you'll need at least two candidate slots for switching to work.

Reflector entries declare their supported protocols: `icmp-timestamps`, `icmp-echo` or `irtt`. ICMP measurements use IPv4; IRTT supports IPv4 and IPv6, and its client is built into the default binary.

You can add your own reflectors with `[[reflectors.entries]]` in TOML or repeated `config reflector` sections in UCI:

```uci
config reflector
    option instance 'wan'
    option host 'global.irtt.lochnair.net'
    list protocols 'irtt'
```

This adds to the instance’s CSV inventory. Use `config bootstrap_reflector` with the same fields, or `[[reflectors.bootstrap]]` in TOML, to replace the built-in bootstrap list for that instance. To skip file loading and use the built-in bootstrap hosts plus any inline entries, use `[reflectors]` with `files = []` in TOML, or a `config reflectors` section with `option files 'none'` in UCI. UCI drops empty string options, so `option files ''` won't disable file loading.

To load a different CSV file in UCI, use `list files '/path/to/reflectors.csv'` under `config reflectors`. For IRTT entries, absent or blank `irtt_hmac` values use unauthenticated sessions.

unRAVEL checks directional timestamps before passing the results to the control law. If the router's clock jumps, SQMA drops to the configured minimum rates for five seconds while queues drain, then rebuilds its delay baselines. A timing change at one reflector only resets that reflector's baseline in SQMA.

## Logs and statistics

Logs go to stdout at `info` by default. On OpenWrt, the init script sends them to the system log unless you set `option use_syslog '0'` in the instance's `output` section. Each log line starts with the instance name.

SQMA writes two CSV files by default.

- `/tmp/unravel/wan/stats.csv` contains traffic load, delay above baseline and shaping rates.
- `/tmp/unravel/wan/speedhist.csv` contains the speed history snapshots.

SQMA recreates these files on startup. Set `output.suppress_statistics = true` to turn them off. Each instance writes under `<output.folder>/<instance>/`, so a named instance such as `wan2` uses `/tmp/unravel/wan2/` by default. The law's saved state goes in a subfolder named after the law.

## Metrics

unRAVEL can send InfluxDB line protocol over UDP or TCP. Set `enabled`, `protocol`, `host` and `port` in the instance’s `config observability` on OpenWrt or in `[observability]` in TOML. Change the address to match your collector.

```toml
[observability]
enabled = true
protocol = "udp"
host = "192.168.1.100"
port = 8089
```

Every line has `host` and `instance` tags. When you enable the exporter, it sends rates and traffic (`export_rate_metrics`) and events (`export_events`) by default. Turn on `export_ping_metrics` for probe results or `export_law_metrics` for metrics from the control law.

| Measurement | Sent | Tags | Fields |
| --- | --- | --- | --- |
| `unravel_rate` | on each successful rate update (simulated in dry run) | `direction` | `rate_kbps` |
| `unravel_traffic` | per traffic sample | `direction` | `traffic_kbps` |
| `unravel_ping` | per reply (`export_ping_metrics`) | `reflector`, `type`, `role` | `rtt`, `up_time`, `down_time` (when one-way times exist) |
| `unravel_loss` | per lost probe (`export_ping_metrics`) | `reflector`, `type`, `role` | `count` |
| `unravel_event` | on events | `type`, `reason`, `reflector`, … | `count` |
| `unravel_law` | by the control law (`export_law_metrics`) | `law`, `series`, the law's own | the law's own |
| `unravel_metrics_dropped` | when the queue overflowed | | `count` |

Use `role=active` to pick out active reflectors from the probe metrics. Candidate probes have `role=candidate`.

For SQMA, `unravel_law` with `series=baseline` has the `baseline_ewma` and `recent_ewma` fields, tagged by `reflector` and `direction`. The `delta` series has `delta_delay`, tagged by `direction`.

The cake-autorate script sends `unravel_law` with `series=cake` after each load window. It usually reports on every traffic sample, less often on slow links. The tags are `direction` and `reason`. The reason records the strongest action since the last sample (`bufferbloat`, `high_load`, `decay` or `hold`). Each sample has `rate_kbps`, `achieved_kbps`, `load`, `avg_delta_ms` and `delayed`.

## License

[Mozilla Public License 2.0](LICENSE), as used by the original sqm-autorate project.
