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

  -- Optional. Each option this law takes in its law section, with its
  -- default. A configured value must have the default's type; other keys are
  -- rejected. Tables nest, a list's first element is the template for the
  -- rest, and an empty list takes any elements. For example:
  --
  --   config = { delay_ms = 15, mode = "auto", hosts = { "a", "b" } },
  config = {},

  -- Optional. JSON Schema keywords per option for settings pages: title,
  -- description, unit, minimum, maximum, exclusiveMinimum, exclusiveMaximum
  -- and enum. unravel enforces the bounds and enum. A table option's fields
  -- are described under properties, a list's elements under items.
  --
  --   describe = {
  --     delay_ms = { title = "Delay threshold", unit = "ms", exclusiveMinimum = 0 },
  --     hosts = { items = { description = "A reflector host." } },
  --   },
}

-- Optional. Rules between options, which types and bounds can't express.
-- Called before init and by --check-config, with the same config. Return
-- nothing when it is fine, or a message or list of messages.
--
--   function law:validate(config)
--     if config.min_ms > config.max_ms then return "min_ms must be at most max_ms" end
--   end

-- Optional. `config` holds every declared option, set or defaulted.
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
