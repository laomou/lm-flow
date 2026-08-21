//! Single-kernel test harness shared by Rust, C++ and Python hosts.
//!
//! Unlike [`crate::Graph`], this harness does not parse YAML or create an executor.
//! It drives one registered kernel directly through its contract and lifecycle.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::Arc;

use crate::config::GraphConfig;
use crate::context::{Context, Options};
use crate::kernel::{Contract, KernelInstance, PortTable};
use crate::packet::Packet;
use crate::runtime::GraphShared;
use crate::status::{Error, Result};
use crate::timestamp::Timestamp;

pub struct KernelRunner {
    kernel: KernelInstance,
    contract: Contract,
    context: Context,
    opened: bool,
    pending_inputs: Vec<Option<Packet>>,
    outputs: Vec<VecDeque<Packet>>,
}

impl KernelRunner {
    pub fn new(kernel_name: &str, input_ports: usize, output_ports: usize) -> Result<Self> {
        let inputs = Arc::new(PortTable::build(
            &(0..input_ports)
                .map(|i| format!("in{i}"))
                .collect::<Vec<_>>(),
            "kernel runner input_ports",
        )?);
        let outputs = Arc::new(PortTable::build(
            &(0..output_ports)
                .map(|i| format!("out{i}"))
                .collect::<Vec<_>>(),
            "kernel runner output_ports",
        )?);
        let mut contract = Contract::new(inputs.clone(), outputs.clone());
        unsafe {
            KernelInstance::fill_contract(
                kernel_name,
                &mut contract as *mut Contract as *mut c_void,
            )?;
        }
        if let Some(error) = contract.take_error() {
            return Err(Error::InvalidArg(format!(
                "kernel runner contract: {error}"
            )));
        }
        let kernel = KernelInstance::create(kernel_name)?;
        let shared = Arc::new(GraphShared::new(GraphConfig::default()));
        let context = Context::new(
            "kernel_runner".into(),
            kernel_name.into(),
            inputs,
            outputs,
            Arc::new(Options::new(serde_yaml::Value::Mapping(
                serde_yaml::Mapping::new(),
            ))),
            Arc::new(BTreeMap::new()),
            shared,
        );
        Ok(Self {
            kernel,
            contract,
            context,
            opened: false,
            pending_inputs: (0..input_ports).map(|_| None).collect(),
            outputs: (0..output_ports).map(|_| VecDeque::new()).collect(),
        })
    }

    pub fn contract(&self) -> &Contract {
        &self.contract
    }

    pub fn set_options_json(&mut self, json: &str) -> Result<()> {
        if self.opened {
            return Err(Error::State(
                "kernel runner options must be set before open".into(),
            ));
        }
        let value: serde_yaml::Value = serde_json::from_str(json)
            .map_err(|error| Error::InvalidArg(format!("kernel runner options: {error}")))?;
        self.context.options = Arc::new(Options::new(value));
        Ok(())
    }

    pub fn set_side_packet(&mut self, name: &str, packet: Packet) -> Result<()> {
        if self.opened {
            return Err(Error::State(
                "kernel runner side packets must be set before open".into(),
            ));
        }
        Arc::make_mut(&mut self.context.side_packets).insert(name.into(), packet);
        Ok(())
    }

    pub fn open(&mut self) -> Result<()> {
        if self.opened {
            return Ok(());
        }
        for name in &self.contract.required_side_packets {
            if !self.context.side_packets.contains_key(name) {
                return Err(Error::InvalidArg(format!(
                    "kernel runner missing required side packet `{name}`"
                )));
            }
        }
        let status = unsafe { self.kernel.open(&mut self.context as *mut _ as *mut c_void) };
        if status != 0 {
            return Err(self.context.take_error(status));
        }
        self.opened = true;
        Ok(())
    }

    pub fn process(
        &mut self,
        inputs: Vec<Option<Packet>>,
        timestamp: Timestamp,
    ) -> Result<Vec<Vec<Packet>>> {
        self.open()?;
        if inputs.len() != self.context.inputs.len() {
            return Err(Error::InvalidArg(format!(
                "kernel runner expected {} inputs, got {}",
                self.context.inputs.len(),
                inputs.len()
            )));
        }
        self.context.reset();
        for (port, packet) in inputs.iter().enumerate() {
            if let Some(packet) = packet {
                let expected = self.contract.input_types[port];
                if expected != 0 && packet.type_id() != expected {
                    return Err(Error::InvalidArg(format!(
                        "kernel runner input {port} type mismatch: expected {}, got {}",
                        expected,
                        packet.type_id()
                    )));
                }
            }
        }
        self.context.inputs = inputs
            .into_iter()
            .map(|packet| {
                packet.map(|packet| {
                    if packet.timestamp() == Timestamp::unset() {
                        packet.at(timestamp)
                    } else {
                        packet
                    }
                })
            })
            .collect();
        self.context.input_ts = timestamp;
        let status = unsafe {
            self.kernel
                .process(&mut self.context as *mut _ as *mut c_void)
        };
        if status != 0 {
            let error = self.context.take_error(status);
            self.context.discard_staging();
            return Err(error);
        }
        if let Err(error) = self.validate_outputs() {
            self.context.discard_staging();
            return Err(error);
        }
        let staging = std::mem::take(&mut self.context.staging);
        for (port, packets) in staging.iter().enumerate() {
            self.outputs[port].extend(packets.iter().cloned());
        }
        Ok(staging)
    }

    pub fn add_input(&mut self, port: usize, packet: Packet) -> Result<()> {
        let slot = self
            .pending_inputs
            .get_mut(port)
            .ok_or_else(|| Error::InvalidArg(format!("input port index {port} is out of range")))?;
        if slot.is_some() {
            return Err(Error::State(format!(
                "kernel runner input port {port} already has a packet for the next process call"
            )));
        }
        *slot = Some(packet);
        Ok(())
    }

    pub fn process_pending(&mut self, timestamp: Timestamp) -> Result<Vec<Vec<Packet>>> {
        let inputs = std::mem::take(&mut self.pending_inputs);
        let result = self.process(inputs, timestamp);
        self.pending_inputs = (0..self.context.inputs.len()).map(|_| None).collect();
        result
    }

    pub fn try_output(&mut self, port: usize) -> Result<Option<Packet>> {
        self.outputs
            .get_mut(port)
            .ok_or_else(|| Error::InvalidArg(format!("output port index {port} is out of range")))
            .map(VecDeque::pop_front)
    }

    pub fn close(&mut self) -> Result<Vec<Vec<Packet>>> {
        if !self.opened {
            return Ok((0..self.context.staging.len())
                .map(|_| Vec::new())
                .collect());
        }
        self.context.reset();
        self.context.close_reason = crate::runtime::CLOSE_NORMAL;
        let status = unsafe {
            self.kernel
                .close(&mut self.context as *mut _ as *mut c_void)
        };
        self.opened = false;
        if status != 0 {
            let error = self.context.take_error(status);
            self.context.discard_staging();
            return Err(error);
        }
        if let Err(error) = self.validate_outputs() {
            self.context.discard_staging();
            return Err(error);
        }
        let staging = std::mem::take(&mut self.context.staging);
        for (port, packets) in staging.iter().enumerate() {
            self.outputs[port].extend(packets.iter().cloned());
        }
        Ok(staging)
    }

    fn validate_outputs(&self) -> Result<()> {
        for (port, packets) in self.context.staging.iter().enumerate() {
            let expected = self.contract.output_types[port];
            if expected != 0 {
                if let Some(packet) = packets.iter().find(|packet| packet.type_id() != expected) {
                    return Err(Error::Kernel(format!(
                        "kernel runner output {port} type mismatch: expected {}, got {}",
                        expected,
                        packet.type_id()
                    )));
                }
            }
        }
        Ok(())
    }
}

impl Drop for KernelRunner {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
