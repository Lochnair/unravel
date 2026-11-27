-- The smallest script law: it sets both directions to their base rate once
-- and leaves them there.
--
--   config law
--   	option instance 'wan'
--   	option type 'script'
--   	option path '/etc/unravel/minimal.lua'

local law = {
  -- Seconds. `traffic` defaults to `probe`; `timer` is needed by on_timer.
  cadence = { probe = 0.5, traffic = 0.5, timer = 1.0 },

  -- Required. The options this law takes in its law section, in the order a
  -- frontend shows them. unravel checks each value and applies the defaults
  -- before init, and rejects options that aren't listed. For example:
  --
  --   { name = "delay_ms", type = "number", default = 15, above = 0,
  --     label = "Delay threshold", unit = "ms" },
  --
  -- Types are boolean, integer, number and string. Numbers take min and max
  -- (inclusive) or above and below (exclusive); strings take choices.
  options = {},
}

-- Optional. `config` holds the declared options; one without a value or a
-- default is nil.
function law:init(config, ctx)
  for _, direction in ipairs({ "download", "upload" }) do
    local base = ctx:base(direction)
    if base then
      ctx:set_rate(direction, base)
    end
  end
  ctx:log("info", "minimal law started")
end

-- m = { reflector, protocol, role, t, loss, rtt_ms, up_ms, down_ms, one_way }
function law:on_measurement(m, ctx) end

-- s = { t, download_kbps, upload_kbps }
function law:on_traffic(s, ctx) end

-- Optional; needs cadence.timer.
function law:on_timer(now, ctx) end

-- Optional. The local clock jumped.
function law:on_clock_step(ctx) end

return law
