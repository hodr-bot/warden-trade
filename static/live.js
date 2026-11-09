// warden live ticks: one EventSource for the whole app (guarded, so HTMX
// swaps never open a second stream). Updates price cells + portfolio cards
// in place with a brief directional flash. If the stream drops, the 30s
// HTMX polling keeps pages fresh — this script stays silent.
//
// NOTE: base.html loads this file with ?v=<release> because Cloudflare
// caches /static/* at the edge. Bump the query string in base.html (the
// single place) on every live.js change.
(function () {
  if (window.__wardenLive) return;
  window.__wardenLive = true;

  // Logout button (nav, all authenticated pages): expire the session
  // cookie server-side, then land on /login. Runs even if the live
  // stream never connects.
  var logout = document.getElementById("logout");
  if (logout) {
    logout.addEventListener("click", function () {
      logout.disabled = true;
      fetch("/api/auth/logout", { method: "POST" }).finally(function () {
        window.location.href = "/login";
      });
    });
  }

  function fmt(n) {
    return Number(n).toLocaleString("en-US", {
      minimumFractionDigits: 2,
      maximumFractionDigits: 2,
    });
  }

  // Brief green/red flash on change of direction. Respects reduced motion
  // via CSS (no animation class is added when the user opts out).
  var reduceMotion = window.matchMedia &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  function flash(el, dir) {
    if (reduceMotion || !dir) return;
    el.classList.remove("tick-up", "tick-down");
    void el.offsetWidth; // restart the animation
    el.classList.add(dir > 0 ? "tick-up" : "tick-down");
    window.setTimeout(function () {
      el.classList.remove("tick-up", "tick-down");
    }, 700);
  }

  function setText(el, text, num) {
    var prev = parseFloat(el.getAttribute("data-last") || "NaN");
    if (el.textContent !== text) {
      el.textContent = text;
      if (!isNaN(prev) && !isNaN(num) && num !== prev) {
        flash(el, num - prev);
      }
      el.setAttribute("data-last", String(num));
    }
  }

  // Status bar (shell footer): feed state + time of the last tick. The bar
  // lives outside the swapped regions, so these references stay valid.
  var stFeed = document.getElementById("st-feed");
  var stText = document.getElementById("st-feed-text");
  var stTime = document.getElementById("st-time");
  var lastMsg = 0;
  function setFeed(state, label) {
    if (!stFeed) return;
    stFeed.className = "st st-" + state;
    if (stText) stText.textContent = label;
  }

  var es;
  try {
    es = new EventSource("/api/live/stream");
  } catch (e) {
    setFeed("off", "Polling");
    return; // no EventSource support: HTMX polling covers freshness
  }

  es.onopen = function () {
    setFeed("live", "Live");
  };

  es.onerror = function () {
    // A network drop leaves readyState CONNECTING: the browser retries by
    // itself, so the stream must NOT be closed or probed (a probe on the
    // stream URL would open a second never-ending connection server-side).
    // CLOSED means the server refused the stream: 401 = session expired.
    setFeed("off", es.readyState === EventSource.CLOSED ? "Offline" : "Reconnecting");
    if (es.readyState === EventSource.CLOSED) {
      fetch("/api/live/status", { credentials: "same-origin" }).then(function (resp) {
        if (resp.status === 401 && window.location.pathname !== "/login") {
          window.location.href = "/login";
        }
      }).catch(function () { /* offline: stay, polling covers it */ });
    }
  };

  // No tick for 8s while the connection looks open = stale marks.
  window.setInterval(function () {
    if (lastMsg && Date.now() - lastMsg > 8000) setFeed("stale", "Stale");
  }, 2000);

  es.onmessage = function (ev) {
    var msg;
    try {
      msg = JSON.parse(ev.data);
    } catch (e) {
      return;
    }
    lastMsg = Date.now();
    setFeed("live", "Live");
    if (stTime) stTime.textContent = "Updated " + new Date().toLocaleTimeString("en-GB");
    // Marks -> every price cell for that symbol.
    // Kraken REST pairs shown on /markets mirror our symbols 1:1.
    var ALIAS = { "XBTUSD": "BTC-USD", "ETHUSD": "ETH-USD", "SOLUSD": "SOL-USD" };
    if (msg.marks) {
      var paint = function (sym, price) {
        document.querySelectorAll('[data-mark="' + sym + '"]').forEach(function (el) {
          setText(el, fmt(price), price);
        });
      };
      Object.keys(msg.marks).forEach(function (sym) {
        paint(sym, msg.marks[sym]);
      });
      Object.keys(ALIAS).forEach(function (krakenSym) {
        var src = ALIAS[krakenSym];
        if (msg.marks[src] !== undefined) paint(krakenSym, msg.marks[src]);
      });
    }
    // Portfolio block -> dashboard cards + detail table (server-formatted).
    var pf = msg.portfolio;
    if (pf) {
      [["capital", "capital_sub", "capital_class"],
       ["value", "value_sub", "class"],
       ["realized", "realized_sub", "rclass"],
       ["unrealized", "unrealized_sub", "uclass"]
      ].forEach(function (keys) {
        var main = document.querySelectorAll('[data-pf="' + keys[0] + '"]');
        var sub = document.querySelectorAll('[data-pf="' + keys[1] + '"]');
        main.forEach(function (el) {
          if (pf[keys[0]] && el.textContent !== pf[keys[0]]) {
            var prev = parseFloat(el.getAttribute("data-last") || "NaN");
            var cur = parseFloat(String(pf[keys[0]]).replace(/[^0-9.\-]/g, ""));
            el.textContent = pf[keys[0]];
            if (!isNaN(prev) && !isNaN(cur) && cur !== prev) flash(el, cur - prev);
            el.setAttribute("data-last", String(cur));
            if (pf[keys[2]] !== undefined) {
              var cls = pf[keys[2]];
              el.classList.remove("up", "down");
              if (cls) el.classList.add(cls);
              // Detail table: the colour lives on the <td>, not the span.
              var td = el.closest ? el.closest("td") : null;
              if (td) {
                td.classList.remove("up", "down");
                if (cls) td.classList.add(cls);
              }
            }
          }
        });
        sub.forEach(function (el) {
          if (pf[keys[1]] && el.textContent !== pf[keys[1]]) el.textContent = pf[keys[1]];
        });
      });
      document.querySelectorAll('[data-pf="closed"]').forEach(function (el) {
        if (pf.closed !== undefined && el.textContent !== String(pf.closed)) {
          el.textContent = String(pf.closed);
        }
      });
    }
  };

  // Expired session: HTMX swaps silently do nothing on auth failure (the
  // 302 to /login is followed, then hx-select finds no target). Any 401
  // or a swap whose final URL is the login page means "log in again".
  document.addEventListener("htmx:responseError", function (ev) {
    var s = ev.detail && ev.detail.xhr ? ev.detail.xhr.status : 0;
    if (s === 401 || s === 403) window.location.href = "/login";
  });
  document.addEventListener("htmx:beforeSwap", function (ev) {
    var url = ev.detail && ev.detail.xhr ? ev.detail.xhr.responseURL : "";
    if (url && url.indexOf("/login") !== -1) window.location.href = "/login";
  });

  // Timeline dots: tap (or click) a dot to pin its value in the readout
  // line under the chart. Delegated on document so HTMX swaps keep working.
  document.addEventListener("click", function (ev) {
    var dot = ev.target && ev.target.closest ? ev.target.closest(".eq-dot") : null;
    if (!dot) return;
    var readout = document.getElementById("eq-readout");
    if (!readout) return;
    var t = dot.getAttribute("data-t");
    var v = dot.getAttribute("data-v");
    if (t === null || v === null) return;
    var d = new Date(parseInt(t, 10) * 1000);
    var when = isNaN(d.getTime()) ? ("t=" + t) : d.toISOString().slice(5, 16).replace("T", " ") + " UTC";
    var num = parseFloat(v);
    readout.textContent = when + " · " + (isNaN(num) ? v : (num >= 0 ? "+" : "") + num.toFixed(2) + " $");
  });
})();
