pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Controls
import QtQuick.Shapes
import Quickshell
import Quickshell.Hyprland
import Quickshell.Io
import Quickshell.Wayland
import qs.Commons
import qs.Commons as Commons
import qs.Ui

Panel {
  id: root
  moduleName: "daftmause.omadai"
  ipcTarget: "daftmause.omadai"
  manageIpc: false

  readonly property color foreground: bar ? bar.foreground : Commons.Color.foreground
  readonly property color urgent: bar ? bar.urgent : Commons.Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)
  readonly property color accent: Commons.Color.accent
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family

  readonly property var addablePlatforms: [
    { id: "openai", name: "OpenAI" },
    { id: "anthropic", name: "Anthropic" },
    { id: "fal", name: "fal.ai" },
    { id: "higgsfield", name: "Higgsfield" },
    { id: "byteplus", name: "BytePlus" },
    { id: "flux", name: "Flux" },
    { id: "fish", name: "Fish Audio" },
    { id: "xai", name: "xAI" },
    { id: "gemini", name: "Gemini" }
  ]

  readonly property var refreshChoices: [
    { value: "15", label: "15m" },
    { value: "30", label: "30m" },
    { value: "60", label: "1h" },
    { value: "180", label: "3h" },
    { value: "360", label: "6h" }
  ]
  readonly property var allowedMinutes: [15, 30, 60, 180, 360]

  // The drag handle's hit area, the column the app's key lines and items indent past.
  readonly property real gripWidth: Style.space(16)
  readonly property color deleteTint: Qt.tint(Commons.Color.urgent, Util.alpha(foreground, 0.35))
  readonly property string fetchFailedText: "Fetch failed · open the app for details"
  readonly property int staleMs: 300000
  readonly property int panelMaxRows: 10
  readonly property int panelMaxItems: 3
  // The first month the arrows reach: the ChatGPT API launch, 2023-03-01 (openai.com, "Introducing ChatGPT and Whisper APIs").
  readonly property string floorMonth: "2023-03"
  readonly property string infoIcon: "\u{F02FD}"
  readonly property string logIcon: "\u{F0219}"

  // User values reach this script only as positional arguments ($1 the action id, the rest the `omadai key` argv).
  readonly property string keyActionScript: 'id=$1; shift; err=$(omadai key "$@" 2>&1 >/dev/null); st=$?; '
    + 'line=$(printf \'%s\\n\' "$err" | awk \'{ sub(/^[ \\t]+/, ""); sub(/[ \\t]+$/, "") } '
    + '/^omadai: / { print; found = 1; exit } NF && !/^waiting: / && first == "" { first = $0 } '
    + 'END { if (!found) print first }\'); '
    + 'for i in $(seq 120); do omarchy-shell daftmause.omadai keyActionDone "$id" "$st" "$line" >/dev/null 2>&1 && exit 0; sleep 0.5; done'
  readonly property int keyActionTimeoutMs: 60000
  readonly property int keyActionLateMs: 600000
  readonly property string keyActionTimeoutText: "the key change has not reported back yet; check the key list"

  property bool binaryMissing: false
  property string startProblem: ""
  property double nowMs: Date.now()
  // The UTC month the clock is in, kept current by monthClock so bindings follow a month rollover.
  property string nowMonth: currentMonth()

  // The reporting panel: the current month only.
  property var barReport: null
  property string barReportProblem: ""
  property var keys: []
  property string keysProblem: ""
  // The panel's total sums connected platforms, so it is unknown until the key list has been read once.
  property bool keysLoaded: false
  property var panelExpanded: ({})

  // Fetches run one at a time on the fetch owner, which copies these to every instance; fetchQueue[0] is the month running while fetchProc is busy.
  property var fetchQueue: []
  property var fetchFailures: ({})
  // Set when a fetch that ran printed a "log: " line, and cleared by the next one that did not.
  property bool fetchLogProblem: false
  property string fetchNotice: ""
  // Months waiting for staleChecker to read their report and decide whether to fetch them.
  property var staleQueue: []
  // When each month was last fetched because it was stale, in ms; every instance holds the same copy, so every bar shares one backoff whichever owns the fetch.
  property var staleAttempts: ({})
  // Months a key changed during their running fetch, which run once more when it ends.
  property var rerunMonths: []
  property bool fetching: false
  // Set while this instance is being destroyed, so it is no longer the owner, a requester or an IPC target.
  property bool departing: false
  // Who queued each month (instances, by reference), so closing an app drops only the months nobody else wants.
  property var fetchRequesters: ({})
  // The month refused because another process holds the fetch lock, and when its refusals began.
  property string retryMonth: ""
  property double retryStartedMs: 0
  readonly property int retryDelayMs: 5000
  readonly property int retryBoundMs: 600000

  // The app window: its own month or year, report and key controls.
  property string windowMode: "month"
  property string appMonth: ""
  property string appYear: ""
  property var appReport: null
  property string appReportProblem: ""
  property var appExpanded: ({})
  property string renamePlatform: ""
  property string renameLabel: ""
  // The label typed so far, which the inline field of the key being renamed writes as it changes.
  property string renameDraft: ""
  // Tells the inline field to take the draft and the keyboard, since it lives in a row delegate no id reaches.
  signal renameStarted()
  property var removeTarget: null
  property string actionError: ""
  // The CLI's note on a change that succeeded, such as a removed label whose credential was already gone.
  property string actionNote: ""
  // The detached key change this instance started, until keyActionDone or the timeout ends it.
  property string pendingActionId: ""
  property string pendingActionKind: ""
  property string lateActionId: ""
  property string lateActionKind: ""
  property double pendingActionStartedMs: 0
  // Set on every instance while any of them has a key change running.
  property bool keyActionBusy: false
  // The app list during a drag: the order shown, the row held and its top edge; incoming reports wait until the drop.
  property var appRowsFrozen: null
  property var dragOrder: []
  property string dragId: ""
  property real dragTop: 0
  property real dragStartTop: 0
  property var rowHeights: ({})
  // How many grips the pointer is over, since a held grip slides across the others while it is dragged.
  property int gripsHovered: 0
  readonly property bool gripHovered: gripsHovered > 0
  property bool dragEnding: false
  property bool addOpen: false
  property string addError: ""
  // The error log as `omadai log --json` last printed it, newest first; every instance holds the same copy.
  property var logEntries: []
  property string logProblem: ""
  property bool logOpen: false
  // What Export printed, or why Export or Clear failed, for the log window's caption.
  property string logExported: ""
  property string logActionError: ""
  readonly property int refreshMinutes: normalizeMinutes(setting("refreshMinutes", 60))
  readonly property double refreshIntervalMs: refreshMinutes * 60000
  property bool appOpen: false
  readonly property var ownScreen: root.QsWindow.window ? root.QsWindow.window.screen : null
  readonly property string ownScreenName: ownScreen ? String(ownScreen.name || "") : ""

  readonly property var connectedPlatforms: {
    var ids = []
    for (var i = 0; i < keys.length; i++)
      if (ids.indexOf(keys[i].platform) < 0) ids.push(keys[i].platform)
    return ids
  }
  // The CLI holds one Gemini export, so Gemini leaves the add list once it has a key.
  readonly property var offeredPlatforms: addablePlatforms.filter(function(p) {
    return p.id !== "gemini" || connectedPlatforms.indexOf("gemini") < 0
  })
  // Changes only when the set of keys does, unlike the array, which is rebuilt on every key-list read.
  readonly property string keySet: keys.map(function(k) { return k.platform + "\n" + k.label }).sort().join("\n\n")
  // When the set of keys last changed, in ms, as noteKeySet keeps it; a fetch from before it may lack a new key's spend.
  property double keysChangedMs: 0

  // One row per platform with a key: the report's row where it has one, else a not-yet-fetched placeholder.
  readonly property var panelRows: {
    var rows = []
    var seen = []
    var platforms = barReport ? barReport.platforms : []
    for (var i = 0; i < platforms.length; i++) {
      if (connectedPlatforms.indexOf(platforms[i].platform) >= 0) {
        rows.push(platforms[i])
        seen.push(platforms[i].platform)
      }
    }
    for (var j = 0; j < connectedPlatforms.length; j++) {
      var id = connectedPlatforms[j]
      if (seen.indexOf(id) < 0) rows.push(placeholderRow(id))
    }
    return orderRows(rows)
  }

  // The saved platform order: ids, including ones not connected now, so a reconnected platform returns to its place.
  readonly property var platformOrder: {
    var v = setting("platformOrder", [])
    return Array.isArray(v) ? v.filter(function(x) { return typeof x === "string" }) : []
  }

  // The connected platforms, with the app window's figures where it has them, in the saved order.
  readonly property var appRowsLive: {
    // A report that failed still leaves the keys listed, so they can be renamed or removed.
    if (!appReport && appReportProblem === "") return []
    var rows = []
    var seen = []
    var platforms = appReport ? appReport.platforms : []
    for (var i = 0; i < platforms.length; i++) {
      if (connectedPlatforms.indexOf(platforms[i].platform) >= 0) {
        rows.push(platforms[i])
        seen.push(platforms[i].platform)
      }
    }
    for (var j = 0; j < connectedPlatforms.length; j++)
      if (seen.indexOf(connectedPlatforms[j]) < 0) rows.push(placeholderRow(connectedPlatforms[j]))
    return orderRows(rows)
  }
  readonly property var appRows: appRowsFrozen !== null ? appRowsFrozen : appRowsLive
  readonly property var appRowIds: appRows.map(function(r) { return r.platform })
  readonly property var shownOrder: dragId !== "" ? dragOrder : appRowIds

  // The panel lists connected platforms only, so its total is theirs rather than report.total.
  readonly property string panelTotal: {
    var values = []
    for (var i = 0; i < panelRows.length; i++) values.push(panelRows[i].total)
    return sumDecimals(values)
  }

  // The app's total is the rows it shows, so a platform whose last key was removed never counts.
  readonly property string appSum: {
    var values = []
    for (var i = 0; i < appRows.length; i++) values.push(appRows[i].total)
    return sumDecimals(values)
  }

  readonly property string panelUpdated: newestFetch(panelRows)
  readonly property string appUpdated: newestFetch(appRows)

  // The app's counterpart of panelFetchFailed, over every month on screen.
  readonly property string appFetchFailure: {
    var months = viewMonths()
    var rows = appReport ? appReport.platforms : []
    for (var i = 0; i < months.length; i++) {
      var text = fetchFailures[months[i]]
      if (text === undefined) continue
      var id = failedPlatform(text)
      if (id === "" || !rows.some(function(p) { return p.platform === id && !!p.last_error }))
        return fetchLogProblem ? "Fetch failed · run omadai fetch in a terminal" : "Fetch failed · details in the error log"
    }
    return fetchLogProblem ? "The error log had a problem · run omadai log in a terminal" : ""
  }

  // A failure the rows cannot show: of the whole fetch, or of a platform whose row recorded no error, as when its cache could not be saved.
  readonly property bool panelFetchFailed: {
    var text = fetchFailures[nowMonth]
    if (text === undefined) return false
    var id = failedPlatform(text)
    var rows = barReport ? barReport.platforms : []
    return id === "" || !rows.some(function(p) { return p.platform === id && !!p.last_error })
  }

  // Saved ids first in their saved order, then the rest in the order given.
  function orderRows(rows) {
    var saved = platformOrder
    return rows.map(function(r, i) { return { r: r, i: i, s: saved.indexOf(r.platform) } })
      .sort(function(a, b) {
        if (a.s >= 0 && b.s >= 0) return a.s - b.s
        if (a.s >= 0) return -1
        if (b.s >= 0) return 1
        return a.i - b.i
      })
      .map(function(x) { return x.r })
  }

  function normalizeMinutes(value) {
    var n = parseInt(value, 10)
    if (!isFinite(n)) n = 60
    n = Math.min(360, Math.max(15, n))
    var best = allowedMinutes[0]
    for (var i = 1; i < allowedMinutes.length; i++)
      if (Math.abs(allowedMinutes[i] - n) < Math.abs(best - n)) best = allowedMinutes[i]
    return best
  }

  function keysFor(platform) {
    return keys.filter(function(k) { return k.platform === platform })
  }

  // Items largest first, for the panel's top three; the figures are only ordered here, never shown from the float.
  function largestFirst(items) {
    return (items || []).slice().sort(function(a, b) { return (parseFloat(b.total) || 0) - (parseFloat(a.total) || 0) })
  }

  // The current month and the five before it fetch on their own; older months only on Refresh.
  function autoFetchable(month) {
    var n = monthCount(month, nowMonth)
    return n >= 1 && n <= 6
  }

  // The report's kind as its chip reads, with the description the app shows on hover.
  function chipLabel(kind) {
    return kind === "billed" ? "billed" : (kind === "estimated" ? "estimate" : (kind === "derived" ? "from balance" : ""))
  }

  function chipDescription(kind, platform) {
    if (kind === "billed") return "Billed: the platform's own charge"
    if (kind === "estimated") return "Estimate: the platform's running figure, which can change before the invoice"
    if (kind !== "derived") return ""
    var text = "From balance: this platform reports no spend, so omadai works it out from credits going down"
    // Fish Audio's API never states its unit, and this is the one place the app can say so.
    return platform === "fish" ? text + " · Fish Audio's figures are assumed to be US dollars" : text
  }

  // The newest successful fetch among the rows, as its ISO stamp, or "" when none was fetched.
  function newestFetch(rows) {
    var best = ""
    var bestMs = -Infinity
    for (var i = 0; i < rows.length; i++) {
      var t = new Date(String(rows[i].last_fetched || "")).getTime()
      if (isFinite(t) && t > bestMs) { bestMs = t; best = String(rows[i].last_fetched) }
    }
    return best
  }

  // A fetch failure line starts with the platform id when one platform failed, and with "omadai:" when the fetch itself did; returns that id, or "".
  function failedPlatform(text) {
    var m = /^([a-z0-9_-]+): /.exec(String(text || ""))
    if (!m || m[1] === "omadai") return ""
    for (var i = 0; i < addablePlatforms.length; i++) if (addablePlatforms[i].id === m[1]) return m[1]
    var reports = [barReport, appReport]
    for (var r = 0; r < reports.length; r++) {
      var rows = reports[r] ? reports[r].platforms : []
      for (var j = 0; j < rows.length; j++) if (rows[j].platform === m[1]) return m[1]
    }
    return ""
  }

  function placeholderRow(id) {
    return {
      platform: id, name: platformName(id), kind: "", breakdown: "key", total: "0", items: [],
      credits_remaining: null, data_as_of: null, last_fetched: null, last_error: null,
      requirement: null, status_month: null, missing_months: [], warnings: [],
      before_history: [], history_from: null
    }
  }

  // Every month of the view lies before the platform's history, so the row has no figure to show.
  function historyWhole(p, months) {
    var before = p ? (p.before_history || []) : []
    return months.length > 0 && months.every(function(m) { return before.indexOf(m) >= 0 })
  }

  // History starts inside the view and some month of it has figures.
  function historyPartial(p, months) {
    return !!p && !!p.history_from && !!p.last_fetched && !historyWhole(p, months)
  }

  function historyText(date) {
    var parts = String(date || "").split("-")
    if (parts.length !== 3) return ""
    if (parts[2] === "01") return "No history before " + monthTitle(parts[0] + "-" + parts[1])
    var name = monthTitle(parts[0] + "-" + parts[1]).split(" ")[0]
    return "No history before " + name + " " + Number(parts[2]) + ", " + parts[0]
  }

  // `report` writes "<month>: unreadable cache, left out: …" itself and never logs it.
  function unreadableOnly(warnings) {
    var list = warnings || []
    return list.length > 0 && list.every(function(w) { return /^(\d{4}-\d{2}: )?unreadable cache/.test(String(w)) })
  }

  // The disc's hover text: in the app every failure points at the error log; the bar panel keeps its own failure text.
  function discText(p, scope) {
    if (!p) return ""
    if (p.last_error) {
      if (scope !== "app") return fetchFailedText
      var failed = p.status_month ? "Fetch failed for " + monthTitle(p.status_month).split(" ")[0] : "Fetch failed"
      return failed + (logged(p.platform, p.status_month ? [p.status_month] : viewMonths(), false)
        ? " · details in the error log" : " · run omadai report in a terminal")
    }
    if (unreadableOnly(p.warnings)) return "A cached month could not be read · run omadai report in a terminal"
    if ((p.warnings || []).length > 0)
      return "Part of this platform's spend is missing" + (scope !== "app" || logged(p.platform, viewMonths(), true)
        ? " · details in the error log" : " · run omadai report in a terminal")
    return scope === "app" ? "" : "Part of this platform's spend is missing · open the app for details"
  }

  // Whether the log still holds an entry for this platform in one of these months, since it outlives neither 30 days nor Clear.
  function logged(platform, months, warning) {
    return logEntries.some(function(e) {
      return e.platform === platform && months.indexOf(e.month) >= 0
        && (String(e.summary).indexOf("Part of ") === 0) === warning
    })
  }

  function currentMonth() {
    return new Date(Date.now()).toISOString().slice(0, 7)
  }

  function todayUtc() {
    return new Date(Date.now()).toISOString().slice(0, 10)
  }

  function shiftMonth(month, delta) {
    if (!/^\d{4}-\d{2}$/.test(String(month))) return ""
    var parts = String(month).split("-")
    var d = new Date(Date.UTC(Number(parts[0]), Number(parts[1]) - 1 + delta, 1))
    return d.toISOString().slice(0, 7)
  }

  function monthCount(fromMonth, toMonth) {
    var a = String(fromMonth).split("-")
    var b = String(toMonth).split("-")
    return (Number(b[0]) - Number(a[0])) * 12 + (Number(b[1]) - Number(a[1])) + 1
  }

  function monthTitle(month) {
    var parts = String(month).split("-")
    var names = ["January", "February", "March", "April", "May", "June", "July",
                 "August", "September", "October", "November", "December"]
    return names[Number(parts[1]) - 1] + " " + parts[0]
  }

  function validLabel(s) {
    return /^[A-Za-z0-9 _.\-]{1,64}$/.test(s) && s.trim() === s
  }

  // Rounds the decimal string half away from zero to cents, as report.rs dollars() does, without going through a float.
  function money(value) {
    var m = /^(-?)(\d+)(?:\.(\d*))?$/.exec(String(value === null || value === undefined ? "" : value).trim())
    if (!m) return "$?"
    var frac = (m[3] || "") + "000"
    var digits = (m[2] + frac.slice(0, 2)).split("").map(Number)
    if (Number(frac.charAt(2)) >= 5) {
      var i = digits.length - 1
      while (i >= 0 && digits[i] === 9) { digits[i] = 0; i-- }
      if (i < 0) digits.unshift(1)
      else digits[i]++
    }
    var s = digits.join("").replace(/^0+(?=\d{3})/, "")
    var whole = s.slice(0, -2)
    var cents = s.slice(-2)
    var zero = /^0+$/.test(whole + cents)
    return (m[1] === "-" && !zero ? "-$" : "$") + whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",") + "." + cents
  }

  // Adds decimal strings exactly, digit by digit; returns "" where one is not a decimal, which money() shows as $?.
  function sumDecimals(values) {
    var parsed = []
    var scale = 0
    for (var i = 0; i < values.length; i++) {
      var m = /^(-?)(\d+)(?:\.(\d*))?$/.exec(String(values[i] === null || values[i] === undefined ? "" : values[i]).trim())
      if (!m) return ""
      parsed.push({ neg: m[1] === "-", whole: m[2], frac: m[3] || "" })
      scale = Math.max(scale, (m[3] || "").length)
    }
    // Each value as a sign and its digits scaled to a common number of decimals.
    var acc = { neg: false, digits: "0" }
    for (var j = 0; j < parsed.length; j++) {
      var v = parsed[j]
      var digits = (v.whole + (v.frac + "0".repeat(scale)).slice(0, scale)).replace(/^0+(?=\d)/, "")
      acc = addSigned(acc, { neg: v.neg, digits: digits })
    }
    var all = "0".repeat(Math.max(0, scale + 1 - acc.digits.length)) + acc.digits
    var whole = all.slice(0, all.length - scale)
    var frac = all.slice(all.length - scale)
    var zero = /^0+$/.test(all)
    return (acc.neg && !zero ? "-" : "") + whole + (scale > 0 ? "." + frac : "")
  }

  function compareDigits(a, b) {
    if (a.length !== b.length) return a.length < b.length ? -1 : 1
    return a < b ? -1 : (a > b ? 1 : 0)
  }

  function addDigits(a, b) {
    var out = ""
    var carry = 0
    for (var i = a.length - 1, j = b.length - 1; i >= 0 || j >= 0 || carry; i--, j--) {
      var d = (i >= 0 ? Number(a.charAt(i)) : 0) + (j >= 0 ? Number(b.charAt(j)) : 0) + carry
      out = String(d % 10) + out
      carry = d >= 10 ? 1 : 0
    }
    return out.replace(/^0+(?=\d)/, "")
  }

  // a minus b, where a is at least b.
  function subtractDigits(a, b) {
    var out = ""
    var borrow = 0
    for (var i = a.length - 1, j = b.length - 1; i >= 0; i--, j--) {
      var d = Number(a.charAt(i)) - (j >= 0 ? Number(b.charAt(j)) : 0) - borrow
      borrow = d < 0 ? 1 : 0
      out = String(d < 0 ? d + 10 : d) + out
    }
    return out.replace(/^0+(?=\d)/, "")
  }

  function addSigned(a, b) {
    if (a.neg === b.neg) return { neg: a.neg, digits: addDigits(a.digits, b.digits) }
    var c = compareDigits(a.digits, b.digits)
    if (c === 0) return { neg: false, digits: "0" }
    return c > 0 ? { neg: a.neg, digits: subtractDigits(a.digits, b.digits) }
                 : { neg: b.neg, digits: subtractDigits(b.digits, a.digits) }
  }

  function signedMoney(value) {
    var s = money(value)
    return s.charAt(0) === "$" && s !== "$0.00" && s !== "$?" ? "+" + s : s
  }

  function ago(stamp) {
    var d = new Date(String(stamp || ""))
    var t = d.getTime()
    if (!isFinite(t)) return ""
    var minutes = Math.floor(Math.max(0, root.nowMs - t) / 60000)
    if (minutes < 1) return "just now"
    if (minutes < 60) return minutes + " min ago"
    if (minutes < 24 * 60) return Math.floor(minutes / 60) + " h ago"
    return Qt.formatDate(d, d.getFullYear() === new Date(root.nowMs).getFullYear() ? "MMM d" : "MMM d yyyy")
  }

  function platformName(id) {
    var reports = [barReport, appReport]
    for (var r = 0; r < reports.length; r++) {
      var rows = reports[r] ? reports[r].platforms : []
      for (var i = 0; i < rows.length; i++)
        if (rows[i].platform === id && rows[i].name) return rows[i].name
    }
    for (var j = 0; j < addablePlatforms.length; j++)
      if (addablePlatforms[j].id === id) return addablePlatforms[j].name
    return String(id)
  }

  // The first non-blank line of stderr, so a usage block after an error line stays out of the panel.
  function firstLine(text) {
    var lines = String(text || "").split("\n")
    for (var i = 0; i < lines.length; i++)
      if (lines[i].trim() !== "") return lines[i].trim()
    return ""
  }

  // A fetch prints its no-provider and needs lines beside its failures, and only a failure explains the exit status.
  function failureText(prefix, code, stderr) {
    var lines = String(stderr || "").split("\n").map(function(l) { return l.trim() })
      .filter(function(l) { return l !== "" && !/: (no provider yet|needs: )/.test(l) })
    var line = lines.length > 0 ? lines[0] : firstLine(stderr)
    if (line !== "") return line
    return code < 0 ? prefix + " crashed" : prefix + " exited with status " + code
  }

  // ---- reading ------------------------------------------------------------

  function barArgv() {
    return ["omadai", "report", "--json", "--month", nowMonth]
  }

  function appArgv() {
    if (windowMode === "year") {
      var first = appYear + "-01-01"
      var floor = floorMonth + "-01"
      var last = appYear + "-12-31"
      var today = todayUtc()
      return ["omadai", "report", "--json", "--from", first < floor ? floor : first, "--to", last < today ? last : today]
    }
    return ["omadai", "report", "--json", "--month", appMonth]
  }

  function readReports() {
    barReportProc.request(barArgv())
    if (appOpen) appReportProc.request(appArgv())
  }

  function readKeys() {
    keysProc.request(["omadai", "key", "list", "--json"])
  }

  function readAllKeys() {
    var items = instances()
    for (var i = 0; i < items.length; i++)
      if (items[i] && typeof items[i].readKeys === "function") items[i].readKeys()
  }

  function readLog() {
    logProc.request(["omadai", "log", "--json"])
  }

  // One read serves every bar, as the fetch state does.
  function shareLog(entries, problem) {
    var items = instances()
    for (var i = 0; i < items.length; i++) {
      if (!items[i] || typeof items[i].readLog !== "function") continue
      items[i].logEntries = entries
      items[i].logProblem = problem
    }
    logEntries = entries
    logProblem = problem
  }

  function readJson(text) {
    try {
      return { ok: true, value: JSON.parse(text) }
    } catch (e) {
      return { ok: false, value: null }
    }
  }

  // ---- fetching -----------------------------------------------------------

  // Every live instance of this widget; the bar builds one per monitor.
  function instances() {
    var items = bar && typeof bar.moduleWidgets === "function" ? bar.moduleWidgets(moduleName) : []
    var live = items.filter(function(it) { return it && typeof it.queueFetch === "function" && it.departing !== true })
    return live.length > 0 || departing ? live : [root]
  }

  // The first live instance runs every fetch, so one trigger makes one `omadai fetch` however many bars there are.
  function fetchOwner() {
    var items = instances()
    return items.length > 0 ? items[0] : root
  }

  function isFetchOwner() {
    return fetchOwner() === root
  }

  function requestFetch(months) {
    fetchOwner().queueFetch(months, root)
  }

  function queueFetch(months, requester) {
    var queue = fetchQueue.slice()
    var requesters = Object.assign({}, fetchRequesters)
    var who = requester || root
    for (var i = 0; i < months.length; i++) {
      var m = months[i]
      if (queue.indexOf(m) < 0) queue.push(m)
      var list = (requesters[m] || []).slice()
      if (list.indexOf(who) < 0) list.push(who)
      requesters[m] = list
    }
    fetchRequesters = requesters
    fetchQueue = queue
    runNextFetch()
    syncFetchState()
  }

  // The requesters still alive, so a destroyed instance never keeps a month wanted.
  function liveRequesters(list, leaving) {
    var items = instances()
    return (list || []).filter(function(r) { return r !== leaving && items.indexOf(r) >= 0 })
  }

  // Withdraws a requester's months, keeping nowMonth, which the bar always wants, and the month running, whose process cannot be withdrawn.
  function dropFetch(requester, holdStart) {
    var requesters = Object.assign({}, fetchRequesters)
    var queue = fetchQueue.slice()
    for (var m in requesters) {
      var list = liveRequesters(requesters[m], requester)
      if (list.length > 0 || m === nowMonth) {
        requesters[m] = list
        continue
      }
      delete requesters[m]
      var at = queue.indexOf(m)
      if (at > 0 || (at === 0 && !fetchProc.busy)) queue.splice(at, 1)
      if (m === retryMonth) endRetry()
    }
    fetchRequesters = requesters
    fetchQueue = queue
    if (holdStart !== true) runNextFetch()
    syncFetchState()
  }

  // Takes over a departing owner's fetch state; the month it had in flight is still at the front, and waits one retry delay for the killed process to release the lock.
  function adoptFetch(state) {
    var queue = state.queue.slice()
    for (var i = 0; i < fetchQueue.length; i++) if (queue.indexOf(fetchQueue[i]) < 0) queue.push(fetchQueue[i])
    var requesters = Object.assign({}, state.requesters)
    for (var m in fetchRequesters) {
      var list = (requesters[m] || []).slice()
      for (var j = 0; j < fetchRequesters[m].length; j++) if (list.indexOf(fetchRequesters[m][j]) < 0) list.push(fetchRequesters[m][j])
      requesters[m] = list
    }
    fetchQueue = queue
    fetchRequesters = requesters
    fetchFailures = Object.assign({}, state.failures, fetchFailures)
    fetchLogProblem = fetchLogProblem || !!state.fetchLogProblem
    if (state.notice !== "") fetchNotice = state.notice
    if (state.retryMonth !== "") {
      retryMonth = state.retryMonth
      retryStartedMs = state.retryStartedMs
    }
    // A departing owner's process still holds the lock until it is destroyed, so nothing starts before one retry delay.
    dropFetch(state.leaving, state.retrying || state.inFlight)
    if (state.retrying || state.inFlight) retryTimer.restart()
    else runNextFetch()
  }

  // An instance that stopped being the owner, because an earlier monitor came back, gives its queue to the owner once its own fetch ends.
  function handOffIfNotOwner() {
    if (departing || isFetchOwner() || fetchProc.busy) return false
    if (fetchQueue.length === 0 && Object.keys(fetchRequesters).length === 0 && retryMonth === "") return false
    var state = {
      queue: fetchQueue.slice(), requesters: Object.assign({}, fetchRequesters),
      failures: fetchFailures, fetchLogProblem: fetchLogProblem, notice: fetchNotice, retryMonth: retryMonth,
      retryStartedMs: retryStartedMs, retrying: retryTimer.running, inFlight: false, leaving: null
    }
    retryTimer.stop()
    retryMonth = ""
    retryStartedMs = 0
    fetchQueue = []
    fetchRequesters = ({})
    fetchOwner().adoptFetch(state)
    return true
  }

  // Only the owner holds a queue, requesters or a retry, so an instance holding any hands them to the next live one.
  function leaveFetch() {
    var items = instances()
    if (items.length === 0) return
    if (fetchQueue.length === 0 && Object.keys(fetchRequesters).length === 0 && retryMonth === "" && !fetchProc.busy) {
      items[0].dropFetch(root)
      return
    }
    items[0].adoptFetch({
      queue: fetchQueue.slice(), requesters: Object.assign({}, fetchRequesters),
      failures: fetchFailures, fetchLogProblem: fetchLogProblem, notice: fetchNotice, retryMonth: retryMonth,
      retryStartedMs: retryStartedMs, retrying: retryTimer.running, inFlight: fetchProc.busy, leaving: root
    })
  }

  function endRetry() {
    retryTimer.stop()
    retryMonth = ""
    retryStartedMs = 0
    fetchNotice = ""
  }

  function runNextFetch() {
    if (handOffIfNotOwner()) return
    if (departing || fetchProc.busy || retryTimer.running || fetchQueue.length === 0) return
    fetchProc.request(["omadai", "fetch", "--month", fetchQueue[0]])
  }

  // The running fetch read the keys when it started, so a key change during it reruns the month once it ends.
  function markRerun(month) {
    if (fetchProc.busy && fetchQueue[0] === month && rerunMonths.indexOf(month) < 0) rerunMonths = rerunMonths.concat([month])
  }

  function fetchDone(argv, code, stderr) {
    if (departing) return
    var m = argv[argv.length - 1]
    fetchQueue = fetchQueue.filter(function(q) { return q !== m })
    var requesters = Object.assign({}, fetchRequesters)
    if (code === 0 && stderr.indexOf("another fetch is running") >= 0) {
      // Another process holds the lock and a refused fetch calls no provider, so the month goes back to the front and runs again shortly.
      if (m !== retryMonth) {
        retryMonth = m
        retryStartedMs = Date.now()
      }
      var wanted = m === nowMonth || liveRequesters(requesters[m], null).length > 0
      if (wanted && Date.now() - retryStartedMs < retryBoundMs) {
        var queue = fetchQueue.filter(function(q) { return q !== m })
        queue.unshift(m)
        fetchQueue = queue
        fetchNotice = "Another omadai fetch is running · these figures update once it finishes"
        retryTimer.restart()
        syncFetchState()
        return
      }
      if (wanted) {
        var held = Object.assign({}, fetchFailures)
        held[m] = "another omadai fetch has held the lock for 10 minutes"
        fetchFailures = held
      }
      endRetry()
      // The process that held the lock may have written figures for this month.
      readAllReports()
      readLog()
    } else {
      if (m === retryMonth) endRetry()
      fetchNotice = ""
      var failures = Object.assign({}, fetchFailures)
      if (code === 0) delete failures[m]
      else failures[m] = failureText("omadai fetch", code, stderr)
      fetchFailures = failures
      // A fetch that could not keep its error log says so in a "log: " line, whatever its exit code.
      fetchLogProblem = /(^|\n)log: /.test(stderr)
      readAllReports()
      readLog()
    }
    if (rerunMonths.indexOf(m) >= 0) {
      rerunMonths = rerunMonths.filter(function(q) { return q !== m })
      if (fetchQueue.indexOf(m) < 0) fetchQueue = fetchQueue.concat([m])
    }
    if (fetchQueue.indexOf(m) < 0) delete requesters[m]
    fetchRequesters = requesters
    runNextFetch()
    syncFetchState()
  }

  // Copies the owner's fetch state to every instance, so each bar's panel shows the same notice and failures.
  function syncFetchState() {
    var busy = fetchProc.busy || fetchQueue.length > 0 || retryTimer.running
    var items = instances()
    for (var i = 0; i < items.length; i++) {
      var it = items[i]
      if (!it || typeof it.queueFetch !== "function") continue
      it.fetchFailures = fetchFailures
      it.fetchLogProblem = fetchLogProblem
      it.fetchNotice = fetchNotice
      it.fetching = busy
    }
  }

  function readAllReports() {
    var items = instances()
    for (var i = 0; i < items.length; i++)
      if (items[i] && typeof items[i].readReports === "function") items[i].readReports()
  }

  // Refresh always fetches; opening the panel goes through fetchIfStale instead.
  function panelRefresh() {
    nowMs = Date.now()
    requestFetch([nowMonth])
    readAllKeys()
  }

  // Queues months for staleChecker, which reads each one's report and fetches it only where isStale says so.
  function fetchIfStale(months) {
    var queue = staleQueue.slice()
    for (var i = 0; i < months.length; i++)
      if (autoFetchable(months[i]) && !recentlyAsked(months[i]) && queue.indexOf(months[i]) < 0) queue.push(months[i])
    staleQueue = queue
    nextStaleCheck()
  }

  function nextStaleCheck() {
    if (departing || staleChecker.busy || staleQueue.length === 0) return
    staleChecker.request(["omadai", "report", "--json", "--month", staleQueue[0]])
  }

  function staleCheckDone(argv, code, out) {
    var month = argv[argv.length - 1]
    staleQueue = staleQueue.filter(function(m) { return m !== month })
    if (departing) return
    // A month the app no longer shows is no longer wanted; the current month always is.
    if (month === nowMonth || (appOpen && viewMonths().indexOf(month) >= 0)) {
      var r = code === 0 ? readJson(out) : { ok: false, value: null }
      // An unreadable report says nothing about freshness, so it fetches, under the same backoff as a stale month.
      var unreadable = !r.ok || !r.value || !Array.isArray(r.value.platforms)
      if (!recentlyAsked(month) && (unreadable || isStale(r.value, month))) {
        var stamps = Object.assign({}, staleAttempts)
        stamps[month] = Date.now()
        shareStaleAttempts(stamps)
        requestFetch([month])
      }
    }
    nextStaleCheck()
  }

  // A month fetched for staleness in the last five minutes is left alone, so a platform that keeps failing is retried no more often.
  function recentlyAsked(month) {
    var at = staleAttempts[month]
    return at !== undefined && Date.now() - at <= staleMs
  }

  function shareStaleAttempts(stamps) {
    var items = instances()
    for (var i = 0; i < items.length; i++) if (items[i] && typeof items[i].queueFetch === "function") items[i].staleAttempts = stamps
    staleAttempts = stamps
  }

  // Stale when a connected platform lacks a fetch that counts: one under five minutes old for the current month, one after the month ended for an ended month, and for an ended month xAI still estimates, one under five minutes old.
  function isStale(report, month) {
    if (keysLoaded && connectedPlatforms.length === 0) return false
    var ids = keysLoaded ? connectedPlatforms : report.platforms.map(function(p) { return p.platform })
    var parts = month.split("-")
    // Date.UTC counts months from 0, so the 1-based month gives the first instant after this month ends.
    var end = Date.UTC(Number(parts[0]), Number(parts[1]), 1)
    for (var i = 0; i < ids.length; i++) {
      var row = null
      for (var j = 0; j < report.platforms.length; j++) if (report.platforms[j].platform === ids[i]) row = report.platforms[j]
      // A month before the platform's history never gets a fetch time, and fetching it again changes nothing.
      if (row && (row.before_history || []).indexOf(month) >= 0) continue
      // A platform waiting on setup never gets a successful fetch, so only a recent key change, which may be the key it waits for, makes it stale.
      if (row && row.requirement) {
        if (Date.now() - keysChangedMs <= staleMs) return true
        continue
      }
      var t = row ? new Date(String(row.last_fetched || "")).getTime() : NaN
      if (!isFinite(t) || t < keysChangedMs) return true
      var fresh = Date.now() - t <= staleMs
      if (month === nowMonth ? !fresh : (t < end || (row.platform === "xai" && row.kind === "estimated" && !fresh))) return true
    }
    return false
  }

  // ---- the app window -----------------------------------------------------

  function viewMonths() {
    if (windowMode === "month") return [appMonth]
    var out = []
    for (var i = 1; i <= 12; i++) {
      var m = appYear + "-" + (i < 10 ? "0" : "") + i
      if (m > nowMonth) break
      if (m >= floorMonth) out.push(m)
    }
    return out
  }

  // Opens the app overlay on this instance's own screen at the given month, or the current one, and fetches the current month only if stale.
  function openApp(month) {
    nowMs = Date.now()
    closeSiblingApps()
    var m = /^\d{4}-\d{2}$/.test(String(month || "")) && String(month) <= nowMonth ? String(month) : nowMonth
    if (m < floorMonth) m = floorMonth
    if (m !== appMonth || windowMode !== "month") {
      appReport = null
      appReportProblem = ""
    }
    windowMode = "month"
    appMonth = m
    appOpen = true
    logOpen = false
    readKeys()
    readLog()
    appReportProc.request(appArgv())
    fetchIfStale([nowMonth])
  }

  // The panel's "open app" line: the panel closes first, so the overlay takes the keyboard.
  function openAppFromPanel() {
    close()
    openApp(nowMonth)
  }

  // Closing drops any half-done rename, add, removal or drag, so the next open starts clean, and withdraws the months it queued.
  function closeApp() {
    var owner = fetchOwner()
    var queued = owner.fetchQueue.slice()
    owner.dropFetch(root)
    // A month dropFetch withdrew before it ran must not have its stamp hold back the next open; one that ran keeps it.
    var kept = Object.assign({}, staleAttempts)
    for (var i = 0; i < queued.length; i++) if (owner.fetchQueue.indexOf(queued[i]) < 0) delete kept[queued[i]]
    shareStaleAttempts(kept)
    appOpen = false
    closeLogState()
    endDrag(false)
    renamePlatform = ""
    renameLabel = ""
    removeTarget = null
    addOpen = false
    addError = ""
    var nm = nowMonth
    staleQueue = staleQueue.filter(function(m) { return m === nm })
    // An outcome shown while open was seen; one that arrives while closed waits for the next open.
    actionError = ""
    actionNote = ""
  }

  function escapeApp() {
    if (logOpen) closeLog()
    else if (removeTarget) cancelRemove()
    else closeApp()
  }

  // ---- the error log window --------------------------------------------------

  function openLog() {
    if (removeTarget) return
    logExported = ""
    logActionError = ""
    logOpen = true
    readLog()
    Qt.callLater(function() { logHome.forceActiveFocus() })
  }

  function closeLogState() {
    logOpen = false
    logExported = ""
    logActionError = ""
  }

  function closeLog() {
    closeLogState()
    Qt.callLater(function() { appHome.forceActiveFocus() })
  }

  // A fixed command with nothing interpolated, so no entry text ever reaches a shell.
  function copyLog() {
    Util.execArgv(["bash", "-c", "set -o pipefail; out=$(omadai log) && printf %s \"$out\" | wl-copy"])
  }

  function exportLog() {
    logActionError = ""
    logActionProc.request(["omadai", "log", "export"])
  }

  function clearLog() {
    logActionError = ""
    logExported = ""
    logActionProc.request(["omadai", "log", "clear"])
  }

  function logActionDone(argv, code, out, err) {
    var verb = argv[argv.length - 1]
    if (code !== 0) logActionError = failureText("omadai log " + verb, code, err)
    else if (verb === "export") {
      var path = firstLine(out)
      var home = String(Quickshell.env("HOME") || "")
      logExported = home !== "" && path.indexOf(home + "/") === 0 ? "~" + path.slice(home.length) : path
    }
    if (verb === "clear") readLog()
  }

  // "Nov 6 · 16:02" in local time.
  function logStamp(stamp) {
    var d = new Date(String(stamp || ""))
    if (!isFinite(d.getTime())) return ""
    return Qt.formatDate(d, "MMM d") + " · " + Qt.formatTime(d, "HH:mm")
  }

  function closeSiblingApps() {
    var items = instances()
    for (var i = 0; i < items.length; i++)
      if (items[i] && items[i] !== root && items[i].appOpen === true) items[i].closeApp()
  }

  // The bar's pickPanelSlot rule for IPC: an instance whose panel is open where preferOpen, then the one on the focused monitor.
  function panelTarget(preferOpen) {
    var items = instances()
    var pool = preferOpen ? items.filter(function(it) { return it.opened === true }) : []
    if (pool.length === 0) pool = items
    var monitor = Hyprland.focusedMonitor
    var focused = monitor ? String(monitor.name || "") : ""
    if (focused !== "") {
      var onFocused = pool.filter(function(it) { return it.ownScreenName === focused })
      if (onFocused.length > 0) pool = onFocused
    }
    return pool.length > 0 ? pool[0] : root
  }

  // The IPC call reaches one instance, so it relays to the instance on the focused monitor, after closing any open bar panel.
  function openAppOnFocused() {
    var open = instances()
    for (var k = 0; k < open.length; k++)
      if (open[k] && open[k].opened === true) open[k].close()
    var monitor = Hyprland.focusedMonitor
    var focused = monitor ? String(monitor.name || "") : ""
    var items = instances()
    var target = root
    for (var i = 0; i < items.length; i++) {
      if (items[i] && focused !== "" && items[i].ownScreenName === focused) {
        target = items[i]
        break
      }
    }
    target.openApp()
  }

  function appRefresh() {
    nowMs = Date.now()
    requestFetch(viewMonths())
    readAllKeys()
  }

  function showMonth(month) {
    windowMode = "month"
    appMonth = month
    appReport = null
    appReportProblem = ""
    appReportProc.request(appArgv())
    fetchIfStale([month])
  }

  function showYear(year) {
    windowMode = "year"
    appYear = year
    appReport = null
    appReportProblem = ""
    appReportProc.request(appArgv())
    fetchIfStale(viewMonths())
  }

  function canStep(delta) {
    if (windowMode === "year") {
      var year = Number(appYear) + delta
      return appYear !== "" && year >= Number(floorMonth.slice(0, 4)) && year <= Number(nowMonth.slice(0, 4))
    }
    var next = shiftMonth(appMonth, delta)
    return next !== "" && next >= floorMonth && next <= nowMonth
  }

  function step(delta) {
    if (removeTarget || !canStep(delta)) return
    if (windowMode === "year") showYear(String(Number(appYear) + delta))
    else showMonth(shiftMonth(appMonth, delta))
  }

  // Year shows the year of the month on screen; Month shows that year's last month, or the current month in the current year.
  function toggleMode() {
    if (removeTarget) return
    if (windowMode === "year") showMonth(appYear === nowMonth.slice(0, 4) ? nowMonth : appYear + "-12")
    else showYear(appMonth.slice(0, 4))
  }

  // The panel opens one platform at a time, so ten rows plus one platform's items always fit on the screen.
  function toggleExpanded(scope, id) {
    var next = scope === "app" ? Object.assign({}, appExpanded) : (panelExpanded[id] ? Object.assign({}, panelExpanded) : {})
    next[id] = !next[id]
    if (scope === "app") appExpanded = next
    else panelExpanded = next
  }

  function addKey(platformId) {
    var known = false
    for (var i = 0; i < addablePlatforms.length; i++)
      if (addablePlatforms[i].id === platformId) known = true
    if (!known) return
    var label = addLabelField.text.trim()
    var argv = ["omarchy-launch-terminal", "omadai", "key", "add", platformId]
    if (label !== "") {
      if (!validLabel(label)) {
        addError = "A label is 1 to 64 letters, digits, spaces, _ . or -."
        return
      }
      argv.push("--label", label)
    }
    addError = ""
    addLabelField.text = ""
    // The overlay holds exclusive keyboard focus, so it goes first or the terminal opens unable to take input.
    closeApp()
    Qt.callLater(function() { Util.execArgv(argv) })
  }

  function startRename(key) {
    if (keyActionBusy) return
    actionError = ""
    renameDraft = key.label
    renamePlatform = key.platform
    renameLabel = key.label
    renameStarted()
  }

  function cancelRename() {
    renamePlatform = ""
    renameLabel = ""
    renameDraft = ""
    actionError = ""
    appHome.forceActiveFocus()
  }

  function commitRename() {
    if (removeTarget || renamePlatform === "") return
    var to = renameDraft.trim()
    if (!validLabel(to)) {
      actionError = "A label is 1 to 64 letters, digits, spaces, _ . or -."
      return
    }
    if (to === renameLabel) {
      cancelRename()
      return
    }
    if (keyActionBusy) {
      actionError = "Another key change is still running · try again when it finishes"
      return
    }
    actionError = ""
    startKeyAction("rename", [renamePlatform, renameLabel, to])
  }

  function askRemove(key) {
    if (keyActionBusy) return
    actionError = ""
    removeTarget = { platform: key.platform, label: key.label }
    removeConfirm.selectedIndex = 0
    confirmKeys.forceActiveFocus()
  }

  function cancelRemove() {
    removeTarget = null
    appHome.forceActiveFocus()
  }

  function confirmRemove() {
    var t = removeTarget
    if (!t) return
    if (keyActionBusy) {
      actionError = "Another key change is still running · try again when it finishes"
      return
    }
    actionError = ""
    startKeyAction("remove", [t.platform, t.label])
    removeTarget = null
    appHome.forceActiveFocus()
  }

  // ---- detached key changes ------------------------------------------------

  // A key change survives this widget being destroyed, since the bar can rebuild it between the keyring change and the config save.
  function startKeyAction(kind, args) {
    var id = Date.now().toString(36) + "-" + Math.floor(Math.random() * 1e9).toString(36)
    pendingActionId = id
    pendingActionKind = kind
    pendingActionStartedMs = Date.now()
    actionNote = ""
    keyActionTimer.interval = keyActionTimeoutMs
    keyActionTimer.restart()
    setKeyActionBusyAll(true)
    Quickshell.execDetached(["bash", "-c", keyActionScript, "bash", id, kind, "--"].concat(args))
  }

  function setKeyActionBusyAll(busy) {
    var items = instances()
    for (var i = 0; i < items.length; i++)
      if (items[i] && typeof items[i].startKeyAction === "function") items[i].keyActionBusy = busy
    keyActionBusy = busy
  }

  // The IPC callback: the instance that started the action shows its outcome, and every instance re-reads.
  function routeKeyActionDone(id, status, message) {
    var items = instances()
    var matched = false
    for (var i = 0; i < items.length; i++) {
      if (!items[i] || id === "") continue
      if (items[i].pendingActionId === id) { items[i].finishKeyAction(status, message); matched = true }
      else if (items[i].lateActionId === id) { items[i].finishLateAction(status, message); matched = true }
    }
    // The widget that started the change was rebuilt with no survivor to adopt it, so a failure is shown on every widget.
    var code = parseInt(status, 10)
    if (!matched && code !== 0) {
      var text = "A key change failed: " + keyActionFailure("change", code, message)
      for (var j = 0; j < items.length; j++) if (items[j]) items[j].actionError = text
    }
    setKeyActionBusyAll(items.some(function(it) { return it && (it.pendingActionId !== "" || it.lateActionId !== "") }))
    readAllKeys()
    readAllReports()
  }

  function finishKeyAction(status, message) {
    var kind = pendingActionKind
    pendingActionId = ""
    pendingActionKind = ""
    keyActionTimer.stop()
    var code = parseInt(status, 10)
    if (code === 0) {
      actionError = ""
      actionNote = firstLine(message).slice(0, 300)
      if (kind === "rename") {
        renamePlatform = ""
        renameLabel = ""
        if (appOpen) appHome.forceActiveFocus()
      }
    } else {
      actionError = keyActionFailure(kind, code, message)
    }
  }

  function keyActionFailure(kind, code, message) {
    var line = firstLine(message)
    return line !== "" ? line.slice(0, 300) : "omadai key " + kind + " exited with status " + (isFinite(code) ? code : "unknown")
  }

  // A change that reports after the timeout replaces the timeout notice with its real outcome.
  function finishLateAction(status, message) {
    var kind = lateActionKind
    lateActionId = ""
    lateActionKind = ""
    if (pendingActionId === "") keyActionTimer.stop()
    var code = parseInt(status, 10)
    if (code !== 0) actionError = keyActionFailure(kind, code, message)
    else {
      if (actionError === keyActionTimeoutText) actionError = ""
      actionNote = firstLine(message).slice(0, 300)
    }
  }

  // The command may still be waiting on the key lock, so its id is kept for a late report and the rename field is let go.
  function keyActionTimedOut() {
    // A late report that never comes stops holding key changes back after a second, longer wait.
    if (pendingActionId === "" && lateActionId !== "") {
      lateActionId = ""
      lateActionKind = ""
      setKeyActionBusyAll(instances().some(function(it) { return it && (it.pendingActionId !== "" || it.lateActionId !== "") }))
      return
    }
    if (pendingActionId === "") return
    keyActionTimer.interval = keyActionLateMs
    keyActionTimer.restart()
    lateActionId = pendingActionId
    lateActionKind = pendingActionKind
    pendingActionId = ""
    pendingActionKind = ""
    if (lateActionKind === "rename") {
      renamePlatform = ""
      renameLabel = ""
      if (appOpen) appHome.forceActiveFocus()
    }
    actionError = keyActionTimeoutText
    readAllKeys()
    readAllReports()
  }

  // A departing instance hands its pending action to a survivor, so the wait still ends with a re-read.
  function adoptKeyAction(id, kind, startedMs) {
    pendingActionId = id
    pendingActionKind = kind
    pendingActionStartedMs = startedMs
    setKeyActionBusyAll(true)
    keyActionTimer.interval = Math.max(1, keyActionTimeoutMs - (Date.now() - startedMs))
    keyActionTimer.restart()
  }

  function adoptLateAction(id, kind) {
    lateActionId = id
    lateActionKind = kind
    setKeyActionBusyAll(true)
    if (pendingActionId === "") {
      keyActionTimer.interval = keyActionLateMs
      keyActionTimer.restart()
    }
  }

  // ---- settings and order ---------------------------------------------------

  // Saves into this widget's own bar entry in shell.json through the shell, as the clock panel does; the plugin writes no file.
  function saveSetting(name, value) {
    var values = {}
    values[name] = value
    saveSettings(values)
  }

  function saveSettings(values) {
    var entry = { id: moduleName }
    for (var key in settings) if (key !== "id") entry[key] = settings[key]
    for (var name in values) entry[name] = values[name]
    var items = instances()
    for (var i = 0; i < items.length; i++)
      if (items[i] && typeof items[i].saveSetting === "function") items[i].settings = entry
    if (bar && bar.shell && typeof bar.shell.updateEntryInline === "function")
      bar.shell.updateEntryInline(moduleName, entry)
  }

  function setRefreshMinutes(minutes) {
    saveSetting("refreshMinutes", String(normalizeMinutes(minutes)))
  }

  // The new saved order: slots of the old list that hold shown ids take the shown ids in their new order, others stay put.
  function mergedOrder(shown) {
    var saved = platformOrder
    var next = saved.slice()
    var k = 0
    for (var i = 0; i < saved.length; i++)
      if (shown.indexOf(saved[i]) >= 0) next[i] = shown[k++]
    for (; k < shown.length; k++) next.push(shown[k])
    return next
  }

  function commitOrder(shown) {
    var next = mergedOrder(shown)
    if (JSON.stringify(next) !== JSON.stringify(platformOrder)) saveSetting("platformOrder", next)
  }

  // Moves the row at one index of the app list to another and saves the result, as a drop does.
  function moveAppRow(from, to) {
    var ids = appRowIds.slice()
    if (from < 0 || from >= ids.length || to < 0 || to >= ids.length) return false
    var moved = ids.splice(from, 1)[0]
    ids.splice(to, 0, moved)
    commitOrder(ids)
    return true
  }

  // ---- dragging -------------------------------------------------------------

  function setRowHeight(id, h) {
    if (rowHeights[id] === h) return
    var next = Object.assign({}, rowHeights)
    next[id] = h
    rowHeights = next
  }

  function slotTop(id, order) {
    var top = 0
    var gap = Style.space(14)
    for (var i = 0; i < order.length; i++) {
      if (order[i] === id) return top
      top += (rowHeights[order[i]] || 0) + gap
    }
    return top
  }

  function stackHeight(order) {
    var h = 0
    for (var i = 0; i < order.length; i++) h += (rowHeights[order[i]] || 0) + (i > 0 ? Style.space(14) : 0)
    return h
  }

  function beginDrag(id) {
    appRowsFrozen = appRowsLive
    dragOrder = appRowIds.slice()
    dragStartTop = slotTop(id, dragOrder)
    dragTop = dragStartTop
    dragId = id
  }

  // The held row goes before the first other row whose middle its top edge has not passed, so it never flips back and forth.
  function dragTo(offset) {
    if (dragId === "") return
    var others = dragOrder.filter(function(x) { return x !== dragId })
    var h = rowHeights[dragId] || 0
    dragTop = Math.max(0, Math.min(dragStartTop + offset, stackHeight(others) + Style.space(14)))
    var gap = Style.space(14)
    var acc = 0
    var at = others.length
    for (var i = 0; i < others.length; i++) {
      var oh = rowHeights[others[i]] || 0
      if (dragTop < acc + oh / 2) { at = i; break }
      acc += oh + gap
    }
    var next = others.slice()
    next.splice(at, 0, dragId)
    if (JSON.stringify(next) !== JSON.stringify(dragOrder)) dragOrder = next
  }

  // The drop commits the order; delegates are rebuilt after the drag handler has returned.
  function endDrag(commit) {
    if (dragId === "" || dragEnding) return
    // The first end wins, so a close that cancels is not overridden by the handler's own release before the queued reset runs.
    dragEnding = true
    var order = dragOrder.slice()
    Qt.callLater(function() {
      if (commit) commitOrder(order)
      dragId = ""
      dragEnding = false
      appRowsFrozen = null
    })
  }

  // A launch that never started: find out whether omadai is on PATH before calling it missing.
  function startFailed(argv) {
    if (departing) return
    if (argv[0] === "omadai" && argv[1] === "fetch") {
      fetchQueue = []
      fetchRequesters = ({})
      endRetry()
      syncFetchState()
    }
    probeProc.request(["sh", "-c", "command -v omadai"])
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  // The first live instance alone serves IPC, since the shell routes a target to one handler.
  property bool ipcOwner: false
  property bool started: false

  function electIpcOwner() {
    ipcOwner = isFetchOwner()
    // An instance can elect before the bar lists it, so one that stands down makes sure the owner it defers to has taken IPC.
    if (!ipcOwner) {
      var owner = fetchOwner()
      if (owner !== root && owner.ipcOwner !== true && typeof owner.electIpcOwner === "function") Qt.callLater(function() { owner.electIpcOwner() })
    }
    handOffIfNotOwner()
  }

  // The bar signals no change in its instances, so each arrival and departure re-elects on the survivors after the slot list settles.
  function reelectAll(leaving) {
    var items = instances()
    for (var i = 0; i < items.length; i++) {
      var it = items[i]
      if (!it || it === leaving || typeof it.electIpcOwner !== "function") continue
      Qt.callLater(function(target) { if (target) target.electIpcOwner() }, it)
    }
  }

  Component.onCompleted: {
    // A widget built while another holds a key change must not start a second one.
    keyActionBusy = instances().some(function(it) { return it && it !== root && (it.pendingActionId !== "" || it.lateActionId !== "") })
    var siblings = instances().filter(function(it) { return it && it !== root && it.staleAttempts })
    if (siblings.length > 0) staleAttempts = siblings[0].staleAttempts
    readReports()
    readKeys()
    started = true
    reelectAll(null)
  }

  Component.onDestruction: {
    departing = true
    leaveFetch()
    var survivors = instances()
    if (pendingActionId !== "" && survivors.length > 0) survivors[0].adoptKeyAction(pendingActionId, pendingActionKind, pendingActionStartedMs)
    if (lateActionId !== "" && survivors.length > 0) survivors[0].adoptLateAction(lateActionId, lateActionKind)
    reelectAll(root)
  }

  onOpenedChanged: if (opened) {
    nowMs = Date.now()
    readReports()
    readAllKeys()
    fetchIfStale([nowMonth])
    Qt.callLater(function() { keyCatcher.forceActiveFocus() })
  }

  // A check that ran before the first key list read the report's platforms only, so it runs again with the keys.
  onKeysLoadedChanged: if (keysLoaded && (opened || appOpen)) fetchIfStale([nowMonth])

  // The key set's fingerprint and when it last changed live in the plugin's settings, so a key added while the shell was off still makes older fetches not count.
  function noteKeySet() {
    var signature = Qt.md5(keySet)
    if (signature === String(setting("keySetSignature", ""))) {
      var at = Number(setting("keySetChangedMs", 0))
      if (!isFinite(at) || at <= keysChangedMs) return
      keysChangedMs = at
      keySetMoved(false)
      return
    }
    keysChangedMs = Date.now()
    saveSettings({ keySetSignature: signature, keySetChangedMs: String(keysChangedMs) })
    keySetMoved(true)
  }

  // A key added, renamed or removed makes every month on screen worth checking again at once.
  function keySetMoved(rerun) {
    var owner = fetchOwner()
    // Only the instance that saw the change resets the shared backoff and reruns a running fetch; one that learns it from the settings leaves both alone.
    if (rerun) {
      shareStaleAttempts({})
      var months = [nowMonth].concat(appOpen ? viewMonths() : [])
      for (var i = 0; i < months.length; i++) owner.markRerun(months[i])
    }
    if (opened || appOpen) fetchIfStale(appOpen ? [nowMonth].concat(viewMonths()) : [nowMonth])
  }

  // The initial binding also signals a change, which is not a rollover.
  onNowMonthChanged: if (started) {
    readReports()
    if (isFetchOwner()) queueFetch([nowMonth])
  }

  OmadaiProcess {
    id: fetchProc
    onDone: function(argv, code, out, err) { root.fetchDone(argv, code, err) }
    onStartFailed: function(argv) { root.startFailed(argv) }
  }

  OmadaiProcess {
    id: barReportProc
    onDone: function(argv, code, out, err) {
      if (barReportProc.pending !== null || JSON.stringify(argv) !== JSON.stringify(root.barArgv())) return
      if (code !== 0) {
        root.barReportProblem = root.failureText("omadai report", code, err)
        return
      }
      var r = root.readJson(out)
      if (r.ok && r.value && Array.isArray(r.value.platforms)) {
        // An unchanged report is not reassigned, so a re-read does not rebuild the rows under an open rename field.
        if (JSON.stringify(r.value) !== JSON.stringify(root.barReport)) root.barReport = r.value
        root.barReportProblem = ""
      } else {
        root.barReportProblem = root.firstLine(err) !== "" ? root.firstLine(err) : "omadai report printed output the panel could not read"
      }
    }
    onStartFailed: function(argv) { root.startFailed(argv) }
  }

  OmadaiProcess {
    id: appReportProc
    onDone: function(argv, code, out, err) {
      if (appReportProc.pending !== null || JSON.stringify(argv) !== JSON.stringify(root.appArgv())) return
      if (code !== 0) {
        root.appReportProblem = root.failureText("omadai report", code, err)
        return
      }
      var r = root.readJson(out)
      if (r.ok && r.value && Array.isArray(r.value.platforms)) {
        if (JSON.stringify(r.value) !== JSON.stringify(root.appReport)) root.appReport = r.value
        root.appReportProblem = ""
      } else {
        root.appReportProblem = root.firstLine(err) !== "" ? root.firstLine(err) : "omadai report printed output the app could not read"
      }
    }
    onStartFailed: function(argv) { root.startFailed(argv) }
  }

  OmadaiProcess {
    id: keysProc
    onDone: function(argv, code, out, err) {
      if (keysProc.pending !== null) return
      if (code !== 0) {
        root.keysProblem = root.failureText("omadai key list", code, err)
        return
      }
      var k = root.readJson(out)
      if (k.ok && Array.isArray(k.value)) {
        // An unchanged list is not reassigned, so the periodic re-read does not rebuild the app's rows.
        if (JSON.stringify(k.value) !== JSON.stringify(root.keys)) root.keys = k.value
        root.keysLoaded = true
        root.noteKeySet()
        root.keysProblem = ""
      } else {
        root.keysProblem = root.firstLine(err) !== "" ? root.firstLine(err) : "omadai key list printed output the panel could not read"
      }
    }
    onStartFailed: function(argv) { root.startFailed(argv) }
  }

  OmadaiProcess {
    id: logProc
    onDone: function(argv, code, out, err) {
      if (logProc.pending !== null) return
      if (code !== 0) {
        root.shareLog(root.logEntries, root.failureText("omadai log", code, err))
        return
      }
      var r = root.readJson(out)
      if (r.ok && Array.isArray(r.value)) {
        root.shareLog(JSON.stringify(r.value) === JSON.stringify(root.logEntries) ? root.logEntries : r.value, "")
      } else {
        root.shareLog(root.logEntries, root.firstLine(err) !== "" ? root.firstLine(err) : "omadai log printed output the app could not read")
      }
    }
    onStartFailed: function(argv) { root.startFailed(argv) }
  }

  OmadaiProcess {
    id: logActionProc
    onDone: function(argv, code, out, err) { root.logActionDone(argv, code, out, err) }
    onStartFailed: function(argv) { root.startFailed(argv) }
  }

  OmadaiProcess {
    id: staleChecker
    onDone: function(argv, code, out, err) { root.staleCheckDone(argv, code, out) }
    onStartFailed: function(argv) {
      root.staleQueue = []
      root.startFailed(argv)
    }
  }

  OmadaiProcess {
    id: probeProc
    onDone: function(argv, code, out, err) {
      root.binaryMissing = code !== 0
      root.startProblem = code === 0 ? "omadai is installed at " + out.trim() + " but could not be started" : ""
    }
    onStartFailed: function(argv) { root.startProblem = "omadai could not be started, and sh to look for it could not either" }
  }

  Timer {
    id: keyActionTimer
    repeat: false
    onTriggered: root.keyActionTimedOut()
  }

  Timer {
    id: retryTimer
    interval: root.retryDelayMs
    repeat: false
    onTriggered: {
      root.runNextFetch()
      root.syncFetchState()
    }
  }

  // Every instance keeps this timer, but only the fetch owner's tick fetches.
  Timer {
    interval: root.refreshIntervalMs
    running: true
    repeat: true
    onTriggered: if (root.isFetchOwner()) root.queueFetch([root.nowMonth])
  }

  Timer {
    id: monthClock
    interval: 60000
    running: true
    repeat: true
    onTriggered: root.nowMonth = root.currentMonth()
  }

  Timer {
    interval: 30000
    running: root.opened || root.appOpen
    repeat: true
    onTriggered: root.nowMs = Date.now()
  }

  // Picks up a key added in the terminal while the app window is open.
  Timer {
    interval: 10000
    running: root.appOpen
    repeat: true
    onTriggered: root.readKeys()
  }

  IpcHandler {
    enabled: root.ipcOwner
    target: root.ipcTarget
    function open(): void { root.panelTarget(false).open() }
    function close(): void { root.panelTarget(true).close() }
    function toggle(): void { root.panelTarget(true).toggle() }
    function refresh(): string { root.panelRefresh(); return "ok" }
    function openApp(): void { root.openAppOnFocused() }
    function keyActionDone(id: string, status: string, message: string): void { root.routeKeyActionDone(id, status, message) }
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    iconComponent: deltaIcon
    tooltipText: root.binaryMissing ? "omadai is not installed"
      : (root.barReport && root.keysLoaded ? "AI spend this month: " + root.money(root.panelTotal) : "AI spend this month")
    onPressed: function(buttonCode) { root.toggle() }
  }

  Component {
    id: deltaIcon

    Item {
      // The 34x28 delta-and-spark geometry, scaled to 94% of the icon canvas height.
      Shape {
        width: 34
        height: 28
        anchors.centerIn: parent
        scale: parent.height / 28 * 0.94
        preferredRendererType: Shape.CurveRenderer

        // Left leg, right leg and a base that stops two thirds of the way left, cut parallel to the left leg.
        ShapePath {
          strokeColor: "transparent"
          strokeWidth: -1
          fillColor: button.foreground
          PathSvg { path: "M2 25 L13 4 L13 9.603 L4.935 25 Z M13 4 L24 25 L19.703 22.4 L13 9.603 Z M9.333 25 L24 25 L19.703 22.4 L10.695 22.4 Z" }
        }

        ShapePath {
          strokeColor: "transparent"
          strokeWidth: -1
          fillColor: button.foreground
          PathSvg { path: "M22.5 2.8 L23.592 5.908 L26.7 7 L23.592 8.092 L22.5 11.2 L21.408 8.092 L18.3 7 L21.408 5.908 Z" }
        }
      }
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

      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) { if (t === "r" || t === "R") root.panelRefresh() }

      Column {
        id: column
        width: parent.width
        spacing: Style.space(10)

        Column {
          visible: root.binaryMissing
          width: parent.width
          spacing: Style.space(6)

          Line { text: "omadai is not installed · build it from the omadai checkout and put target/release/omadai on PATH:"; color: root.foreground }
          Line { text: "bin/omadai build"; font.bold: true; color: root.foreground; font.pixelSize: Style.font.body }
        }

        Column {
          visible: !root.binaryMissing
          width: parent.width
          spacing: Style.space(12)

          Item {
            width: parent.width
            implicitHeight: Math.max(panelMonth.implicitHeight, panelTotalText.implicitHeight)

            Text {
              id: panelMonth
              textFormat: Text.PlainText
              anchors.left: parent.left
              anchors.verticalCenter: parent.verticalCenter
              text: root.monthTitle(root.nowMonth)
              color: root.foreground
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption * 1.8
            }

            Text {
              id: panelTotalText
              textFormat: Text.PlainText
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              text: root.barReport && root.keysLoaded ? root.money(root.panelTotal) : "…"
              color: root.foreground
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption * 1.8
              font.bold: true
            }
          }

          Line { visible: text !== ""; text: root.startProblem; color: root.urgent }
          Line { visible: text !== ""; text: root.barReportProblem; color: root.urgent }
          Line { visible: text !== ""; text: root.keysProblem; color: root.urgent }

          Rule {}

          Column {
            width: parent.width
            spacing: Style.space(10)

            Line {
              visible: root.panelRows.length === 0
              text: "No platform connected yet · open the app to add a key"
            }

            Repeater {
              model: root.panelRows.slice(0, root.panelMaxRows)

              PlatformRow {
                required property var modelData
                width: column.width
                p: modelData
              }
            }

            Line {
              visible: root.panelRows.length > root.panelMaxRows
              text: "+ " + (root.panelRows.length - root.panelMaxRows) + " more platforms · open the app to see them"

              MouseArea {
                anchors.fill: parent
                cursorShape: Qt.PointingHandCursor
                onClicked: root.openAppFromPanel()
              }
            }
          }

          Rule {}

          Item {
            width: parent.width
            implicitHeight: panelButtons.implicitHeight

            // A failure of the whole fetch, which no row can show.
            ErrorDisc {
              id: panelFailDisc
              visible: root.panelFetchFailed
              anchors.left: parent.left
              anchors.verticalCenter: parent.verticalCenter
              tip: root.fetchFailedText
              tipLeft: true
            }

            Line {
              anchors.left: panelFailDisc.visible ? panelFailDisc.right : parent.left
              anchors.leftMargin: panelFailDisc.visible ? Style.space(8) : 0
              anchors.right: panelButtons.left
              anchors.rightMargin: Style.space(8)
              anchors.verticalCenter: parent.verticalCenter
              text: root.panelUpdated !== "" ? "last updated " + root.ago(root.panelUpdated) : ""
              wrapMode: Text.NoWrap
              elide: Text.ElideRight
            }

            Row {
              id: panelButtons
              anchors.right: parent.right
              spacing: Style.space(8)

              ActionButton {
                text: root.fetching ? "Fetching…" : "Refresh"
                onClicked: root.panelRefresh()
              }

              ActionButton {
                text: "Open app"
                onClicked: root.openAppFromPanel()
              }
            }
          }
        }
      }
    }
  }

  PanelWindow {
    id: appWindow
    visible: root.appOpen
    screen: root.ownScreen
    anchors { top: true; bottom: true; left: true; right: true }
    color: "transparent"
    WlrLayershell.namespace: "omadai-app"
    WlrLayershell.layer: WlrLayer.Overlay
    WlrLayershell.keyboardFocus: WlrKeyboardFocus.Exclusive
    exclusionMode: ExclusionMode.Ignore

    onVisibleChanged: if (visible) Qt.callLater(function() { appHome.forceActiveFocus() })

    // Only Esc or the close button closes the app; a click on the backdrop does nothing.
    Rectangle {
      anchors.fill: parent
      color: Commons.Color.menu.scrim
    }

    BorderSurface {
      id: appCard
      visible: !root.logOpen
      readonly property real pad: Style.space(22)
      readonly property real gap: Style.space(14)
      readonly property int fitRows: Math.max(1, root.connectedPlatforms.length)
      readonly property int fitExtraKeys: Math.max(0, root.keys.length - root.connectedPlatforms.length)
      // How far the list reaches past the column on each side, so the rename outline and the lifted row are not clipped.
      readonly property real bleed: Style.space(8)
      // Sized from the connected platforms and their keys alone, so stepping, switching to Year or a message never changes it; the list scrolls instead.
      readonly property real fittedHeight: pad * 2
        + appHeader.implicitHeight + gap + appTotal.implicitHeight + gap + 1 + gap
        + fitRows * protoRow.implicitHeight + fitExtraKeys * (protoRow2.implicitHeight - protoRow.implicitHeight)
        + (fitRows - 1) * gap + gap + addKeyButton.implicitHeight
        + gap + appSettings.implicitHeight
      width: Math.min(Style.space(580), appWindow.width - Style.gapsOut * 2)
      height: Math.min(fittedHeight, appWindow.height - Style.gapsOut * 2)
      anchors.centerIn: parent
      radius: Style.cornerRadius
      color: Commons.Color.background
      borderSpec: Border.surfaceSpec("menu", "border", Commons.Color.menu.border, Math.max(1, Style.space(2)))

      // Measure a collapsed platform row with one key and with two for the fitted height; never shown.
      AppPlatformRow {
        id: protoRow
        visible: false
        width: appTop.width
        p: root.placeholderRow("openai")
        measureOnly: true
      }

      AppPlatformRow {
        id: protoRow2
        visible: false
        width: appTop.width
        p: root.placeholderRow("openai")
        measureOnly: true
        measureKeys: 2
      }

      FocusScope {
        id: appFocus
        anchors.fill: parent
        focus: true

        // A FocusScope hands focus back to the field that last held it, so leaving a field needs a plain item to land on.
        Item { id: appHome; focus: true }

        Keys.onEscapePressed: root.escapeApp()

        Column {
          id: appTop
          x: appCard.pad
          y: appCard.pad
          width: appCard.width - appCard.pad * 2
          spacing: appCard.gap

          Item {
            id: appHeader
            width: parent.width
            implicitHeight: Math.max(pickerRow.implicitHeight, appHeaderRight.implicitHeight)

            Row {
              id: pickerRow
              anchors.left: parent.left
              anchors.verticalCenter: parent.verticalCenter
              spacing: Style.space(8)

              ActionButton {
                text: "‹"
                enabled: root.canStep(-1)
                onClicked: root.step(-1)
              }

              Text {
                textFormat: Text.PlainText
                width: Math.max(Style.space(150), implicitWidth)
                anchors.verticalCenter: parent.verticalCenter
                horizontalAlignment: Text.AlignHCenter
                text: root.windowMode === "year" ? root.appYear
                  : (root.appMonth === "" ? "" : root.monthTitle(root.appMonth))
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
              }

              ActionButton {
                text: "›"
                enabled: root.canStep(1)
                onClicked: root.step(1)
              }

              ActionButton {
                text: root.windowMode === "year" ? "Year" : "Month"
                onClicked: root.toggleMode()
              }
            }

            Row {
              id: appHeaderRight
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              spacing: Style.space(8)

              ErrorDisc {
                visible: root.appFetchFailure !== ""
                anchors.verticalCenter: parent.verticalCenter
                tip: root.appFetchFailure
              }

              Text {
                visible: text !== ""
                textFormat: Text.PlainText
                anchors.verticalCenter: parent.verticalCenter
                text: root.appUpdated !== "" ? root.ago(root.appUpdated) : ""
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.caption
              }

              ActionButton {
                text: root.fetching ? "Fetching…" : "Refresh"
                onClicked: root.appRefresh()
              }

              ActionButton {
                text: "✕"
                onClicked: root.closeApp()
              }
            }
          }

          Item {
            id: appTotal
            width: parent.width
            implicitHeight: Math.max(totalLabel.implicitHeight, totalValue.implicitHeight)

            Text {
              id: totalLabel
              textFormat: Text.PlainText
              anchors.left: parent.left
              anchors.verticalCenter: parent.verticalCenter
              text: "Total spent"
              color: root.foreground
              font.family: root.fontFamily
              font.pixelSize: Style.font.display
            }

            Text {
              id: totalValue
              textFormat: Text.PlainText
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              text: root.appReport && root.keysLoaded ? root.money(root.appSum) : "…"
              color: root.foreground
              font.family: root.fontFamily
              font.pixelSize: Style.font.display
              font.bold: true
            }
          }

          Rule {}
        }

        Flickable {
          id: appFlick
          anchors.left: appTop.left
          anchors.leftMargin: -appCard.bleed
          anchors.right: appTop.right
          anchors.rightMargin: -appCard.bleed
          anchors.top: appTop.bottom
          anchors.topMargin: appTop.spacing - appCard.bleed
          anchors.bottom: addKeyButton.top
          anchors.bottomMargin: appCard.gap - appCard.bleed
          contentWidth: width
          contentHeight: appList.implicitHeight
          clip: true
          boundsBehavior: Flickable.StopAtBounds
          // The Flickable would take a drag from the handle once past its threshold, so it stands still while the pointer is on one.
          interactive: contentHeight > height && root.dragId === "" && !root.gripHovered
          // Sits in the card's padding, outside the list, so it never covers the pencil and trash column.
          ScrollBar.vertical: ScrollBar {
            parent: appFlick.parent
            anchors.top: appFlick.top
            anchors.bottom: appFlick.bottom
            anchors.left: appFlick.right
            anchors.leftMargin: Style.space(2)
            policy: ScrollBar.AsNeeded
          }
          // The add form lands below the rows, so its opening scrolls to it once the list has been laid out again.
          Timer {
            id: revealTimer
            interval: 50
            onTriggered: appFlick.contentY = Math.max(0, appFlick.contentHeight - appFlick.height)
          }

          Connections {
            target: root
            function onAddOpenChanged() { if (root.addOpen) revealTimer.restart() }
            // Key-change outcomes sit at the top of the list, so one arriving scrolls there.
            function onActionErrorChanged() { if (root.actionError !== "") appFlick.contentY = 0 }
            function onActionNoteChanged() { if (root.actionNote !== "") appFlick.contentY = 0 }
          }

          Column {
            id: appList
            x: appCard.bleed
            width: appFlick.width - appCard.bleed * 2
            topPadding: appCard.bleed
            bottomPadding: appCard.bleed
            spacing: appCard.gap

            // Problems of any length scroll with the rows, so the fixed-height card never squeezes the list away.
            Column {
              visible: root.binaryMissing
              width: parent.width
              spacing: Style.space(6)

              Line { text: "omadai is not installed · build it from the omadai checkout and put target/release/omadai on PATH:"; color: root.foreground }
              Line { text: "bin/omadai build"; font.bold: true; color: root.foreground; font.pixelSize: Style.font.body }
            }

            Line { visible: text !== ""; text: root.actionError; color: root.urgent }
            Line { visible: text !== ""; text: root.actionNote }
            Line { visible: root.pendingActionKind === "remove"; text: "Removing the key…" }
            Line { visible: text !== ""; text: root.startProblem; color: root.urgent }
            Line { visible: text !== ""; text: root.appReportProblem; color: root.urgent }
            Line { visible: text !== ""; text: root.keysProblem; color: root.urgent }
            Line { visible: text !== ""; text: root.fetchNotice }

            Line {
              visible: !!root.appReport && root.appRows.length === 0
              text: "No platform connected yet · add a key below"
            }

            // Rows are placed by hand from shownOrder, so a drag can slide the others aside and rows of any height stack.
            Item {
              id: rowStack
              visible: root.appRows.length > 0
              width: parent.width
              height: root.stackHeight(root.appRowIds)

              // Lifts the held row off the rows it passes over.
              Rectangle {
                visible: root.dragId !== ""
                x: -Style.space(8)
                y: root.dragTop - Style.space(6)
                z: 1.5
                width: parent.width + Style.space(16)
                height: (root.rowHeights[root.dragId] || 0) + Style.space(12)
                radius: Style.cornerRadius
                color: Commons.Color.background
                border.width: 1
                border.color: Util.alpha(root.foreground, 0.15)
              }

              // Darkens and outlines the platform whose key is being renamed.
              Rectangle {
                readonly property string renamed: root.renamePlatform
                visible: renamed !== "" && root.shownOrder.indexOf(renamed) >= 0
                x: -Style.space(8)
                y: (root.dragId === renamed ? root.dragTop : root.slotTop(renamed, root.shownOrder)) - Style.space(6)
                z: 0.5
                width: parent.width + Style.space(16)
                height: (root.rowHeights[renamed] || 0) + Style.space(12)
                radius: Style.cornerRadius
                color: Qt.darker(Commons.Color.background, 1.6)
                border.width: 1
                border.color: Util.alpha(root.foreground, 0.35)
              }

              Repeater {
                model: root.appRows

                AppPlatformRow {
                  id: appRow
                  required property var modelData
                  readonly property bool held: root.dragId !== "" && root.dragId === p.platform
                  width: rowStack.width
                  p: modelData
                  y: held ? root.dragTop : root.slotTop(p.platform, root.shownOrder)
                  z: held ? 2 : 1
                  onImplicitHeightChanged: root.setRowHeight(p.platform, implicitHeight)
                  Component.onCompleted: root.setRowHeight(p.platform, implicitHeight)

                  Behavior on y {
                    enabled: root.dragId !== "" && !appRow.held
                    NumberAnimation { duration: 140; easing.type: Easing.OutCubic }
                  }
                }
              }
            }

            Column {
              visible: root.addOpen
              width: parent.width
              spacing: Style.space(6)

              Line { text: "Pick the platform · the key is typed into the terminal that opens, never into this window" }

              AppField {
                id: addLabelField
                width: parent.width
                placeholderText: "Label (optional, default \"default\")"
              }

              Flow {
                width: parent.width
                spacing: Style.space(6)

                Repeater {
                  model: root.offeredPlatforms

                  ActionButton {
                    required property var modelData
                    text: modelData.name
                    onClicked: root.addKey(modelData.id)
                  }
                }
              }

              Line { visible: text !== ""; text: root.addError; color: root.urgent }
            }
          }
        }

        // Outside the list, so the card's fixed height never cuts it off.
        ActionButton {
          id: addKeyButton
          anchors.left: appTop.left
          anchors.bottom: appSettings.top
          anchors.bottomMargin: appCard.gap
          text: root.addOpen ? "Cancel adding" : "+ Add key"
          onClicked: { root.addOpen = !root.addOpen; root.addError = "" }
        }

        Column {
          id: appSettings
          anchors.left: appTop.left
          anchors.right: appTop.right
          anchors.bottom: parent.bottom
          anchors.bottomMargin: appCard.pad
          spacing: appCard.gap

          Rule {}

          Item {
            width: parent.width
            implicitHeight: Math.max(settingsHeading.implicitHeight, logButton.implicitHeight)

            Text {
              id: settingsHeading
              textFormat: Text.PlainText
              anchors.left: parent.left
              anchors.verticalCenter: parent.verticalCenter
              text: "SETTINGS"
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              font.bold: true
            }

            // Chip-style, so it reads as a count rather than a command.
            Rectangle {
              id: logButton
              readonly property int count: root.logEntries.length
              readonly property color tint: count > 0 || root.logProblem !== "" ? root.foreground : root.dim
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              implicitWidth: logButtonRow.implicitWidth + Style.space(14)
              implicitHeight: logButtonRow.implicitHeight + Style.space(4)
              radius: Style.cornerRadius
              color: "transparent"
              border.width: 1
              border.color: logMouse.containsMouse ? Util.alpha(root.foreground, 0.6) : Util.alpha(root.foreground, 0.35)

              Row {
                id: logButtonRow
                anchors.centerIn: parent
                spacing: Style.space(6)

                Text {
                  textFormat: Text.PlainText
                  anchors.verticalCenter: parent.verticalCenter
                  text: root.logIcon
                  color: logButton.tint
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.caption
                }

                Text {
                  textFormat: Text.PlainText
                  anchors.verticalCenter: parent.verticalCenter
                  text: logButton.count > 0 ? "log · " + logButton.count : "log"
                  color: logButton.tint
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.caption
                }
              }

              MouseArea {
                id: logMouse
                anchors.fill: parent
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                onClicked: root.openLog()
              }
            }
          }

          Item {
            width: parent.width
            implicitHeight: refreshGroup.implicitHeight

            Text {
              textFormat: Text.PlainText
              anchors.left: parent.left
              anchors.verticalCenter: parent.verticalCenter
              text: "Refresh interval"
              color: root.foreground
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
            }

            ButtonGroup {
              id: refreshGroup
              anchors.right: parent.right
              spacing: Style.space(6)
              options: root.refreshChoices
              value: String(root.refreshMinutes)
              foreground: root.foreground
              fontFamily: root.fontFamily
              fontSize: Style.font.bodySmall
              onChanged: function(v) { root.setRefreshMinutes(Number(v)) }
            }
          }
        }

        // Holds keyboard focus while the remove confirmation is open, so Enter in a text field never reaches it.
        Item {
          id: confirmKeys
          Keys.onPressed: function(event) {
            if (removeConfirm.handleKey(event)) event.accepted = true
          }
        }

        ConfirmDialog {
          id: removeConfirm
          anchors.fill: parent
          z: 10
          opened: !!root.removeTarget
          message: root.removeTarget
            ? "Remove the " + root.platformName(root.removeTarget.platform) + " key \"" + root.removeTarget.label + "\" from the keyring?"
            : ""
          confirmText: "Remove"
          fontFamily: root.fontFamily
          onCanceled: root.cancelRemove()
          onConfirmed: root.confirmRemove()
        }
      }
    }

    // The error log, in place of the app card until Esc or ✕.
    BorderSurface {
      id: logCard
      visible: root.logOpen
      readonly property real pad: Style.space(22)
      readonly property real gap: Style.space(14)
      width: Math.min(Style.space(620), appWindow.width - Style.gapsOut * 2)
      height: Math.min(pad * 2 + logTop.implicitHeight + gap + logList.implicitHeight, appWindow.height - Style.gapsOut * 2)
      anchors.centerIn: parent
      radius: Style.cornerRadius
      color: Commons.Color.background
      borderSpec: Border.surfaceSpec("menu", "border", Commons.Color.menu.border, Math.max(1, Style.space(2)))

      FocusScope {
        anchors.fill: parent
        focus: true

        Item { id: logHome; focus: true }

        Keys.onEscapePressed: root.closeLog()

        Column {
          id: logTop
          x: logCard.pad
          y: logCard.pad
          width: logCard.width - logCard.pad * 2
          spacing: logCard.gap

          Item {
            width: parent.width
            implicitHeight: Math.max(logTitle.implicitHeight, logButtons.implicitHeight)

            Text {
              id: logTitle
              textFormat: Text.PlainText
              anchors.left: parent.left
              anchors.verticalCenter: parent.verticalCenter
              text: "Error log"
              color: root.foreground
              font.family: root.fontFamily
              font.pixelSize: Style.font.display
            }

            Row {
              id: logButtons
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              spacing: Style.space(8)

              ActionButton {
                text: "Copy"
                enabled: root.logEntries.length > 0
                onClicked: root.copyLog()
              }

              ActionButton {
                text: "Export"
                enabled: !logActionProc.busy && root.logEntries.length > 0
                onClicked: root.exportLog()
              }

              ActionButton {
                text: "Clear"
                enabled: !logActionProc.busy && root.logEntries.length > 0
                onClicked: root.clearLog()
              }

              ActionButton {
                text: "✕"
                onClicked: root.closeLog()
              }
            }
          }

          Line {
            text: "newest first · kept for 30 days" + (root.logExported !== "" ? " · exported to " + root.logExported : "")
          }
          Line { visible: text !== ""; text: root.logActionError; color: root.urgent }
          Line { visible: text !== ""; text: root.logProblem; color: root.urgent }

          Rule {}
        }

        Flickable {
          id: logFlick
          x: logCard.pad
          anchors.top: logTop.bottom
          anchors.topMargin: logCard.gap
          anchors.bottom: parent.bottom
          anchors.bottomMargin: logCard.pad
          width: logTop.width
          contentWidth: width
          contentHeight: logList.implicitHeight
          clip: true
          boundsBehavior: Flickable.StopAtBounds
          interactive: contentHeight > height
          ScrollBar.vertical: ScrollBar {
            parent: logFlick.parent
            anchors.top: logFlick.top
            anchors.bottom: logFlick.bottom
            anchors.left: logFlick.right
            anchors.leftMargin: Style.space(2)
            policy: ScrollBar.AsNeeded
          }

          Column {
            id: logList
            width: logFlick.width
            spacing: logCard.gap

            Line {
              visible: root.logEntries.length === 0
              text: "Nothing has failed in the last 30 days"
            }

            Repeater {
              model: root.logEntries

              Column {
                id: entry
                required property var modelData
                readonly property real indent: Style.space(24)
                width: logList.width
                spacing: Style.space(4)

                Item {
                  width: parent.width
                  implicitHeight: Math.max(entryHead.implicitHeight, entryWhen.implicitHeight)

                  Row {
                    id: entryHead
                    anchors.left: parent.left
                    anchors.right: entryWhen.left
                    anchors.rightMargin: Style.space(8)
                    anchors.verticalCenter: parent.verticalCenter
                    spacing: Style.space(10)
                    clip: true

                    ErrorDisc {
                      anchors.verticalCenter: parent.verticalCenter
                      size: Math.round(Style.font.bodySmall * 0.95)
                    }

                    Text {
                      textFormat: Text.PlainText
                      anchors.verticalCenter: parent.verticalCenter
                      text: entry.modelData.platform ? root.platformName(entry.modelData.platform) : "omadai"
                      color: root.foreground
                      font.family: root.fontFamily
                      font.pixelSize: Style.font.bodySmall
                    }

                    Text {
                      visible: text !== ""
                      textFormat: Text.PlainText
                      anchors.verticalCenter: parent.verticalCenter
                      text: entry.modelData.month ? root.monthTitle(entry.modelData.month) : ""
                      color: root.dim
                      font.family: root.fontFamily
                      font.pixelSize: Style.font.caption
                    }
                  }

                  Text {
                    id: entryWhen
                    textFormat: Text.PlainText
                    anchors.right: parent.right
                    anchors.verticalCenter: parent.verticalCenter
                    text: root.logStamp(entry.modelData.last_at) + (entry.modelData.count > 1 ? " · ×" + entry.modelData.count : "")
                    color: root.dim
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.caption
                  }
                }

                Text {
                  textFormat: Text.PlainText
                  x: entry.indent
                  width: parent.width - x
                  wrapMode: Text.Wrap
                  text: String(entry.modelData.summary || "")
                  color: root.foreground
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.caption
                }

                Text {
                  visible: text !== ""
                  textFormat: Text.PlainText
                  x: entry.indent
                  width: parent.width - x
                  wrapMode: Text.WrapAnywhere
                  text: String(entry.modelData.detail || "")
                  color: root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.caption * 0.92
                }
              }
            }
          }
        }
      }
    }
  }

  // Runs one omadai argv at a time; a request while busy replaces the pending one and runs when the current one ends.
  component OmadaiProcess: Process {
    id: proc
    property var pending: null
    property bool busy: false
    property bool sawStart: false
    property bool outDone: false
    property bool errDone: false
    property bool exitDone: false
    property int code: 0
    property string outText: ""
    property string errText: ""
    signal done(var argv, int code, string out, string err)
    signal startFailed(var argv)
    running: false

    function request(argv) {
      proc.pending = argv
      if (!proc.busy) proc.startPending()
    }

    function startPending() {
      if (proc.busy || proc.pending === null) return
      var argv = proc.pending
      proc.pending = null
      proc.busy = true
      proc.sawStart = false
      proc.outDone = false
      proc.errDone = false
      proc.exitDone = false
      proc.outText = ""
      proc.errText = ""
      proc.command = argv
      proc.running = true
    }

    function finish() {
      if (!proc.busy || !proc.outDone || !proc.errDone || !proc.exitDone) return
      proc.busy = false
      proc.done(proc.command, proc.code, proc.outText, proc.errText.trim())
      if (proc.pending !== null) Qt.callLater(proc.startPending)
    }

    onStarted: {
      proc.sawStart = true
      root.binaryMissing = false
      root.startProblem = ""
    }
    onExited: function(exitCode, exitStatus) {
      proc.code = exitStatus === 0 ? exitCode : -1
      proc.exitDone = true
      proc.finish()
    }
    onRunningChanged: if (!proc.running && !proc.sawStart && proc.busy) {
      proc.busy = false
      proc.pending = null
      proc.startFailed(proc.command)
    }

    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: { proc.outText = text; proc.outDone = true; proc.finish() }
    }
    stderr: StdioCollector {
      waitForEnd: true
      onStreamFinished: { proc.errText = text; proc.errDone = true; proc.finish() }
    }
  }

  // A text field in the app window: Escape closes the window before the field sees it, as in the clipboard manager,
  // and Enter does nothing while a removal waits for its confirmation.
  component AppField: TextField {
    foreground: root.foreground
    font.family: root.fontFamily
    Keys.priority: Keys.BeforeItem
    Keys.onPressed: function(event) {
      if (event.key === Qt.Key_Escape) {
        root.escapeApp()
        event.accepted = true
      } else if (root.removeTarget && (event.key === Qt.Key_Return || event.key === Qt.Key_Enter)) {
        event.accepted = true
      }
    }
  }

  component ActionButton: Button {
    bordered: true
    foreground: root.foreground
    fontFamily: root.fontFamily
    fontSize: Style.font.bodySmall
    opacity: enabled ? 1 : 0.35
  }

  component Line: Text {
    textFormat: Text.PlainText
    width: parent ? parent.width : 0
    color: root.dim
    font.family: root.fontFamily
    font.pixelSize: Style.font.caption
    wrapMode: Text.Wrap
  }

  // A figure chip; with describe set, hovering it shows what the kind means.
  component Chip: Rectangle {
    id: chip
    property string kind: ""
    property string platform: ""
    property bool describe: false
    visible: root.chipLabel(kind) !== ""
    implicitWidth: chipText.implicitWidth + Style.space(10)
    implicitHeight: chipText.implicitHeight + Style.space(2)
    radius: Style.cornerRadius
    color: "transparent"
    border.width: 1
    border.color: Util.alpha(root.foreground, 0.35)

    Text {
      id: chipText
      textFormat: Text.PlainText
      anchors.centerIn: parent
      text: root.chipLabel(chip.kind)
      color: root.accent
      font.family: root.fontFamily
      font.pixelSize: Style.font.caption
    }

    HoverHandler {
      id: chipHover
      enabled: chip.describe
    }

    Tip {
      shown: chip.describe && chipHover.hovered
      x: 0
      y: chip.height + Style.space(4)
      tipWidth: Style.space(330)
      text: root.chipDescription(chip.kind, chip.platform)
    }
  }

  // A hover pop-up in the window's overlay layer, so no list clips it and no later row draws over it.
  component Tip: Popup {
    id: tip
    property bool shown: false
    property string text: ""
    property bool error: false
    property real tipWidth: Style.space(300)
    visible: shown && text !== ""
    width: Math.min(tipWidth, tipText.implicitWidth + leftPadding + rightPadding)
    padding: 0
    leftPadding: Style.space(10)
    rightPadding: Style.space(10)
    topPadding: Style.space(8)
    bottomPadding: Style.space(8)
    modal: false
    focus: false
    closePolicy: Popup.NoAutoClose

    background: Rectangle {
      // The shell's tooltip colour made opaque, with a one-pixel border rather than the theme's thick one.
      color: Qt.rgba(Commons.Color.tooltip.background.r, Commons.Color.tooltip.background.g, Commons.Color.tooltip.background.b, 1)
      border.width: 1
      border.color: tip.error ? root.deleteTint : Commons.Color.tooltip.border
      radius: Style.cornerRadius
    }

    contentItem: Text {
      id: tipText
      textFormat: Text.PlainText
      text: tip.text
      wrapMode: Text.WordWrap
      color: Commons.Color.tooltip.text
      font.family: root.fontFamily
      font.pixelSize: Style.font.caption
    }
  }

  // A solid pastel red disc with the "!" drawn as two shapes, so it sits in the middle whatever the font; hovering shows tip.
  component ErrorDisc: Rectangle {
    id: disc
    property real size: Math.round(Style.font.body * 0.95)
    property string tip: ""
    // The pop-up's left edge on the disc, for a disc at a row's left end; otherwise right-aligned under it.
    property bool tipLeft: false
    implicitWidth: size
    implicitHeight: size
    width: size
    height: size
    radius: size / 2
    color: root.deleteTint

    Rectangle {
      x: (disc.size - width) / 2
      y: disc.size * 0.2
      width: Math.max(2, Math.round(disc.size * 0.16))
      height: disc.size * 0.38
      radius: width / 2
      color: Commons.Color.background
    }

    Rectangle {
      x: (disc.size - width) / 2
      y: disc.size * 0.66
      width: Math.max(2, Math.round(disc.size * 0.16))
      height: width
      radius: width / 2
      color: Commons.Color.background
    }

    HoverHandler {
      id: discHover
      enabled: disc.tip !== ""
    }

    Tip {
      shown: discHover.hovered
      error: true
      tipWidth: Style.space(270)
      x: disc.tipLeft ? 0 : disc.width - width
      y: disc.height + Style.space(4)
      text: disc.tip
    }
  }

  // The grey ⓘ beside a row whose history starts inside the view, or after it.
  component InfoMark: Text {
    id: mark
    property string tip: ""
    textFormat: Text.PlainText
    text: root.infoIcon
    color: root.dim
    font.family: root.fontFamily
    font.pixelSize: Style.font.body

    HoverHandler { id: markHover }

    Tip {
      shown: markHover.hovered
      tipWidth: Style.space(270)
      x: mark.width - width
      y: mark.height + Style.space(4)
      text: mark.tip
    }
  }

  // One platform in the bar panel: chevron, name, chip and amount, its credits, and its three largest items when expanded.
  component PlatformRow: Column {
    id: row
    property var p: null
    // A lone item that repeats the platform total has nothing more to show, so it does not expand.
    readonly property bool expandable: !!p && p.items.length > 0
        && !(p.items.length === 1 && root.sumDecimals([p.items[0].total]) === root.sumDecimals([p.total]))
    readonly property bool open: expandable && !!root.panelExpanded[p.platform]
    readonly property bool needs: !!p && !!p.requirement
    readonly property bool fetched: !!p && !!p.last_fetched
    readonly property bool failed: !!p && !!p.last_error
    // A fetch that succeeded yet left spend out.
    readonly property bool incomplete: !!p && ((p.warnings || []).length > 0 || (p.missing_months || []).length > 0)
    readonly property bool historyWhole: root.historyWhole(p, [root.nowMonth])
    readonly property bool historyPartial: root.historyPartial(p, [root.nowMonth])
    readonly property var largest: p ? root.largestFirst(p.items) : []
    readonly property real indent: chevron.implicitWidth + Style.space(8)
    spacing: Style.space(3)

    Item {
      width: parent.width
      implicitHeight: Math.max(rowName.implicitHeight, rowFigure.implicitHeight)

      MouseArea {
        anchors.fill: parent
        enabled: row.expandable
        cursorShape: Qt.PointingHandCursor
        onClicked: root.toggleExpanded("panel", row.p.platform)
      }

      Row {
        anchors.left: parent.left
        anchors.verticalCenter: parent.verticalCenter
        spacing: Style.space(8)

        Text {
          id: chevron
          textFormat: Text.PlainText
          anchors.verticalCenter: parent.verticalCenter
          text: row.open ? "▾" : "▸"
          opacity: row.expandable ? 1 : 0
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
        }

        Text {
          id: rowName
          textFormat: Text.PlainText
          anchors.verticalCenter: parent.verticalCenter
          text: row.p ? row.p.name : ""
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.body
        }

        Chip {
          anchors.verticalCenter: parent.verticalCenter
          kind: row.p && !row.historyWhole ? row.p.kind : ""
        }
      }

      Row {
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        spacing: Style.space(8)

        InfoMark {
          visible: (row.historyWhole || row.historyPartial) && !row.needs
          anchors.verticalCenter: parent.verticalCenter
          tip: row.p ? root.historyText(row.p.history_from) : ""
        }

        // Before the last good figure, or before the dash of a figure the month never had.
        ErrorDisc {
          visible: (row.failed || row.incomplete) && !row.needs && !row.historyWhole
          anchors.verticalCenter: parent.verticalCenter
          tip: root.discText(row.p, "panel")
        }

        Text {
          id: rowFigure
          readonly property bool dash: !row.needs && (row.historyWhole || !row.fetched)
          textFormat: Text.PlainText
          anchors.verticalCenter: parent.verticalCenter
          text: !row.p ? "" : (row.needs ? "needs setup" : (dash ? "—" : root.money(row.p.total)))
          color: row.needs ? root.urgent : (dash ? root.dim : root.foreground)
          font.family: root.fontFamily
          font.pixelSize: Style.font.body
          font.bold: !row.needs
        }
      }
    }

    Line {
      visible: text !== ""
      leftPadding: row.indent
      textFormat: Text.StyledText
      text: row.p && row.p.credits_remaining !== null && row.p.credits_remaining !== undefined
        ? "<font color='" + root.accent + "'>" + root.money(row.p.credits_remaining) + "</font> credits left" : ""
    }

    Line {
      visible: row.needs
      leftPadding: row.indent
      text: row.needs ? row.p.requirement : ""
      color: root.foreground
    }

    Repeater {
      model: row.open ? row.largest.slice(0, root.panelMaxItems) : []

      Item {
        id: panelItem
        required property var modelData
        width: row.width
        implicitHeight: itemName.implicitHeight

        Text {
          id: itemName
          textFormat: Text.PlainText
          anchors.left: parent.left
          anchors.leftMargin: Style.space(28)
          anchors.right: itemTotal.left
          anchors.rightMargin: Style.space(8)
          anchors.verticalCenter: parent.verticalCenter
          text: panelItem.modelData.name ? panelItem.modelData.name : panelItem.modelData.id
          elide: Text.ElideRight
          color: root.accent
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
        }

        Text {
          id: itemTotal
          textFormat: Text.PlainText
          anchors.right: parent.right
          anchors.verticalCenter: parent.verticalCenter
          text: root.money(panelItem.modelData.total)
          color: root.accent
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
        }
      }
    }

    Line {
      visible: row.open && row.largest.length > root.panelMaxItems
      leftPadding: Style.space(28)
      text: "+" + (row.largest.length - root.panelMaxItems) + " more · open app"

      MouseArea {
        anchors.fill: parent
        cursorShape: Qt.PointingHandCursor
        onClicked: root.openAppFromPanel()
      }
    }
  }

  component Rule: Rectangle {
    width: parent ? parent.width : 0
    height: 1
    color: Util.alpha(root.foreground, 0.15)
  }

  // A borderless icon button whose hit area is larger than the glyph.
  component Glyph: Item {
    id: glyph
    property string icon: ""
    property color idleColor: root.dim
    property color hotColor: root.foreground
    signal clicked()
    implicitWidth: glyphText.implicitWidth + Style.space(8)
    implicitHeight: glyphText.implicitHeight + Style.space(6)
    opacity: enabled ? 1 : 0.35

    Text {
      id: glyphText
      anchors.centerIn: parent
      textFormat: Text.PlainText
      text: glyph.icon
      color: glyphMouse.containsMouse ? glyph.hotColor : glyph.idleColor
      font.family: root.fontFamily
      font.pixelSize: Style.font.bodySmall
    }

    MouseArea {
      id: glyphMouse
      anchors.fill: parent
      hoverEnabled: true
      cursorShape: Qt.PointingHandCursor
      onClicked: glyph.clicked()
    }
  }

  // The drag handle: three by three dots across the whole handle area; a drag starts nowhere else.
  component Grip: Item {
    id: grip
    readonly property bool hot: gripHover.hovered || gripDrag.active
    signal dragStarted()
    signal dragMoved(real dy)
    signal dragEnded()
    implicitWidth: root.gripWidth
    implicitHeight: Style.space(16)
    Component.onDestruction: if (gripHover.hovered) root.gripsHovered = Math.max(0, root.gripsHovered - 1)

    Grid {
      anchors.centerIn: parent
      columns: 3
      rows: 3
      columnSpacing: Style.space(2)
      rowSpacing: Style.space(2)

      Repeater {
        model: 9

        Rectangle {
          width: Style.space(2)
          height: width
          radius: width / 2
          color: grip.hot ? root.foreground : Util.alpha(root.dim, 0.85)
        }
      }
    }

    HoverHandler {
      id: gripHover
      cursorShape: Qt.OpenHandCursor
      onHoveredChanged: root.gripsHovered = Math.max(0, root.gripsHovered + (hovered ? 1 : -1))
    }

    DragHandler {
      id: gripDrag
      target: null
      xAxis.enabled: false
      cursorShape: Qt.ClosedHandCursor
      onActiveChanged: active ? grip.dragStarted() : grip.dragEnded()
      onActiveTranslationChanged: if (active) grip.dragMoved(activeTranslation.y)
    }
  }

  // One platform in the app: handle, name, chip, its own facts and amount, a line per key, then its items behind "spend by".
  component AppPlatformRow: Column {
    id: arow
    property var p: null
    property bool measureOnly: false
    property int measureKeys: 1
    readonly property var rowKeys: {
      if (!p) return []
      if (!measureOnly) return root.keysFor(p.platform)
      var fake = []
      for (var i = 0; i < measureKeys; i++) fake.push({ platform: p.platform, label: "default" })
      return fake
    }
    readonly property bool open: !!p && !measureOnly && !!root.appExpanded[p.platform]
    readonly property bool needs: !!p && !!p.requirement
    readonly property bool fetched: !!p && !!p.last_fetched
    // The measured row keeps the "spend by" line, so a platform with items never makes the fixed-height card scroll.
    readonly property bool hasBreakdown: measureOnly || (!!p && p.items.length > 0)
    readonly property bool failed: !!p && !!p.last_error
    readonly property bool warned: !!p && (p.warnings || []).length > 0
    readonly property bool historyWhole: !measureOnly && root.historyWhole(p, root.viewMonths())
    readonly property bool historyPartial: !measureOnly && root.historyPartial(p, root.viewMonths())
    readonly property real indent: root.gripWidth + Style.space(10)
    readonly property bool canEdit: !root.keyActionBusy && root.renamePlatform === ""
    // Credits and settlement belong to the platform, so they sit on its line and never on a key's.
    readonly property string accountText: {
      if (!p) return ""
      var parts = []
      if (p.credits_remaining !== null && p.credits_remaining !== undefined)
        parts.push("<font color='" + root.accent + "'>" + root.money(p.credits_remaining) + "</font> credits left")
      if (p.settlement_adjustment !== undefined && p.settlement_adjustment !== null)
        parts.push("settlement " + root.signedMoney(p.settlement_adjustment))
      return parts.join(" · ")
    }
    readonly property string missingText: {
      if (!p || p.missing_months.length === 0) return ""
      var months = p.missing_months.slice().sort()
      var what = months.length > 2 ? root.monthTitle(months[0]) + " to " + root.monthTitle(months[months.length - 1])
        : months.map(function(m) { return root.monthTitle(m) }).join(" and ")
      // Automatic fetches cover the last six months only, so an older month waits for Refresh.
      return what + " not fetched yet · Refresh to add them"
    }
    spacing: Style.space(5)

    Item {
      width: parent.width
      implicitHeight: Math.max(nameText.implicitHeight, amountText.implicitHeight, rowGrip.implicitHeight)

      Grip {
        id: rowGrip
        anchors.left: parent.left
        anchors.top: parent.top
        anchors.bottom: parent.bottom
        onDragStarted: if (arow.p && !arow.measureOnly) root.beginDrag(arow.p.platform)
        onDragMoved: function(dy) { root.dragTo(dy) }
        onDragEnded: root.endDrag(true)
      }

      Row {
        anchors.left: rowGrip.right
        anchors.leftMargin: Style.space(10)
        anchors.right: amountRow.left
        anchors.rightMargin: Style.space(8)
        anchors.verticalCenter: parent.verticalCenter
        spacing: Style.space(10)
        clip: true

        Text {
          id: nameText
          textFormat: Text.PlainText
          anchors.verticalCenter: parent.verticalCenter
          text: arow.p ? arow.p.name : ""
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.body
        }

        Chip {
          anchors.verticalCenter: parent.verticalCenter
          kind: arow.p && !arow.historyWhole ? arow.p.kind : ""
          platform: arow.p ? arow.p.platform : ""
          describe: !arow.measureOnly && root.dragId === ""
        }

        Text {
          visible: text !== ""
          textFormat: Text.StyledText
          anchors.verticalCenter: parent.verticalCenter
          text: arow.accountText
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
        }
      }

      Row {
        id: amountRow
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        spacing: Style.space(8)

        InfoMark {
          visible: (arow.historyWhole || arow.historyPartial) && !arow.needs
          anchors.verticalCenter: parent.verticalCenter
          tip: arow.p ? root.historyText(arow.p.history_from) : ""
        }

        ErrorDisc {
          visible: (arow.failed || arow.warned) && !arow.needs && !arow.historyWhole
          anchors.verticalCenter: parent.verticalCenter
          tip: root.dragId === "" ? root.discText(arow.p, "app") : ""
        }

        Text {
          id: amountText
          readonly property bool dash: !arow.needs && (arow.historyWhole || !arow.fetched)
          textFormat: Text.PlainText
          anchors.verticalCenter: parent.verticalCenter
          text: !arow.p ? "" : (arow.needs ? "needs setup" : (dash ? "—" : root.money(arow.p.total)))
          color: arow.needs ? root.urgent : (dash ? root.dim : root.foreground)
          font.family: root.fontFamily
          font.pixelSize: Style.font.body
          font.bold: !arow.needs
        }
      }
    }

    Repeater {
      model: arow.rowKeys

      Item {
        id: keyRow
        required property var modelData
        required property int index
        readonly property bool renaming: !arow.measureOnly && root.renamePlatform === modelData.platform && root.renameLabel === modelData.label
        width: arow.width
        implicitHeight: Math.max(keyText.implicitHeight, keyGlyphs.implicitHeight, renameLoader.implicitHeight)

        Rectangle {
          visible: keyRow.index > 0
          x: arow.indent
          y: -Math.round((arow.spacing + 1) / 2)
          width: parent.width - arow.indent
          height: 1
          color: Util.alpha(root.foreground, 0.12)
        }

        Text {
          id: keyText
          visible: !keyRow.renaming
          textFormat: Text.PlainText
          anchors.left: parent.left
          anchors.leftMargin: arow.indent
          anchors.right: keyGlyphs.left
          anchors.rightMargin: Style.space(8)
          anchors.verticalCenter: parent.verticalCenter
          text: keyRow.modelData.label
          elide: Text.ElideRight
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
        }

        Loader {
          id: renameLoader
          active: keyRow.renaming
          anchors.left: parent.left
          anchors.leftMargin: arow.indent
          anchors.verticalCenter: parent.verticalCenter

          sourceComponent: Row {
            spacing: Style.space(6)

            AppField {
              id: inlineField
              width: Style.space(200)
              anchors.verticalCenter: parent.verticalCenter
              onTextChanged: root.renameDraft = text
              onAccepted: root.commitRename()
              Component.onCompleted: {
                text = root.renameDraft
                Qt.callLater(function() { if (inlineField) inlineField.forceActiveFocus() })
              }

              Connections {
                target: root
                function onRenameStarted() {
                  inlineField.text = root.renameDraft
                  inlineField.forceActiveFocus()
                }
              }
            }

            ActionButton {
              anchors.verticalCenter: parent.verticalCenter
              text: root.pendingActionKind === "rename" ? "Saving…" : "Save"
              enabled: !root.keyActionBusy
              onClicked: root.commitRename()
            }

            ActionButton {
              anchors.verticalCenter: parent.verticalCenter
              text: "Cancel"
              enabled: root.pendingActionKind !== "rename"
              onClicked: root.cancelRename()
            }
          }
        }

        // The trash's right edge meets the amounts' right edge; the glyphs' hit padding hangs past it.
        Row {
          id: keyGlyphs
          anchors.right: parent.right
          anchors.rightMargin: -Style.space(4)
          anchors.verticalCenter: parent.verticalCenter
          spacing: Style.space(6)

          Glyph {
            anchors.verticalCenter: parent.verticalCenter
            icon: "\u{F03EB}"
            idleColor: keyRow.renaming ? root.foreground : root.dim
            enabled: arow.canEdit || (keyRow.renaming && root.pendingActionKind !== "rename")
            onClicked: {
              if (arow.measureOnly) return
              if (keyRow.renaming) root.cancelRename()
              else root.startRename(keyRow.modelData)
            }
          }

          Glyph {
            anchors.verticalCenter: parent.verticalCenter
            icon: "\u{F0A7A}"
            idleColor: root.deleteTint
            hotColor: Commons.Color.urgent
            enabled: arow.canEdit
            onClicked: if (!arow.measureOnly) root.askRemove(keyRow.modelData)
          }
        }
      }
    }

    Line {
      visible: arow.needs
      leftPadding: arow.indent
      text: arow.needs ? arow.p.requirement : ""
      color: root.foreground
    }

    Line {
      visible: text !== ""
      leftPadding: arow.indent
      text: arow.missingText
    }

    Line {
      visible: arow.hasBreakdown
      leftPadding: arow.indent
      text: (arow.open ? "▾ " : "▸ ") + "spend by " + (arow.p ? arow.p.breakdown : "key")

      MouseArea {
        x: arow.indent
        width: parent.contentWidth
        height: parent.height
        cursorShape: Qt.PointingHandCursor
        onClicked: if (arow.p && !arow.measureOnly) root.toggleExpanded("app", arow.p.platform)
      }
    }

    Column {
      visible: arow.open && arow.hasBreakdown
      x: arow.indent + Style.space(22)
      width: parent.width - x
      spacing: Style.space(2)

      Repeater {
        model: arow.open && arow.p ? root.largestFirst(arow.p.items) : []

        Item {
          id: appItem
          required property var modelData
          width: parent.width
          implicitHeight: appItemName.implicitHeight

          Text {
            id: appItemName
            textFormat: Text.PlainText
            anchors.left: parent.left
            anchors.right: appItemTotal.left
            anchors.rightMargin: Style.space(8)
            anchors.verticalCenter: parent.verticalCenter
            text: appItem.modelData.name ? appItem.modelData.name : appItem.modelData.id
            elide: Text.ElideRight
            color: root.accent
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
          }

          Text {
            id: appItemTotal
            textFormat: Text.PlainText
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            text: root.money(appItem.modelData.total)
            color: root.accent
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
          }
        }
      }
    }
  }
}
