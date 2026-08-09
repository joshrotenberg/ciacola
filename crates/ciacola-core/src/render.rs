//! The three formatting helpers a plugin needs to contribute a board
//! section, kept in core so the board itself can be optional.
//!
//! It also owns the page shell, which is less obvious. A plugin can
//! own a whole board page (the kanban's per-item journey is one), and
//! making it depend on the board crate for the surrounding HTML would
//! undo the board being optional. So core owns what a page *looks
//! like*, and the board owns what is *on* the overview.
//!
//! Deliberately tiny. A plugin renders its own fragment because it
//! knows its own data; it should not have to reimplement escaping or
//! agree independently on what a state colour means.

use axum::response::Html;

/// HTML-escape. Every value a plugin interpolates goes through this.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Micro-USD as money. Four places, because a haiku turn costs less
/// than a cent and rounding it to zero was how cost got missed.
pub fn usd(micro: i64) -> String {
    format!("${:.4}", micro as f64 / 1e6)
}

/// A state pill. One place decides that running is blue and failed is
/// red, so eight plugins cannot disagree about it.
pub fn chip(state: &str) -> String {
    let color = match state {
        "ok" | "idle" | "done" | "active" | "completed" => "#3fb950",
        "running" | "preparing" | "publishing" => "#58a6ff",
        "queued" | "doing" | "skipped" | "retained" | "finishing" => "#d29922",
        "failed" | "stale" => "#f85149",
        "killed" | "dropped" => "#8b949e",
        _ => "#8b949e",
    };
    format!(
        "<span class=\"chip\" style=\"border-color:{color};color:{color}\">{}</span>",
        esc(state)
    )
}

/// "4m ago", not a timestamp: a reader asking whether something is
/// stuck should not have to do arithmetic. Zero means never.
pub fn ago(unix: i64) -> String {
    if unix == 0 {
        return "never".into();
    }
    let secs = (crate::time::now_unix() - unix).max(0);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// 595.7k rather than 595713: the header is read at a glance.
pub fn human_count(n: i64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// Wrap a body in the page shell. `live` opts into the SSE refresh,
/// which only the overview needs: a detail page is a snapshot of one
/// finished thing and reloading it under the reader is unhelpful.
pub fn page_with(title: &str, body: &str, live: bool) -> Html<String> {
    Html(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
\
         <title>{}</title><style>\
         :root{{--bg:#0d1117;--panel:#161b22;--line:#30363d;--muted:#8b949e;--text:#e6edf3;--blue:#58a6ff;--green:#3fb950;--amber:#d29922;--red:#f85149}}\
         *{{box-sizing:border-box}}\
         body{{background:var(--bg);color:var(--text);font:14px/1.5 -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;margin:0;padding:2rem}}\
         main{{width:min(100%,88rem);margin:0 auto}}\
         a{{color:#58a6ff;text-decoration:none}} a:hover{{text-decoration:underline}}\
         h1{{font-size:1.65rem;line-height:1.2;margin:0}} h2{{font-size:0.85rem;text-transform:uppercase;letter-spacing:0.08em;color:var(--muted);margin:2rem 0 0.5rem}}\
         h3{{font-size:0.85rem;margin:1.25rem 0 0.5rem}} p{{margin:0.35rem 0 0.8rem}}\
         .board-header{{display:flex;align-items:flex-start;justify-content:space-between;gap:1.5rem;margin-bottom:1.25rem}}\
         .eyebrow{{color:var(--muted);font-size:0.72rem;font-weight:600;letter-spacing:0.12em;margin:0 0 0.25rem;text-transform:uppercase}}\
         .live-status{{align-items:center;border:1px solid var(--line);border-radius:999px;color:var(--muted);display:inline-flex;font-size:0.75rem;gap:0.45rem;padding:0.3rem 0.65rem;white-space:nowrap}}\
         .live-dot{{background:var(--amber);border-radius:50%;height:0.5rem;width:0.5rem}}\
         body[data-live='yes'] .live-dot{{background:var(--green)}} body[data-live='no'] .live-dot{{background:var(--red)}}\
         .stat-grid{{display:grid;gap:0.7rem;grid-template-columns:repeat(auto-fit,minmax(8.5rem,1fr));margin:0 0 1rem}}\
         .stat{{background:var(--panel);border:1px solid var(--line);border-radius:8px;display:block;margin:0;padding:0.75rem 0.85rem;min-width:0}}\
         .stat b{{display:block;font-size:1.3rem;font-weight:600}} .stat span{{color:var(--muted);font-size:0.75rem}}\
         .panel{{background:var(--panel);border:1px solid var(--line);border-radius:8px;margin:0 0 1rem;padding:1rem}} .panel h2{{margin:0 0 0.75rem}}\
         .panel.attention{{border-color:#6e2a2a}} .panel.active{{border-color:#244e78}}\
         .panel-heading{{align-items:baseline;display:flex;gap:0.65rem;justify-content:space-between}}\
         .panel-heading .count{{color:var(--muted);font-size:0.75rem}}\
         .section-body{{max-width:100%;overflow-x:auto}}\
         .empty{{color:var(--muted);margin:0}}\
         .table-wrap{{max-width:100%;overflow-x:auto}}\
         table{{border-collapse:collapse;width:100%}}\
         th{{text-align:left;font-size:0.75rem;text-transform:uppercase;letter-spacing:0.05em;color:#8b949e;font-weight:500;padding:0.4rem 0.75rem;border-bottom:1px solid #21262d}}\
         td{{padding:0.5rem 0.75rem;border-bottom:1px solid #21262d;vertical-align:top}}\
         tr:last-child td{{border-bottom:0}}\
         .chip{{border:1px solid;border-radius:1rem;padding:0.05rem 0.6rem;font-size:0.75rem;white-space:nowrap}}\
         .num{{text-align:right;font-variant-numeric:tabular-nums}}\
         .dim{{color:#8b949e}} .mono{{font-family:ui-monospace,monospace;font-size:0.85em}}\
         .msg{{margin:0.25rem 0 0.75rem;padding:0.6rem 0.9rem;border-left:3px solid #21262d;white-space:pre-wrap}}\
         .msg.them{{border-color:#58a6ff}} .msg.it{{border-color:#3fb950}} .msg.err{{border-color:#f85149}}\
         details{{border-top:1px solid var(--line);margin-top:0.9rem;padding-top:0.8rem}} summary{{color:var(--muted);cursor:pointer;font-weight:600}}\
         .sr-only{{clip:rect(0,0,0,0);clip-path:inset(50%);height:1px;overflow:hidden;position:absolute;white-space:nowrap;width:1px}}\
         .kanban{{display:flex;gap:1rem;align-items:flex-start}}\
         .lane{{flex:1;min-width:0;background:#161b22;border:1px solid #21262d;border-radius:6px;padding:0.6rem}}\
         .lane h3{{font-size:0.72rem;text-transform:uppercase;letter-spacing:0.08em;color:#8b949e;margin:0 0 0.5rem;font-weight:600}}\
         .card{{background:#0d1117;border:1px solid #21262d;border-radius:6px;padding:0.5rem 0.6rem;margin-bottom:0.5rem}}\
         .card b{{font-weight:600;font-size:0.85rem}}\
         .card .dim{{font-size:0.78rem;display:block;margin-top:0.15rem}}\
         @media(max-width:720px){{body{{padding:1rem}}.board-header{{align-items:flex-start}}.stat-grid{{grid-template-columns:repeat(2,minmax(0,1fr))}}.panel{{padding:0.8rem}}th,td{{padding:0.45rem 0.55rem}}.kanban{{overflow-x:auto}}\
         .responsive-table,.responsive-table tbody,.responsive-table tr,.responsive-table td{{display:block;width:100%}}\
         .responsive-table tr:first-child{{clip:rect(0,0,0,0);clip-path:inset(50%);height:1px;overflow:hidden;position:absolute;white-space:nowrap;width:1px}}\
         .responsive-table tr:not(:first-child){{border-bottom:1px solid var(--line);padding:0.35rem 0}}\
         .responsive-table tr:last-child{{border-bottom:0}}\
         .responsive-table td{{border:0;min-height:1.8rem;padding:0.3rem 0 0.3rem 7.25rem;position:relative}}\
         .responsive-table td::before{{color:var(--muted);content:attr(data-label);font-size:0.68rem;font-weight:600;left:0;letter-spacing:0.05em;position:absolute;text-transform:uppercase;top:0.4rem;width:6.75rem}}}}\
         </style>{}</head><body><main>{}</main></body></html>",
        esc(title),
        if live { LIVE_SCRIPT } else { "" },
        body
    ))
}

/// Reconnects on its own, swaps one fragment, and leaves scroll alone.
const LIVE_SCRIPT: &str = r#"<script>
(() => {
  const main = () => document.querySelector('main');
  const setLive = (value, label) => {
    if (document.body) document.body.dataset.live = value;
    const el = document.querySelector('[data-live-label]');
    if (el) el.textContent = label;
  };
  let es;
  const connect = () => {
    setLive('connecting', 'connecting');
    es = new EventSource('/board/events');
    es.addEventListener('board', async () => {
      try {
        const html = await (await fetch('/board/fragment')).text();
        const el = main();
        if (el && el.innerHTML !== html) el.innerHTML = html;
        setLive('yes', 'live');
      } catch (_) { /* a failed fetch is a dropped frame, not an error */ }
    });
    // EventSource retries by itself, but not after the server restarts
    // mid-stream, which during development is most of the time.
    es.onerror = () => { setLive('no', 'reconnecting'); es.close(); setTimeout(connect, 2000); };
  };
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', connect, { once: true });
  } else {
    connect();
  }
})();
</script>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_page_waits_for_the_body_and_declares_a_mobile_viewport() {
        let html = page_with("board", "<h1>board</h1>", true).0;

        assert!(html.contains("name=\"viewport\" content=\"width=device-width,initial-scale=1\""));
        assert!(html.contains("DOMContentLoaded"));
        assert!(html.contains("data-live-label"));
        assert!(html.contains(".responsive-table td::before"));
    }

    #[test]
    fn state_chips_cover_repository_journey_states() {
        assert!(chip("active").contains("#3fb950"));
        assert!(chip("retained").contains("#d29922"));
        assert!(chip("stale").contains("#f85149"));
        assert!(chip("completed").contains("#3fb950"));
    }
}
