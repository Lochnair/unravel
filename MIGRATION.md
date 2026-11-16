# Migrating to unRAVEL

The package and executable have changed from `sqm-autorate-rust` to `unravel`. The OpenWrt service is now `/etc/init.d/unravel`. Existing OpenWrt installations won't automatically upgrade across the package rename. Back up your configuration, stop and disable the old service, then remove the old package and install `unravel` for your OpenWrt release and architecture.

Move your settings and reflector inventory to the new locations:

| Purpose | Old path | New path |
| --- | --- | --- |
| UCI configuration | `/etc/config/sqm-autorate-rust` | `/etc/config/unravel` |
| TOML configuration | `/etc/sqm-autorate-rust/config.toml` | `/etc/unravel/config.toml` |
| Reflector inventory | `/etc/sqm-autorate-rust/reflectors.csv` | `/etc/unravel/reflectors.csv` |
| Statistics | `/tmp/sqm-autorate-rust.csv` | `/tmp/unravel.csv` |
| Speed history | `/tmp/sqm-speedhist.csv` | `/tmp/unravel-speedhist.csv` |

Rename environment variables from `SQMA__SECTION__OPTION` to `UNRAVEL__SECTION__OPTION`, for example `SQMA__NETWORK__DOWNLOAD_INTERFACE` becomes `UNRAVEL__NETWORK__DOWNLOAD_INTERFACE`. Update service overrides, startup scripts and any tools reading the CSV files. Explicitly configured paths keep their values, so change those too if you want to use the new locations.

The configuration sections, option names and source precedence are unchanged by the rename. The old configuration paths and environment prefix are no longer read. Exported `sqm_*` metric names are unchanged.

## Reflector management

- Remove `advanced_settings.measurement_type`. Protocols are now selected automatically from each reflector's `protocols` list, including the built-in bootstrap hosts. unRAVEL prefers IRTT and ICMP timestamps, falls back to echo when directional service degrades, and switches back when it recovers.
- Remove `reflectors.peer_reselection_time`. The manager now probes candidates in the background. `reflectors.num_candidates` defaults to `3`, and `reflectors.candidate_period` defaults to `3.0` seconds per candidate. There's no direct equivalent of the old reselection interval. Set `num_candidates` to `0` for a fixed startup set, or use at least `2` to allow protocol switching with multiple active reflectors.

## Older configurations

If you're upgrading from v0.4.1, also check these changes:

- UCI configuration belongs in `/etc/config/unravel`, rather than the Lua project's `/etc/config/sqm-autorate`.
- Environment variables include the section name and double underscores, e.g. `SQMA_DOWNLOAD_INTERFACE` becomes `UNRAVEL__NETWORK__DOWNLOAD_INTERFACE`.
- Reflector files use the protocol-aware CSV format in this repo's `reflectors.csv`. The old `reflectors-icmp.csv` isn't compatible. Set file paths through `reflectors.files` and the active reflector count through `reflectors.num_reflectors`.
- The ICMP echo protocol is named `icmp-echo`.
