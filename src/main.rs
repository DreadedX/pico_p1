#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use core::cell::RefCell;

use embassy_boot_rp::{AlignedBuffer, BlockingFirmwareUpdater, FirmwareUpdaterConfig};
use heapless::{String, Vec};
use rand::{
    rngs::{SmallRng, StdRng},
    RngCore, SeedableRng,
};

use cyw43::PowerManagementMode;
use cyw43_pio::PioSpi;
use embassy_executor::Spawner;
use embassy_futures::{
    select::{select3, Either3},
    yield_now,
};
use embassy_net::{dns::DnsQueryType, tcp::TcpSocket, Config, Stack, StackResources};
use embassy_rp::{
    bind_interrupts,
    clocks::RoscRng,
    flash::{Flash, WRITE_SIZE},
    gpio::{Level, Output},
    peripherals::{DMA_CH1, PIN_23, PIN_25, PIO0, UART0},
    pio::{self, Pio},
    uart::{self, Async, Parity, UartRx},
};
use embassy_sync::{
    blocking_mutex::{raw::NoopRawMutex, Mutex},
    channel::{Channel, Sender},
};
use embassy_time::{Duration, Ticker, Timer};

use dsmr5::Readout;
use nourl::Url;
use rust_mqtt::{
    client::{
        client::MqttClient,
        client_config::{ClientConfig, MqttVersion},
    },
    packet::v5::publish_packet::QualityOfService,
};
use serde::Deserialize;
use static_cell::make_static;

use const_format::formatcp;
use defmt::*;

use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    UART0_IRQ => uart::InterruptHandler<UART0>;
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
});

const ID: &str = env!("ID");
const TOPIC_BASE: &str = formatcp!("pico/{}", ID);
const TOPIC_STATUS: &str = formatcp!("{}/status", TOPIC_BASE);
const TOPIC_UPDATE: &str = formatcp!("{}/update", TOPIC_BASE);
const VERSION: &str = git_version::git_version!();
const PUBLIC_SIGNING_KEY: &[u8] = include_bytes!("../key.pub");

const FLASH_SIZE: usize = 2 * 1024 * 1024;

#[derive(Deserialize)]
struct UpdateMessage<'a> {
    url: &'a str,
}

impl UpdateMessage<'_> {
    fn get_url(&self) -> String<1024> {
        self.url.into()
    }
}

#[embassy_executor::task]
async fn wifi_task(
    runner: cyw43::Runner<
        'static,
        Output<'static, PIN_23>,
        PioSpi<'static, PIN_25, PIO0, 0, DMA_CH1>,
    >,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    stack.run().await
}

async fn get_readout(rx: &mut UartRx<'static, UART0, Async>) -> Readout {
    let mut buffer: Vec<u8, 2048> = Vec::new();
    buffer.push(b'/').unwrap();

    let mut byte = [0; 1];
    debug!("Waiting for next telegram...");
    loop {
        rx.read(&mut byte).await.unwrap();

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
            rx.read(&mut crc).await.unwrap();

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
    mut rx: UartRx<'static, UART0, Async>,
    sender: Sender<'static, NoopRawMutex, dsmr5::state::State, 1>,
) {
    info!("Wating for serial data");
    loop {
        let readout = get_readout(&mut rx).await;
        let telegram = &readout.to_telegram().unwrap();
        let state = telegram.try_into().unwrap();

        match sender.try_send(state) {
            Ok(_) => {}
            Err(_) => warn!("Queue is full!"),
        }
    }
}

/// Get the cyw43 firmware blobs
///
/// # Safety
/// When building without `include_firmwares` make sure to flash the firmwares using the following
/// commands:
/// ```bash
/// probe-rs download firmware/43439A0.bin --format bin --chip RP2040 --base-address 0x101BE000
/// probe-rs download firmware/43439A0_clm.bin --format bin --chip RP2040 --base-address 0x101FE000
/// ```
unsafe fn get_firmware() -> (&'static [u8], &'static [u8]) {
    cfg_if::cfg_if! {
        if #[cfg(feature = "include_firmwares")] {
            let fw = include_bytes!("../firmware/43439A0.bin");
            let clm = include_bytes!("../firmware/43439A0_clm.bin");

            (fw, clm)
        } else {
            // TODO: It would be nice if it could automatically get the correct size
            extern "C" {
                #[link_name = "__fw_start"]
                static fw: [u8; 230321];
                #[link_name = "__clm_start"]
                static clm: [u8; 4752];
            }

            (&fw, &clm)
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Starting...");
    let p = embassy_rp::init(Default::default());

    let channel = make_static!(Channel::<NoopRawMutex, _, 1>::new());

    // === UART ===
    let mut config = uart::Config::default();
    config.parity = Parity::ParityNone;
    #[cfg(feature = "invert_rx")]
    {
        config.invert_rx = true;
    }

    let rx = UartRx::new(p.UART0, p.PIN_17, Irqs, p.DMA_CH0, config);

    spawner.spawn(uart_rx_task(rx, channel.sender())).unwrap();

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);

    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH1,
    );

    let (fw, clm) = unsafe { get_firmware() };

    let state = make_static!(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    spawner.spawn(wifi_task(runner)).unwrap();

    control.init(clm).await;
    control
        .set_power_management(PowerManagementMode::PowerSave)
        .await;

    // Turn LED on while trying to connect
    control.gpio_set(0, true).await;

    let config = Config::dhcpv4(Default::default());

    // Use the Ring Oscillator of the RP2040 as a source of true randomness to seed the
    // cryptographically secure PRNG
    let mut rng = StdRng::from_rng(&mut RoscRng).unwrap();

    let stack = make_static!(Stack::new(
        net_device,
        config,
        make_static!(StackResources::<6>::new()),
        rng.next_u64(),
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

    info!("Waiting for DHCP...");
    let cfg = wait_for_config(stack).await;
    info!("IP Address: {}", cfg.address.address());

    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];

    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    // socket.set_timeout(Some(Duration::from_secs(10)));
    let url = Url::parse(env!("MQTT_ADDRESS")).unwrap();
    debug!("MQTT URL: {}", url);
    let ip = stack.dns_query(url.host(), DnsQueryType::A).await.unwrap()[0];
    let addr = (ip, url.port_or_default());
    debug!("MQTT ADDR: {}", addr);

    while let Err(e) = socket.connect(addr).await {
        warn!("Connect error: {:?}", e);
        Timer::after(Duration::from_secs(1)).await;
    }
    info!("TCP Connected!");

    let up = updater::Updater::new(TOPIC_STATUS, TOPIC_UPDATE, VERSION, PUBLIC_SIGNING_KEY);

    let mut config = ClientConfig::new(
        MqttVersion::MQTTv5,
        // Use fast and simple PRNG to generate packet identifiers, there is no need for this to be
        // cryptographically secure
        SmallRng::from_rng(&mut RoscRng).unwrap(),
    );

    config.add_username(env!("MQTT_USERNAME"));
    config.add_password(env!("MQTT_PASSWORD"));
    config.add_max_subscribe_qos(QualityOfService::QoS1);
    config.add_client_id(ID);
    up.add_will(&mut config);

    let mut recv_buffer = [0; 1024];
    let mut write_buffer = [0; 1024];

    let mut client =
        MqttClient::<_, 5, _>::new(socket, &mut write_buffer, &mut recv_buffer, config);

    info!("Connecting to MQTT...");
    client.connect_to_broker().await.unwrap();
    info!("MQTT Connected!");

    // TODO: Ideally we use async flash
    // This has issues with alignment right now
    let flash = Flash::<_, _, FLASH_SIZE>::new_blocking(p.FLASH);
    let flash = Mutex::new(RefCell::new(flash));

    let config = FirmwareUpdaterConfig::from_linkerfile_blocking(&flash);
    let mut aligned = AlignedBuffer([0; WRITE_SIZE]);
    let mut updater = BlockingFirmwareUpdater::new(config, &mut aligned.0);

    // We wait with marking as booted until everything is connected
    updater.mark_booted().unwrap();

    up.ready(&mut client).await;

    // Turn LED off when connected
    control.gpio_set(0, false).await;

    let mut keep_alive = Ticker::every(Duration::from_secs(30));
    let receiver = channel.receiver();
    loop {
        match select3(
            keep_alive.next(),
            receiver.receive(),
            client.receive_message(),
        )
        .await
        {
            Either3::First(_) => client.send_ping().await.unwrap(),
            Either3::Second(state) => {
                // Blink the LED to show that a readout was received
                control.gpio_set(0, true).await;
                control.gpio_set(0, false).await;

                let msg: Vec<u8, 4096> = serde_json_core::to_vec(&state)
                    .expect("The buffer should be large enough to contain all the data");

                client
                    .send_message(TOPIC_BASE, &msg, QualityOfService::QoS1, false)
                    .await
                    .unwrap();
            }
            Either3::Third(message) => {
                let message = match message {
                    Ok(message) => message,
                    Err(err) => {
                        error!("Failed to receive MQTT message: {}", err);
                        continue;
                    }
                };

                let message = match serde_json_core::from_slice::<UpdateMessage>(message.1) {
                    Ok((message, _)) => message,
                    Err(_) => {
                        error!("Unable to parse update message");
                        continue;
                    }
                };

                let url = message.get_url();
                let url = Url::parse(url.as_str()).unwrap();
                up.update(stack, &mut updater, &mut rng, &mut client, url).await;
            }
        }
    }
}

async fn wait_for_config(
    stack: &'static Stack<cyw43::NetDriver<'static>>,
) -> embassy_net::StaticConfigV4 {
    loop {
        // We are essentially busy looping here since there is no Async API for this
        if let Some(config) = stack.config_v4() {
            return config;
        }

        yield_now().await;
    }
}

