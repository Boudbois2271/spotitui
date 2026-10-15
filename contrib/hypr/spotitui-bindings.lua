-- spotitui bindings for ~/.config/hypr/bindings.lua (Omarchy).

-- SUPER+CTRL+S opens the Spotify mini player (boudbois.spotify bar widget).
-- Omarchy binds it to Share by default, which moves to SUPER+SHIFT+CTRL+S.
hl.unbind("SUPER + CTRL + S")
o.bind("SUPER + CTRL + S", "Spotify", "omarchy-shell shell toggle boudbois.spotify")
hl.unbind("SUPER + SHIFT + CTRL + S")
o.bind("SUPER + SHIFT + CTRL + S", "Share", "omarchy-menu toggle share")

-- SUPER+CTRL+ALT+S opens spotitui, or focuses its window if one is open.
o.bind("SUPER + CTRL + ALT + S", "spotitui", "spotitui-open")

-- spotitui's playback keys from anywhere: SUPER+[ / ] volume, SUPER+- / = previous /
-- next track. Omarchy binds SUPER+- / = to expand / shrink window left by default.
o.bind("SUPER + code:34", "spotitui volume down", "spotitui volume down", { repeating = true })
o.bind("SUPER + code:35", "spotitui volume up", "spotitui volume up", { repeating = true })
hl.unbind("SUPER + code:20")
o.bind("SUPER + code:20", "spotitui previous track", "spotitui previous")
hl.unbind("SUPER + code:21")
o.bind("SUPER + code:21", "spotitui next track", "spotitui next")
