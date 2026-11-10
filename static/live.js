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

  // Strategy lab (dashboard #lab section): delegated submit + re-render,
  // so the 30s HTMX dashboard refresh never kills it. All cells are built
  // with textContent — model-derived strings never touch innerHTML.
  var labLast = null;

  function labCell(row, text, cls) {
    var td = document.createElement("td");
    td.textContent = text;
    if (cls) td.className = cls;
    row.appendChild(td);
    return td;
  }

  function labVerdict(r) {
    return r.promoted ? "✓ promoted" : "✗ rejected";
  }

  function renderLabReports(list) {
    var body = document.getElementById("lab-results");
    var wrap = document.getElementById("lab-results-wrap");
    if (!body || !wrap) return;
    body.textContent = "";
    list.forEach(function (r) {
      var tr = document.createElement("tr");
      labCell(tr, r.spec.strategy + " " + r.spec.fast + "/" + r.spec.slow);
      labCell(tr, String(r.trades), "num");
      labCell(tr, (r.win_rate * 100).toFixed(1) + " %", "num");
      labCell(tr, (r.total_pnl >= 0 ? "+" : "") + r.total_pnl.toFixed(2) + " $", "num " + (r.total_pnl >= 0 ? "pos" : "neg"));
      labCell(tr, r.max_drawdown.toFixed(2) + " $", "num");
      labCell(tr, labVerdict(r), r.promoted ? "pos" : "neg");
      body.appendChild(tr);
    });
    wrap.hidden = list.length === 0;
  }

  function renderLabJournal(rows) {
    var body = document.getElementById("lab-journal");
    if (!body) return;
    body.textContent = "";
    if (rows.length === 0) {
      var tr = document.createElement("tr");
      labCell(tr, "No evaluations yet — run one above.", "muted").colSpan = 6;
      body.appendChild(tr);
      return;
    }
    rows.forEach(function (j) {
      var tr = document.createElement("tr");
      var d = new Date(j.created_at);
      labCell(tr, isNaN(d.getTime()) ? j.created_at : d.toISOString().slice(5, 16).replace("T", " ") + " UTC");
      labCell(tr, j.spec.strategy + " " + j.spec.fast + "/" + j.spec.slow);
      labCell(tr, String(j.trades), "num");
      labCell(tr, (Number(j.total_pnl) >= 0 ? "+" : "") + Number(j.total_pnl).toFixed(2) + " $", "num " + (Number(j.total_pnl) >= 0 ? "pos" : "neg"));
      labCell(tr, j.promoted ? "✓ promoted" : "✗ rejected", j.promoted ? "pos" : "neg");
      labCell(tr, j.gate_reason, "muted");
      body.appendChild(tr);
    });
  }

  function labStatus(text) {
    var el = document.getElementById("lab-status");
    if (el) el.textContent = text;
  }

  function loadLabJournal() {
    if (!document.getElementById("lab-journal")) return;
    fetch("/api/lab/journal", { credentials: "same-origin" }).then(function (resp) {
      if (!resp.ok) throw new Error("journal " + resp.status);
      return resp.json();
    }).then(function (rows) {
      renderLabJournal(rows);
    }).catch(function () {
      labStatus("Journal unreachable — retry in a moment.");
    });
  }

  document.addEventListener("submit", function (ev) {
    var form = ev.target;
    if (!form || form.id !== "lab-form") return;
    ev.preventDefault();
    var btn = document.getElementById("lab-submit");
    var product = document.getElementById("lab-product");
    var fast = document.getElementById("lab-fast");
    var slow = document.getElementById("lab-slow");
    var propose = document.getElementById("lab-propose");
    if (!product || !fast || !slow) return;
    var url = "/api/lab/evaluate?product=" + encodeURIComponent(product.value) +
      (propose && propose.checked ? "&propose=1" : "");
    if (btn) btn.disabled = true;
    labStatus("Evaluating on daily closes…");
    fetch(url, {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify([{ strategy: "sma-cross", fast: Number(fast.value), slow: Number(slow.value) }]),
    }).then(function (resp) {
      if (resp.status === 401 || resp.status === 403) {
        window.location.href = "/login";
        throw new Error("auth");
      }
      if (!resp.ok) throw new Error("evaluate " + resp.status);
      return resp.json();
    }).then(function (reports) {
      labLast = reports;
      renderLabReports(reports);
      loadLabJournal();
      labStatus(reports.length + " spec(s) evaluated.");
    }).catch(function (err) {
      if (err && err.message !== "auth") labStatus("Evaluation failed — retry in a moment.");
    }).finally(function () {
      if (btn) btn.disabled = false;
    });
  });

  document.addEventListener("htmx:afterSwap", function () {
    if (document.getElementById("lab-journal")) {
      loadLabJournal();
      if (labLast) renderLabReports(labLast);
    }
  });

  loadLabJournal();
})();
