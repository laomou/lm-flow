use lmflow::{register_kernel, Graph, Kernel, KernelCtx, Packet, Timestamp};
use std::sync::Once;
use std::time::Duration;

#[derive(Default)]
struct EmitTimestamp;
impl EmitTimestamp {
    fn emit(c: &mut KernelCtx) -> lmflow::Result<()> {
        let ts = Timestamp(c.option_i64("timestamp", 0));
        c.emit(0, Packet::from_i64(42).at(ts))
    }
}
impl Kernel for EmitTimestamp {
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        if c.option_bool("on_close", false) {
            Ok(())
        } else {
            Self::emit(c)
        }
    }
    fn close(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        if c.option_bool("on_close", false) {
            Self::emit(c)
        } else {
            Ok(())
        }
    }
}
#[derive(Default)]
struct Sink;
impl Kernel for Sink {
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        c.counter_add("received", 1);
        Ok(())
    }
}
fn graph(ts: Timestamp, on_close: bool) -> Graph {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register_kernel::<EmitTimestamp>("EmitTimestamp").unwrap();
        register_kernel::<Sink>("TimestampSink").unwrap();
    });
    Graph::from_yaml(&format!(
        "executors: [{{name: host, type: DelegatingExecutor}}]
nodes:
  - {{name: emitter, kernel: EmitTimestamp, executor: host, input_ports: [in], output_ports: [out], options: {{timestamp: {}, on_close: {on_close}}}}}
  - {{kernel: TimestampSink, executor: host, input_ports: [out]}}
input_ports: [in]
output_ports: [out]", ts.0
    )).unwrap()
}
fn send_and_close(g: &Graph) {
    g.start().unwrap();
    g.input("in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(7)))
        .unwrap();
    g.close_all_inputs();
}
#[test]
fn invalid_output_sentinels_fail_at_emitter_without_dispatch() {
    for ts in [
        Timestamp::unstarted(),
        Timestamp::one_over_post_stream(),
        Timestamp::done(),
    ] {
        let g = graph(ts, false);
        let out = g.add_poller("out").unwrap();
        send_and_close(&g);
        let err = g
            .wait_done_timeout(Duration::from_secs(1))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("emitter") && err.contains("timestamp"),
            "{ts:?}: {err}"
        );
        assert_eq!(g.counter_value("received"), 0);
        assert!(out.try_next().is_none());
    }
}
#[test]
fn close_output_cannot_inherit_done() {
    let g = graph(Timestamp::unset(), true);
    let out = g.add_poller("out").unwrap();
    send_and_close(&g);
    let err = g
        .wait_done_timeout(Duration::from_secs(1))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("emitter") && err.contains("timestamp"),
        "{err}"
    );
    assert_eq!(g.counter_value("received"), 0);
    assert!(out.try_next().is_none());
}
#[test]
fn legal_outputs_and_process_inheritance_still_work() {
    for on_close in [false, true] {
        for ts in [
            Timestamp::pre_stream(),
            Timestamp::min(),
            Timestamp(7),
            Timestamp::max(),
            Timestamp::post_stream(),
            Timestamp::unset(),
        ] {
            if on_close && ts == Timestamp::unset() {
                continue;
            }
            let g = graph(ts, on_close);
            let out = g.add_poller("out").unwrap();
            send_and_close(&g);
            g.wait_done_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(g.counter_value("received"), 1);
            let expected = if ts == Timestamp::unset() {
                Timestamp(7)
            } else {
                ts
            };
            assert_eq!(out.try_next().unwrap().timestamp(), expected);
            assert!(out.try_next().is_none());
        }
    }
}
