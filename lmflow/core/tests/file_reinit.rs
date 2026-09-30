mod common;
use lmflow::ffi::*;
use std::ffi::{CStr, CString};

#[test]
fn file_init_rejects_existing_graph_and_preserves_handles() {
    common::register_test_kernels();
    let yaml = CString::new("nodes: [{kernel: PassThrough, input_ports: [in], output_ports: [out]}]\ninput_ports: [in]\noutput_ports: [out]").unwrap();
    let path = std::env::temp_dir().join(format!("lmflow-reinit-{}.yaml", std::process::id()));
    std::fs::write(&path, yaml.as_bytes()).unwrap();
    let file = CString::new(path.to_str().unwrap()).unwrap();
    let input_name = CString::new("in").unwrap();
    let output_name = CString::new("out").unwrap();
    unsafe {
        let graph = lmflow_graph_new();
        // The initial file load still works.
        assert_eq!(lmflow_graph_init_from_yaml_file(graph, file.as_ptr()), 0);
        let input = lmflow_graph_input(graph, input_name.as_ptr());
        let poller = lmflow_graph_add_poller(graph, output_name.as_ptr());
        assert!(!input.is_null() && !poller.is_null());
        // Both Initialized and Running must reject replacement before reading the path.
        for running in [false, true] {
            if running {
                assert_eq!(lmflow_graph_start(graph), 0);
            }
            let expected = lmflow_graph_init_from_yaml(graph, yaml.as_ptr());
            assert_ne!(expected, 0);
            assert_eq!(
                lmflow_graph_init_from_yaml_file(graph, file.as_ptr()),
                expected
            );
            assert_eq!(
                lmflow_graph_init_from_yaml_file(graph, std::ptr::null()),
                expected
            );
            assert!(CStr::from_ptr(lmflow_last_error())
                .to_str()
                .unwrap()
                .contains("already initialized"));
        }
        assert_eq!(lmflow_input_send(input, lmflow_packet_from_i64(42, 0)), 0);
        lmflow_input_close(input);
        let mut output = LMFlowPacket::default();
        assert_eq!(lmflow_poller_next_timeout(poller, &mut output, 1000), 0);
        let mut value = 0;
        assert!(lmflow_packet_as_i64(&output, &mut value));
        assert_eq!(value, 42);
        lmflow_packet_drop(&mut output);
        assert_eq!(lmflow_graph_wait_done(graph), 0);
        lmflow_poller_free(poller);
        lmflow_input_free(input);
        lmflow_graph_free(graph);
    }
    std::fs::remove_file(path).unwrap();
}
