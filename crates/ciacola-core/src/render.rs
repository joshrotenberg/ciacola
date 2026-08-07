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
        "ok" | "idle" | "done" => "#3fb950",
        "running" => "#58a6ff",
        "queued" | "doing" | "skipped" => "#d29922",
        "failed" => "#f85149",
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
\
         <title>{}</title><style>\
         body{{background:#0d1117;color:#e6edf3;font:14px/1.5 -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;margin:0;padding:2rem;max-width:70rem}}\
         a{{color:#58a6ff;text-decoration:none}} a:hover{{text-decoration:underline}}\
         h1{{font-size:1.2rem;margin:0 0 1.5rem}} h2{{font-size:0.85rem;text-transform:uppercase;letter-spacing:0.08em;color:#8b949e;margin:2rem 0 0.5rem}}\
         table{{border-collapse:collapse;width:100%}}\
         th{{text-align:left;font-size:0.75rem;text-transform:uppercase;letter-spacing:0.05em;color:#8b949e;font-weight:500;padding:0.4rem 0.75rem;border-bottom:1px solid #21262d}}\
         td{{padding:0.5rem 0.75rem;border-bottom:1px solid #21262d;vertical-align:top}}\
         .chip{{border:1px solid;border-radius:1rem;padding:0.05rem 0.6rem;font-size:0.75rem;white-space:nowrap}}\
         .num{{text-align:right;font-variant-numeric:tabular-nums}}\
         .dim{{color:#8b949e}} .mono{{font-family:ui-monospace,monospace;font-size:0.85em}}\
         .stat{{display:inline-block;margin-right:2.5rem}} .stat b{{display:block;font-size:1.4rem;font-weight:600}} .stat span{{color:#8b949e;font-size:0.8rem}}\
         .msg{{margin:0.25rem 0 0.75rem;padding:0.6rem 0.9rem;border-left:3px solid #21262d;white-space:pre-wrap}}\
         .msg.them{{border-color:#58a6ff}} .msg.it{{border-color:#3fb950}} .msg.err{{border-color:#f85149}}\
         .kanban{{display:flex;gap:1rem;align-items:flex-start}}\
         .lane{{flex:1;min-width:0;background:#161b22;border:1px solid #21262d;border-radius:6px;padding:0.6rem}}\
         .lane h3{{font-size:0.72rem;text-transform:uppercase;letter-spacing:0.08em;color:#8b949e;margin:0 0 0.5rem;font-weight:600}}\
         .card{{background:#0d1117;border:1px solid #21262d;border-radius:6px;padding:0.5rem 0.6rem;margin-bottom:0.5rem}}\
         .card b{{font-weight:600;font-size:0.85rem}}\
         .card .dim{{font-size:0.78rem;display:block;margin-top:0.15rem}}\
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
  let es;
  const connect = () => {
    es = new EventSource('/board/events');
    es.addEventListener('board', async () => {
      try {
        const html = await (await fetch('/board/fragment')).text();
        const el = main();
        if (el && el.innerHTML !== html) el.innerHTML = html;
        document.body.dataset.live = 'yes';
      } catch (_) { /* a failed fetch is a dropped frame, not an error */ }
    });
    // EventSource retries by itself, but not after the server restarts
    // mid-stream, which during development is most of the time.
    es.onerror = () => { document.body.dataset.live = 'no'; es.close(); setTimeout(connect, 2000); };
  };
  connect();
})();
</script>"#;
