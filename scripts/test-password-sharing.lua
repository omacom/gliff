-- Run from the repository root: lua scripts/test-password-sharing.lua
local script = assert(io.open("crates/gliff-server/src/password_sharing.lua")):read("*a")
local timers, rules = {}, {}
local function handle()
  return {
    enabled = true,
    set_enabled = function(self, enabled) self.enabled = enabled end,
    is_enabled = function(self) return self.enabled end,
  }
end
hl = {
  window_rule = function(spec)
    assert(spec.no_screen_share == false)
    local rule = handle()
    table.insert(rules, rule)
    return rule
  end,
  timer = function(callback, opts)
    assert(opts.timeout == 1000 and opts.type == "repeat")
    local timer = handle()
    timer.tick = function(self) if self.enabled then callback() end end
    table.insert(timers, timer)
    return timer
  end,
}
local function update(id, connected)
  assert(load("local lease, connected = ...;\n" .. script))(id, connected)
end
local function tick(count)
  for _ = 1, count do timers[#timers]:tick() end
end
local function enabled() return rules[#rules]:is_enabled() end

update("absent", false)
assert(#rules == 0, "release must not create a rule")
update("a", true)
assert(enabled())
tick(4)
assert(enabled())
tick(1)
assert(not enabled(), "a crashed server must expire")
assert(not timers[#timers]:is_enabled())

update("a", true)
tick(3)
update("a", true)
tick(3)
assert(enabled(), "renewal must extend the lease")
update("b", true)
update("a", false)
assert(enabled(), "one disconnect must not revoke another session")
update("b", false)
assert(not enabled(), "last disconnect must restore protection immediately")
assert(#rules == 1 and #timers == 1, "reconnections must reuse the rule and timer")

update("dead", true)
for _ = 1, 6 do
  update("alive", true)
  tick(1)
end
update("alive", false)
assert(not enabled(), "other renewals must not keep a dead lease alive")

-- Reload destroys Hyprland's Lua state, rules and timers. The next heartbeat
-- must re-create the override, while a late release remains harmless.
_gliff_password_sharing = nil
update("a", true)
assert(#rules == 2 and enabled())
update("a", false)
assert(not enabled())
print("password sharing lease tests passed")
