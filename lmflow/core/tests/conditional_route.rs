use lmflow::{Graph, Packet, Timestamp};

fn route_graph(mode: &str) -> Graph {
    Graph::from_yaml(&format!(
        r#"
nodes:
  - name: route
    type: route
    input_ports: [input]
    output_ports: [high, tagged, other]
    mode: {mode}
    unmatched: other
    routes:
      - to: high
        when: {{ metadata: confidence, op: gte, value: 0.8 }}
      - to: tagged
        when:
          all:
            - {{ metadata: category, op: contains, value: person }}
            - not:
                timestamp: {{ op: lt, value: 10 }}
input_ports: [input]
output_ports: [high, tagged, other]
"#
    ))
    .unwrap()
}

#[test]
fn first_routes_to_first_matching_output_without_copying_payload() {
    let graph = route_graph("first");
    let high = graph.add_poller("high").unwrap();
    let tagged = graph.add_poller("tagged").unwrap();
    graph.start().unwrap();

    let packet = Packet::from_i64(7)
        .at(Timestamp(12))
        .with_metadata("confidence", 0.9)
        .with_metadata("category", "person/vehicle");
    graph.input("input").unwrap().send(packet).unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();

    let output = high.next().unwrap();
    assert_eq!(output.as_i64(), Some(7));
    assert_eq!(output.metadata_value("confidence"), Some(&0.9.into()));
    assert!(tagged.try_next().is_none());
}

#[test]
fn all_routes_to_every_match_and_preserves_shared_payload() {
    let graph = route_graph("all");
    let high = graph.add_poller("high").unwrap();
    let tagged = graph.add_poller("tagged").unwrap();
    graph.start().unwrap();

    graph
        .input("input")
        .unwrap()
        .send(
            Packet::from_i64(9)
                .at(Timestamp(12))
                .with_metadata("confidence", 0.9)
                .with_metadata("category", "person"),
        )
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();

    let first = high.next().unwrap();
    let second = tagged.next().unwrap();
    assert_eq!(first.as_i64(), Some(9));
    assert_eq!(second.as_i64(), Some(9));
    assert!(std::ptr::eq(
        first.payload().unwrap(),
        second.payload().unwrap()
    ));
}

#[test]
fn unmatched_port_receives_packet() {
    let graph = route_graph("first");
    let other = graph.add_poller("other").unwrap();
    graph.start().unwrap();
    graph
        .input("input")
        .unwrap()
        .send(Packet::from_i64(3).at(Timestamp(1)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(other.next().and_then(|packet| packet.as_i64()), Some(3));
}
