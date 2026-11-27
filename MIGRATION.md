# Migrating to unRAVEL

These steps are for upgrading to `master`. If you're using a package from the OpenWrt feed, check its source revision and use the configuration for that build. [OpenWrt package recipe](https://github.com/Lochnair/openwrt-feeds/blob/main/net/unravel/Makefile)

## Moving from sqm-autorate-rust

The package and executable are now called `unravel`, and the OpenWrt service is `/etc/init.d/unravel`. OpenWrt won't upgrade across the package rename by itself. Back up your settings and reflector list, then stop and disable the old service before removing the old package and installing `unravel`.

Use your old settings to fill in the new configuration. Check the option changes below before starting the service.

| Purpose | Old path | New path |
| --- | --- | --- |
| UCI configuration | `/etc/config/sqm-autorate-rust` (or `/etc/config/sqm-autorate` in v0.4.1) | `/etc/config/unravel` |
| TOML configuration | `/etc/sqm-autorate-rust/config.toml` | `/etc/unravel/<instance>.toml` |
| Reflector inventory | `/etc/sqm-autorate-rust/reflectors.csv` | `config reflector` sections, or JSON pool files in `reflectors.files` |

Update any scripts or service overrides that set environment variables. For example, `SQMA__NETWORK__DOWNLOAD_INTERFACE` becomes `UNRAVEL__NETWORK__DOWNLOAD_INTERFACE`. If you're coming from v0.4.1, add the section name as well. `SQMA_DOWNLOAD_INTERFACE` goes to the same new name.

## Changing your configuration

If you already run an earlier build of `unravel`, check these settings too. Unknown UCI section types and options can stop the service from starting, so remove the old keys from `/etc/config/unravel` after moving their values. Check TOML and environment overrides too.

### Assigning an instance name and section ownership

1. Replace each UCI `config network` with `config instance '<name>'`, keeping its interface and rate options. Choose a name such as `wan` for a previously anonymous instance. Names use ASCII letters, digits and underscores.
2. Move each instance's selected `law`, `reflectors`, `output` and `observability` settings into its own sections. Add `option instance '<name>'` to each section and remove the profile reference options from the instance. If several WANs used one profile, copy its settings into a section for each owner. Each of these section types can appear at most once for a given owner.
3. Assign every `config reflector` to an instance with `option instance '<name>'`. Copy entries to each WAN that needs them. Set reflector pool files in each WAN's owned `reflectors` section. Multiple instances may reference the same reflector pool or Lua file.
4. Remove `advanced_settings` from UCI and TOML. Put `option dry_run '1'` or `'0'` directly in the UCI instance section. In TOML, place `dry_run = true` or `false` at the top of the file before table headers. Rename `UNRAVEL__ADVANCED_SETTINGS__DRY_RUN` to `UNRAVEL__DRY_RUN`.
5. Rename the old `/etc/unravel/config.toml` to `/etc/unravel/<name>.toml` and use `unravel --instance <name>` for manual starts and service wrappers. The TOML table for network settings is still `[network]`.
6. Update ubus clients to call `unravel.<name>`. The old unnamed `unravel` object and anonymous instance are removed. An instance named `default` uses the same conventions as any other name.

See the [shipped UCI configuration](dist/openwrt/unravel.config) for a complete named `wan` example.

### SQMA settings

SQMA is still the default control law. If you've set any of the following in `advanced_settings`, move them to the SQMA settings.

`download_delay_ms`, `upload_delay_ms`, `high_load_level`, `min_change_interval`, `speed_hist_size`, `tick_interval`, `low_load_level`, `low_load_cut_floor` and `idle_recovery_percent`.

- In UCI, put them in the instance's owned `config law` with `option type 'sqma'`.
- In TOML, use `[law.sqma]`.
- For environment overrides, use `UNRAVEL__LAW__SQMA__` followed by the option name, for example `UNRAVEL__LAW__SQMA__DOWNLOAD_DELAY_MS`.

If you set `history_file`, see [CSV files and saved history](#csv-files-and-saved-history) below.

### Reflectors

Remove `measurement_type` and `peer_reselection_time`. unRAVEL probes each reflector with its configured `protocol` and tries replacement candidates in the background. Use `reflectors.num_candidates` and `reflectors.candidate_period` to change how many candidates it probes and how often. The defaults are three candidates and three seconds per candidate. Set `num_candidates` to `0` to keep the startup set fixed, or use at least `2` to allow protocol switching with multiple active reflectors.

If you're coming from v0.4.1, replace `reflectors-icmp.csv` with the bundled inventory, or convert your own list to the JSON pool format in the [README](README.md#reflectors-and-measurement-protocols). The name for ICMP echo is `icmp-echo`. Replace `advanced_settings.reflector_list_file` with `reflectors.files` and move `num_reflectors` from `advanced_settings` to `reflectors`.

The reflector configuration changed in this release, and old entries stop the service from starting:

- Replace CSV pool files with JSON. Each record has a `protocol` and an `endpoint`, plus an optional `authentication` for IRTT. Point `reflectors.files` at the new file. No pool file ships any more, and `files` is empty by default: remove `list files '/etc/unravel/reflectors.csv'` and copy the `config reflector` sections from the [shipped configuration](dist/openwrt/unravel.config).
- Replace `host` with `endpoint`, and `protocols` with a single `protocol`. Repeat an entry for each protocol a host supports. Use IP addresses rather than host names. IRTT endpoints need their port, for example `192.0.2.10:2112`.
- Rename `irtt_hmac` to `authentication`, and remove `irtt_port`.
- Remove `config bootstrap_reflector` sections and `[[reflectors.bootstrap]]` tables, or turn them into ordinary reflectors. An unknown section type makes the whole UCI package invalid, so every instance fails to start until it's gone. unRAVEL now picks its starting reflectors from the inventory.
- Remove `countries` and the `country` column. The shipped reflectors are anycast, so they suit any location.

### CSV files and saved history

Replace `output.stats_file` and `output.speed_hist_file` with `output.folder`, which defaults to `/tmp/unravel`. Update anything reading the CSV files to use the new paths.

| File | Previous default | New default |
| --- | --- | --- |
| Statistics | `/tmp/unravel.csv`, or `/tmp/sqm-autorate-rust.csv` before the rename | `/tmp/unravel/wan/stats.csv` |
| Speed history snapshots | `/tmp/unravel-speedhist.csv`, or `/tmp/sqm-speedhist.csv` before the rename | `/tmp/unravel/wan/speedhist.csv` |

In v0.4.1, the default statistics path was `/tmp/sqm-autorate.csv`. The examples use the instance name `wan`. If you previously used the unnamed instance, move any state you want to keep from `<output.folder>/default/` into the directory for the name you chose.

If you used `history_file` to save SQMA's learned rates, remove it and enable `save_history` in the SQMA settings. To carry over the saved rates, copy that file to `<output.folder>/<instance>/sqma/history.txt` before you start the new build. SQMA restores history up to six hours old if the history size and base rate still match.

### Metrics

If you export metrics, replace `observability.export_baseline_metrics` with `observability.export_law_metrics` and update your queries.

| Old measurement or field | Where to find it now |
| --- | --- |
| `sqm_rate.rate_kbps` | `unravel_rate.rate_kbps`, sent when the rate is set. Traffic samples go to `unravel_traffic.traffic_kbps`. |
| `sqm_rate.load` | Divide the sampled `traffic_kbps` by the last `rate_kbps` for the same host, instance and direction. |
| `sqm_rate.delta_delay` | `unravel_law.delta_delay` with `law=sqma` and `series=delta`. |
| `sqm_baseline` | `unravel_law` with `law=sqma` and `series=baseline`. |
| `sqm_ping` | `unravel_ping`. Filter on `role=active` if you only want the reflectors used by SQMA. Losses go to `unravel_loss`. |
| `sqm_event`, `sqm_metrics_dropped` | `unravel_event`, `unravel_metrics_dropped`. |

Use the `instance` tag to keep WANs separate in your dashboards. If you renamed an instance, update any dashboard filters that used its former tag value. [Metrics reference](README.md#metrics)

### Script laws

If you wrote a script law for v0.6.0-rc1, add an `options` table next to `cadence` before you upgrade. It lists every option the script takes. A script with no options declares `options = {}`. unRAVEL now checks each value against its declared type and bounds, fills in the defaults, and hands `init` the result. It won't start the script without the table. [Using a Lua script](README.md#using-a-lua-script)

Options in the `law` section that the script doesn't declare now stop the instance from starting. Before, they reached the script, and `cake.lua` logged a warning and ignored them. Check the startup error for the misspelled option and where it was set.

`cake.lua` options are now typed. `detection_window`, `detection_threshold` and the `*_wire_packet_bytes` options must be whole numbers. `min_rates_on_timeout` takes the same boolean values as other UCI options, for example `1`, `yes` or `on`.

If you set a string option of a script law in an environment variable, the variable's value mustn't look like a number. unRAVEL reads `007` from the environment as the number 7 and rejects it for a string option. Set it in UCI or TOML instead.
