use log::LevelFilter;
use serialport::{available_ports, SerialPort};
use std::convert::TryInto;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Duration;
use std::{error::Error, fs};

#[macro_use]
mod macros;
mod elf;
mod init_packet;
mod messages;
mod slip;

use messages::*;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const USB_VID: u16 = 0x1915;
const USB_PID: u16 = 0x521f;

/// Bootloader protocol version we support.
const PROTOCOL_VERSION: u8 = 1;

fn main() {
    match run() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    }
}

fn run() -> Result<()> {
    // We show info and higer levels by default, but allow overriding this via `RUST_LOG`.
    env_logger::builder()
        .filter_level(LevelFilter::Info)
        .parse_default_env()
        .init();

    let elf_path = std::env::args_os().skip(1).next();
    let image = match elf_path {
        Some(path) => {
            let elf = fs::read(&path)?;
            Some(elf::read_elf_image(&elf)?)
        }
        None => None,
    };

    let matching_ports: Vec<_> = available_ports()?
        .into_iter()
        .filter(|port| match &port.port_type {
            serialport::SerialPortType::UsbPort(usb) => usb.vid == USB_VID && usb.pid == USB_PID,
            _ => false,
        })
        .collect();

    let port = match matching_ports.len() {
        0 => return Err(format!("no matching USB serial device found").into()),
        1 => serialport::new(&matching_ports[0].port_name, 115200)
            .timeout(Duration::from_millis(1000))
            .open()?,
        _ => return Err(format!("multiple matching USB serial devices found").into()),
    };

    let mut conn = BootloaderConnection::new(port)?;

    // Disable receipt notification. USB is a reliable transport.
    conn.set_receipt_notification(0)?;

    let obj_select = conn.select_object_command();
    println!("select object response: {:?}", obj_select);

    let version = conn.fetch_protocol_version()?;
    println!("protocol version: {}", version);

    let hw_version = conn.fetch_hardware_version()?;
    println!("hardware version: {:?}", hw_version);

    let image = image.unwrap_or_else(|| vec![1, 2, 3]);

    let init_packet = std::fs::read("loopback.dat").expect("couldn't read 'loopback.dat'");
    //let init_packet = init_packet::build_init_packet(&image);
    conn.send_init_packet(&init_packet)?;

    let test_image = Path::new("loopback.bin");
    let mut bin_file = File::open(test_image).expect("firmware file not found");
    let mut data = Vec::new();
    bin_file.read_to_end(&mut data)?;
    conn.send_firmware(&data)?;

    Ok(())
}

struct BootloaderConnection {
    serial: Box<dyn SerialPort>,
    buf: Vec<u8>,
    mtu: u16,
}

impl BootloaderConnection {
    fn new(serial: Box<dyn SerialPort>) -> Result<Self> {
        let mut this = Self {
            serial,
            buf: Vec::new(),
            mtu: 0,
        };

        // We must check the protocol version before doing anything else, since any other command
        // might change if the version changes.
        let proto_version = this.fetch_protocol_version()?;
        if proto_version != PROTOCOL_VERSION {
            return Err(format!(
                "device reports protocol version {}, we only support {}",
                proto_version, PROTOCOL_VERSION
            )
            .into());
        }

        let mtu = this.fetch_mtu()?;
        println!("MTU = {} Bytes", mtu);
        this.mtu = mtu;
        Ok(this)
    }

    /// send `req` and do not fetch any response
    fn request<R: Request>(&mut self, req: R) -> Result<()> {
        let mut buf = vec![R::OPCODE as u8];
        req.write_payload(&mut buf)?;
        eprintln!("--> {:?}", buf);

        // Go through an intermediate buffer to avoid writing every byte individually.
        self.buf.clear();
        slip::encode_frame(&buf, &mut self.buf)?;
        self.serial.write_all(&self.buf)?;

        Ok(())
    }

    /// send `req` and expect a response.
    /// aborts if no response is received within timeout window.
    fn request_response<R: Request>(&mut self, req: R) -> Result<R::Response> {
        self.request(req)?;

        self.buf.clear();
        slip::decode_frame(&mut self.serial, &mut self.buf)?;
        eprintln!("<-- {:?}", self.buf);

        messages::parse_response::<R>(&self.buf)
    }

    fn fetch_protocol_version(&mut self) -> Result<u8> {
        let response = self.request_response(ProtocolVersionRequest);
        match response {
            Ok(version_response) => Ok(version_response.version),
            Err(e) => Err(e),
        }
    }

    fn fetch_hardware_version(&mut self) -> Result<HardwareVersionResponse> {
        self.request_response(HardwareVersionRequest)
    }

    /// Sends the `.dat` file that's zipped into our firmware DFU .zip(?)
    /// modeled after `pc-nrfutil`s `dfu_transport_serial::send_init_packet()`
    fn send_init_packet(&mut self, data: &[u8]) -> Result<()> {
        println!("Sending init packet...");
        let select_response = self.select_object_command()?;
        println!("Object selected: {:?}", select_response);

        let data_size = data.len() as u32;

        println!("Creating Command...");
        self.create_command_object(data_size)?;
        println!("Command created");

        println!("Streaming Data: len: {}", data_size);
        self.write_object_data(data)?;
        // TODO: calculate crc and check against what we received
        let _target_crc = self.get_crc()?;

        self.execute()?;

        Ok(())
    }

    /// Sends the firmware image at `bin_path`.
    /// This is done in chunks to avoid exceeding our MTU  and involves periodic CRC checks.
    fn send_firmware(&mut self, image: &[u8]) -> Result<()> {
        println!("Sending firmware image...");

        println!("Selecting Object: type Data");
        let select_response = self.select_object_data()?;
        println!("Object selected: {:?}", select_response);

        let max_size = select_response.max_size;

        // On the wire, the write request contains the opcode byte, and is then SLIP-encoded,
        // potentially doubling the size, so the chunk size has to be smaller than the MTU.
        let max_chunk_size = usize::from(self.mtu / 2 - 1);

        for chunk in image.chunks( max_size.try_into().unwrap()){
            let curr_chunk_sz: u32 = chunk.len().try_into().unwrap();
            self.create_data_object(curr_chunk_sz)?;
            println!("Streaming Data: len: {}", curr_chunk_sz);

            for slippable_chunk in chunk.chunks(max_chunk_size) {
                self.write_object_data(slippable_chunk)?;
            }

            // TODO: calculate crc and check against what we received
            let _target_crc = self.get_crc()?;

            self.execute()?;
        }

        Ok(())
    }

    /// Sends a
    /// Request Type: `Select`
    /// Parameters:   `Object type = Command`
    fn select_object_command(&mut self) -> Result<SelectResponse> {
        self.request_response(SelectRequest(ObjectType::Command))
    }

    /// Sends a
    /// Request Type: `Select`
    /// Parameters:   `Object type = Data`
    fn select_object_data(&mut self) -> Result<SelectResponse> {
        self.request_response(SelectRequest(ObjectType::Data))
    }

    /// Sends a
    /// Request Type: `Create`
    /// Parameters:   `Object type = Command`
    ///               `size`
    fn create_command_object(&mut self, size: u32) -> Result<()> {
        self.request_response(CreateObjectRequest {
            obj_type: ObjectType::Command,
            size,
        })?;
        Ok(())
    }

    /// Sends a
    /// Request Type: `Create`
    /// Parameters:   `Object type = Data`
    ///               `size`
    fn create_data_object(&mut self, size: u32) -> Result<()> {
        // Note: Data objects cannot be created if no init packet has been sent. This results in an
        // `OperationNotPermitted` error.
        self.request_response(CreateObjectRequest {
            obj_type: ObjectType::Data,
            size,
        })?;
        Ok(())
    }

    fn set_receipt_notification(&mut self, every_n_packets: u16) -> Result<()> {
        self.request_response(SetPrnRequest(every_n_packets))?;
        Ok(())
    }

    fn fetch_mtu(&mut self) -> Result<u16> {
        Ok(self.request_response(GetMtuRequest)?.0)
    }

    fn write_object_data(&mut self, data: &[u8]) -> Result<()> {
        // On the wire, the write request contains the opcode byte, and is then SLIP-encoded,
        // potentially doubling the size, so the chunk size has to be smaller than the MTU.
        let max_chunk_size = usize::from(self.mtu / 2 - 1);

        assert!(data.len() <= max_chunk_size, "trying to write object that's larger than the MTU");

        // TODO: this also needs to take into account the receipt response. In our case we turn
        // it off, so there's nothing to do here.
        self.request(WriteRequest {
            request_payload: data,
        })?;

        Ok(())
    }

    fn get_crc(&mut self) -> Result<CrcResponse> {
        self.request_response(CrcRequest)
    }

    // tell the target to execute whatever request setup we sent them before
    fn execute(&mut self) -> Result<ExecuteResponse> {
        self.request_response(ExecuteRequest)
    }
}
