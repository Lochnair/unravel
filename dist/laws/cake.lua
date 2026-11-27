-- SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
--
-- SPDX-License-Identifier: MPL-2.0

-- The rate control of cake-autorate 3.3 as a script law.
--
-- Each reply from an active reflector updates that reflector's delay
-- baselines and adds one vote per direction to a window shared by all
-- reflectors. When enough votes in the window are over the delay threshold,
-- the rate is cut by an amount that grows with the window's mean delay.
-- Under high load without bufferbloat the rate is raised, by less as the
-- mean delay nears the threshold. Otherwise it decays back towards base.
--
-- Reflector selection, health and replacement are left to unravel. A
-- reflector that is swapped out, or whose one-way times are missing, rejected
-- or move to a new reference, loses its votes and starts a new baseline. Late
-- replies and old traffic samples are not acted on. Upstream sleep mode is
-- not ported.
--
-- Needs a maximum rate for each shaped direction:
--
--   config instance 'wan'
--   	option download_max_percent '400'
--   	option upload_max_percent '175'
--
--   config law
--   	option instance 'wan'
--   	option type 'script'
--   	option path '/usr/share/unravel/laws/cake.lua'
--
-- Every option in CONFIG can be set in the law section. unravel applies the
-- defaults and checks each option's type and bounds before init.

local DIRECTIONS = { "download", "upload" }

-- Each option with its default. A value set in the law section must have the
-- default's type. Bounds in DESCRIBE are checked by unravel; rules between
-- options are checked in law:validate.
local CONFIG = {
  detection_window = 6,
  detection_threshold = 3,
  alpha_baseline_increase = 0.001,
  alpha_baseline_decrease = 0.9,
  adjust_down_bufferbloat_min = 0.99,
  adjust_down_bufferbloat_max = 0.75,
  adjust_up_load_high_min = 1.0,
  adjust_up_load_high_max = 1.04,
  adjust_down_load_low = 0.99,
  adjust_up_load_low = 1.01,
  high_load_level = 0.75,
  bufferbloat_refractory_ms = 300,
  decay_refractory_ms = 1000,
  reply_timeout_s = 10.0,
  min_rates_on_timeout = false,
}

-- For settings pages: JSON Schema keywords per option.
local DESCRIBE = {
  detection_window = { title = "Detection window", unit = "replies", minimum = 1,
    description = "Bufferbloat is detected when detection_threshold of the last "
      .. "detection_window replies are delayed." },
  detection_threshold = { title = "Detection threshold", unit = "replies", minimum = 1 },
  alpha_baseline_increase = { title = "Baseline rise factor", exclusiveMinimum = 0, maximum = 1,
    description = "How fast a reflector's baseline follows its delay up." },
  alpha_baseline_decrease = { title = "Baseline fall factor", exclusiveMinimum = 0, maximum = 1,
    description = "How fast a reflector's baseline follows its delay down." },
  adjust_down_bufferbloat_min = { title = "Smallest bufferbloat cut",
    exclusiveMinimum = 0, maximum = 1,
    description = "Factor applied to the rate on bufferbloat at low mean delay." },
  adjust_down_bufferbloat_max = { title = "Largest bufferbloat cut",
    exclusiveMinimum = 0, maximum = 1,
    description = "Factor applied to the rate on bufferbloat at high mean delay." },
  adjust_up_load_high_min = { title = "Smallest high-load raise", minimum = 1,
    description = "Factor applied to the rate under high load as the mean delay nears the threshold." },
  adjust_up_load_high_max = { title = "Largest high-load raise", minimum = 1,
    description = "Factor applied to the rate under high load at low mean delay." },
  adjust_down_load_low = { title = "Low-load decay down", exclusiveMinimum = 0, maximum = 1,
    description = "Factor that moves a rate above base back towards it on low load." },
  adjust_up_load_low = { title = "Low-load decay up", minimum = 1,
    description = "Factor that moves a rate below base back towards it on low load." },
  high_load_level = { title = "High load level", exclusiveMinimum = 0,
    description = "Achieved rate over the current rate above which load is high." },
  bufferbloat_refractory_ms = { title = "Bufferbloat refractory period", unit = "ms", minimum = 0 },
  decay_refractory_ms = { title = "Decay refractory period", unit = "ms", minimum = 0 },
  reply_timeout_s = { title = "Reply timeout", unit = "s", exclusiveMinimum = 0,
    description = "With no reply from any active reflector for this long, send a "
      .. "reflector_unavailable event." },
  min_rates_on_timeout = { title = "Minimum rates on timeout",
    description = "Drop to the minimum rates after the reply timeout. Off upstream." },
}

for _, d in ipairs(DIRECTIONS) do
  local Dir = d:sub(1, 1):upper() .. d:sub(2)
  CONFIG[d .. "_delay_ms"] = 30.0
  CONFIG[d .. "_adjust_up_delay_ms"] = 10.0
  CONFIG[d .. "_adjust_down_delay_ms"] = 60.0
  -- Largest packet on the wire, MTU plus CAKE's overhead and ATM or PTM
  -- framing. 0 reads it from the interface and its qdisc. At low rates the
  -- time to send one raises every delay threshold and stretches the load window.
  CONFIG[d .. "_wire_packet_bytes"] = 0
  DESCRIBE[d .. "_delay_ms"] = { title = Dir .. " delay threshold", unit = "ms",
    exclusiveMinimum = 0, description = "Delay over baseline that counts as a delayed reply." }
  DESCRIBE[d .. "_adjust_up_delay_ms"] = { title = Dir .. " full-increase delay", unit = "ms",
    minimum = 0, description = "Mean delay at or below which increases are at their largest." }
  DESCRIBE[d .. "_adjust_down_delay_ms"] = { title = Dir .. " full-cut delay", unit = "ms",
    minimum = 0, description = "Mean delay at or above which cuts are at their largest." }
  DESCRIBE[d .. "_wire_packet_bytes"] = { title = Dir .. " wire packet size", unit = "bytes",
    minimum = 0, description = "Overrides the size read from the interface and its qdisc; 0 reads it." }
end

local FALLBACK_WIRE_PACKET_BYTES = 1500

-- After a local clock step, replies are ignored for this long.
local SETTLE_S = 5.0
-- Replies and traffic samples older than this are not acted on, as upstream
-- skips replies over 500 ms old.
local FRESHNESS_S = 0.5

local law = {
  -- Upstream probes each reflector every 0.3 s and samples traffic every
  -- 0.2 s. The cadence is read before init, so it is not configurable.
  cadence = { probe = 0.3, traffic = 0.2, timer = 1.0 },
  config = CONFIG,
  describe = DESCRIBE,
}

-- unravel checks each option's type and bounds; these are the rules between
-- options. It calls this before init and for --check-config.
function law:validate(c)
  local problems = {}
  local function check(ok, message)
    if not ok then
      problems[#problems + 1] = message
    end
  end

  check(c.detection_threshold <= c.detection_window,
    "detection_threshold must be at most detection_window")
  check(c.adjust_down_bufferbloat_max <= c.adjust_down_bufferbloat_min,
    "adjust_down_bufferbloat_max must be at most adjust_down_bufferbloat_min")
  check(c.adjust_up_load_high_min <= c.adjust_up_load_high_max,
    "adjust_up_load_high_min must be at most adjust_up_load_high_max")
  return problems
end

local function clamp(x, lo, hi)
  if x < lo then
    return lo
  elseif x > hi then
    return hi
  end
  return x
end

-- The thresholds grow by the time one wire packet takes at the current rate.
local function compensate(self, s)
  local c = self.config
  local extra_ms = s.wire_bits / s.rate
  s.delay_ms = c[s.name .. "_delay_ms"] + extra_ms
  s.adjust_up_delay_ms = c[s.name .. "_adjust_up_delay_ms"] + extra_ms
  s.adjust_down_delay_ms = c[s.name .. "_adjust_down_delay_ms"] + extra_ms
end

local function apply(self, s, rate, ctx)
  rate = clamp(math.floor(rate), s.min, s.max)
  if rate ~= s.rate then
    s.rate = rate
    ctx:set_rate(s.name, rate)
    compensate(self, s)
  end
end

local function set_min_rates(self, ctx)
  for _, s in pairs(self.shapers) do
    apply(self, s, s.min, ctx)
  end
end

function law:init(config, ctx)
  self.config = config
  self.shapers = {}
  for _, d in ipairs(DIRECTIONS) do
    local base = ctx:base(d)
    if base then
      local max = ctx:max(d)
      if not max then
        error(string.format(
          "cake needs a maximum %s rate: set network.%s_max_percent", d, d), 0)
      end
      local wire_bytes = self.config[d .. "_wire_packet_bytes"]
      local source = "set"
      if wire_bytes == 0 then
        wire_bytes, source = ctx:wire_packet_bytes(d), "detected"
      end
      if not wire_bytes then
        wire_bytes, source = FALLBACK_WIRE_PACKET_BYTES, "assumed"
      end
      ctx:log("info", string.format("cake: %s wire packet %d bytes (%s)", d, wire_bytes, source))
      local s = {
        name = d,
        wire_bits = wire_bytes * 8,
        -- Traffic since the last load sample, in kbit.
        traffic_kbits = 0,
        base = base,
        min = ctx:min(d),
        max = max,
        rate = base,
        load = 0,
        traffic_t = nil,
        fresh_traffic = false,
        last_bufferbloat = -math.huge,
        last_decay = -math.huge,
        reason = "hold",
      }
      compensate(self, s)
      ctx:set_rate(d, s.rate)
      self.shapers[d] = s
    end
  end

  -- Per reflector: { sent = last accepted send time, download = ms, upload = ms }.
  -- A reflector without a baseline waits for its next usable reply.
  self.baselines = {}
  -- The last detection_window votes, oldest first:
  -- { key = reflector, download = { delta, delayed }, upload = ... }
  self.window = {}
  self.now = 0
  -- Time covered by the traffic since the last load sample, and the time of
  -- the previous traffic sample.
  self.traffic_s = 0
  self.sample_t = nil
  self.last_reply = 0
  self.timed_out = false
  self.settle_until = nil
  self.settle_pending = false

  ctx:log("info", "cake law started")
end

local function key(r)
  return r.protocol .. " " .. r.reflector
end

-- Drop a reflector's votes, so its old delays stop counting.
local function remove_votes(self, k)
  for i = #self.window, 1, -1 do
    if self.window[i].key == k then
      table.remove(self.window, i)
    end
  end
end

-- Forget a reflector: its baseline restarts from its next usable reply.
local function forget(self, k, sent)
  remove_votes(self, k)
  self.baselines[k] = sent and { sent = sent } or nil
end

-- Forget reflectors unravel no longer probes as active, so the votes of one
-- that was swapped out stop counting straight away.
local function forget_inactive(self, ctx)
  local active = {}
  for _, r in ipairs(ctx:active_reflectors()) do
    active[key(r)] = true
  end
  for k in pairs(self.baselines) do
    if not active[k] then
      forget(self, k)
    end
  end
end

-- The delays this reply adds to the window, or nil.
local function observe(self, m)
  local k = key(m)
  local b = self.baselines[k]
  -- A reply to an older probe than one already used.
  if b and m.sent <= b.sent then
    return nil
  end

  local down, up
  if m.one_way == "usable" then
    down, up = m.down_ms, m.up_ms
  elseif m.protocol == "icmp-echo" and m.rtt_ms then
    down, up = m.rtt_ms / 2, m.rtt_ms / 2
  elseif m.one_way == "reference_changed" then
    -- The one-way times moved to a new reference: start over from here.
    remove_votes(self, k)
    self.baselines[k] = { sent = m.sent, download = m.down_ms, upload = m.up_ms }
    return nil
  elseif m.one_way == "out_of_order" then
    -- Late, but says nothing about the reflector's newer replies.
    return nil
  else
    -- Missing or rejected one-way times invalidate the reflector's votes.
    forget(self, k, m.sent)
    return nil
  end

  if b == nil or b.download == nil then
    -- Upstream starts at 100 ms; the first reply avoids a long false
    -- bufferbloat on paths with more delay than that.
    self.baselines[k] = { sent = m.sent, download = down, upload = up }
    return 0, 0
  end

  local c = self.config
  local function follow(baseline, delay)
    local alpha = delay >= baseline and c.alpha_baseline_increase or c.alpha_baseline_decrease
    return alpha * delay + (1 - alpha) * baseline
  end
  b.sent = m.sent
  b.download = follow(b.download, down)
  b.upload = follow(b.upload, up)
  return down - b.download, up - b.upload
end

local function vote(self, k, down, up)
  local v = { key = k }
  for d, delta in pairs({ download = down, upload = up }) do
    local s = self.shapers[d]
    v[d] = { delta = delta, delayed = s ~= nil and delta > s.delay_ms }
  end
  if #self.window >= self.config.detection_window then
    table.remove(self.window, 1)
  end
  self.window[#self.window + 1] = v
end

-- The window's count of delayed replies and mean delay. Like upstream,
-- empty slots count as zero delay.
local function tally(self, d)
  local delayed, sum = 0, 0
  for _, v in ipairs(self.window) do
    sum = sum + v[d].delta
    if v[d].delayed then
      delayed = delayed + 1
    end
  end
  return delayed, sum / self.config.detection_window
end

local function adjust(self, s, now, ctx)
  local c = self.config
  local delayed, avg = tally(self, s.name)
  s.delayed, s.avg_delta = delayed, avg
  ctx:report_delay(s.name, avg, s.delay_ms)
  local bb_refractory = c.bufferbloat_refractory_ms / 1000
  -- Upstream assumes a traffic sample every 200 ms.
  local load_fresh = s.traffic_t ~= nil and now - s.traffic_t <= FRESHNESS_S
  local rate = s.rate

  if delayed >= c.detection_threshold then
    if now > s.last_bufferbloat + bb_refractory then
      local factor
      if s.adjust_down_delay_ms <= s.delay_ms then
        factor = 1
      elseif avg > s.delay_ms then
        factor = math.min(1, (avg - s.delay_ms) / (s.adjust_down_delay_ms - s.delay_ms))
      else
        factor = 0
      end
      local down = c.adjust_down_bufferbloat_min
        - factor * (c.adjust_down_bufferbloat_min - c.adjust_down_bufferbloat_max)
      rate = s.rate * down
      s.last_bufferbloat = now
      s.last_decay = now
      s.reason = "bufferbloat"
    end
  elseif not load_fresh then
    -- Without a recent traffic sample the load is unknown: hold.
    rate = s.rate
  elseif s.load > c.high_load_level then
    if s.fresh_traffic and now > s.last_bufferbloat + bb_refractory then
      local factor
      if s.delay_ms <= s.adjust_up_delay_ms then
        factor = 1
      elseif s.delay_ms > avg then
        factor = math.min(1, (s.delay_ms - avg) / (s.delay_ms - s.adjust_up_delay_ms))
      else
        factor = 0
      end
      local up = c.adjust_up_load_high_min
        - factor * (c.adjust_up_load_high_min - c.adjust_up_load_high_max)
      rate = s.rate * up
      s.fresh_traffic = false
      s.last_decay = now
      if s.reason ~= "bufferbloat" then
        s.reason = "high_load"
      end
    end
  elseif now > s.last_decay + c.decay_refractory_ms / 1000 then
    if s.rate > s.base then
      rate = math.max(s.rate * c.adjust_down_load_low, s.base)
    elseif s.rate < s.base then
      rate = math.min(s.rate * c.adjust_up_load_low, s.base)
    end
    s.last_decay = now
    if rate ~= s.rate and s.reason == "hold" then
      s.reason = "decay"
    end
  end

  apply(self, s, rate, ctx)
end

function law:on_measurement(m, ctx)
  local stale = m.t < self.now - FRESHNESS_S
  self.now = math.max(self.now, m.t)
  if self.settle_pending then
    self.settle_until = self.now + SETTLE_S
    self.settle_pending = false
  end
  if m.role ~= "active" or m.loss or stale then
    return
  end
  if self.settle_until and m.sent < self.settle_until then
    return
  end

  -- Any reply with a valid RTT shows the reflector is there, even when its
  -- one-way times can't be used.
  if m.rtt_ms then
    self.last_reply = m.t
    if self.timed_out then
      self.timed_out = false
      ctx:log("info", "cake: reflector replies again")
    end
  end

  forget_inactive(self, ctx)
  local down, up = observe(self, m)
  if down == nil then
    return
  end

  vote(self, key(m), down, up)
  for _, d in ipairs(DIRECTIONS) do
    local s = self.shapers[d]
    if s then
      adjust(self, s, m.t, ctx)
    end
  end
end

-- How long a load sample covers. Like upstream, at least ten times the time
-- a wire packet takes down and back up, so a few packets on a slow link
-- don't read as high load.
local function load_window_s(self)
  local packet_rtt_ms = 0
  for _, s in pairs(self.shapers) do
    packet_rtt_ms = packet_rtt_ms + s.wire_bits / s.rate
  end
  return math.max(law.cadence.traffic, 10 * packet_rtt_ms / 1000)
end

function law:on_traffic(sample, ctx)
  self.now = math.max(self.now, sample.t)
  local dt = self.sample_t and sample.t - self.sample_t or law.cadence.traffic
  self.sample_t = sample.t
  if dt <= 0 then
    dt = law.cadence.traffic
  end

  self.traffic_s = self.traffic_s + dt
  for _, s in pairs(self.shapers) do
    s.traffic_kbits = s.traffic_kbits + sample[s.name .. "_kbps"] * dt
    -- The sampler is alive; the load stays the last complete one.
    s.traffic_t = sample.t
  end
  -- Half a sample of slack, so jitter in the sample times doesn't add one.
  if self.traffic_s + law.cadence.traffic / 2 < load_window_s(self) then
    return
  end

  for _, d in ipairs(DIRECTIONS) do
    local s = self.shapers[d]
    if s then
      local achieved = s.traffic_kbits / self.traffic_s
      local load = achieved / s.rate
      -- reason is the strongest action since the previous load sample.
      ctx:metric("cake", {
        rate_kbps = s.rate,
        achieved_kbps = achieved,
        load = load,
        avg_delta_ms = s.avg_delta or 0,
        delayed = s.delayed or 0,
      }, { direction = d, reason = s.reason })
      s.reason = "hold"
      s.load = load
      s.fresh_traffic = true
      s.traffic_kbits = 0
    end
  end
  self.traffic_s = 0
end

function law:on_timer(now, ctx)
  self.now = math.max(self.now, now)
  if not self.timed_out and now - self.last_reply > self.config.reply_timeout_s then
    self.timed_out = true
    ctx:log("warn", string.format(
      "cake: no reflector reply for %.0f s", self.config.reply_timeout_s))
    ctx:event("reflector_unavailable", "cake")
    if self.config.min_rates_on_timeout then
      set_min_rates(self, ctx)
    end
  end
end

function law:on_clock_step(ctx)
  ctx:log("warn", "cake: local clock step, resetting baselines")
  self.baselines = {}
  self.window = {}
  set_min_rates(self, ctx)
  -- The hook gets no time; the settle period starts at the next reply.
  self.settle_pending = true
  -- Restart the reply timeout too, so the settle period does not trip it.
  self.last_reply = self.now
end

return law

