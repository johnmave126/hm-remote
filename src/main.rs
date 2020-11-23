use btleplug::api::{BDAddr, Central, CentralEvent, Peripheral, ValueNotification, UUID};
#[cfg(target_os = "linux")]
use btleplug::bluez::{adapter::ConnectedAdapter, manager::Manager};
#[cfg(target_os = "macos")]
use btleplug::corebluetooth::{adapter::Adapter, manager::Manager};
#[cfg(target_os = "windows")]
use btleplug::winrtble::{adapter::Adapter, manager::Manager};

use clap::{App, AppSettings, Arg, SubCommand};

use std::{
    collections::HashMap,
    io::{stdout, Write},
    str::FromStr,
    sync::{atomic::AtomicBool, Arc},
    thread,
    time::Duration,
};

use crossbeam_channel::{self as c_channel, unbounded};

use thiserror::Error;

use crossterm::{
    queue,
    style::{Colorize, Print, PrintStyledContent},
};

use dialoguer::theme::CustomPromptCharacterTheme;

// 0000ffe1-0000-1000-8000-00805f9b34fb
const UUID_NOTIFY: UUID = UUID::B128([
    0xfb, 0x34, 0x9b, 0x5f, 0x80, 0x00, 0x00, 0x80, 0x00, 0x10, 0x00, 0x00, 0xe1, 0xff, 0x00, 0x00,
]);

#[derive(Error, Debug)]
enum Error {
    #[error("Bluetooth error: {0}")]
    Bluetooth(btleplug::Error),
    #[error("Signal handler registeration error: {0}")]
    CtrlC(#[from] ctrlc::Error),
    #[error("Terminal I/O error: {0}")]
    Crossterm(#[from] crossterm::ErrorKind),
    #[error("Cannot find bluetooth adapter")]
    NoAdapter,
    #[error("{0}")]
    InvalidAddr(btleplug::api::ParseBDAddrError),
    #[error("Bluetooth adapter unexpectedly stopped")]
    AdapterStopped,
    #[error("IO error")]
    IOError(#[from] std::io::Error),
    #[error("Device is not a HM device")]
    NotHMDevice,
    #[error("Unknown error")]
    Unknown,
}

impl From<btleplug::Error> for Error {
    fn from(err: btleplug::Error) -> Error {
        Error::Bluetooth(err)
    }
}

impl From<btleplug::api::ParseBDAddrError> for Error {
    fn from(err: btleplug::api::ParseBDAddrError) -> Error {
        Error::InvalidAddr(err)
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn get_central(manager: &Manager) -> Result<Adapter, Error> {
    let adapters = manager.adapters()?;
    adapters
        .into_iter()
        .nth(0)
        .map_or(Err(Error::NoAdapter), |adapter| Ok(adapter))
}

#[cfg(target_os = "linux")]
fn get_central(manager: &Manager) -> Result<ConnectedAdapter, Error> {
    let adapters = manager.adapters()?;
    let adapter = adapters
        .into_iter()
        .nth(0)
        .map_or(Err(Error::NoAdapter), Result::Ok)?;
    Ok(adapter.connect()?)
}

fn addr_to_string<P: Peripheral, C: Central<P>>(central: &C, addr: BDAddr) -> String {
    let device = central.peripheral(addr).unwrap();
    let properties = device.properties();
    format!(
        "{} {}",
        properties.address,
        properties.local_name.unwrap_or("<Unnamed>".to_string())
    )
}

#[derive(Debug, PartialEq, Eq)]
enum DeviceStatus {
    Discovered,
    Updated,
}

fn run_scan(verbose: bool, filter_unnamed: bool) -> Result<(), Error> {
    let manager = Manager::new()?;
    let central = get_central(&manager)?;
    let mut stdout = stdout();

    let terminated = Arc::new(AtomicBool::new(false));
    {
        let terminated = terminated.clone();
        ctrlc::set_handler(move || {
            terminated.store(true, std::sync::atomic::Ordering::SeqCst);
        })?;
    }

    let mut device_status = HashMap::new();

    let receiver = central.event_receiver().unwrap();
    central.start_scan()?;
    while !terminated.load(std::sync::atomic::Ordering::SeqCst) {
        match receiver.recv().or(Err(Error::AdapterStopped))? {
            CentralEvent::DeviceDiscovered(addr) => {
                device_status.insert(addr, DeviceStatus::Discovered);
                if verbose {
                    queue!(
                        stdout,
                        PrintStyledContent("[ADVERTISED] ".blue()),
                        Print(addr.to_string()),
                        Print("\n"),
                    )?;
                    stdout.flush()?;
                }
            }
            CentralEvent::DeviceLost(addr) => {
                queue!(
                    stdout,
                    PrintStyledContent("[LOST] ".red()),
                    Print(addr_to_string(&central, addr)),
                    Print("\n"),
                )?;
                stdout.flush()?;
                device_status.remove(&addr);
            }
            CentralEvent::DeviceUpdated(addr) => {
                if verbose {
                    queue!(
                        stdout,
                        PrintStyledContent("[UPDATE] ".yellow()),
                        Print(addr_to_string(&central, addr)),
                        Print("\n"),
                    )?;
                    stdout.flush()?;
                }
                let status = device_status
                    .entry(addr)
                    .or_insert(DeviceStatus::Discovered);
                if *status == DeviceStatus::Discovered {
                    let device = central.peripheral(addr).unwrap();
                    let name = device.properties().local_name;
                    if name.is_some() || !filter_unnamed {
                        queue!(
                            stdout,
                            PrintStyledContent("[NEW] ".green()),
                            Print(addr_to_string(&central, addr)),
                            Print("\n"),
                        )?;
                        stdout.flush()?;
                    }
                    *status = DeviceStatus::Updated;
                }
            }
            _ => {}
        };
    }
    central.stop_scan()?;
    Ok(())
}

fn create_central_channel<P: Peripheral, C: Central<P>>(
    central: &C,
) -> c_channel::Receiver<CentralEvent> {
    let (sender, receiver) = unbounded();

    let bt_receiver = central.event_receiver().unwrap();
    thread::spawn(move || {
        while let Ok(data) = bt_receiver.recv() {
            if sender.send(data).is_err() {
                break;
            }
        }
    });

    receiver
}

fn create_ctrlc_channel() -> Result<c_channel::Receiver<()>, Error> {
    let (sender, receiver) = unbounded();
    ctrlc::set_handler(move || {
        let _ = sender.send(());
    })?;
    Ok(receiver)
}

fn create_prompt_channel(
    sync_receiver: c_channel::Receiver<()>,
) -> c_channel::Receiver<Result<String, Error>> {
    let (sender, receiver) = unbounded();
    thread::spawn(move || loop {
        let input = dialoguer::Input::<String>::with_theme(&CustomPromptCharacterTheme::new(' '))
            .with_prompt(">")
            .validate_with(|input: &str| -> Result<(), &str> {
                if !input.starts_with("AT") && input != "quit" {
                    Err("Invalid Input, can only be AT command or quit")
                } else {
                    Ok(())
                }
            })
            .interact();
        match input {
            Ok(command) => {
                if sender.send(Ok(command)).is_err() {
                    break;
                }
            }
            Err(err) => {
                let _ = sender.send(Err(Error::from(err)));
                break;
            }
        }
        if sync_receiver.recv().is_err() {
            break;
        }
    });
    receiver
}

fn find_device<P: Peripheral, C: Central<P>>(
    central: &C,
    device_addr: &BDAddr,
    bt_receiver: &c_channel::Receiver<CentralEvent>,
    ctclc_receiver: &c_channel::Receiver<()>,
) -> Result<Option<P>, Error> {
    loop {
        c_channel::select! {
            recv(bt_receiver) -> event => {
                let event = event.or(Err(Error::AdapterStopped))?;
                if let CentralEvent::DeviceUpdated(addr) = event {
                    if  addr == *device_addr {
                        return Ok(Some(central.peripheral(addr).unwrap()));
                    }
                }
            },
            recv(ctclc_receiver) -> _ => {
                break;
            }
        }
    }
    Ok(None)
}

fn keep_connect<P: Peripheral>(device: &P) -> Result<(), Error> {
    while !device.is_connected() {
        let result = device.connect();
        if let Err(err) = result {
            match err {
                btleplug::Error::NotConnected => (),
                _ => {
                    return Err(Error::from(err));
                }
            }
        }
    }
    Ok(())
}

fn run_console<P: Peripheral>(
    bt_receiver: c_channel::Receiver<CentralEvent>,
    ctclc_receiver: c_channel::Receiver<()>,
    device: P,
) -> Result<(), Error> {
    println!("Connecting to {}", device.address());
    keep_connect(&device)?;
    let properties = device.properties();
    let name = format!(
        "{} {}",
        properties.address,
        properties.local_name.unwrap_or("<Unnamed>".to_string())
    );
    println!("Connected: {}", name);
    let characteristics = device.discover_characteristics()?;
    if !characteristics.iter().any(|c| c.uuid == UUID_NOTIFY) {
        return Err(Error::NotHMDevice);
    }

    device.on_notification(Box::new(|notification: ValueNotification| {
        let value = notification.value.clone();
        if value.len() > 0 {
            println!(
                "{}",
                match String::from_utf8(value) {
                    Ok(s) => s,
                    Err(_) => format!("Failed to decode message: {:x?}", notification.value),
                }
            );
        }
    }));
    let notify_service = characteristics
        .iter()
        .find(|c| c.uuid == UUID_NOTIFY)
        .unwrap();

    device.subscribe(&notify_service)?;

    let (sync_sender, sync_receiver) = unbounded();
    let prompt_receiver = create_prompt_channel(sync_receiver);

    loop {
        c_channel::select! {
            recv(bt_receiver) -> event => {
                let event = event.or(Err(Error::AdapterStopped))?;
                match event {
                      CentralEvent::DeviceLost(addr)
                    | CentralEvent::DeviceDisconnected(addr)
                    if addr == device.address() => {
                        println!("Device disconnected!");
                        break;
                    }
                    _ => (),
                }
            },
            recv(ctclc_receiver) -> _ => {
                device.disconnect()?;
                break;
            },
            recv(prompt_receiver) -> command => {
                let command = command.or(Err(Error::Unknown))??;
                if &command[..] == "quit" {
                    device.disconnect()?;
                    break;
                }
                for chunk in command.as_bytes().chunks(20) {
                    device.command(&notify_service, chunk)?;
                }
                thread::sleep(Duration::from_millis(10));
                sync_sender.send(()).unwrap();
            }
        }
    }
    Ok(())
}

fn run_connect(addr: &str) -> Result<(), Error> {
    let device_addr = BDAddr::from_str(addr)?;

    let manager = Manager::new()?;
    let central = get_central(&manager)?;

    central.start_scan()?;

    let bt_receiver = create_central_channel(&central);
    let ctrlc_receiver = create_ctrlc_channel()?;

    println!("Scanning for {}", device_addr);
    let device = find_device(&central, &device_addr, &bt_receiver, &ctrlc_receiver)?;
    central.stop_scan()?;
    if let Some(device) = device {
        run_console(bt_receiver, ctrlc_receiver, device)?;
    }
    println!("Bye!");

    Ok(())
}

fn main() -> Result<(), Error> {
    let cmd = App::new("hm-remote")
        .version("0.1")
        .author("Youmu")
        .about("Remote AT console for HM series BLE device")
        .setting(AppSettings::ArgRequiredElseHelp)
        .subcommand(
            SubCommand::with_name("scan")
                .about("Scans BLE devices")
                .arg(
                    Arg::with_name("verbose")
                        .short("v")
                        .long("verbose")
                        .help("Displays BLE device update"),
                )
                .arg(
                    Arg::with_name("filter-unnamed")
                        .short("f")
                        .long("filter-unnamed")
                        .help("Only displays BLE device with a name"),
                ),
        )
        .subcommand(
            SubCommand::with_name("connect")
                .about("Connects to a BLE device")
                .arg(
                    Arg::with_name("ADDRESS")
                        .required(true)
                        .help("The MAC address of the device to connect"),
                ),
        )
        .get_matches();

    if let Some(matches) = cmd.subcommand_matches("scan") {
        let (verbose, filter_unnamed) = (
            matches.is_present("verbose"),
            matches.is_present("filter-unnamed"),
        );
        run_scan(verbose, filter_unnamed)
    } else if let Some(matches) = cmd.subcommand_matches("connect") {
        run_connect(matches.value_of("ADDRESS").unwrap())
    } else {
        unreachable!()
    }
}
