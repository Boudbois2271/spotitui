-- spotitui bindings for ~/.config/hypr/bindings.lua (Omarchy).

-- SUPER+CTRL+S opens the Spotify mini player (boudbois.spotify bar widget).
-- Omarchy binds it to Share by default, which moves to SUPER+SHIFT+CTRL+S.
hl.unbind("SUPER + CTRL + S")
o.bind("SUPER + CTRL + S", "Spotify", "omarchy-shell shell toggle boudbois.spotify")
hl.unbind("SUPER + SHIFT + CTRL + S")
o.bind("SUPER + SHIFT + CTRL + S", "Share", "omarchy-menu toggle share")

-- SUPER+CTRL+ALT+S opens spotitui, or focuses its window if one is open.
o.bind("SUPER + CTRL + ALT + S", "spotitui", "spotitui-open")
