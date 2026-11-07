// warden live ticks: one EventSource for the whole app (guarded, so HTMX
// swaps never open a second stream). Updates price cells + portfolio cards
// in place with a brief directional flash. If the stream drops, the 30s
// HTMX polling keeps pages fresh — this script stays silent.
(function () {
  if (window.__wardenLive) return;
  window.__wardenLive = true;

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

  var es;
  try {
    es = new EventSource("/api/live/stream");
  } catch (e) {
    return; // no EventSource support: HTMX polling covers freshness
  }

  es.onmessage = function (ev) {
    var msg;
    try {
      msg = JSON.parse(ev.data);
    } catch (e) {
      return;
    }
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
})();
