mod common;
use lmflow::{Error, Packet, Timestamp};
#[test]
fn invalid_sentinels_do_not_poison_monotonicity_or_queues() {
    let g = common::graph_from_yaml("nodes: [{kernel: PassThrough, input_ports: [in], output_ports: [out]}]\ninput_ports: [in]\noutput_ports: [out]").unwrap();
    let out = g.add_poller("out").unwrap();
    g.start().unwrap();
    let input = g.input("in").unwrap();
    for ts in [
        Timestamp::unset(),
        Timestamp::unstarted(),
        Timestamp::one_over_post_stream(),
        Timestamp::done(),
    ] {
        assert!(matches!(
            input.send(Packet::from_i64(42).at(ts)),
            Err(Error::InvalidArg(_))
        ));
        assert!(matches!(
            input.try_send(Packet::from_i64(42).at(ts)),
            Err(Error::InvalidArg(_))
        ));
    }
    input.send(Packet::from_i64(7).at(Timestamp(0))).unwrap();
    input.close();
    g.wait_done().unwrap();
    assert_eq!(
        out.next_result().unwrap().unwrap().timestamp(),
        Timestamp(0)
    );
    assert!(out.next_result().unwrap().is_none());
}
#[test]
fn legal_boundary_timestamps_remain_accepted() {
    for ts in [
        Timestamp::pre_stream(),
        Timestamp::min(),
        Timestamp::max(),
        Timestamp::post_stream(),
    ] {
        let g = common::graph_from_yaml("nodes: [{kernel: PassThrough, input_ports: [in], output_ports: [out]}]\ninput_ports: [in]\noutput_ports: [out]").unwrap();
        let out = g.add_poller("out").unwrap();
        g.start().unwrap();
        g.input("in")
            .unwrap()
            .send(Packet::from_i64(42).at(ts))
            .unwrap();
        g.close_all_inputs();
        g.wait_done().unwrap();
        assert_eq!(out.next_result().unwrap().unwrap().timestamp(), ts);
        assert!(out.next_result().unwrap().is_none());
    }
}
