//! Regression test for span-close events: `tracing_subscriber::fmt()`
//! configured with `.with_span_events(FmtSpan::CLOSE)` must render a
//! `close` line when an instrumented span exits, the same shape as the
//! subscriber built in `main`.

use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tracing::instrument]
fn probe_span(n: i32) {
    let _ = n;
}

#[test]
fn span_close_events_render() {
    let buf = Buf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_writer(buf.clone())
        .with_ansi(false)
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        probe_span(1);
    });

    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        out.contains("close"),
        "expected a span-close event in output, got: {out}"
    );
}
