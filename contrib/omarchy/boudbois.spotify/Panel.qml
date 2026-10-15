import QtQuick
import Quickshell
import Quickshell.Io
import Quickshell.Services.Mpris
import qs.Commons
import qs.Ui

// Spotify mini player for spotitui. Playback state and controls come from
// spotitui's MPRIS player; the tracks around the current one, the service's
// status and the device Spotify plays on come from
// $XDG_RUNTIME_DIR/spotitui/queue.json, which spotitui keeps up to date.
Panel {
  id: root
  moduleName: "boudbois.spotify"
  ipcTarget: "boudbois.spotify"

  readonly property string glyph: "󰓇"
  readonly property string queuePath: Quickshell.env("XDG_RUNTIME_DIR") + "/spotitui/queue.json"
  // ~/.local/bin/spotitui-open: focuses the spotitui window, or opens one.
  readonly property var launchCommand: [Quickshell.env("HOME") + "/.local/bin/spotitui-open"]

  // What is playing, such as a playlist's name, from queue.json.
  property string context: ""
  // Why the service can't play right now ("connecting", "offline", "logged out"…),
  // empty while connected; and the other device Spotify plays on, if any.
  property string serviceStatus: ""
  property string device: ""
  property bool devicePlaying: false
  // The latest queue.json, kept until it matches the playing track.
  property var pendingQueue: null
  // Seconds; refreshed by the timer below, since MPRIS doesn't push positions.
  property real position: 0
  property bool seeking: false

  readonly property var player: {
    var list = Mpris.players ? Mpris.players.values : []
    for (var i = 0; i < list.length; i++) {
      if (String(list[i].dbusName || "").toLowerCase().indexOf("spotitui") !== -1) return list[i]
    }
    return null
  }
  readonly property bool running: player !== null
  readonly property bool hasTrack: running && !!player.trackTitle
  readonly property bool playing: running && player.isPlaying
  readonly property bool loggedOut: running && serviceStatus === "logged out"
  // Seeking and skipping work once the track is loaded here; Play brings it here.
  readonly property bool controllable: hasTrack && player.canGoNext
  readonly property real length: hasTrack && player.lengthSupported ? player.length : 0
  // The list always has five slots: one before the playing track, the playing
  // track, three after. On a track change the slots shift at once (we already
  // know what comes next) and animate; queue.json then confirms or corrects
  // them. A fixed height keeps the panel from resizing while songs change.
  readonly property int slotCount: 5
  readonly property real rowHeight: Style.space(30)
  readonly property real rowGap: Style.space(4)
  // +1 when rows move up (going forward), -1 when they move down (going back).
  property int shiftDirection: 1
  readonly property bool showList: hasTrack && slots.count === slotCount

  ListModel { id: slots }

  readonly property string statusText: {
    if (!running) return "NOT RUNNING"
    if (serviceStatus) return serviceStatus.toUpperCase()
    if (!hasTrack) return "NOTHING PLAYED YET"
    if (!playing && device) return (devicePlaying ? "PLAYING ON " : "PAUSED ON ") + device.toUpperCase()
    return playing ? "PLAYING" : "PAUSED"
  }

  function formatTime(seconds) {
    var s = Math.max(0, Math.floor(seconds || 0))
    var m = Math.floor(s / 60)
    var rest = s % 60
    if (m >= 60) return Math.floor(m / 60) + ":" + String(m % 60).padStart(2, "0") + ":" + String(rest).padStart(2, "0")
    return m + ":" + String(rest).padStart(2, "0")
  }

  // When the track changes, MPRIS still reports the old track's position for a
  // moment; don't let that flash on the new track's progress bar. Once that
  // moment is over, a paused track (as after spotitui starts) shows where it is.
  property double trackChangedAt: 0

  Timer {
    id: settleTimer
    interval: 1100
    onTriggered: root.syncPosition()
  }

  function syncPosition() {
    // `player` goes before `hasTrack` follows when the service stops.
    if (!player || !hasTrack || seeking) return
    player.positionChanged()
    if (Date.now() - trackChangedAt < 1000 && player.position > 3) return
    position = player.position
  }

  function togglePlay() { if (running && player.canTogglePlaying) player.togglePlaying() }
  function previous() { if (controllable && player.canGoPrevious) player.previous() }
  function next() { if (running && player.canGoNext) player.next() }
  function seekTo(seconds) {
    if (!hasTrack || !player.canSeek) return
    player.position = Math.max(0, Math.min(length, seconds))
    position = player.position
  }
  // Jumps to a track in the list: -1 is the one before, 1.. the ones after.
  // spotitui's io.spotitui.Queue.Skip does it in one ordered step; MPRIS alone
  // can't go back reliably (Previous restarts the track after 3 seconds).
  function skipTo(offset) {
    if (!controllable || offset === 0) return
    skipProc.offset = offset
    skipProc.command = ["busctl", "--user", "call", "--", String(player.dbusName),
                        "/org/mpris/MediaPlayer2", "io.spotitui.Queue", "Skip", "i", String(offset)]
    skipProc.running = true
  }

  // For a spotitui without io.spotitui.Queue.
  function skipWithMpris(offset) {
    if (!hasTrack) return
    if (offset < 0) {
      goBack()
      return
    }
    for (var i = 0; i < offset; i++) {
      if (player.canGoNext) player.next()
    }
  }

  // MPRIS Previous only goes back within a track's first seconds; later it
  // restarts the track. So rewind first, wait until the player reports the
  // rewind, then go back. (Sending both at once raced: the restart won.)
  function goBack() {
    player.positionChanged()
    if (player.position < root.restartThreshold) {
      player.previous()
      return
    }
    if (player.canSeek) player.position = 0
    else player.previous() // restarts the track; the timer then goes back
    backTimer.attempts = 0
    backTimer.start()
  }

  readonly property real restartThreshold: 2.5

  Timer {
    id: backTimer
    property int attempts: 0
    interval: 100
    repeat: true
    onTriggered: {
      attempts += 1
      if (!root.hasTrack || attempts > 20) { stop(); return }
      root.player.positionChanged()
      if (root.player.position < root.restartThreshold) {
        stop()
        root.player.previous()
      }
    }
  }

  Process {
    id: skipProc
    property int offset: 0
    onExited: function(code) { if (code !== 0) root.skipWithMpris(offset) }
  }

  function launch() {
    Quickshell.execDetached(launchCommand)
    root.close()
  }

  function loadQueue(text) {
    var parsed = null
    try {
      parsed = JSON.parse(String(text || ""))
    } catch (e) {}
    pendingQueue = parsed && parsed.tracks instanceof Array ? parsed : null
    // Not tied to the playing track: these apply at once.
    serviceStatus = parsed ? String(parsed.status || "") : ""
    device = parsed ? String(parsed.device || "") : ""
    devicePlaying = !!(parsed && parsed.device_playing)
    applyPending()
  }

  function trackId(uri) {
    var s = String(uri || "")
    return s.substring(s.lastIndexOf(":") + 1)
  }

  // The playing track's Spotify ID, from xesam:url (open.spotify.com/track/<id>)
  // or else mpris:trackid, which Quickshell stringifies as QDBusObjectPath("…/<id>").
  function playingId() {
    if (!hasTrack || !player.metadata) return ""
    var sources = [player.metadata["xesam:url"], player.metadata["mpris:trackid"]]
    for (var i = 0; i < sources.length; i++) {
      var match = /track\/([A-Za-z0-9]+)/.exec(String(sources[i] || ""))
      if (match) return match[1]
    }
    return ""
  }

  // `pending` slots wait for queue.json; the others are known, possibly empty
  // (no track before the first one of a playlist).
  function slotData(track, offset, pending) {
    return {
      uri: track ? String(track.uri || "") : "",
      title: track ? String(track.title || "") : "",
      artist: track ? String(track.artist || "") : "",
      offset: offset,
      empty: !track,
      pending: !track && pending
    }
  }

  function fillSlots(entries) {
    if (slots.count !== slotCount) {
      slots.clear()
      for (var i = 0; i < entries.length; i++) slots.append(entries[i])
      return
    }
    for (var j = 0; j < entries.length; j++) {
      var was = slots.get(j)
      var now = entries[j]
      if (was.uri !== now.uri || was.title !== now.title || was.artist !== now.artist
          || was.empty !== now.empty || was.pending !== now.pending || was.offset !== now.offset) {
        slots.set(j, now)
      }
    }
  }

  // Takes queue.json once it describes the playing track.
  function applyPending() {
    var queue = pendingQueue
    if (!queue || !hasTrack) return
    var byOffset = {}
    for (var i = 0; i < queue.tracks.length; i++) byOffset[queue.tracks[i].offset] = queue.tracks[i]
    if (!byOffset[0] || trackId(byOffset[0].uri) !== playingId()) return
    var entries = []
    for (var offset = -1; offset < slotCount - 1; offset++) entries.push(slotData(byOffset[offset], offset, false))
    fillSlots(entries)
    context = String(queue.context || "")
  }

  // Moves every slot by `k` places: k > 0 when skipping forward.
  function shift(k) {
    shiftDirection = k > 0 ? 1 : -1
    for (var n = 0; n < Math.abs(k); n++) {
      if (k > 0) {
        slots.remove(0)
        slots.append(slotData(null, slotCount - 2, true))
      } else {
        slots.remove(slots.count - 1)
        slots.insert(0, slotData(null, -1, true))
      }
    }
    for (var i = 0; i < slots.count; i++) slots.setProperty(i, "offset", i - 1)
  }

  // A new track plays: shift the list if we know where it was, else show it alone.
  function followTrack() {
    if (!hasTrack) {
      slots.clear()
      return
    }
    var id = playingId()
    var found = null
    for (var i = 0; i < slots.count; i++) {
      var slot = slots.get(i)
      if (!slot.empty && trackId(slot.uri) === id) { found = slot.offset; break }
    }
    if (found === null) {
      var entries = []
      for (var offset = -1; offset < slotCount - 1; offset++) {
        entries.push(offset === 0
          ? slotData({ uri: "spotify:track:" + id, title: player.trackTitle, artist: player.trackArtist }, 0, false)
          : slotData(null, offset, true))
      }
      fillSlots(entries)
    } else if (found !== 0) {
      shift(found)
    }
    applyPending()
  }

  // Command-line control: omarchy-shell boudbois.spotify-player seek 90 | skip 2
  IpcHandler {
    target: "boudbois.spotify-player"
    function seek(seconds: real): void { root.seekTo(seconds) }
    function skip(offset: int): void { root.skipTo(offset) }
    function playPause(): void { root.togglePlay() }
  }

  FileView {
    id: queueFile
    path: root.queuePath
    watchChanges: true
    printErrors: false
    onLoaded: root.loadQueue(text())
    // text() is stale inside the change signal; reload and parse in onLoaded.
    onFileChanged: reload()
    onLoadFailed: root.loadQueue("")
  }

  onOpenedChanged: if (opened) { queueFile.reload(); syncPosition() }
  onPlayerChanged: { syncPosition(); Qt.callLater(followTrack) }
  onHasTrackChanged: Qt.callLater(followTrack)
  Component.onCompleted: Qt.callLater(followTrack)

  Timer {
    interval: 500
    running: root.opened && root.playing
    repeat: true
    onTriggered: root.syncPosition()
  }

  // spotitui replaces the file atomically; re-read now and then in case the
  // watch lost track of the replaced file.
  Timer {
    interval: 3000
    running: root.opened
    repeat: true
    onTriggered: queueFile.reload()
  }

  Connections {
    target: root.player
    ignoreUnknownSignals: true
    function onTrackTitleChanged() { root.position = 0; Qt.callLater(root.syncPosition) }
    function onPostTrackChanged() {
      root.trackChangedAt = Date.now()
      settleTimer.restart()
      root.position = 0
      Qt.callLater(root.followTrack)
      heroFade.restart()
    }
    function onMetadataChanged() { Qt.callLater(root.followTrack) }
    function onIsPlayingChanged() { root.syncPosition() }
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.glyph
    tooltipText: root.hasTrack ? root.player.trackTitle + (root.player.trackArtist ? " — " + root.player.trackArtist : "") : ""
    // Left: the mini player. Right: the spotitui window.
    onPressed: function(b) {
      if (b === Qt.LeftButton) root.toggle()
      else if (b === Qt.RightButton) root.launch()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(380))
    contentHeight: panel.fittedContentHeight(column.implicitHeight)

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onActivateRequested: root.hasTrack && !root.loggedOut ? root.togglePlay() : root.launch()
      onMoveRequested: function(dx, dy) {
        if (dx < 0) root.previous()
        else if (dx > 0) root.next()
      }
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) {
        if (t === "n") root.next()
        else if (t === "p") root.previous()
      }

      Column {
        id: column
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        spacing: Style.space(14)

        // ---------- Hero: cover · title / artist / status ----------
        Item {
          width: parent.width
          implicitHeight: Math.max(cover.height, heroLabels.implicitHeight)

          Rectangle {
            id: cover
            width: Style.space(56)
            height: width
            anchors.left: parent.left
            anchors.verticalCenter: parent.verticalCenter
            radius: Style.cornerRadius > 0 ? Style.space(6) : 0
            color: Util.alpha(root.bar.foreground, 0.08)
            clip: true

            Image {
              id: art
              anchors.fill: parent
              // Local files only: loading https covers in several widget copies at once
              // crashed Quickshell inside Qt's OpenSSL setup.
              source: root.hasTrack && String(root.player.trackArtUrl || "").indexOf("file://") === 0
                ? root.player.trackArtUrl : ""
              fillMode: Image.PreserveAspectCrop
              asynchronous: true
              sourceSize.width: Math.round(width * Screen.devicePixelRatio)
              sourceSize.height: Math.round(height * Screen.devicePixelRatio)
              visible: status === Image.Ready
            }

            Text {
              anchors.centerIn: parent
              visible: !art.visible
              text: root.glyph
              color: root.bar.foreground
              opacity: root.hasTrack ? 1.0 : 0.5
              font.family: root.bar.fontFamily
              font.pixelSize: Style.font.display
            }
          }

          // A short crossfade when the track changes.
          SequentialAnimation {
            id: heroFade
            ParallelAnimation {
              NumberAnimation { target: heroLabels; property: "opacity"; to: 0.25; duration: 90 }
              NumberAnimation { target: cover; property: "opacity"; to: 0.4; duration: 90 }
            }
            ParallelAnimation {
              NumberAnimation { target: heroLabels; property: "opacity"; to: 1; duration: 260; easing.type: Easing.OutCubic }
              NumberAnimation { target: cover; property: "opacity"; to: 1; duration: 260; easing.type: Easing.OutCubic }
            }
          }

          Column {
            id: heroLabels
            anchors.left: cover.right
            anchors.leftMargin: Style.space(14)
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            spacing: Style.space(2)

            Text {
              width: parent.width
              textFormat: Text.PlainText
              text: root.hasTrack ? root.player.trackTitle : "Spotify"
              color: root.bar.foreground
              font.family: root.bar.fontFamily
              font.pixelSize: Style.font.title
              font.bold: true
              elide: Text.ElideRight
            }

            Text {
              width: parent.width
              visible: root.hasTrack
              textFormat: Text.PlainText
              text: [root.player ? root.player.trackArtist : "", root.player ? root.player.trackAlbum : ""]
                .filter(function(part) { return !!part }).join(" · ")
              color: root.bar.foreground
              opacity: 0.7
              font.family: root.bar.fontFamily
              font.pixelSize: Style.font.bodySmall
              elide: Text.ElideRight
            }

            Text {
              width: parent.width
              textFormat: Text.PlainText
              text: root.statusText
              color: Qt.darker(root.bar.foreground, 1.4)
              font.family: root.bar.fontFamily
              font.pixelSize: Style.font.caption
              font.bold: true
              font.letterSpacing: 1.2
              elide: Text.ElideRight
            }
          }
        }

        // ---------- Not running, logged out or nothing played yet: the window helps ----------
        Button {
          visible: !root.hasTrack || root.loggedOut
          width: parent.width
          text: root.loggedOut ? "Log in" : "Open spotitui"
          foreground: root.bar.foreground
          fontFamily: root.bar.fontFamily
          bordered: true
          onClicked: root.launch()
        }

        // ---------- Controls ----------
        Row {
          id: controls
          visible: root.hasTrack
          anchors.horizontalCenter: parent.horizontalCenter
          spacing: Style.space(10)

          Button {
            iconText: "󰒮"
            tooltipText: "Previous"
            foreground: root.bar.foreground
            fontFamily: root.bar.fontFamily
            iconSize: Style.font.title
            bordered: true
            enabled: root.controllable
            onClicked: root.previous()
          }

          Button {
            iconText: root.playing ? "󰏤" : "󰐊"
            tooltipText: root.playing ? "Pause" : "Play"
            foreground: root.bar.foreground
            fontFamily: root.bar.fontFamily
            iconSize: Style.font.title
            horizontalPadding: Style.spacing.controlPaddingX * 2
            bordered: true
            active: root.playing
            enabled: root.hasTrack && !root.loggedOut
            onClicked: root.togglePlay()
          }

          Button {
            iconText: "󰒭"
            tooltipText: "Next"
            foreground: root.bar.foreground
            fontFamily: root.bar.fontFamily
            iconSize: Style.font.title
            bordered: true
            enabled: root.controllable
            onClicked: root.next()
          }
        }

        // ---------- Progress ----------
        Column {
          visible: root.hasTrack
          width: parent.width
          spacing: Style.space(2)

          PanelSlider {
            id: progress
            bar: root.bar
            width: parent.width
            minimum: 0
            maximum: Math.max(1, root.length)
            step: 1
            value: root.position
            enabled: root.player !== null && root.player.canSeek
            onDraggingChanged: root.seeking = dragging
            onReleased: function(v) { root.seekTo(v) }
          }

          Item {
            width: parent.width
            implicitHeight: elapsedLabel.implicitHeight

            Text {
              id: elapsedLabel
              anchors.left: parent.left
              textFormat: Text.PlainText
              text: root.formatTime(progress.dragging ? progress.liveValue : root.position)
              color: root.bar.foreground
              opacity: 0.6
              font.family: root.bar.fontFamily
              font.pixelSize: Style.font.caption
            }

            Text {
              anchors.right: parent.right
              textFormat: Text.PlainText
              text: root.formatTime(root.length)
              color: root.bar.foreground
              opacity: 0.6
              font.family: root.bar.fontFamily
              font.pixelSize: Style.font.caption
            }
          }
        }

        // ---------- Around the current track ----------
        PanelSeparator { visible: root.showList; foreground: root.bar.foreground }

        Column {
          visible: root.showList
          width: parent.width
          spacing: Style.space(4)

          PanelSectionHeader {
            text: root.context ? root.context.toUpperCase() : "QUEUE"
            foreground: root.bar.foreground
            fontFamily: root.bar.fontFamily
          }

          ListView {
            id: list
            width: parent.width
            height: root.slotCount * root.rowHeight + (root.slotCount - 1) * root.rowGap
            spacing: root.rowGap
            interactive: false
            clip: true
            model: slots

            readonly property int slideDuration: 280

            // Skipping forward: the first row leaves upwards, a new one rises from below.
            // Going back: the reverse.
            add: Transition {
              NumberAnimation { property: "opacity"; from: 0; to: 1; duration: list.slideDuration }
              NumberAnimation {
                property: "y"
                from: root.shiftDirection > 0 ? list.height : -root.rowHeight
                duration: list.slideDuration
                easing.type: Easing.OutCubic
              }
            }
            remove: Transition {
              NumberAnimation { property: "opacity"; to: 0; duration: list.slideDuration * 0.7 }
              NumberAnimation {
                property: "y"
                to: root.shiftDirection > 0 ? -root.rowHeight : list.height
                duration: list.slideDuration
                easing.type: Easing.OutCubic
              }
            }
            displaced: Transition {
              NumberAnimation { property: "y"; duration: list.slideDuration; easing.type: Easing.OutCubic }
            }

            delegate: Rectangle {
              id: row
              required property string uri
              required property string title
              required property string artist
              required property int offset
              required property bool empty
              required property bool pending
              readonly property bool current: offset === 0

              width: ListView.view.width
              height: root.rowHeight
              radius: Style.cornerRadius > 0 ? Style.space(4) : 0
              color: current ? Style.selectedFillFor(root.bar.foreground, Color.accent)
                : rowMouse.containsMouse ? Style.hoverFillFor(root.bar.foreground, Color.accent)
                : "transparent"
              Behavior on color { ColorAnimation { duration: list.slideDuration } }

              MouseArea {
                id: rowMouse
                anchors.fill: parent
                enabled: !row.current && !row.empty && root.controllable
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                onClicked: root.skipTo(row.offset)
              }

              // Waiting for Spotify to say what goes here.
              Rectangle {
                visible: row.pending
                anchors.left: parent.left
                anchors.leftMargin: Style.space(32)
                anchors.verticalCenter: parent.verticalCenter
                width: parent.width * 0.45
                height: Style.space(6)
                radius: height / 2
                color: Util.alpha(root.bar.foreground, 0.12)

                SequentialAnimation on opacity {
                  running: row.pending
                  loops: Animation.Infinite
                  NumberAnimation { from: 0.4; to: 1.0; duration: 600; easing.type: Easing.InOutSine }
                  NumberAnimation { from: 1.0; to: 0.4; duration: 600; easing.type: Easing.InOutSine }
                }
              }

              Item {
                id: content
                anchors.fill: parent
                opacity: row.empty ? 0 : 1
                Behavior on opacity { NumberAnimation { duration: 200 } }

                Text {
                  id: marker
                  anchors.left: parent.left
                  anchors.leftMargin: Style.space(8)
                  anchors.verticalCenter: parent.verticalCenter
                  width: Style.space(16)
                  textFormat: Text.PlainText
                  text: row.offset === 0 ? (root.playing ? "󰎈" : "󰏤") : (row.offset < 0 ? "󰒮" : String(row.offset))
                  color: root.bar.foreground
                  opacity: row.current ? 1.0 : 0.5
                  font.family: root.bar.fontFamily
                  font.pixelSize: Style.font.caption
                  horizontalAlignment: Text.AlignHCenter
                }

                Text {
                  id: titleText
                  anchors.left: marker.right
                  anchors.leftMargin: Style.space(8)
                  anchors.verticalCenter: parent.verticalCenter
                  width: Math.min(implicitWidth, parent.width - x - Style.space(8)
                    - (artistText.text ? Math.min(artistText.implicitWidth, parent.width * 0.35) + Style.space(10) : 0))
                  textFormat: Text.PlainText
                  text: row.title || row.uri
                  color: root.bar.foreground
                  opacity: row.offset < 0 ? 0.55 : 1.0
                  font.family: root.bar.fontFamily
                  font.pixelSize: Style.font.bodySmall
                  font.bold: row.current
                  elide: Text.ElideRight
                  Behavior on opacity { NumberAnimation { duration: list.slideDuration } }
                }

                Text {
                  id: artistText
                  anchors.left: titleText.right
                  anchors.leftMargin: Style.space(10)
                  anchors.right: parent.right
                  anchors.rightMargin: Style.space(8)
                  // Same text line as the title (smaller text centred on its own sits higher).
                  anchors.baseline: titleText.baseline
                  textFormat: Text.PlainText
                  text: row.artist
                  color: root.bar.foreground
                  opacity: row.offset < 0 ? 0.35 : 0.6
                  font.family: root.bar.fontFamily
                  font.pixelSize: Style.font.caption
                  elide: Text.ElideRight
                  Behavior on opacity { NumberAnimation { duration: list.slideDuration } }
                }
              }
            }
          }
        }
      }
    }
  }
}
