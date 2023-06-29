#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use cyw43_pio::PioSpi;
use defmt::{debug, info, warn, Format};
use dsmr5::Readout;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_net::{tcp::TcpSocket, Ipv4Address, Ipv4Cidr, Stack, StackResources};
use embassy_rp::{
    bind_interrupts, gpio,
    peripherals::{DMA_CH0, PIN_23, PIN_25, PIO0, UART0},
    pio::Pio,
    uart::{self, BufferedUartRx, Parity},
};

use embedded_io::asynch::Read;

use embassy_sync::{
    blocking_mutex::raw::NoopRawMutex,
    channel::{Channel, Sender},
};
use gpio::{Level, Output};
use heapless::Vec;
use static_cell::make_static;

use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    UART0_IRQ => uart::BufferedInterruptHandler<UART0>;
});

#[derive(Format)]
struct Test {
    counter: u32,
}

// #[embassy_executor::task]
// async fn usb_task(mut device: UsbDevice<'static, Driver<'static, peripherals::USB>>) -> ! {
//     device.run().await
// }

// #[embassy_executor::task]
// async fn echo_task(
//     mut class: CdcAcmClass<'static, Driver<'static, peripherals::USB>>,
//     sender: Sender<'static, NoopRawMutex, Message, 1>,
// ) -> ! {
//     loop {
//         class.wait_connection().await;
//         info!("Connected");
//         let _ = echo(&mut class, &sender).await;
//         info!("Disconnected");
//     }
// }

#[embassy_executor::task]
async fn wifi_task(
    runner: cyw43::Runner<
        'static,
        Output<'static, PIN_23>,
        PioSpi<'static, PIN_25, PIO0, 0, DMA_CH0>,
    >,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    stack.run().await
}

async fn get_readout(rx: &mut BufferedUartRx<'static, UART0>) -> Readout {
    let mut buffer: Vec<u8, 2048> = Vec::new();
    buffer.push(b'/').unwrap();

    let mut byte = [0; 1];
    debug!("Waiting for next telegram...");
    loop {
        rx.read_exact(&mut byte).await.unwrap();

        if byte[0] == b'/' {
            break;
        }
    }

    debug!("Start of telegram detected");

    loop {
        rx.read(&mut byte).await.unwrap();
        buffer.push(byte[0]).unwrap();

        if byte[0] == b'!' {
            debug!("Start of CRC detected");

            let mut crc = [0; 4];
            rx.read_exact(&mut crc).await.unwrap();

            buffer.extend_from_slice(&crc).unwrap();

            debug!("Received telegram");

            // Fill the rest of the buffer with zeroes
            buffer.resize(2048, 0).unwrap();

            return Readout {
                buffer: buffer.into_array().unwrap(),
            };
        }
    }
}

#[embassy_executor::task]
async fn uart_rx_task(
    mut rx: BufferedUartRx<'static, UART0>,
    sender: Sender<'static, NoopRawMutex, Readout, 1>,
) {
    info!("Wating for serial data");
    loop {
        let readout = get_readout(&mut rx).await;
        match sender.try_send(readout) {
            Ok(_) => {}
            Err(_) => warn!("Queue is full!"),
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Hello world!");
    let p = embassy_rp::init(Default::default());

    let channel = make_static!(Channel::<NoopRawMutex, Readout, 1>::new());

    // === UART ===
    let mut config = uart::Config::default();
    config.parity = Parity::ParityNone;
    // config.invert_rx = true;

    let buf = make_static!([0u8; 2048]);
    let rx = BufferedUartRx::new_with_rts(p.UART0, Irqs, p.PIN_1, p.PIN_3, buf, config);

    spawner.spawn(uart_rx_task(rx, channel.sender())).unwrap();

    // === USB SERIAL ===
    // let driver = Driver::new(p.USB, Irqs);
    //
    // let mut config = Config::new(0xc0de, 0xcafe);
    // config.manufacturer = Some("huizinga.dev");
    // config.product = Some("Raspberry Pi Pico");
    // config.serial_number = Some("123456789");
    // config.max_power = 0;
    // config.max_packet_size_0 = 64;
    //
    // // Needed for windows compatiblilty
    // config.device_class = 0xEF;
    // config.device_sub_class = 0x02;
    // config.device_protocol = 0x01;
    // config.composite_with_iads = true;
    //
    // let mut builder = Builder::new(
    //     driver,
    //     config,
    //     singleton!([0; 256]),
    //     singleton!([0; 256]),
    //     singleton!([0; 256]),
    //     singleton!([0; 64]),
    // );
    //
    // let class = CdcAcmClass::new(&mut builder, singleton!(State::new()), 64);
    // let usb = builder.build();
    //
    // spawner.spawn(usb_task(usb)).unwrap();
    // spawner.spawn(echo_task(class, channel.sender())).unwrap();

    // === WIFI ===
    // To make flashing faster for development, you may want to flash the firmwares independently
    // at hardcoded addresses, instead of baking them into the program with `include_bytes!`:
    //     probe-rs-cli download 43439A0.bin --format bin --chip RP2040 --base-address 0x10100000
    //     probe-rs-cli download 43439A0_clm.bin --format bin --chip RP2040 --base-address 0x10140000
    let fw = unsafe { core::slice::from_raw_parts(0x10100000 as *const u8, 224190) };
    let clm = unsafe { core::slice::from_raw_parts(0x10140000 as *const u8, 4752) };
    // let fw = include_bytes!("../firmware/43439A0.bin");
    // let clm = include_bytes!("../firmware/43439A0_clm.bin");

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);

    let mut pio = Pio::new(p.PIO0);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );

    let state = make_static!(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    spawner.spawn(wifi_task(runner)).unwrap();

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    let ip = Ipv4Address::new(10, 0, 0, 77);

    // let config = embassy_net::Config::Dhcp(Default::default());
    let config = embassy_net::Config::Static(embassy_net::StaticConfig {
        address: Ipv4Cidr::new(ip, 24),
        dns_servers: Vec::new(),
        gateway: Some(Ipv4Address::new(10, 0, 0, 1)),
    });

    let seed = 0x51ac_3101_6468_8cdf;
    let stack = make_static!(Stack::new(
        net_device,
        config,
        make_static!(StackResources::<2>::new()),
        seed,
    ));

    spawner.spawn(net_task(stack)).unwrap();

    // Connect to wifi
    loop {
        match control
            .join_wpa2(env!("WIFI_NETWORK"), env!("WIFI_PASSWORD"))
            .await
        {
            Ok(_) => break,
            Err(err) => {
                info!("Failed to join with status = {}", err.status)
            }
        }
    }

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    let receiver = channel.receiver();

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        // socket.set_timeout(Some(Duration::from_secs(10)));

        control.gpio_set(0, true).await;
        let port = 1234;
        info!("Listening on {}:{}...", ip, port);

        if let Err(e) = socket.accept(port).await {
            warn!("Accept error: {:?}", e);
            continue;
        }

        info!("Received connection from {:?}", socket.remote_endpoint());
        control.gpio_set(0, false).await;

        socket.write_all("Connected!\n".as_bytes()).await.unwrap();

        loop {
            match select(socket.read(&mut [0; 64]), receiver.recv()).await {
                Either::First(Ok(0)) => {
                    warn!("Read OEF");
                    break;
                }
                Either::First(Ok(_)) => {
                    // Received something over the socket, currently does nothing
                }
                Either::First(Err(e)) => {
                    warn!("Read error: {:?}", e);
                }
                Either::Second(readout) => {
                    let telegram = readout.to_telegram().unwrap();

                    debug!("checksum: {}", telegram.checksum);
                    debug!("prefix: {}", telegram.prefix);
                    debug!("identification: {}", telegram.identification);

                    let state = dsmr5::Result::<dsmr5::state::State>::from(&telegram).unwrap();
                    debug!("datetime: {}", state.datetime.unwrap().year);
                    debug!("meterreadings[0]: {}", state.meterreadings[0].to.unwrap());
                    debug!("meterreadings[1]: {}", state.meterreadings[1].to.unwrap());

                    debug!("slave:");
                    debug!("\tdevice_type: {}", state.slaves[0].device_type.unwrap());
                    debug!(
                        "\tmeter_reading: {}",
                        state.slaves[0].meter_reading.as_ref().unwrap().1
                    );

                }
                // Either::Second(message) => match socket.write(&message).await {
                //     Ok(_) => {}
                //     Err(e) => {
                //         warn!("Write error: {:?}", e);
                //         break;
                //     }
                // },
            }
        }
    }
}

// struct Disconnected;
//
// impl From<EndpointError> for Disconnected {
//     fn from(value: EndpointError) -> Self {
//         match value {
//             EndpointError::BufferOverflow => panic!("Buffer overflow"),
//             EndpointError::Disabled => Self {},
//         }
//     }
// }
//
// /// Echo back received packets
// async fn echo<'d>(
//     class: &mut CdcAcmClass<'d, Driver<'d, peripherals::USB>>,
//     sender: &Sender<'static, NoopRawMutex, Message, 1>,
// ) -> Result<(), Disconnected> {
//     let mut buf = [0; 64];
//     loop {
//         let n = class.read_packet(&mut buf).await?;
//         let data = from_utf8_mut(&mut buf[..n]).unwrap();
//
//         data.make_ascii_uppercase();
//
//         sender
//             .send(Message {
//                 data: buf,
//                 length: n,
//             })
//             .await;
//     }
// }
