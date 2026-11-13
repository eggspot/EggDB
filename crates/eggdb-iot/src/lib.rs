//! EggDB's industrial ingest (D1072): Sparkplug B messages taken from any MQTT broker (EggDB's
//! own included) and stored per metric with names resolved from birth certificates, and Modbus
//! TCP devices polled at an interval with every bit and register stored. Sparkplug is a
//! trademark of the Eclipse Foundation and Modbus of Schneider Electric; EggDB is compatible with
//! their protocols.

mod modbus;
mod mqtt;
mod sparkplug;
mod sql;

pub use modbus::{Device, ModbusPoller, Table, request as modbus_request, values as modbus_values};
pub use mqtt::{Broker, Delivered, Subscriber};
pub use sparkplug::{SparkplugCollector, SparkplugSource, Topic, statements as sparkplug_statements};

#[cfg(test)]
mod tests;
