-- `lease` and `connected` are supplied by the server. Keep the watchdog in
-- the compositor so a killed server cannot leave password capture enabled.
local state = _gliff_password_sharing
if not state then
  if not connected then return end
  state = { leases = {} }
  state.rule = hl.window_rule({
    name = "gliff-password-sharing",
    match = { class = "^(1[pP]assword|com\\.onepassword\\.OnePassword)$" },
    no_screen_share = false,
  })
  state.rule:set_enabled(false)
  state.timer = hl.timer(function()
    for id, ticks in pairs(state.leases) do
      state.leases[id] = ticks > 1 and ticks - 1 or nil
    end
    if next(state.leases) == nil then
      state.rule:set_enabled(false)
      state.timer:set_enabled(false)
    end
  end, { timeout = 1000, type = "repeat" })
  _gliff_password_sharing = state
end

state.leases[lease] = connected and 5 or nil
local enabled = next(state.leases) ~= nil
if state.rule:is_enabled() ~= enabled then
  state.rule:set_enabled(enabled)
end
if state.timer:is_enabled() ~= enabled then
  state.timer:set_enabled(enabled)
end
